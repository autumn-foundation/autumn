//! Golden end-to-end proof of the double-entry money ledger (issue #1837).
//!
//! This is the issue's **worked example and success metric**, and it runs on
//! every CI push: an in-memory `SQLite` database, no Docker. The Postgres fork
//! of the same machinery is `tests/integration/money_ledger_postgres.rs`, which
//! the Docker sweep picks up.
//!
//! What it pins:
//!
//! * **A duplicate charge posts once.** The same charge submitted twice leaves
//!   one balanced transaction; the second call reports `Replayed` and returns
//!   the first call's transaction.
//! * **A balance is the sum of the postings.** Queried after each charge.
//! * **The zero-sum invariant holds globally.** `trial_balance` is zero for
//!   every currency after every test body.
//! * **Postings commit with the app rows.** The charge and the order row share
//!   one `Db::tx`; a failure after the post rolls both back.
//! * **Nothing is rewritten.** An out-of-band `UPDATE` or `DELETE` is aborted
//!   by the trigger the migration installs.
//! * **Fault injection.** 64 logical charges, each submitted two to four times
//!   with a share of the attempts killed mid-post, ends with exactly one
//!   balanced transaction per charge and `sum(debits) == sum(credits)`.
//!
//! Only meaningful under `--features sqlite`. The file is
//! `#![cfg(feature = "sqlite")]`, so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_money_ledger`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::money::ledger::{
    self, Account, IdempotencyKey, LedgerError, PostOutcome, Posting, Side, Transaction,
};
use autumn_web::money::{Money, Usd};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

type SqlitePool = Pool<RuntimeConnection>;

/// The migration SQL Autumn actually ships, applied verbatim — so a syntax
/// error or a schema change in `migrations_sqlite/` fails this suite rather
/// than sailing past it.
const LEDGER_UP: &str =
    include_str!("../migrations_sqlite/20260917120000_create_money_ledger/up.sql");

/// An app table, so the "commits with your rows" test has rows to commit.
const ORDERS_UP: &str = "CREATE TABLE ml_orders (\
     id TEXT PRIMARY KEY, \
     state TEXT NOT NULL\
 )";

async fn boot_pool(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    {
        let mut conn = pool.get().await.expect("checkout");
        for ddl in [LEDGER_UP, ORDERS_UP] {
            conn.batch_execute(ddl)
                .await
                .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
        }
    }
    pool
}

fn usd(minor: i64) -> Money<Usd> {
    Money::<Usd>::from_minor(minor)
}

/// The two accounts every test in this file posts between.
async fn open_accounts(conn: &mut RuntimeConnection) {
    ledger::ensure_account(conn, Account::new("customer:wallet", Usd::currency()))
        .await
        .expect("open the wallet");
    ledger::ensure_account(conn, Account::new("platform:revenue", Usd::currency()))
        .await
        .expect("open the revenue account");
}

fn charge(amount: i64, order: &str) -> Transaction {
    let postings = vec![
        Posting::debit("customer:wallet", usd(amount)),
        Posting::credit("platform:revenue", usd(amount)),
    ];
    // Idempotent by construction: the key is the money, so a retry collapses
    // without the caller having to remember anything.
    let key = IdempotencyKey::derive(order, &postings);
    Transaction::new(key, postings).memo(format!("charge for {order}"))
}

/// Every currency's total must be zero. A balanced transaction contributes
/// zero, so anything else means money was created or destroyed.
async fn assert_books_balance(conn: &mut RuntimeConnection) {
    for total in ledger::trial_balance(conn).await.expect("trial balance") {
        assert_eq!(
            total.total.minor(),
            0,
            "{} does not balance: {}",
            total.currency,
            total.total
        );
    }
}

