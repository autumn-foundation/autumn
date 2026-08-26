//! Postgres-tier proof of the bitemporal, tamper-evident record ledger
//! (issue #1699).
//!
//! **Requires Docker** to be running; picked up automatically by CI's
//! `--ignored` sweep over the consolidated `integration_tests` binary.
//!
//! `tests/sqlite_ledger.rs` is the golden end-to-end test and runs Docker-free on
//! every push. This file proves the *Postgres fork* of the same machinery — the
//! `jsonb` snapshot cast, `Timestamptz` binds, and the `COALESCE(tenant_id, '')`
//! expression unique index — behaves identically:
//!
//! * every write appends a chained revision carrying both time axes;
//! * as-of reconstruction at a past transaction instant matches an oracle
//!   recorded live at that instant, byte for byte;
//! * `ledger_verify` accepts an intact chain and names the first broken link
//!   after an out-of-band `UPDATE`;
//! * the chain unique index refuses a duplicated sequence number.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::current::with_actor;
use autumn_web::hooks::Patch;
use autumn_web::ledger::LedgerBreak;
use autumn_web::version_history::VersionOp;
use chrono::{DateTime, Duration, Utc};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    test_ledger_invoices (id) {
        id -> Int8,
        reference -> Text,
        amount_cents -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "test_ledger_invoices")]
#[derive(PartialEq, Eq)]
pub struct LedgerInvoice {
    #[id]
    pub id: i64,
    pub reference: String,
    pub amount_cents: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgerInvoice,
    table = "test_ledger_invoices",
    soft_delete,
    ledgered = true
)]
pub trait LedgerInvoiceRepository {}

/// The Postgres DDL shipped in
/// `autumn/version_history_migrations/20260826000000_create_ledger_revisions/up.sql`,
/// applied by hand so this test also proves that CREATE is valid Postgres.
const LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS _autumn_ledger_revisions (
        id          BIGSERIAL   PRIMARY KEY,
        table_name  TEXT        NOT NULL,
        tenant_id   TEXT,
        record_id   BIGINT      NOT NULL,
        seq         BIGINT      NOT NULL,
        op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
        actor       TEXT        NOT NULL DEFAULT 'system',
        request_id  TEXT,
        snapshot    JSONB       NOT NULL DEFAULT '{}',
        valid_from  TIMESTAMPTZ NOT NULL,
        recorded_at TIMESTAMPTZ NOT NULL,
        prev_hash   TEXT,
        hash        TEXT        NOT NULL
    )";

const LEDGER_CHAIN_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS \
    idx_autumn_ledger_revisions_chain ON _autumn_ledger_revisions \
    (table_name, COALESCE(tenant_id, ''), record_id, seq)";

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
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    for ddl in [
        "CREATE TABLE IF NOT EXISTS test_ledger_invoices (
             id BIGSERIAL PRIMARY KEY,
             reference TEXT NOT NULL,
             amount_cents BIGINT NOT NULL,
             deleted_at TIMESTAMP
         )",
        "CREATE TABLE IF NOT EXISTS _autumn_version_history (
            id          BIGSERIAL   PRIMARY KEY,
            table_name  TEXT        NOT NULL,
            tenant_id   TEXT,
            record_id   BIGINT      NOT NULL,
            op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
            actor       TEXT        NOT NULL DEFAULT 'system',
            request_id  TEXT,
            changes     JSONB       NOT NULL DEFAULT '[]',
            recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        LEDGER_DDL,
        LEDGER_CHAIN_INDEX,
    ] {
        diesel::sql_query(ddl)
            .execute(&mut conn)
            .await
            .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
    }

    (pool, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgLedgerInvoiceRepository {
    PgLedgerInvoiceRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "test asserts against real recorded_at values"
)]
fn now() -> DateTime<Utc> {
    Utc::now()
}

