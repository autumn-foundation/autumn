//! Postgres-tier proof of the bitemporal, tamper-evident record ledger
//! (issue #1699).
//!
//! **Requires Docker** to be running; picked up automatically by CI's
//! `--ignored` sweep over the consolidated `integration_tests` binary.
//!
//! `tests/sqlite_ledger.rs` is the golden end-to-end test and runs Docker-free on
//! every push. This file proves the *Postgres fork* of the same machinery — the
//! `Timestamptz` binds, the `COALESCE(tenant_id, '')` expression unique index,
//! and above all the `TEXT` snapshot column (a `JSONB` one would re-render
//! numbers through `numeric` and break the hash) — behaves identically:
//!
//! * every write appends a chained revision carrying both time axes;
//! * as-of reconstruction at a past transaction instant matches an oracle
//!   recorded live at that instant, byte for byte;
//! * `ledger_verify` accepts an intact chain and names the first broken link
//!   after an out-of-band `UPDATE`;
//! * the chain unique index refuses a duplicated sequence number.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]
// The `f64` column makes the `#[model]` expansion (change tracking uses strict
// equality) trip clippy's `float_cmp`; that generated comparison is the macro's
// concern, not this test's, and an item-level allow can't reach the
// macro-emitted impls. Same convention as `form_for_derive.rs`.
#![allow(clippy::float_cmp)]

use autumn_web::current::with_actor;
use autumn_web::hooks::Patch;
use autumn_web::ledger::LedgerBreak;
use autumn_web::version_history::VersionOp;
use chrono::{DateTime, Duration, Utc};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection as _};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    test_ledger_invoices (id) {
        id -> Int8,
        reference -> Text,
        amount_cents -> Int8,
        amount_rate -> Double,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "test_ledger_invoices")]
pub struct LedgerInvoice {
    #[id]
    pub id: i64,
    pub reference: String,
    pub amount_cents: i64,
    /// The column this tier exists to test. Postgres `jsonb` renders numbers
    /// through `numeric`, so a value serde writes as `1e16` would come back as
    /// `10000000000000000` and re-canonicalize to different bytes than the ones
    /// that were hashed — `ledger_verify` would report tampering on an untouched
    /// chain. The snapshot column is `TEXT` on both tiers precisely to avoid
    /// that, and this column keeps the decision under test on the tier where it
    /// would actually bite.
    pub amount_rate: f64,
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

/// The migration SQL Autumn actually ships, applied verbatim — so a syntax
/// error or a schema change in `version_history_migrations/` fails this suite
/// rather than sailing past it.
const LEDGER_UP: &str =
    include_str!("../../version_history_migrations/20260826000000_create_ledger_revisions/up.sql");

/// The #2323 high-water marks, from the same shipped migration set.
///
/// Applied after `LEDGER_UP` because its backfill reads the revisions table.
const LEDGER_CHAIN_HEADS_UP: &str = include_str!(
    "../../version_history_migrations/20260901000000_create_ledger_chain_heads/up.sql"
);

/// A `ledgered` repository implies `versioned`, so both tables must exist.
const VERSION_HISTORY_UP: &str =
    include_str!("../../version_history_migrations/20260526000000_create_version_history/up.sql");

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
             amount_rate DOUBLE PRECISION NOT NULL,
             deleted_at TIMESTAMP
         )",
        VERSION_HISTORY_UP,
        LEDGER_UP,
        LEDGER_CHAIN_HEADS_UP,
    ] {
        conn.batch_execute(ddl)
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

/// Byte-for-byte model comparison.
///
/// The models carry an `f64` column, so `#[derive(PartialEq)]` on them would
/// trip `clippy::float_cmp` from inside the `#[model]` expansion (where an
/// `#[allow]` on the struct does not reach). Comparing serialized forms is both
/// lint-clean and the stronger assertion: "byte-for-byte identical to what a
/// plain query would have returned" is exactly what the issue asks for.
fn assert_same_record<T: serde::Serialize>(left: &T, right: &T, context: &str) {
    assert_eq!(
        serde_json::to_string(left).expect("serialize left"),
        serde_json::to_string(right).expect("serialize right"),
        "{context}"
    );
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
// One linear scenario — three writes, then chain shape, oracle reconstruction,
// diff, verify and head all asserted against it. Splitting it would mean
// re-running the container and the writes per assertion.
#[allow(clippy::too_many_lines)]
async fn ledger_records_chains_and_reconstructs_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let mut oracle: Vec<(DateTime<Utc>, LedgerInvoice)> = Vec::new();

    let created = with_actor("alice", async {
        repo.save(&NewLedgerInvoice {
            reference: "PG-1".to_string(),
            amount_cents: 100,
            // Exponential-notation territory: this is the value that would come
            // back renormalized from a `jsonb` column.
            amount_rate: 1e16,
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
        assert_same_record(
            &reconstructed,
            expected,
            &format!("as-of state at {instant}"),
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
    //
    // `snapshot` is TEXT, not JSONB — the whole point of this tier's float
    // column — so the jsonb helpers need explicit casts either side. Editing the
    // text directly would work too; going through jsonb keeps the tamper
    // expressed as "change this one field" rather than a string substitution
    // coupled to the canonical formatting.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_revisions \
             SET snapshot = jsonb_set(snapshot::jsonb, '{amount_cents}', '999999')::text \
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

/// The float column above is the whole point of this test: it is the shape that
/// a `jsonb` snapshot column would silently corrupt.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_float_snapshot_round_trips_without_renormalization() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-FLOAT".to_string(),
            amount_cents: 1,
            amount_rate: 1e16,
        })
        .await
        .expect("insert");

    // The stored bytes must be the bytes that were hashed.
    let report = repo.ledger_verify(created.id).await.expect("verify");
    assert!(
        report.is_intact(),
        "a float snapshot must survive storage byte-for-byte: {report:?}"
    );

    let reconstructed = repo
        .ledger_as_of(created.id, Utc::now() + Duration::seconds(1))
        .await
        .expect("as-of")
        .expect("state");
    assert_same_record(&reconstructed, &created, "as-of must reproduce the insert");

    // And a window with no writes reports no phantom change.
    let quiet = repo
        .ledger_diff(created.id, Utc::now(), Utc::now() + Duration::seconds(1))
        .await
        .expect("diff");
    assert!(quiet.is_empty(), "{quiet:?}");
}

/// `restore` is the sanctioned inverse of a ledgered delete, so it records one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restore_records_a_revision_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-3".to_string(),
            amount_cents: 5,
            amount_rate: 1.5,
        })
        .await
        .expect("insert");
    repo.delete_by_id(created.id).await.expect("soft delete");
    repo.restore(created.id).await.expect("restore");

    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
        vec![VersionOp::Insert, VersionOp::Delete, VersionOp::Update],
    );

    let live = repo
        .find_by_id(created.id)
        .await
        .expect("live read")
        .expect("live row");
    let reconstructed = repo
        .ledger_as_of(created.id, Utc::now() + Duration::seconds(1))
        .await
        .expect("as-of")
        .expect("state");
    assert_same_record(&reconstructed, &live, "as-of(now) must equal the live row");
    assert!(
        repo.ledger_verify(created.id)
            .await
            .expect("verify")
            .is_intact()
    );
}