// ── The golden test ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_duplicate_charge_posts_once_and_balances() {
    let pool = boot_pool("mlg_golden").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let transfer = charge(2500, "order:9911");

    let first = ledger::post(&mut conn, &transfer).await.expect("first post");
    assert!(first.is_posted(), "the first submit writes the transaction");

    // The retry. Same money, same key, no second entry.
    let second = ledger::post(&mut conn, &transfer).await.expect("retry");
    assert!(second.is_replayed(), "the retry must not write a second entry");
    assert_eq!(
        second.transaction().id(),
        first.transaction().id(),
        "the retry observes the first result"
    );
    assert_eq!(second.transaction().total().minor(), 2500);
    assert_eq!(second.transaction().memo(), "charge for order:9911");

    // One transaction, two postings.
    assert_eq!(count_transactions(&mut conn).await, 1);
    assert_eq!(count_postings(&mut conn).await, 2);

    // The balance is the sum of the postings.
    let wallet = ledger::balance(&mut conn, "customer:wallet")
        .await
        .expect("wallet balance");
    let revenue = ledger::balance(&mut conn, "platform:revenue")
        .await
        .expect("revenue balance");
    assert_eq!(wallet.minor(), 2500);
    assert_eq!(revenue.minor(), -2500);
    assert_eq!(wallet.to_string(), "25.00 USD");

    assert_books_balance(&mut conn).await;
}

#[tokio::test]
async fn a_balance_is_the_sum_of_every_posting() {
    let pool = boot_pool("mlg_balance").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    for (amount, order) in [(2500_i64, "order:1"), (1000, "order:2"), (75, "order:3")] {
        ledger::post(&mut conn, &charge(amount, order))
            .await
            .expect("post");
    }
    // A refund, the other way round.
    let refund = vec![
        Posting::credit("customer:wallet", usd(500)),
        Posting::debit("platform:revenue", usd(500)),
    ];
    let key = IdempotencyKey::derive("refund:order:1", &refund);
    ledger::post(&mut conn, &Transaction::new(key, refund))
        .await
        .expect("refund");

    let wallet = ledger::balance(&mut conn, "customer:wallet")
        .await
        .expect("wallet balance");
    assert_eq!(wallet.minor(), 2500 + 1000 + 75 - 500);
    assert_eq!(
        ledger::balance(&mut conn, "platform:revenue")
            .await
            .expect("revenue balance")
            .minor(),
        -(2500 + 1000 + 75 - 500)
    );
    assert_books_balance(&mut conn).await;
}

#[tokio::test]
async fn a_balance_on_an_account_with_no_postings_is_zero() {
    let pool = boot_pool("mlg_empty").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let balance = ledger::balance(&mut conn, "customer:wallet")
        .await
        .expect("balance");
    assert!(balance.is_zero());
    assert_eq!(balance.currency(), Usd::currency());
}

// ── The invariant is enforced, not asserted ─────────────────────────────────

#[tokio::test]
async fn an_unbalanced_transaction_never_reaches_the_database() {
    let pool = boot_pool("mlg_unbalanced").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let transfer = Transaction::new(
        IdempotencyKey::new("bad-1").expect("key"),
        vec![
            Posting::debit("customer:wallet", usd(2500)),
            Posting::credit("platform:revenue", usd(2499)),
        ],
    );
    let error = ledger::post(&mut conn, &transfer)
        .await
        .expect_err("an unbalanced transaction is refused");
    assert!(matches!(error, LedgerError::Unbalanced { .. }), "{error}");

    // Nothing was written — not even the transaction row.
    assert_eq!(count_transactions(&mut conn).await, 0);
    assert_eq!(count_postings(&mut conn).await, 0);
    assert!(
        ledger::transaction_by_key(&mut conn, &IdempotencyKey::new("bad-1").expect("key"))
            .await
            .expect("lookup")
            .is_none()
    );
}

#[tokio::test]
async fn a_posting_to_an_account_that_does_not_exist_is_refused() {
    let pool = boot_pool("mlg_unknown_account").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let transfer = Transaction::new(
        IdempotencyKey::new("ghost-1").expect("key"),
        vec![
            Posting::debit("customer:ghost", usd(100)),
            Posting::credit("platform:revenue", usd(100)),
        ],
    );
    let error = ledger::post(&mut conn, &transfer)
        .await
        .expect_err("an unknown account is refused");
    assert!(
        matches!(&error, LedgerError::UnknownAccount { id } if id == "customer:ghost"),
        "{error}"
    );
    assert_eq!(count_transactions(&mut conn).await, 0);
}

