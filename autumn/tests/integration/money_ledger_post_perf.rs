//! Ledger perf harness for `money::ledger::post`: statements, buffers and WAL
//! per post, read from `pg_stat_statements`, against a production-shaped
//! books fixture. The record is `docs/perf/money-ledger-post/`.
//!
//! **Requires Docker** unless `AUTUMN_PERF_PG_URL` names a Postgres 16 server
//! that preloads `pg_stat_statements` (for a host without Docker). CI's
//! `--ignored` sweep runs it at the default size. Run it by hand with:
//!
//! ```text
//! LEDGER_PERF_TXNS=200000 cargo test -p autumn-web --test integration_tests \
//!   -- --ignored money_ledger_post_profile --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! `LEDGER_PERF_TXNS` posted transactions (default 20,000), and one user
//! wallet for every 25 of them, plus four house accounts. Every transaction is
//! a wallet top-up (debit the wallet, credit `house:cash`) or a purchase
//! (credit the wallet, debit `house:revenue`, and for 30% of them a fee line
//! to `house:fees`). Wallets are picked with a cubic skew, so a few wallets
//! hold most of the history and the house accounts hold all of it. 5% of the
//! wallets have a `NULL` stored balance, as rows that predate the stored
//! balance do; making them `NULL` leaves dead tuples in the accounts table.
//! Then `VACUUM ANALYZE`.
//!
//! ## Workload
//!
//! `WORKLOAD_OPS` calls of the public `ledger::post`, each in its own
//! transaction, the way a wallet service calls it from a request: 55% top-ups,
//! 25% purchases (2 or 3 lines), 10% marketplace splits (one wallet pays five
//! sellers and a fee: 7 lines), and 10% retries of an earlier key, which
//! replay. Every input is derived from the operation index, not from a clock
//! or a random source, so two runs post the same money and the row dump at the
//! end is comparable byte for byte across a code change.

#![cfg(feature = "db")]

use std::fmt::Write as _;

use autumn_web::money::ledger::{self, IdempotencyKey, PostOutcome, Posting, Transaction};
use autumn_web::money::{Money, Usd};
use diesel::sql_types::{BigInt, Double, Nullable, Text};
use diesel_async::{
    AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _,
};
use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::postgres::Postgres;

const LEDGER_UP: &str = include_str!("../../migrations/20260917120000_create_money_ledger/up.sql");
const BALANCE_UP: &str =
    include_str!("../../migrations/20261029183512_add_money_account_balance/up.sql");

/// Operations the workload runs against the fixture.
const WORKLOAD_OPS: usize = 400;

const fn usd(minor: i64) -> Money<Usd> {
    Money::<Usd>::from_minor(minor)
}

