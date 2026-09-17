//! Postgres tier of the double-entry money ledger (issue #1837).
//!
//! **Requires Docker**; picked up automatically by CI's `--ignored` sweep over
//! the consolidated `integration_tests` binary.
//!
//! `tests/sqlite_money_ledger.rs` is the golden suite and runs Docker-free on
//! every push. This file proves the **Postgres fork** of the same machinery —
//! the `BIGSERIAL` posting id, the `plpgsql` append-only trigger, the
//! `CAST(SUM(...) AS BIGINT)` that keeps Postgres from widening a balance to
//! `NUMERIC`, and above all the `SELECT ... FOR UPDATE` account lock, which has
//! no `SQLite` equivalent to exercise.
//!
//! What only this tier can show:
//!
//! * **Concurrent duplicate submits collapse.** Eight connections post the same
//!   charge at once. Exactly one writes; the rest replay. `SQLite` serializes
//!   writers, so this is the real race.
//! * **The negative-balance check is exact under concurrency.** Two concurrent
//!   payouts from an account that holds enough for one: exactly one succeeds.
//! * **The trigger refuses an out-of-band rewrite.**

#![cfg(feature = "db")]

use autumn_web::money::ledger::{
    self, Account, IdempotencyKey, LedgerError, PostOutcome, Posting, Transaction,
};
use autumn_web::money::{Money, Usd};

use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{
    AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _,
};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// The migration SQL Autumn actually ships, applied verbatim — so a syntax
/// error or a schema change in `migrations/` fails this suite rather than
/// sailing past it.
const LEDGER_UP: &str = include_str!("../../migrations/20260917120000_create_money_ledger/up.sql");

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(16).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute(LEDGER_UP)
        .await
        .unwrap_or_else(|err| panic!("apply the ledger migration: {err}"));

    (pool, container)
}

const fn usd(minor: i64) -> Money<Usd> {
    Money::<Usd>::from_minor(minor)
}

/// `post` refuses to run outside a transaction, so every call here gets one.
async fn post_tx(
    conn: &mut AsyncPgConnection,
    transfer: &Transaction,
) -> Result<PostOutcome, LedgerError> {
    conn.transaction::<_, LedgerError, _>(async move |conn| ledger::post(conn, transfer).await)
        .await
}

fn charge(amount: i64, order: &str) -> Transaction {
    let postings = vec![
        Posting::debit("platform:cash", usd(amount)),
        Posting::credit("platform:revenue", usd(amount)),
    ];
    let key = IdempotencyKey::derive(order, &postings);
    Transaction::new(key, postings).memo(format!("charge for {order}"))
}

async fn open_accounts(pool: &Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("conn");
    ledger::ensure_account(&mut conn, Account::new("platform:cash", Usd::currency()))
        .await
        .expect("open the cash account");
    ledger::ensure_account(&mut conn, Account::new("platform:revenue", Usd::currency()))
        .await
        .expect("open the revenue account");
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

async fn count_rows(pool: &Pool<AsyncPgConnection>, table: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    let rows: Vec<CountRow> = diesel::sql_query(format!("SELECT COUNT(*) AS count FROM {table}"))
        .load(&mut conn)
        .await
        .expect("count");
    rows.into_iter().next().map_or(0, |row| row.count)
}

/// Every currency's total must be zero.
async fn assert_books_balance(pool: &Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("conn");
    for total in ledger::trial_balance(&mut conn)
        .await
        .expect("trial balance")
    {
        assert_eq!(
            total.total().minor(),
            0,
            "{} does not balance: {}",
            total.currency(),
            total.total()
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_duplicate_charge_posts_once_on_postgres() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    let mut conn = pool.get().await.expect("conn");

    let transfer = charge(2500, "order:9911");
    let first = post_tx(&mut conn, &transfer).await.expect("first post");
    assert!(first.is_posted());
    let second = post_tx(&mut conn, &transfer).await.expect("retry");
    assert!(second.is_replayed());
    assert_eq!(second.transaction().id(), first.transaction().id());

    // `CAST(SUM(...) AS BIGINT)`: without it Postgres returns NUMERIC and the
    // decoder fails. This is the assertion that pins it.
    assert_eq!(
        ledger::balance(&mut conn, "platform:cash")
            .await
            .expect("balance")
            .minor(),
        2500
    );
    assert_eq!(count_rows(&pool, "_autumn_money_transactions").await, 1);
    assert_eq!(count_rows(&pool, "_autumn_money_postings").await, 2);
    assert!(!first.transaction().posted_at().is_empty());
    assert_books_balance(&pool).await;
}

/// The race the issue is about: the same charge submitted from several
/// connections at once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_duplicate_submits_collapse_to_one_transaction() {
    const SUBMITS: usize = 8;

    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;

    let mut handles = Vec::with_capacity(SUBMITS);
    for _ in 0..SUBMITS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = pool.get().await.expect("conn");
            conn.transaction::<_, LedgerError, _>(async move |conn| {
                ledger::post(conn, &charge(2500, "order:9911")).await
            })
            .await
        }));
    }

    let mut posted = 0_usize;
    let mut replayed = 0_usize;
    for handle in handles {
        match handle.await.expect("task joins").expect("post succeeds") {
            outcome if outcome.is_posted() => posted += 1,
            _ => replayed += 1,
        }
    }
    assert_eq!(posted, 1, "exactly one submit may write the transaction");
    assert_eq!(replayed, SUBMITS - 1, "the rest observe the first result");

    assert_eq!(count_rows(&pool, "_autumn_money_transactions").await, 1);
    assert_eq!(count_rows(&pool, "_autumn_money_postings").await, 2);
    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        ledger::balance(&mut conn, "platform:cash")
            .await
            .expect("balance")
            .minor(),
        2500,
        "a duplicate submit must not double the balance"
    );
    assert_books_balance(&pool).await;
}