#[tokio::test]
async fn a_posting_in_the_wrong_currency_for_the_account_is_refused() {
    use autumn_web::money::Eur;

    let pool = boot_pool("mlg_account_currency").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;
    ledger::ensure_account(&mut conn, Account::new("customer:euro", Eur::currency()))
        .await
        .expect("open a euro account");

    let transfer = Transaction::new(
        IdempotencyKey::new("wrong-currency").expect("key"),
        vec![
            Posting::debit("customer:euro", usd(100)),
            Posting::credit("platform:revenue", usd(100)),
        ],
    );
    let error = ledger::post(&mut conn, &transfer)
        .await
        .expect_err("a currency the account does not hold is refused");
    assert!(matches!(error, LedgerError::AccountCurrency { .. }), "{error}");
    assert_eq!(count_transactions(&mut conn).await, 0);
}

#[tokio::test]
async fn the_ledger_refuses_to_be_rewritten() {
    let pool = boot_pool("mlg_append_only").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;
    ledger::post(&mut conn, &charge(2500, "order:1"))
        .await
        .expect("post");

    for statement in [
        "UPDATE _autumn_ledger_postings SET amount_minor = 1",
        "DELETE FROM _autumn_ledger_postings",
        "UPDATE _autumn_ledger_transactions SET memo = 'rewritten'",
        "DELETE FROM _autumn_ledger_transactions",
    ] {
        let result = conn.batch_execute(statement).await;
        assert!(
            result.is_err(),
            "the ledger must refuse `{statement}`, but it succeeded"
        );
    }

    // And the books are untouched.
    assert_eq!(
        ledger::balance(&mut conn, "customer:wallet")
            .await
            .expect("balance")
            .minor(),
        2500
    );
    assert_books_balance(&mut conn).await;
}

// ── Idempotency ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_same_key_for_different_money_is_refused() {
    let pool = boot_pool("mlg_key_reuse").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let key = IdempotencyKey::new("order:9911").expect("key");
    ledger::post(
        &mut conn,
        &Transaction::new(
            key.clone(),
            vec![
                Posting::debit("customer:wallet", usd(2500)),
                Posting::credit("platform:revenue", usd(2500)),
            ],
        ),
    )
    .await
    .expect("first post");

    let error = ledger::post(
        &mut conn,
        &Transaction::new(
            key.clone(),
            vec![
                Posting::debit("customer:wallet", usd(9900)),
                Posting::credit("platform:revenue", usd(9900)),
            ],
        ),
    )
    .await
    .expect_err("the same key for different money is a conflict");
    assert!(matches!(error, LedgerError::KeyReuse { .. }), "{error}");

    // The first transaction stands, and no second one was written.
    assert_eq!(count_transactions(&mut conn).await, 1);
    assert_eq!(
        ledger::balance(&mut conn, "customer:wallet")
            .await
            .expect("balance")
            .minor(),
        2500
    );
}

#[tokio::test]
async fn a_retry_may_word_the_memo_differently() {
    let pool = boot_pool("mlg_memo").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let postings = vec![
        Posting::debit("customer:wallet", usd(2500)),
        Posting::credit("platform:revenue", usd(2500)),
    ];
    let key = IdempotencyKey::derive("order:9911", &postings);
    ledger::post(
        &mut conn,
        &Transaction::new(key.clone(), postings.clone()).memo("first attempt"),
    )
    .await
    .expect("first post");

    let replay = ledger::post(
        &mut conn,
        &Transaction::new(key, postings).memo("second attempt, same money"),
    )
    .await
    .expect("the retry replays");
    assert!(replay.is_replayed());
    // The stored memo is the first one: the transaction was not rewritten.
    assert_eq!(replay.transaction().memo(), "first attempt");
    assert_eq!(count_transactions(&mut conn).await, 1);
}