/// A truncated tail leaves an internally perfect chain; only the live row
/// exposes it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn verify_detects_a_truncated_tail_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-4".to_string(),
            amount_cents: 1,
            amount_rate: 1.0,
        })
        .await
        .expect("insert");
    repo.update(
        created.id,
        &UpdateLedgerInvoice {
            amount_cents: Patch::Set(2),
            ..Default::default()
        },
    )
    .await
    .expect("update");

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'test_ledger_invoices' AND record_id = $1 AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(created.id)
        .execute(&mut conn)
        .await
        .expect("lop off the newest revision");
    }

    let broken = repo
        .ledger_verify(created.id)
        .await
        .expect("verify")
        .broken
        .expect("a truncated tail must be detected");
    assert_eq!(
        broken.kind,
        LedgerBreak::MissingRevision,
        "the #2323 mark outlives the revision it names, so the break is the \
         absent sequence number rather than a live row that merely disagrees"
    );
    assert_eq!(broken.seq, 2);

    // Cover the mark up too — the adversary the threat model concedes — and the
    // live-row cross-check still holds the line.
    resync_high_water_mark(&pool, created.id).await;
    let broken = repo
        .ledger_verify(created.id)
        .await
        .expect("verify")
        .broken
        .expect("the live row must still expose the truncation");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
    assert_eq!(broken.seq, 1);
}

/// Rewrite a record's #2323 high-water mark to agree with whatever revisions
/// survive, or remove it when none do — the `SQLite` tier's helper, on this tier.
async fn resync_high_water_mark(pool: &Pool<AsyncPgConnection>, record_id: i64) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "DELETE FROM _autumn_ledger_chain_heads \
         WHERE table_name = 'test_ledger_invoices' AND record_id = $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(record_id)
    .execute(&mut conn)
    .await
    .expect("clear the mark");
    diesel::sql_query(
        "INSERT INTO _autumn_ledger_chain_heads \
         (table_name, tenant_key, record_id, high_seq, head_hash, recorded_at) \
         SELECT table_name, COALESCE(tenant_id, ''), record_id, seq, hash, recorded_at \
         FROM _autumn_ledger_revisions \
         WHERE table_name = 'test_ledger_invoices' AND record_id = $1 \
         ORDER BY seq DESC LIMIT 1",
    )
    .bind::<diesel::sql_types::BigInt, _>(record_id)
    .execute(&mut conn)
    .await
    .expect("re-establish the mark over the surviving chain");
}