async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ledger_records_chains_and_reconstructs_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let mut oracle: Vec<(DateTime<Utc>, LedgerInvoice)> = Vec::new();

    let created = with_actor("alice", async {
        repo.save(&NewLedgerInvoice {
            reference: "PG-1".to_string(),
            amount_cents: 100,
        })
        .await
        .expect("insert")
    })
    .await;
    let id = created.id;
    tick().await;
    oracle.push((now(), created));
    tick().await;

    let updated = with_actor("alice", async {
        repo.update(
            id,
            &UpdateLedgerInvoice {
                amount_cents: Patch::Set(200),
                ..Default::default()
            },
        )
        .await
        .expect("update")
    })
    .await;
    tick().await;
    oracle.push((now(), updated));
    tick().await;

    let renamed = with_actor("bob", async {
        repo.update(
            id,
            &UpdateLedgerInvoice {
                reference: Patch::Set("PG-1-REV".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("second update")
    })
    .await;
    tick().await;
    oracle.push((now(), renamed));

    // Chain shape.
    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
        vec![VersionOp::Insert, VersionOp::Update, VersionOp::Update],
    );
    assert_eq!(
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(revisions[0].prev_hash, None);
    assert_eq!(
        revisions[1].prev_hash.as_deref(),
        Some(revisions[0].hash.as_str())
    );
    assert_eq!(
        revisions[2].prev_hash.as_deref(),
        Some(revisions[1].hash.as_str())
    );
    assert_eq!(revisions[2].actor, "bob");

    // As-of reconstruction matches the oracle byte for byte at every instant.
    for (instant, expected) in &oracle {
        let reconstructed = repo
            .ledger_as_of(id, *instant)
            .await
            .expect("as-of read")
            .expect("the record existed at this instant");
        assert_eq!(&reconstructed, expected, "as-of state at {instant}");
        assert_eq!(
            serde_json::to_string(&reconstructed).expect("serialize reconstructed"),
            serde_json::to_string(expected).expect("serialize oracle"),
        );
    }
    assert!(
        repo.ledger_as_of(id, oracle[0].0 - Duration::seconds(60))
            .await
            .expect("as-of read")
            .is_none(),
    );

    // A field-level diff across the first update.
    let diff = repo
        .ledger_diff(id, oracle[0].0, oracle[1].0)
        .await
        .expect("diff");
    assert_eq!(diff.changes.len(), 1, "{:?}", diff.changes);
    assert_eq!(diff.changes[0].column, "amount_cents");

    // An intact chain verifies and exports its head.
    let report = repo.ledger_verify(id).await.expect("verify");
    assert!(report.is_intact(), "{report:?}");
    let head = repo.ledger_head(id).await.expect("head").expect("a head");
    assert_eq!(report.head_hash.as_deref(), Some(head.hash.as_str()));
    assert_eq!(head.seq, 3);

    // Out-of-band mutation is detected at the tampered link.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_revisions \
             SET snapshot = jsonb_set(snapshot, '{amount_cents}', '999999') \
             WHERE table_name = 'test_ledger_invoices' AND record_id = $1 AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut conn)
        .await
        .expect("tamper");
    }
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("mutation detected");
    assert_eq!(broken.kind, LedgerBreak::HashMismatch);
    assert_eq!(broken.seq, 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn soft_delete_appends_a_revision_and_the_chain_index_refuses_a_fork() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-2".to_string(),
            amount_cents: 10,
        })
        .await
        .expect("insert");
    let id = created.id;
    repo.delete_by_id(id).await.expect("soft delete");

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
        vec![VersionOp::Insert, VersionOp::Delete],
        "a ledgered soft-delete records a revision rather than erasing history"
    );
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());

    // A soft-deleted record still reconstructs, carrying the deleted_at a
    // `with_deleted()` query would have shown.
    let latest = repo
        .ledger_as_of(id, Utc::now() + Duration::seconds(1))
        .await
        .expect("as-of")
        .expect("state exists");
    assert!(latest.deleted_at.is_some());

    // The expression unique index refuses a duplicated sequence number, so a
    // race that slipped past the write transaction's row lock is a hard error.
    let mut conn = pool.get().await.expect("conn");
    let forked = diesel::sql_query(
        "INSERT INTO _autumn_ledger_revisions \
         (table_name, tenant_id, record_id, seq, op, actor, snapshot, valid_from, \
          recorded_at, prev_hash, hash) \
         SELECT table_name, tenant_id, record_id, seq, op, actor, snapshot, valid_from, \
                recorded_at, prev_hash, 'duplicate' \
         FROM _autumn_ledger_revisions \
         WHERE table_name = 'test_ledger_invoices' AND record_id = $1 AND seq = 1",
    )
    .bind::<diesel::sql_types::BigInt, _>(id)
    .execute(&mut conn)
    .await;
    assert!(forked.is_err(), "the chain unique index must refuse a fork");
}