/// Distinct charges under concurrency: every one lands, exactly once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_distinct_charges_all_land_exactly_once() {
    const CHARGES: usize = 16;
    const SUBMITS_EACH: usize = 3;

    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;

    let mut handles = Vec::with_capacity(CHARGES * SUBMITS_EACH);
    for index in 0..CHARGES {
        for _ in 0..SUBMITS_EACH {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let amount = 100 + i64::try_from(index).expect("in range") * 7;
                let order = format!("order:{index}");
                let mut conn = pool.get().await.expect("conn");
                conn.transaction::<_, LedgerError, _>(async move |conn| {
                    ledger::post(conn, &charge(amount, &order)).await
                })
                .await
            }));
        }
    }

    let mut posted = 0_usize;
    for handle in handles {
        if handle
            .await
            .expect("task joins")
            .expect("post succeeds")
            .is_posted()
        {
            posted += 1;
        }
    }
    assert_eq!(posted, CHARGES, "one write per logical charge");
    assert_eq!(
        count_rows(&pool, "_autumn_money_transactions").await,
        i64::try_from(CHARGES).expect("in range")
    );

    let expected: i64 = (0..CHARGES)
        .map(|index| 100 + i64::try_from(index).expect("in range") * 7)
        .sum();
    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        ledger::balance(&mut conn, "platform:cash")
            .await
            .expect("balance")
            .minor(),
        expected
    );
    assert_books_balance(&pool).await;
}

/// The `FOR UPDATE` account lock is what makes this exact: two payouts race for
/// a float that can only cover one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_negative_balance_check_holds_under_concurrency() {
    let (pool, _container) = setup_pool().await;
    {
        let mut conn = pool.get().await.expect("conn");
        ledger::ensure_account(&mut conn, Account::new("platform:cash", Usd::currency()))
            .await
            .expect("open the cash account");
        ledger::ensure_account(
            &mut conn,
            Account::new("platform:float", Usd::currency()).disallow_negative(),
        )
        .await
        .expect("open the float");

        // Fund the float with exactly one payout's worth.
        let funding = vec![
            Posting::debit("platform:float", usd(2500)),
            Posting::credit("platform:cash", usd(2500)),
        ];
        let key = IdempotencyKey::derive("funding", &funding);
        post_tx(&mut conn, &Transaction::new(key, funding))
            .await
            .expect("funding");
    }

    let mut handles = Vec::new();
    for attempt in 0..2 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let payout = vec![
                Posting::credit("platform:float", usd(2500)),
                Posting::debit("platform:cash", usd(2500)),
            ];
            // Distinct keys: this is a race on the balance, not on idempotency.
            let key = IdempotencyKey::new(format!("payout:{attempt}")).expect("key");
            let mut conn = pool.get().await.expect("conn");
            conn.transaction::<_, LedgerError, _>(async move |conn| {
                ledger::post(conn, &Transaction::new(key, payout)).await
            })
            .await
        }));
    }

    let mut allowed = 0_usize;
    let mut refused = 0_usize;
    for handle in handles {
        match handle.await.expect("task joins") {
            Ok(_) => allowed += 1,
            Err(LedgerError::NegativeBalance { .. }) => refused += 1,
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!(allowed, 1, "the float covers exactly one payout");
    assert_eq!(refused, 1);

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        ledger::balance(&mut conn, "platform:float")
            .await
            .expect("balance")
            .minor(),
        0
    );
    assert_books_balance(&pool).await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_postgres_trigger_refuses_a_rewrite() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    {
        let mut conn = pool.get().await.expect("conn");
        post_tx(&mut conn, &charge(2500, "order:1"))
            .await
            .expect("post");
    }

    for statement in [
        "UPDATE _autumn_money_postings SET amount_minor = 1",
        "DELETE FROM _autumn_money_postings",
        "UPDATE _autumn_money_transactions SET memo = 'rewritten'",
        "DELETE FROM _autumn_money_transactions",
    ] {
        let mut conn = pool.get().await.expect("conn");
        let result = conn.batch_execute(statement).await;
        assert!(
            result.is_err(),
            "the ledger must refuse `{statement}`, but it succeeded"
        );
    }

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        ledger::balance(&mut conn, "platform:cash")
            .await
            .expect("balance")
            .minor(),
        2500
    );
    assert_books_balance(&pool).await;
}