/// The Postgres fork of #2323's headline case: delete the newest revision, then
/// let an ordinary write land. The append must allocate past the gap rather than
/// re-use the deleted sequence number — which on this tier means the
/// `_autumn_ledger_chain_heads` primary key, the `ON CONFLICT … DO UPDATE …
/// WHERE` upsert and the `clock_timestamp()` chain-state read all behaving as
/// the `SQLite` tier's do.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_post_truncation_append_leaves_a_gap_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-HW".to_string(),
            amount_cents: 1,
            amount_rate: 1.0,
        })
        .await
        .expect("insert");
    for step in 2..=3 {
        repo.update(
            created.id,
            &UpdateLedgerInvoice {
                amount_cents: Patch::Set(step),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }

    let mark = repo
        .ledger_high_water(created.id)
        .await
        .expect("mark")
        .expect("a written record has a mark");
    assert_eq!(mark.seq, 3, "the mark tracks every append");
    let head = repo
        .ledger_head(created.id)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(mark.hash, head.hash, "the mark names the head revision");
    assert_eq!(mark.recorded_at, head.recorded_at);

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'test_ledger_invoices' AND record_id = $1 AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(created.id)
        .execute(&mut conn)
        .await
        .expect("lop off the newest revision");
    }

    repo.update(
        created.id,
        &UpdateLedgerInvoice {
            amount_cents: Patch::Set(4_242),
            ..Default::default()
        },
    )
    .await
    .expect("an ordinary write lands after the truncation");

    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 4],
        "the append must allocate past the deleted sequence number"
    );

    let broken = repo
        .ledger_verify(created.id)
        .await
        .expect("verify")
        .broken
        .expect("the gap the append left must be reported");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 3);
}

/// The Postgres fork of the transaction-time guarantee: `recorded_at` comes from
/// `clock_timestamp()` and is clamped against the chain's floor, so it is
/// non-decreasing along a chain however the writing host's clock behaves.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn transaction_time_comes_from_the_database_and_never_regresses_on_postgres() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    let before = now();
    let created = repo
        .save(&NewLedgerInvoice {
            reference: "PG-TT".to_string(),
            amount_cents: 1,
            amount_rate: 1.0,
        })
        .await
        .expect("insert");
    for step in 2..=4 {
        repo.update(
            created.id,
            &UpdateLedgerInvoice {
                amount_cents: Patch::Set(step),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }

    let recorded: Vec<DateTime<Utc>> = repo
        .ledger_revisions(created.id)
        .await
        .expect("revisions")
        .iter()
        .map(|r| r.recorded_at)
        .collect();
    assert_eq!(recorded.len(), 4);
    assert!(
        recorded.windows(2).all(|w| w[0] <= w[1]),
        "transaction time must be non-decreasing along a chain: {recorded:?}"
    );
    // The container's clock and this host's are the same machine here, so this
    // is a sanity bound rather than a skew test: what it pins is that
    // `clock_timestamp()` advances per statement rather than being frozen at the
    // transaction's start, which `now()` would have been.
    assert!(
        recorded[0] >= before - Duration::minutes(5),
        "the database clock read is a real instant: {:?}",
        recorded[0]
    );
    assert!(
        recorded[0] < recorded[3],
        "four separate writes must not share one frozen transaction timestamp"
    );

    // Push the record's floor an hour ahead, as a fast host clock would have,
    // and let an ordinary write land behind it.
    let ahead = autumn_web::ledger::truncate_to_micros(now() + Duration::hours(1));
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_chain_heads SET recorded_at = $1 \
             WHERE table_name = 'test_ledger_invoices' AND record_id = $2",
        )
        .bind::<diesel::sql_types::Timestamptz, _>(ahead)
        .bind::<diesel::sql_types::BigInt, _>(created.id)
        .execute(&mut conn)
        .await
        .expect("move the record's transaction-time floor forward");
    }

    repo.update(
        created.id,
        &UpdateLedgerInvoice {
            amount_cents: Patch::Set(7),
            ..Default::default()
        },
    )
    .await
    .expect("update");

    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    let head = revisions.last().expect("head");
    assert!(
        head.recorded_at >= ahead,
        "a write behind the chain's floor must be clamped up to it: {} < {ahead}",
        head.recorded_at
    );
    assert!(
        repo.ledger_verify(created.id)
            .await
            .expect("verify")
            .is_intact(),
        "the clamped instant is the one that was hashed, so the chain still verifies"
    );
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
            amount_rate: 2.5,
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
