//! End-to-end proof of `#[repository(..., soft_delete, ledgered = true)]` — the
//! bitemporal, tamper-evident record ledger (issue #1699) — on the `SQLite`
//! runtime backend.
//!
//! This is the issue's **golden test**: create → update → update a record,
//! reconstruct the as-of state at each intermediate instant and assert it is
//! byte-for-byte identical to an oracle recorded live at that instant, then
//! tamper with a stored revision and assert `ledger_verify` fails at the
//! tampered link.
//!
//! Around it, the same harness pins the rest of the first slice:
//!
//! * one marker, no app code: every insert / update / soft-delete appends a
//!   revision carrying both time axes, with no per-write call;
//! * a field-level diff between two instants;
//! * per-record hash chaining, with the head hash exported for external pinning;
//! * all three injected tampering classes — mutation, insertion, deletion —
//!   detected at the first broken link, with zero false positives on an intact
//!   chain;
//! * fail-closed tenant isolation: a ledgered read scoped to tenant B must not
//!   see tenant A's revisions.
//!
//! Uses an in-memory shared-cache `SQLite` database — no Docker — so the golden
//! test runs on every CI push rather than only in the Docker sweep. The Postgres
//! tier is covered by `tests/integration/ledger_postgres.rs`. Run:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_ledger`.
#![cfg(feature = "sqlite")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::config::DatabaseConfig;
use autumn_web::current::with_actor;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::hooks::Patch;
use autumn_web::ledger::{LedgerAsOf, LedgerBreak};
use autumn_web::reexports::{chrono, diesel, diesel_async};
use autumn_web::tenancy::with_tenant;
use autumn_web::version_history::VersionOp;

use chrono::{DateTime, Duration, Utc};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        lg_invoices (id) {
            id -> Int8,
            reference -> Text,
            amount_cents -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_tenant_invoices (id) {
            id -> Int8,
            reference -> Text,
            tenant_id -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::{lg_invoices, lg_tenant_invoices};

#[autumn_web::model(table = "lg_invoices")]
#[derive(PartialEq, Eq)]
pub struct LgInvoice {
    #[id]
    pub id: i64,
    pub reference: String,
    pub amount_cents: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgInvoice, table = "lg_invoices", soft_delete, ledgered = true)]
pub trait LgInvoiceRepository {}

#[autumn_web::model(table = "lg_tenant_invoices")]
#[derive(PartialEq, Eq)]
pub struct LgTenantInvoice {
    #[id]
    pub id: i64,
    pub reference: String,
    #[default]
    pub tenant_id: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LgTenantInvoice,
    table = "lg_tenant_invoices",
    tenant_scoped,
    soft_delete,
    ledgered = true
)]
pub trait LgTenantInvoiceRepository {}

/// The `SQLite` `_autumn_ledger_revisions` DDL — the exact fork shipped in
/// `autumn/version_history_migrations_sqlite/…/up.sql`. Applied by hand here so
/// the test proves the `SQLite` CREATE (expression unique index included) is
/// valid and that the writer/reader work against it without booting the full
/// migration harness.
const LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS _autumn_ledger_revisions (\
        id          INTEGER PRIMARY KEY, \
        table_name  TEXT    NOT NULL, \
        tenant_id   TEXT, \
        record_id   BIGINT  NOT NULL, \
        seq         BIGINT  NOT NULL, \
        op          TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')), \
        actor       TEXT    NOT NULL DEFAULT 'system', \
        request_id  TEXT, \
        snapshot    TEXT    NOT NULL DEFAULT '{}', \
        valid_from  TEXT    NOT NULL, \
        recorded_at TEXT    NOT NULL, \
        prev_hash   TEXT, \
        hash        TEXT    NOT NULL\
    )";

const LEDGER_CHAIN_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS \
    idx_autumn_ledger_revisions_chain ON _autumn_ledger_revisions \
    (table_name, COALESCE(tenant_id, ''), record_id, seq)";

/// The `SQLite` `_autumn_version_history` DDL. A `ledgered` repository implies
/// `versioned`, so both tables must exist for a write to succeed.
const VERSION_HISTORY_DDL: &str = "CREATE TABLE IF NOT EXISTS _autumn_version_history (\
        id          INTEGER PRIMARY KEY, \
        table_name  TEXT    NOT NULL, \
        tenant_id   TEXT, \
        record_id   BIGINT  NOT NULL, \
        op          TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')), \
        actor       TEXT    NOT NULL DEFAULT 'system', \
        request_id  TEXT, \
        changes     TEXT    NOT NULL DEFAULT '[]', \
        recorded_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP\
    )";

async fn boot_pool(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        for ddl in [
            "CREATE TABLE lg_invoices (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 reference TEXT NOT NULL, \
                 amount_cents BIGINT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_tenant_invoices (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 reference TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            VERSION_HISTORY_DDL,
            LEDGER_DDL,
            LEDGER_CHAIN_INDEX,
        ] {
            diesel::sql_query(ddl)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
        }
    }

    pool
}