#[tokio::test]
async fn a_replayed_transaction_reads_back_as_it_was_written() {
    let pool = boot_pool("mlg_readback").await;
    let mut conn = pool.get().await.expect("checkout");
    open_accounts(&mut conn).await;

    let posted = ledger::post(&mut conn, &charge(2500, "order:9911"))
        .await
        .expect("post");
    let stored = ledger::transaction_by_key(&mut conn, posted.transaction().key())
        .await
        .expect("lookup")
        .expect("the transaction is there");

    assert_eq!(stored.id(), posted.transaction().id());
    assert_eq!(stored.currency(), Usd::currency());
    assert!(!stored.posted_at().is_empty(), "the store sets the clock");
    assert_eq!(stored.postings().len(), 2);
    assert_eq!(stored.postings()[0].account_id(), "customer:wallet");
    assert_eq!(stored.postings()[0].side(), Side::Debit);
    assert_eq!(stored.postings()[0].amount().minor(), 2500);
    assert_eq!(stored.postings()[1].account_id(), "platform:revenue");
    assert_eq!(stored.postings()[1].side(), Side::Credit);
    assert_eq!(stored.postings()[1].amount().minor(), 2500);
    assert_eq!(stored.total().minor(), 2500);
}

// ── Accounts ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_account_that_refuses_a_negative_balance_refuses_the_posting() {
    let pool = boot_pool("mlg_negative").await;
    let mut conn = pool.get().await.expect("checkout");
    ledger::ensure_account(&mut conn, Account::new("customer:wallet", Usd::currency()))
        .await
        .expect("open the wallet");
    ledger::ensure_account(
        &mut conn,
        Account::new("platform:float", Usd::currency()).disallow_negative(),
    )
    .await
    .expect("open the float");

    // The float has nothing in it, so paying out of it goes negative.
    let payout = vec![
        Posting::credit("platform:float", usd(2500)),
        Posting::debit("customer:wallet", usd(2500)),
    ];
    let key = IdempotencyKey::derive("payout:1", &payout);
    let error = ledger::post(&mut conn, &Transaction::new(key, payout))
        .await
        .expect_err("the float refuses to go negative");
    assert!(
        matches!(&error, LedgerError::NegativeBalance { account, .. } if account == "platform:float"),
        "{error}"
    );

    // Fund it, and the same payout goes through.
    let funding = vec![
        Posting::debit("platform:float", usd(5000)),
        Posting::credit("customer:wallet", usd(5000)),
    ];
    let key = IdempotencyKey::derive("funding:1", &funding);
    ledger::post(&mut conn, &Transaction::new(key, funding))
        .await
        .expect("funding the float");
    let payout = vec![
        Posting::credit("platform:float", usd(2500)),
        Posting::debit("customer:wallet", usd(2500)),
    ];
    let key = IdempotencyKey::derive("payout:2", &payout);
    ledger::post(&mut conn, &Transaction::new(key, payout))
        .await
        .expect("a funded payout goes through");
    assert_eq!(
        ledger::balance(&mut conn, "platform:float")
            .await
            .expect("balance")
            .minor(),
        2500
    );
}