fn fixture_txns() -> u64 {
    std::env::var("LEDGER_PERF_TXNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

/// Seed the books with SQL, not through `post`: the fixture is history, and
/// writing it one `post` at a time would take minutes for no extra realism.
/// The balance trigger still fires on these inserts, so the stored balances
/// are the true ones.
async fn seed_fixture(conn: &mut AsyncPgConnection, txns: u64) {
    let users = (txns / 25).max(100);
    // Wallet index for transaction `t`: a deterministic hash in [0, 1), cubed,
    // so low indices are hot.
    let wallet = format!(
        "'user:' || lpad((1 + floor({users} * power(((t * 2654435761) % 1000003) / 1000003.0, 3)))::int::text, 6, '0')"
    );
    let sql = format!(
        "INSERT INTO _autumn_money_accounts (id, currency) \
           SELECT 'house:' || n, 'USD' FROM unnest(ARRAY['cash', 'revenue', 'fees', 'sellers']) n;
         INSERT INTO _autumn_money_accounts (id, currency, allow_negative) \
           SELECT 'user:' || lpad(i::text, 6, '0'), 'USD', FALSE FROM generate_series(1, {users}) i;
         INSERT INTO _autumn_money_accounts (id, currency) \
           SELECT 'seller:' || lpad(i::text, 4, '0'), 'USD' FROM generate_series(1, 500) i;
         INSERT INTO _autumn_money_transactions (id, idempotency_key, request_hash, currency, memo, posted_at) \
           SELECT md5('tx' || t), 'seed:' || t, md5('h' || t), 'USD', \
                  CASE WHEN t % 3 = 0 THEN '' ELSE 'order ' || t END, \
                  TIMESTAMPTZ '2025-01-01 00:00:00Z' + (t || ' seconds')::interval \
             FROM generate_series(1, {txns}) t;
         -- 60% top-ups: debit the wallet, credit cash.
         INSERT INTO _autumn_money_postings (transaction_id, seq, account_id, amount_minor, currency, posted_at) \
           SELECT md5('tx' || t), s.seq, CASE s.seq WHEN 0 THEN {wallet} ELSE 'house:cash' END, \
                  CASE s.seq WHEN 0 THEN 1 ELSE -1 END * (5000 + t % 20000), 'USD', \
                  TIMESTAMPTZ '2025-01-01 00:00:00Z' + (t || ' seconds')::interval \
             FROM generate_series(1, {txns}) t, (VALUES (0), (1)) s(seq) WHERE t % 10 < 6;
         -- 40% purchases: credit the wallet, debit revenue, 30% of them with a fee line.
         INSERT INTO _autumn_money_postings (transaction_id, seq, account_id, amount_minor, currency, posted_at) \
           SELECT md5('tx' || t), s.seq, \
                  CASE s.seq WHEN 0 THEN {wallet} WHEN 1 THEN 'house:revenue' ELSE 'house:fees' END, \
                  CASE s.seq \
                    WHEN 0 THEN -(1000 + t % 4000) \
                    WHEN 1 THEN (1000 + t % 4000) - CASE WHEN t % 10 = 9 THEN 50 ELSE 0 END \
                    ELSE 50 END, \
                  'USD', TIMESTAMPTZ '2025-01-01 00:00:00Z' + (t || ' seconds')::interval \
             FROM generate_series(1, {txns}) t, (VALUES (0), (1), (2)) s(seq) \
            WHERE t % 10 >= 6 AND (s.seq < 2 OR t % 10 = 9);
         UPDATE _autumn_money_accounts SET balance_minor = NULL \
          WHERE id LIKE 'user:%' AND right(id, 2) IN ('07', '42', '77', '93', '11');
"
    );
    conn.batch_execute(&sql).await.expect("seed the books");
    conn.batch_execute(
        "VACUUM ANALYZE _autumn_money_accounts, _autumn_money_transactions, _autumn_money_postings",
    )
    .await
    .expect("vacuum analyze");
}

/// Wallet for workload operation `i`, with the same cubic skew as the fixture.
fn workload_wallet(i: usize, users: u64) -> String {
    let h = (i as u64).wrapping_mul(2_654_435_761) % 1_000_003;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let idx = 1 + ((users as f64) * (h as f64 / 1_000_003.0).powi(3)).floor() as u64;
    format!("user:{idx:06}")
}

#[allow(clippy::cast_possible_wrap, clippy::arithmetic_side_effects)]
fn operation(i: usize, users: u64) -> Transaction {
    let wallet = workload_wallet(i, users);
    let amount = 500 + (i as i64 % 37) * 25;
    let (postings, memo) = match i % 20 {
        // 55% top-ups.
        0..=10 => (
            vec![
                Posting::debit(wallet.clone(), usd(amount * 20)),
                Posting::credit("house:cash", usd(amount * 20)),
            ],
            format!("top-up {i}"),
        ),
        // 25% purchases, a third of them with a fee line.
        11..=15 => {
            if i % 3 == 0 {
                (
                    vec![
                        Posting::credit(wallet.clone(), usd(amount)),
                        Posting::debit("house:revenue", usd(amount - 50)),
                        Posting::debit("house:fees", usd(50)),
                    ],
                    format!("purchase {i}"),
                )
            } else {
                (
                    vec![
                        Posting::credit(wallet.clone(), usd(amount)),
                        Posting::debit("house:revenue", usd(amount)),
                    ],
                    String::new(),
                )
            }
        }
        // 10% marketplace splits: 7 lines.
        16 | 17 => {
            let mut lines = vec![Posting::credit(wallet.clone(), usd(amount * 5 + 50))];
            for s in 0..5 {
                let seller = format!("seller:{:04}", 1 + (i * 7 + s * 101) % 500);
                lines.push(Posting::debit(seller, usd(amount)));
            }
            lines.push(Posting::debit("house:fees", usd(50)));
            (lines, format!("split {i}"))
        }
        // 10%: a retry of an earlier operation, posted again below.
        _ => return operation(i - 7, users),
    };
    let key = IdempotencyKey::new(format!("op:{i}")).expect("key");
    Transaction::new(key, postings).memo(memo)
}

#[derive(diesel::QueryableByName)]
struct StatRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
    #[diesel(sql_type = BigInt)]
    hit: i64,
    #[diesel(sql_type = BigInt)]
    read: i64,
    #[diesel(sql_type = BigInt)]
    rows: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
    #[diesel(sql_type = Double)]
    wal_bytes: f64,
    #[diesel(sql_type = BigInt)]
    toplevel: i64,
}

#[derive(diesel::QueryableByName)]
struct PlanLine {
    #[diesel(sql_type = Text)]
    #[diesel(column_name = "QUERY PLAN")]
    line: String,
}

#[derive(diesel::QueryableByName)]
struct TableStat {
    #[diesel(sql_type = Text)]
    relname: String,
    #[diesel(sql_type = BigInt)]
    seq_scan: i64,
    #[diesel(sql_type = BigInt)]
    seq_tup_read: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    idx_scan: Option<i64>,
    #[diesel(sql_type = BigInt)]
    n_live_tup: i64,
    #[diesel(sql_type = BigInt)]
    n_dead_tup: i64,
}

#[derive(diesel::QueryableByName)]
struct DumpRow {
    #[diesel(sql_type = Text)]
    line: String,
}

/// Everything the workload wrote, minus the random transaction ids and the
/// clock: by key, each transaction's currency, memo and lines in order, then
/// every account's stored balance.
async fn dump_books(conn: &mut AsyncPgConnection) -> Vec<String> {
    diesel::sql_query(
        "SELECT t.idempotency_key || ' ' || t.currency || ' [' || t.memo || '] ' || \
                string_agg(p.seq || ':' || p.account_id || '=' || p.amount_minor, ' ' ORDER BY p.seq) AS line \
           FROM _autumn_money_transactions t JOIN _autumn_money_postings p ON p.transaction_id = t.id \
          WHERE t.idempotency_key LIKE 'op:%' GROUP BY t.idempotency_key, t.currency, t.memo \
         UNION ALL \
         SELECT 'balance ' || id || ' ' || COALESCE(balance_minor::text, 'NULL') FROM _autumn_money_accounts \
         ORDER BY 1",
    )
    .load::<DumpRow>(conn)
    .await
    .expect("dump the books")
    .into_iter()
    .map(|r| r.line)
    .collect()
}

async fn explain(conn: &mut AsyncPgConnection, title: &str, sql: &str, out: &mut String) {
    let _ = writeln!(out, "\n=== EXPLAIN {title} ===\n{sql}");
    conn.batch_execute("BEGIN").await.expect("begin");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL) {sql}"
    ))
    .load::<PlanLine>(conn)
    .await
    .expect("explain");
    conn.batch_execute("ROLLBACK").await.expect("rollback");
    for l in lines {
        let _ = writeln!(out, "{}", l.line);
    }
}