/// Wall-clock instant, matching the write path's own clock read.
#[allow(
    clippy::disallowed_methods,
    reason = "test asserts against real recorded_at values"
)]
fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Two writes inside one `SQLite` millisecond would make "the instant between
/// them" ambiguous, so the golden test spaces them.
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

// ── The golden test (issue #1699 AC 6) ───────────────────────────────

/// create → update → update, as-of reconstruction against a live oracle at every
/// intermediate instant, then tamper and assert `verify` names the broken link.
#[tokio::test]
async fn golden_as_of_reconstruction_matches_the_oracle_and_tampering_is_detected() {
    let pool = boot_pool("lg_golden").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());

    // Each step records the live row and the instant it was live at — the oracle
    // an as-of query must reproduce byte for byte.
    let mut oracle: Vec<(DateTime<Utc>, LgInvoice)> = Vec::new();

    let created = with_actor("alice", async {
        repo.save(&NewLgInvoice {
            reference: "INV-1".to_string(),
            amount_cents: 1000,
        })
        .await
        .expect("insert")
    })
    .await;
    let id = created.id;
    tick().await;
    oracle.push((now(), created.clone()));
    tick().await;

    let updated_once = with_actor("alice", async {
        repo.update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(2000),
                ..Default::default()
            },
        )
        .await
        .expect("first update")
    })
    .await;
    tick().await;
    oracle.push((now(), updated_once.clone()));
    tick().await;

    let updated_twice = with_actor("bob", async {
        repo.update(
            id,
            &UpdateLgInvoice {
                reference: Patch::Set("INV-1-REV".to_string()),
                amount_cents: Patch::Set(3000),
                ..Default::default()
            },
        )
        .await
        .expect("second update")
    })
    .await;
    tick().await;
    oracle.push((now(), updated_twice.clone()));

    // ── AC 2: as-of reconstruction is byte-for-byte identical to the oracle ──
    for (instant, expected) in &oracle {
        let reconstructed = repo
            .ledger_as_of(id, *instant)
            .await
            .expect("as-of read")
            .expect("the record existed at this instant");
        assert_eq!(
            &reconstructed, expected,
            "as-of state at {instant} must equal what a live query returned then"
        );
        // "byte-for-byte" in the strongest available sense: the serialized forms
        // match, not just the structural comparison.
        assert_eq!(
            serde_json::to_string(&reconstructed).expect("serialize reconstructed"),
            serde_json::to_string(expected).expect("serialize oracle"),
        );
    }

    // Before the record existed, as-of resolves to nothing.
    let before_creation = oracle[0].0 - Duration::seconds(60);
    assert!(
        repo.ledger_as_of(id, before_creation)
            .await
            .expect("as-of read")
            .is_none(),
        "as-of before creation must report that the record did not exist"
    );

    // ── AC 4: an intact chain verifies, and exports a head hash ─────────
    let report = repo.ledger_verify(id).await.expect("verify");
    assert!(
        report.is_intact(),
        "an untouched chain must verify: {report:?}"
    );
    assert_eq!(report.revisions_checked, 3, "insert + two updates");
    let head = repo
        .ledger_head(id)
        .await
        .expect("head read")
        .expect("a written record has a head");
    assert_eq!(head.seq, 3);
    assert_eq!(report.head_hash.as_deref(), Some(head.hash.as_str()));

    // ── AC 6: tamper with a stored revision; verify fails at that link ──
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_revisions \
             SET snapshot = json_set(snapshot, '$.amount_cents', 999999) \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("tamper with revision 2");
    }

    let tampered = repo
        .ledger_verify(id)
        .await
        .expect("verify after tampering");
    let broken = tampered
        .broken
        .expect("an out-of-band mutation must be detected");
    assert_eq!(broken.kind, LedgerBreak::HashMismatch);
    assert_eq!(broken.seq, 2, "verify must report the first broken link");
    assert!(
        tampered.head_hash.is_none(),
        "a broken chain exports no head"
    );
}