/// A row trigger does not fire on `TRUNCATE`, so the append-only pair needs a
/// statement-level counterpart. Postgres-only: SQLite has no `TRUNCATE`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_postgres_trigger_refuses_a_truncate() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    {
        let mut conn = pool.get().await.expect("conn");
        post_tx(&mut conn, &charge(2500, "order:1"))
            .await
            .expect("post");
    }

    for statement in [
        "TRUNCATE _autumn_money_postings",
        "TRUNCATE _autumn_money_transactions CASCADE",
        "TRUNCATE _autumn_money_postings, _autumn_money_transactions",
    ] {
        let mut conn = pool.get().await.expect("conn");
        let result = conn.batch_execute(statement).await;
        assert!(
            result.is_err(),
            "the ledger must refuse `{statement}`, but it succeeded"
        );
    }

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        ledger::balance(&mut conn, "platform:cash")
            .await
            .expect("balance")
            .minor(),
        2500
    );
    assert_books_balance(&pool).await;
}

/// The Postgres half of the cancellation-safety mechanism: the deferred
/// foreign key turns an orphan posting into a refused COMMIT.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postings_with_no_transaction_row_cannot_commit_on_postgres() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    let mut conn = pool.get().await.expect("conn");

    conn.batch_execute("BEGIN").await.expect("begin");
    diesel::sql_query(
        "INSERT INTO _autumn_money_postings \
           (transaction_id, seq, account_id, amount_minor, currency, posted_at) \
         VALUES ('no-such-transaction', 0, 'platform:cash', 100, 'USD', CURRENT_TIMESTAMP)",
    )
    .execute(&mut conn)
    .await
    .expect("a deferred foreign key accepts the orphan");

    assert!(
        conn.batch_execute("COMMIT").await.is_err(),
        "a transaction holding an orphan posting must not commit"
    );
    let _ = conn.batch_execute("ROLLBACK").await;

    assert_eq!(count_rows(&pool, "_autumn_money_postings").await, 0);
    assert_books_balance(&pool).await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unbalanced_transaction_never_reaches_postgres() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    let mut conn = pool.get().await.expect("conn");

    let transfer = Transaction::new(
        IdempotencyKey::new("bad-1").expect("key"),
        vec![
            Posting::debit("platform:cash", usd(2500)),
            Posting::credit("platform:revenue", usd(2499)),
        ],
    );
    let error = post_tx(&mut conn, &transfer).await.expect_err("refused");
    assert!(matches!(error, LedgerError::Unbalanced { .. }), "{error}");
    assert_eq!(count_rows(&pool, "_autumn_money_transactions").await, 0);
    assert_eq!(count_rows(&pool, "_autumn_money_postings").await, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_same_key_for_different_money_is_refused_on_postgres() {
    let (pool, _container) = setup_pool().await;
    open_accounts(&pool).await;
    let mut conn = pool.get().await.expect("conn");

    let key = IdempotencyKey::new("order:9911").expect("key");
    post_tx(
        &mut conn,
        &Transaction::new(
            key.clone(),
            vec![
                Posting::debit("platform:cash", usd(2500)),
                Posting::credit("platform:revenue", usd(2500)),
            ],
        ),
    )
    .await
    .expect("first post");

    let error = post_tx(
        &mut conn,
        &Transaction::new(
            key,
            vec![
                Posting::debit("platform:cash", usd(9900)),
                Posting::credit("platform:revenue", usd(9900)),
            ],
        ),
    )
    .await
    .expect_err("a reused key for different money is a conflict");
    assert!(matches!(error, LedgerError::KeyReuse { .. }), "{error}");
    assert_eq!(count_rows(&pool, "_autumn_money_transactions").await, 1);
    assert_books_balance(&pool).await;
}
