//! Drives real double-entry postings through `money::ledger::post` — the
//! ledger's public write path (issue #1837) — against an in-memory `SQLite`
//! database, so this profiles without Docker (the Postgres fork of the same
//! store is `tests/integration/money_ledger_postgres.rs`, which the CI Docker
//! sweep already covers; `tests/sqlite_money_ledger.rs` is this bench's
//! non-profiling sibling and the source of the pool/migration setup below).
//!
//! # Workload
//!
//! A recurring payout that splits one funding account across many destination
//! accounts in a single balanced transaction — the shape
//! `Transaction::validate`'s own many-sided unit test documents (a marketplace
//! charge split across sellers) and `MAX_POSTINGS` (1024) anticipates at scale
//! (payroll, marketplace revenue share, affiliate payout). Every posting in
//! every transaction goes through the same public `ledger::post` entry point a
//! deployed app calls from inside `Db::tx`.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench ledger_post --features sqlite
//! BIN=$(find target/release/deps -maxdepth 1 -name "ledger_post-*" -type f ! -name "*.d")
//!
//! # Instruction profile, base-subtracted (`--iterations 0` isolates process
//! # startup + pool/account setup from the marginal per-payout cost).
//! valgrind --tool=callgrind --callgrind-out-file=cg-0.out   "$BIN" --iterations 0
//! valgrind --tool=callgrind --callgrind-out-file=cg-300.out "$BIN" --iterations 300
//! callgrind_annotate --threshold=80 cg-300.out | head -40
//!
//! # Allocation profile.
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 300
//!
//! # Syscall count (each `execute()`/`load()` round trip is real SQLite I/O
//! # even in-process, so this is a legitimate proxy for round-trip count).
//! strace -c -o strace-300.out "$BIN" --iterations 300
//! ```
//!
//! `--iterations N` posts N payouts, each with `DESTINATIONS + 1` postings,
//! after a fixed warm-up.

use std::hint::black_box;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::money::ledger::{self, Account, IdempotencyKey, LedgerError, Posting, Transaction};
use autumn_web::money::{Money, Usd};

use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection as _, SimpleAsyncConnection as _};

type SqlitePool = Pool<RuntimeConnection>;

/// The migration SQL Autumn actually ships, applied verbatim — the same file
/// `tests/sqlite_money_ledger.rs` uses.
const LEDGER_UP: &str =
    include_str!("../migrations_sqlite/20260917120000_create_money_ledger/up.sql");

/// Destinations one payout transaction splits across — a payroll run's
/// per-employee legs, or a marketplace charge's revenue-share legs. Well
/// inside `MAX_POSTINGS` (1024), and large enough that a per-posting cost
/// shows up in a profile rather than hiding in setup noise.
const DESTINATIONS: usize = 39;

/// Cents each destination receives per payout.
const SHARE_MINOR: i64 = 250;

const fn usd(minor: i64) -> Money<Usd> {
    Money::<Usd>::from_minor(minor)
}

async fn boot_pool() -> SqlitePool {
    let config = DatabaseConfig {
        url: Some("sqlite://file:ledger_post_bench?mode=memory&cache=shared".to_owned()),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");

    let mut conn = pool.get().await.expect("checkout");
    conn.batch_execute(LEDGER_UP)
        .await
        .expect("apply the ledger migration");

    ledger::ensure_account(&mut conn, Account::new("payout:pool", Usd::currency()))
        .await
        .expect("open the pool account");
    for i in 0..DESTINATIONS {
        ledger::ensure_account(
            &mut conn,
            Account::new(format!("payout:dest:{i}"), Usd::currency()),
        )
        .await
        .expect("open a destination account");
    }
    drop(conn);
    pool
}

/// One balanced payout: the pool credited for the total, each destination
/// debited its equal share. Signs follow the module's own convention — the
/// pool "holds money for somebody else" until it pays out, so it is credited
/// (runs negative) the way a customer wallet is in the module's own examples.
fn payout(period: u64) -> Transaction {
    let mut postings = Vec::with_capacity(DESTINATIONS + 1);
    #[allow(
        clippy::cast_possible_wrap,
        reason = "DESTINATIONS is a small compile-time constant"
    )]
    let total = SHARE_MINOR * DESTINATIONS as i64;
    postings.push(Posting::credit("payout:pool", usd(total)));
    for i in 0..DESTINATIONS {
        postings.push(Posting::debit(format!("payout:dest:{i}"), usd(SHARE_MINOR)));
    }
    let key = IdempotencyKey::derive(&format!("payout:period:{period}"), &postings);
    Transaction::new(key, postings).memo(format!("payout period {period}"))
}

async fn post_one(conn: &mut RuntimeConnection, transfer: &Transaction) -> ledger::PostOutcome {
    conn.transaction::<_, LedgerError, _>(async move |conn| ledger::post(conn, transfer).await)
        .await
        .expect("post")
}

fn main() {
    let iterations: u64 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async {
        let pool = boot_pool().await;
        let mut conn = pool.get().await.expect("checkout");

        // Warm-up outside the measured window: first payouts pay one-time
        // costs (lazily built caches, the first prepared-statement compile)
        // that steady-state payouts do not.
        for period in 0..10 {
            black_box(post_one(&mut conn, &payout(period)).await);
        }

        for period in 10..(10 + iterations) {
            let transfer = payout(period);
            black_box(post_one(&mut conn, &transfer).await);
        }
    });

    println!(
        "completed {iterations} payouts of {} postings each",
        DESTINATIONS + 1
    );
}