#[tokio::test]
async fn opening_an_account_twice_keeps_the_first_one() {
    let pool = boot_pool("mlg_ensure").await;
    let mut conn = pool.get().await.expect("checkout");

    let first = ledger::ensure_account(
        &mut conn,
        Account::new("platform:float", Usd::currency()).disallow_negative(),
    )
    .await
    .expect("open");
    assert!(!first.allows_negative());

    // A second call with another policy returns the stored account unchanged.
    let second = ledger::ensure_account(&mut conn, Account::new("platform:float", Usd::currency()))
        .await
        .expect("open again");
    assert!(!second.allows_negative(), "ensure_account never rewrites");

    // The policy changes only when asked.
    let changed = ledger::set_allow_negative(&mut conn, "platform:float", true)
        .await
        .expect("change the policy");
    assert!(changed.allows_negative());
    assert!(
        ledger::set_allow_negative(&mut conn, "platform:ghost", true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_account_cannot_change_currency() {
    use autumn_web::money::Eur;

    let pool = boot_pool("mlg_account_swap").await;
    let mut conn = pool.get().await.expect("checkout");
    ledger::ensure_account(&mut conn, Account::new("customer:wallet", Usd::currency()))
        .await
        .expect("open");

    let error = ledger::ensure_account(&mut conn, Account::new("customer:wallet", Eur::currency()))
        .await
        .expect_err("a stored account keeps its currency");
    assert!(matches!(error, LedgerError::AccountCurrency { .. }), "{error}");

    assert!(
        ledger::account(&mut conn, "customer:ghost")
            .await
            .expect("lookup")
            .is_none()
    );
}

// ── Counting helpers ────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

async fn count_rows(conn: &mut RuntimeConnection, table: &str) -> i64 {
    let rows: Vec<CountRow> =
        diesel::sql_query(format!("SELECT COUNT(*) AS count FROM {table}"))
            .load(conn)
            .await
            .expect("count");
    rows.into_iter().next().map_or(0, |row| row.count)
}

async fn count_transactions(conn: &mut RuntimeConnection) -> i64 {
    count_rows(conn, "_autumn_ledger_transactions").await
}

async fn count_postings(conn: &mut RuntimeConnection) -> i64 {
    count_rows(conn, "_autumn_ledger_postings").await
}

// ── Composition with Db::tx (AC-4) ──────────────────────────────────────────

/// The postings and the application rows they justify share one transaction, so
/// they commit together or not at all.
#[tokio::test]
async fn a_posting_commits_with_the_rows_it_justifies() {
    use autumn_web::db::Db;
    use scoped_futures::ScopedFutureExt as _;

    let pool = boot_pool("mlg_tx_commit").await;
    {
        let mut conn = pool.get().await.expect("checkout");
        open_accounts(&mut conn).await;
    }

    {
        let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
        let outcome = db
            .tx(|conn| {
                async move {
                    diesel::sql_query("INSERT INTO ml_orders (id, state) VALUES ('o1', 'paid')")
                        .execute(conn)
                        .await?;
                    ledger::post(conn, &charge(2500, "order:o1"))
                        .await
                        .map_err(autumn_web::AutumnError::from)
                }
                .scope_boxed()
            })
            .await
            .expect("the transaction commits");
        assert!(outcome.is_posted());
    }

    let mut conn = pool.get().await.expect("checkout");
    assert_eq!(count_rows(&mut conn, "ml_orders").await, 1);
    assert_eq!(count_transactions(&mut conn).await, 1);
    assert_eq!(
        ledger::balance(&mut conn, "customer:wallet")
            .await
            .expect("balance")
            .minor(),
        2500
    );
}

/// The other half: a failure after the post takes the postings with it.
#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_postings() {
    use autumn_web::db::Db;
    use scoped_futures::ScopedFutureExt as _;

    let pool = boot_pool("mlg_tx_rollback").await;
    {
        let mut conn = pool.get().await.expect("checkout");
        open_accounts(&mut conn).await;
    }

    {
        let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
        let result: Result<(), _> = db
            .tx(|conn| {
                async move {
                    diesel::sql_query("INSERT INTO ml_orders (id, state) VALUES ('o2', 'paid')")
                        .execute(conn)
                        .await?;
                    ledger::post(conn, &charge(2500, "order:o2"))
                        .await
                        .map_err(autumn_web::AutumnError::from)?;
                    // The app decides the charge cannot stand after all.
                    Err(autumn_web::AutumnError::bad_request_msg("card declined"))
                }
                .scope_boxed()
            })
            .await;
        assert!(result.is_err());
    }

    let mut conn = pool.get().await.expect("checkout");
    assert_eq!(count_rows(&mut conn, "ml_orders").await, 0, "the order rolled back");
    assert_eq!(count_transactions(&mut conn).await, 0, "the posting rolled back");
    assert_eq!(count_postings(&mut conn).await, 0);
    assert_books_balance(&mut conn).await;
    drop(conn);

    // And the key is free again, so an honest retry still posts.
    let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
    let outcome = db
        .tx(|conn| {
            async move {
                ledger::post(conn, &charge(2500, "order:o2"))
                    .await
                    .map_err(autumn_web::AutumnError::from)
            }
            .scope_boxed()
        })
        .await
        .expect("the retry posts");
    assert!(outcome.is_posted());
}

// ── Fault injection (the issue's success metric) ────────────────────────────

/// Duplicate and retry every money-moving call, and kill a share of them
/// mid-post.
///
/// 64 logical charges. Each is submitted between two and four times. A
/// deterministic share of the attempts is killed after the ledger wrote its
/// postings but before the enclosing `Db::tx` commits — the mid-flight crash the
/// issue names. At the end: exactly one balanced transaction per charge, and
/// `sum(debits) == sum(credits)` globally.
///
/// The schedule is deterministic (a small LCG over a fixed seed), so a failure
/// replays rather than needing to be reproduced.
#[tokio::test]
async fn duplicated_retried_and_killed_charges_post_exactly_once() {
    use autumn_web::db::Db;
    use scoped_futures::ScopedFutureExt as _;

    const CHARGES: u64 = 64;

    let pool = boot_pool("mlg_fault_injection").await;
    {
        let mut conn = pool.get().await.expect("checkout");
        open_accounts(&mut conn).await;
    }

    // A linear congruential generator: deterministic, and enough spread to mix
    // the attempt counts and the kill points.
    let mut seed: u64 = 0x1837_1837_1837_1837;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        seed >> 33
    };

    let mut expected_total: i64 = 0;
    for index in 0..CHARGES {
        let amount = 100 + i64::try_from(next() % 5000).expect("in range");
        let order = format!("order:{index}");
        let attempts = 2 + next() % 3;
        expected_total += amount;

        for attempt in 0..attempts {
            // Kill roughly one attempt in three, after the postings are written.
            let kill = next() % 3 == 0;
            let transfer = charge(amount, &order);
            let order_row = format!("{order}:{attempt}");
            let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
            let result = db
                .tx(|conn| {
                    async move {
                        diesel::sql_query(
                            "INSERT OR IGNORE INTO ml_orders (id, state) VALUES (?, 'paid')",
                        )
                        .bind::<diesel::sql_types::Text, _>(order_row)
                        .execute(conn)
                        .await?;
                        let outcome = ledger::post(conn, &transfer)
                            .await
                            .map_err(autumn_web::AutumnError::from)?;
                        if kill {
                            // The process dies here: the transaction never
                            // commits, so neither the order row nor the
                            // postings survive.
                            return Err(autumn_web::AutumnError::bad_request_msg("killed"));
                        }
                        Ok(outcome)
                    }
                    .scope_boxed()
                })
                .await;
            assert_eq!(
                result.is_err(),
                kill,
                "attempt {attempt} of {index} should only fail when it is killed"
            );
        }
    }

    let mut conn = pool.get().await.expect("checkout");

    // Exactly one transaction per logical charge.
    assert_eq!(
        count_transactions(&mut conn).await,
        i64::try_from(CHARGES).expect("in range"),
        "one balanced transaction per logical charge"
    );
    assert_eq!(
        count_postings(&mut conn).await,
        i64::try_from(CHARGES * 2).expect("in range")
    );

    // Every transaction balances, and the books balance globally.
    assert_books_balance(&mut conn).await;
    assert_eq!(
        ledger::balance(&mut conn, "customer:wallet")
            .await
            .expect("balance")
            .minor(),
        expected_total,
        "the wallet holds exactly the sum of the charges"
    );
    assert_eq!(
        ledger::balance(&mut conn, "platform:revenue")
            .await
            .expect("balance")
            .minor(),
        -expected_total
    );

    // Per transaction: debits equal credits.
    #[derive(diesel::QueryableByName)]
    struct ImbalanceRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }
    let rows: Vec<ImbalanceRow> = diesel::sql_query(
        "SELECT COUNT(*) AS count FROM (\
           SELECT transaction_id FROM _autumn_ledger_postings \
           GROUP BY transaction_id HAVING SUM(amount_minor) <> 0\
         )",
    )
    .load(&mut conn)
    .await
    .expect("imbalance scan");
    assert_eq!(
        rows.into_iter().next().map_or(-1, |row| row.count),
        0,
        "no transaction may be unbalanced"
    );
}