async fn connect() -> (String, Option<testcontainers::ContainerAsync<Postgres>>) {
    if let Ok(url) = std::env::var("AUTUMN_PERF_PG_URL") {
        return (url, None);
    }
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
            "-c",
            "pg_stat_statements.max=2000",
        ])
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        Some(container),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)] // one linear profiling script, clearest unsplit
async fn money_ledger_post_profile() {
    let (url, _container) = connect().await;
    let mut admin = AsyncPgConnection::establish(&url).await.expect("connect");
    let db = format!("ledger_post_perf_{}", std::process::id());
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {db}"))
        .await
        .expect("drop a stale database");
    admin
        .batch_execute(&format!("CREATE DATABASE {db}"))
        .await
        .expect("create the database");
    let db_url = match url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{db}"),
        None => panic!("url has no database"),
    };
    let mut conn = AsyncPgConnection::establish(&db_url)
        .await
        .expect("connect");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("pg_stat_statements");
    conn.batch_execute(LEDGER_UP)
        .await
        .expect("ledger migration");
    conn.batch_execute(BALANCE_UP)
        .await
        .expect("balance migration");

    let txns = fixture_txns();
    let users = (txns / 25).max(100);
    seed_fixture(&mut conn, txns).await;
    // A NULL wallet that the workload posts to, so the summing path runs.
    conn.batch_execute(
        "UPDATE _autumn_money_accounts SET balance_minor = NULL WHERE id = 'user:000001'",
    )
    .await
    .expect("null one hot wallet");
    conn.batch_execute("VACUUM ANALYZE _autumn_money_accounts")
        .await
        .expect("vacuum analyze");

    let mut out = String::new();
    let _ = writeln!(
        out,
        "fixture: {txns} transactions, {users} wallets + 500 sellers + 4 house accounts, \
         workload: {WORKLOAD_OPS} ledger::post calls"
    );

    // Only this database's statements, so a shared server's other traffic
    // does not land in the profile.
    conn.batch_execute("SELECT pg_stat_statements_reset(0, (SELECT oid FROM pg_database WHERE datname = current_database()), 0)")
        .await
        .expect("reset");
    let wal_before: f64 = diesel::sql_query("SELECT (pg_current_wal_lsn() - '0/0')::float8 AS v")
        .load::<WalRow>(&mut conn)
        .await
        .expect("lsn")[0]
        .v;

    let (mut posted, mut replayed, mut refused) = (0usize, 0usize, 0usize);
    let mut returned = Vec::with_capacity(WORKLOAD_OPS);
    for i in 0..WORKLOAD_OPS {
        let transfer = operation(i, users);
        let outcome = conn
            .transaction::<_, ledger::LedgerError, _>(async |c| ledger::post(c, &transfer).await)
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            // A wallet without the funds: the refusal is part of the workload.
            Err(ledger::LedgerError::NegativeBalance { account, .. }) => {
                refused += 1;
                returned.push(format!("{} refused: {account}", transfer.key()));
                continue;
            }
            Err(err) => panic!("op {i}: {err}"),
        };
        match &outcome {
            PostOutcome::Posted(_) => posted += 1,
            PostOutcome::Replayed(_) => replayed += 1,
        }
        let t = outcome.transaction();
        let mut line = format!("{} {} [{}]", t.key(), t.currency().code(), t.memo());
        for p in t.postings() {
            let _ = write!(
                line,
                " {}{}={}",
                if p.is_debit() { "+" } else { "-" },
                p.account_id(),
                p.amount().minor()
            );
        }
        returned.push(line);
    }

    let wal_after: f64 = diesel::sql_query("SELECT (pg_current_wal_lsn() - '0/0')::float8 AS v")
        .load::<WalRow>(&mut conn)
        .await
        .expect("lsn")[0]
        .v;

    let stats = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                shared_blks_hit AS hit, shared_blks_read AS read, rows, \
                temp_blks_written AS temp_written, wal_bytes::float8 AS wal_bytes, \
                toplevel::int::int8 AS toplevel \
           FROM pg_stat_statements \
          WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
            AND query NOT LIKE '%pg_stat_statements%' AND query NOT LIKE '%pg_current_wal_lsn%' \
          ORDER BY buffers DESC, calls DESC, query",
    )
    .load::<StatRow>(&mut conn)
    .await
    .expect("read pg_stat_statements");

    let _ = writeln!(
        out,
        "posted: {posted}, replayed: {replayed}, refused: {refused}"
    );
    let top: Vec<&StatRow> = stats.iter().filter(|s| s.toplevel == 1).collect();
    let total_calls: i64 = top.iter().map(|s| s.calls).sum();
    let total_buffers: i64 = top.iter().map(|s| s.buffers).sum();
    let nested_buffers: i64 = stats
        .iter()
        .filter(|s| s.toplevel == 0)
        .map(|s| s.buffers)
        .sum();
    let total_wal: f64 = top.iter().map(|s| s.wal_bytes).sum();
    #[allow(clippy::cast_precision_loss)]
    let per_op = |v: f64| v / WORKLOAD_OPS as f64;
    // `toplevel = false` rows ran inside another statement (the balance
    // trigger, the immediate accounts foreign key) and their buffers are
    // already counted in the statement that fired them. The deferred
    // transactions foreign key runs at COMMIT and is reported as top level.
    let app: Vec<&StatRow> = top
        .iter()
        .copied()
        .filter(|s| {
            !s.query.contains("FOR KEY SHARE OF x") && s.query != "BEGIN" && s.query != "COMMIT"
        })
        .collect();
    let app_calls: i64 = app.iter().map(|s| s.calls).sum();
    let _ = writeln!(
        out,
        "app statements (no BEGIN/COMMIT/FK): {app_calls} ({:.2}/op); all top-level statements: \
         {total_calls} ({:.2}/op); buffers: {total_buffers} ({:.1}/op); statement wal_bytes: \
         {total_wal:.0} ({:.0}/op); wal lsn delta: {:.0} bytes; nested buffers (already in their \
         parent): {nested_buffers}",
        per_op(app_calls as f64),
        per_op(total_calls as f64),
        per_op(total_buffers as f64),
        per_op(total_wal),
        wal_after - wal_before
    );
    #[allow(clippy::cast_precision_loss)]
    let pct = |a: i64, b: i64| {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    };
    let _ = writeln!(
        out,
        "\n--- by buffers (toplevel 1 = issued by the app, 0 = trigger/FK) ---"
    );
    let _ = writeln!(
        out,
        "{:>3} {:>6} {:>7} {:>6} {:>8} {:>6} {:>6} {:>5} {:>9}  query",
        "top", "calls", "buffers", "%buf", "hit", "read", "rows", "temp", "wal_bytes"
    );
    for s in &stats {
        let _ = writeln!(
            out,
            "{:>3} {:>6} {:>7} {:>5.1}% {:>8} {:>6} {:>6} {:>5} {:>9.0}  {}",
            s.toplevel,
            s.calls,
            s.buffers,
            pct(s.buffers, total_buffers),
            s.hit,
            s.read,
            s.rows,
            s.temp_written,
            s.wal_bytes,
            s.query.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
    let mut by_calls: Vec<&&StatRow> = top.iter().collect();
    by_calls.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.query.cmp(&b.query)));
    let _ = writeln!(out, "\n--- app statements by calls ---");
    for s in by_calls {
        let _ = writeln!(
            out,
            "{:>6} {:>5.1}%  {}",
            s.calls,
            pct(s.calls, total_calls),
            s.query.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }

    let tables = diesel::sql_query(
        "SELECT relname::text AS relname, seq_scan, seq_tup_read, idx_scan, n_live_tup, n_dead_tup \
           FROM pg_stat_user_tables ORDER BY seq_tup_read DESC, relname",
    )
    .load::<TableStat>(&mut conn)
    .await
    .expect("pg_stat_user_tables");
    let _ = writeln!(
        out,
        "\n--- pg_stat_user_tables (cumulative, fixture included) ---"
    );
    for t in tables {
        let _ = writeln!(
            out,
            "{:<28} seq_scan={} seq_tup_read={} idx_scan={} live={} dead={}",
            t.relname,
            t.seq_scan,
            t.seq_tup_read,
            t.idx_scan.unwrap_or(0),
            t.n_live_tup,
            t.n_dead_tup
        );
    }

    // Plans of the app's statements, with values the workload used.
    let (tx_id, key): (String, String) = {
        let r = diesel::sql_query(
            "SELECT id AS line FROM _autumn_money_transactions WHERE idempotency_key = 'op:16'",
        )
        .load::<DumpRow>(&mut conn)
        .await
        .expect("op:16")
        .remove(0);
        (r.line, "op:16".to_owned())
    };
    explain(
        &mut conn,
        "lock accounts (7-line split)",
        "SELECT id, currency, allow_negative, balance_minor FROM _autumn_money_accounts \
         WHERE id IN ('house:fees', 'seller:0113', 'seller:0214', 'seller:0315', 'seller:0416', \
         'seller:0017', 'user:000001') ORDER BY id FOR UPDATE",
        &mut out,
    )
    .await;
    explain(
        &mut conn,
        "transaction by key",
        &format!(
            "SELECT id, idempotency_key, request_hash, currency, memo, CAST(posted_at AS TEXT) AS posted_at \
             FROM _autumn_money_transactions WHERE idempotency_key = '{key}'"
        ),
        &mut out,
    )
    .await;
    explain(
        &mut conn,
        "postings of a transaction",
        &format!(
            "SELECT account_id, amount_minor, currency FROM _autumn_money_postings \
             WHERE transaction_id = '{tx_id}' ORDER BY seq"
        ),
        &mut out,
    )
    .await;
    explain(
        &mut conn,
        "sum postings of a NULL-balance hot wallet",
        "SELECT account_id, CAST(COALESCE(SUM(amount_minor / 1000000000), 0) AS BIGINT) AS hi, \
         CAST(COALESCE(SUM(amount_minor % 1000000000), 0) AS BIGINT) AS lo \
         FROM _autumn_money_postings WHERE account_id IN ('user:000002') GROUP BY account_id",
        &mut out,
    )
    .await;
    explain(
        &mut conn,
        "insert 7 postings (fires the balance trigger)",
        "INSERT INTO _autumn_money_postings (transaction_id, seq, account_id, amount_minor, currency, posted_at) VALUES \
         ('explain', 0, 'user:000003', -100, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 1, 'seller:0001', 10, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 2, 'seller:0002', 10, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 3, 'seller:0003', 10, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 4, 'seller:0004', 10, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 5, 'seller:0005', 10, 'USD', CURRENT_TIMESTAMP), \
         ('explain', 6, 'house:fees', 50, 'USD', CURRENT_TIMESTAMP)",
        &mut out,
    )
    .await;

    // Equivalence: what `post` returned, and what the books hold.
    let _ = writeln!(out, "\n--- returned outcomes ---");
    for line in &returned {
        let _ = writeln!(out, "{line}");
    }
    let dump = dump_books(&mut conn).await;
    let mut digest = sha2_hex(&dump.join("\n"));
    digest.truncate(16);
    let _ = writeln!(
        out,
        "\n--- books dump: {} lines, sha256 {digest} ---",
        dump.len()
    );
    for line in dump.iter().filter(|l| l.starts_with("op:")).take(40) {
        let _ = writeln!(out, "{line}");
    }

    println!("{out}");
    if let Ok(path) = std::env::var("LEDGER_PERF_OUT") {
        std::fs::write(&path, &out).expect("write the profile");
        std::fs::write(format!("{path}.books"), dump.join("\n")).expect("write the dump");
    }

    assert_eq!(posted + replayed + refused, WORKLOAD_OPS);
    drop(conn);
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {db}"))
        .await
        .expect("drop the database");
}

#[derive(diesel::QueryableByName)]
struct WalRow {
    #[diesel(sql_type = Double)]
    v: f64,
}

fn sha2_hex(s: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}