// ── AC 1: one marker, every write path, both time axes ───────────────

#[tokio::test]
async fn every_write_appends_a_revision_with_both_time_axes() {
    let pool = boot_pool("lg_writes").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool);

    let id = with_actor("alice", async {
        let created = repo
            .save(&NewLgInvoice {
                reference: "INV-2".to_string(),
                amount_cents: 500,
            })
            .await
            .expect("insert");
        repo.update(
            created.id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(750),
                ..Default::default()
            },
        )
        .await
        .expect("update");
        repo.delete_by_id(created.id).await.expect("soft delete");
        created.id
    })
    .await;

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
        vec![VersionOp::Insert, VersionOp::Update, VersionOp::Delete],
        "insert, update and soft-delete each append exactly one revision"
    );
    assert_eq!(
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "sequence numbers are contiguous from 1"
    );

    for revision in &revisions {
        assert_eq!(revision.actor, "alice", "the ambient actor is attributed");
        assert_eq!(revision.table_name, "lg_invoices");
        assert_eq!(revision.record_id, id);
        assert!(
            revision.snapshot.get("reference").is_some(),
            "a revision carries a FULL snapshot, not just the changed columns"
        );
        assert!(
            revision.snapshot.get("amount_cents").is_some(),
            "a revision carries a FULL snapshot, not just the changed columns"
        );
        // Both time axes are present; with no `valid_time` column declared they
        // coincide — the fact became true when the database learned it.
        assert_eq!(
            revision.valid_from, revision.recorded_at,
            "valid time defaults to transaction time"
        );
    }

    // The chain is linked: seq 1 opens it, each later revision names its parent.
    assert_eq!(
        revisions[0].prev_hash, None,
        "the chain head opens with no parent"
    );
    assert_eq!(
        revisions[1].prev_hash.as_deref(),
        Some(revisions[0].hash.as_str())
    );
    assert_eq!(
        revisions[2].prev_hash.as_deref(),
        Some(revisions[1].hash.as_str())
    );

    // A soft-deleted record still reconstructs — the row exists, with deleted_at
    // set, exactly as `with_deleted()` would return it.
    let after_delete = repo
        .ledger_as_of_at(id, LedgerAsOf::default())
        .await
        .expect("as-of read")
        .expect("a soft-deleted record still has state");
    assert!(
        after_delete.deleted_at.is_some(),
        "the delete revision snapshots the soft-deleted row"
    );

    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
}

// ── AC 3: field-level diff between two instants ──────────────────────

#[tokio::test]
async fn diff_reports_the_field_level_delta_between_two_instants() {
    let pool = boot_pool("lg_diff").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool);

    let created = repo
        .save(&NewLgInvoice {
            reference: "INV-3".to_string(),
            amount_cents: 100,
        })
        .await
        .expect("insert");
    let id = created.id;
    tick().await;
    let after_create = now();
    tick().await;

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(250),
            ..Default::default()
        },
    )
    .await
    .expect("update");
    tick().await;
    let after_update = now();

    let diff = repo
        .ledger_diff(id, after_create, after_update)
        .await
        .expect("diff");
    assert_eq!(diff.from_seq, Some(1));
    assert_eq!(diff.to_seq, Some(2));
    assert_eq!(
        diff.changes.len(),
        1,
        "only amount_cents changed: {:?}",
        diff.changes
    );
    let change = &diff.changes[0];
    assert_eq!(change.column, "amount_cents");
    assert_eq!(
        change.before.as_ref().and_then(serde_json::Value::as_i64),
        Some(100)
    );
    assert_eq!(
        change.after.as_ref().and_then(serde_json::Value::as_i64),
        Some(250)
    );

    // A window with no writes in it is an empty delta, not an error.
    let quiet = repo
        .ledger_diff(id, after_update, after_update + Duration::seconds(5))
        .await
        .expect("diff");
    assert!(quiet.is_empty(), "{quiet:?}");
}

// ── AC 4: all three tampering classes, zero false positives ──────────

#[tokio::test]
async fn verify_detects_deletion_of_a_stored_revision() {
    let pool = boot_pool("lg_tamper_delete").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("delete revision 2");
    }

    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a deleted revision must be detected");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 2);
}

#[tokio::test]
async fn verify_detects_insertion_of_a_forged_revision() {
    let pool = boot_pool("lg_tamper_insert").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        // Appending a plausible-looking revision without re-deriving the chain
        // is the cheapest forgery available to an attacker with table access.
        diesel::sql_query(
            "INSERT INTO _autumn_ledger_revisions \
             (table_name, tenant_id, record_id, seq, op, actor, request_id, snapshot, \
              valid_from, recorded_at, prev_hash, hash) \
             SELECT table_name, tenant_id, record_id, 4, 'update', 'mallory', NULL, \
                    json_set(snapshot, '$.amount_cents', 1), valid_from, recorded_at, \
                    hash, 'f0f0f0f0' \
             FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("insert a forged revision");
    }

    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("an inserted revision must be detected");
    assert_eq!(broken.kind, LedgerBreak::HashMismatch);
    assert_eq!(broken.seq, 4);
}

#[tokio::test]
async fn verify_has_no_false_positives_on_an_untouched_chain() {
    let pool = boot_pool("lg_no_false_positives").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool);

    // Several records, several revisions each, read back repeatedly.
    let mut ids = Vec::new();
    for n in 0..5 {
        let created = repo
            .save(&NewLgInvoice {
                reference: format!("INV-{n}"),
                amount_cents: i64::from(n),
            })
            .await
            .expect("insert");
        for step in 1..=3 {
            repo.update(
                created.id,
                &UpdateLgInvoice {
                    amount_cents: Patch::Set(i64::from(n) * 100 + step),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        }
        ids.push(created.id);
    }

    for _ in 0..3 {
        for id in &ids {
            let report = repo.ledger_verify(*id).await.expect("verify");
            assert!(report.is_intact(), "record {id} must verify: {report:?}");
            assert_eq!(report.revisions_checked, 4);
        }
    }
}

/// The chain-uniqueness index refuses a second revision at the same sequence,
/// so a forked chain is a hard error rather than silent corruption.
#[tokio::test]
async fn duplicate_sequence_numbers_are_rejected_by_the_database() {
    let pool = boot_pool("lg_unique_chain").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let mut conn = pool.get().await.expect("conn");
    let result = diesel::sql_query(
        "INSERT INTO _autumn_ledger_revisions \
         (table_name, tenant_id, record_id, seq, op, actor, snapshot, valid_from, \
          recorded_at, prev_hash, hash) \
         SELECT table_name, tenant_id, record_id, seq, op, actor, snapshot, valid_from, \
                recorded_at, prev_hash, 'duplicate' \
         FROM _autumn_ledger_revisions \
         WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2",
    )
    .bind::<diesel::sql_types::BigInt, _>(id)
    .execute(&mut *conn)
    .await;

    assert!(
        result.is_err(),
        "the (table, tenant, record, seq) unique index must refuse a forked chain"
    );
}

// ── Tenant isolation (adversarial) ───────────────────────────────────

#[tokio::test]
async fn ledger_reads_fail_closed_across_tenants() {
    let pool = boot_pool("lg_tenant").await;
    let repo = PgLgTenantInvoiceRepository::with_pool_untracked(pool);

    let id = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewLgTenantInvoice {
            reference: "A-1".to_string(),
        })
        .await
        .expect("insert as tenant-a")
        .id
    })
    .await;

    // Tenant A sees its own chain.
    let mine = with_tenant("tenant-a".to_string(), async {
        repo.ledger_revisions(id).await.expect("read as tenant-a")
    })
    .await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].tenant_id.as_deref(), Some("tenant-a"));

    // Tenant B, asking for the same record id, sees nothing.
    let theirs = with_tenant("tenant-b".to_string(), async {
        repo.ledger_revisions(id).await.expect("read as tenant-b")
    })
    .await;
    assert!(
        theirs.is_empty(),
        "a ledgered read must not leak another tenant's revisions: {theirs:?}"
    );

    let reconstructed = with_tenant("tenant-b".to_string(), async {
        repo.ledger_as_of_at(id, LedgerAsOf::default())
            .await
            .expect("as-of as tenant-b")
    })
    .await;
    assert!(
        reconstructed.is_none(),
        "as-of must fail closed across tenants"
    );
}

async fn write_three_revisions(repo: &PgLgInvoiceRepository) -> i64 {
    let created = repo
        .save(&NewLgInvoice {
            reference: "INV-X".to_string(),
            amount_cents: 1,
        })
        .await
        .expect("insert");
    for amount in [2, 3] {
        repo.update(
            created.id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(amount),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }
    created.id
}
