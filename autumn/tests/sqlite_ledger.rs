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
// The `f64` column makes the `#[model]` expansion (change tracking uses strict
// equality) trip clippy's `float_cmp`; that generated comparison is the macro's
// concern, not this test's, and an item-level allow can't reach the
// macro-emitted impls. Same convention as `form_for_derive.rs`.
#![allow(clippy::float_cmp)]

use autumn_web::config::DatabaseConfig;
use autumn_web::current::with_actor;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::hooks::Patch;
use autumn_web::ledger::{LedgerAsOf, LedgerBreak};
use autumn_web::reexports::{chrono, diesel, diesel_async};
use autumn_web::tenancy::with_tenant;
use autumn_web::version_history::VersionOp;

use chrono::{DateTime, Duration, Utc};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        lg_invoices (id) {
            id -> Int8,
            reference -> Text,
            amount_cents -> Int8,
            // A float and a nested-JSON column: the two shapes where a
            // snapshot round-trip is most likely to lose fidelity.
            amount_rate -> Double,
            metadata -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_effective_notes (id) {
            id -> Int8,
            body -> Text,
            effective_at -> Timestamp,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_vault_notes (id) {
            id -> Int8,
            body -> Text,
            secret -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_cascade_parents (id) {
            id -> Int8,
            name -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_cascade_children (id) {
            id -> Int8,
            parent_id -> Int8,
            label -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        lg_secret_notes (id) {
            id -> Int8,
            body -> Text,
            internal_note -> Text,
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

use schema::{
    lg_cascade_children, lg_cascade_parents, lg_effective_notes, lg_invoices, lg_secret_notes,
    lg_tenant_invoices, lg_vault_notes,
};

#[autumn_web::model(table = "lg_invoices")]
pub struct LgInvoice {
    #[id]
    pub id: i64,
    pub reference: String,
    pub amount_cents: i64,
    /// A float, deliberately. Postgres `jsonb` renders numbers through
    /// `numeric`, so a value serde writes as `1e16` would come back as
    /// `10000000000000000` and re-canonicalize to different bytes than the ones
    /// that were hashed — which is why the snapshot column is `TEXT` on both
    /// tiers. This column keeps that decision under test.
    pub amount_rate: f64,
    /// Nested JSON, stored as text, to prove key order inside a column value
    /// survives the canonicalization the hash depends on.
    pub metadata: String,
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

/// A model whose valid time comes from its own column, so the two axes diverge.
#[autumn_web::model(table = "lg_effective_notes")]
#[derive(PartialEq, Eq)]
pub struct LgEffectiveNote {
    #[id]
    pub id: i64,
    pub body: String,
    pub effective_at: chrono::NaiveDateTime,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LgEffectiveNote,
    table = "lg_effective_notes",
    soft_delete,
    ledgered(valid_time = "effective_at")
)]
pub trait LgEffectiveNoteRepository {}

// ── A hard-deleting parent cascading into a ledgered child ───────────
//
// The parent's macro cannot see that the child is ledgered — separate
// `#[repository]` invocations — so the combination cannot be refused at compile
// time. Erasing the child would destroy the record its ledger reconstructs;
// soft-deleting it would leave a live foreign key pointing at a parent row about
// to disappear, which the database rejects. It is refused with a typed error.

#[autumn_web::model(table = "lg_cascade_children")]
pub struct LgCascadeChild {
    #[id]
    pub id: i64,
    pub parent_id: i64,
    pub label: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LgCascadeChild,
    table = "lg_cascade_children",
    soft_delete,
    ledgered = true
)]
pub trait LgCascadeChildRepository {}

#[autumn_web::model(table = "lg_cascade_parents")]
pub struct LgCascadeParent {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(
    LgCascadeParent,
    table = "lg_cascade_parents",
    dependent(PgLgCascadeChildRepository, fk = "parent_id", on_delete = destroy)
)]
pub trait LgCascadeParentRepository {}

/// A model with an at-rest-encrypted column, in the default (randomized) mode.
///
/// The ciphertext differs on every write, so the cross-check has to compare the
/// plaintext underneath — otherwise a revision whose only change was to this
/// column would be invisible to it, which is exactly what deleting that revision
/// would exploit.
#[autumn_web::model(table = "lg_vault_notes")]
pub struct LgVaultNote {
    #[id]
    pub id: i64,
    pub body: String,
    #[encrypted]
    pub secret: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgVaultNote, table = "lg_vault_notes", soft_delete, ledgered = true)]
pub trait LgVaultNoteRepository {}

/// A model with a column the public JSON omits.
///
/// `#[model]` stamps `#[serde(skip_serializing)]` on a `#[private]` field, so a
/// serde-shaped live-state cross-check would be blind to it — the hole Codex
/// flagged on #2318, and the mirror-image false positive of comparing a
/// codec-shaped snapshot against a serde-shaped live row.
#[autumn_web::model(table = "lg_secret_notes")]
#[derive(PartialEq, Eq)]
pub struct LgSecretNote {
    #[id]
    pub id: i64,
    pub body: String,
    #[private]
    pub internal_note: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgSecretNote, table = "lg_secret_notes", soft_delete, ledgered = true)]
pub trait LgSecretNoteRepository {}

/// The migration SQL Autumn actually ships, applied verbatim.
///
/// Included rather than hand-copied so a syntax error or a schema change in
/// `version_history_migrations_sqlite/` fails this suite instead of sailing past
/// it — the previous hand-copied constants had already drifted from the shipped
/// file by two indexes.
const LEDGER_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260826000000_create_ledger_revisions/up.sql"
);

/// The #2323 high-water marks, from the same shipped migration set.
///
/// Applied after `LEDGER_UP` because its backfill reads the revisions table.
const LEDGER_HIGH_WATER_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260901213107_create_ledger_high_water/up.sql"
);

/// A `ledgered` repository implies `versioned`, so both tables must exist for a
/// write to succeed.
const VERSION_HISTORY_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260526000000_create_version_history/up.sql"
);

/// Install attribute-encryption keys once for this test binary.
///
/// `LgVaultNote` registers an `#[encrypted]` column, so the codec needs a key
/// ring to encode and decode it. Fixture key material only.
fn install_encryption_keys() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        const PRIMARY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const DETERMINISTIC: &str =
            "3333333333333333333333333333333333333333333333333333333333333333";
        let ring = autumn_web::encryption::KeyRing::from_master_hex(
            PRIMARY,
            &[],
            Some(DETERMINISTIC),
            b"ledger-suite-salt",
        )
        .expect("fixture key material derives");
        autumn_web::encryption::install_key_ring(ring);
    });
}

/// The full schema, including the #2323 high-water table.
async fn boot_pool(db_name: &str) -> SqlitePool {
    let pool = boot_pool_without_high_water(db_name).await;
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        conn.batch_execute(LEDGER_HIGH_WATER_UP)
            .await
            .expect("apply the high-water migration");
    }
    pool
}

/// The revisions-only schema `boot_pool` layers the high-water table onto.
async fn boot_pool_without_high_water(db_name: &str) -> SqlitePool {
    install_encryption_keys();
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
                 amount_rate DOUBLE PRECISION NOT NULL, \
                 metadata TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_tenant_invoices (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 reference TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_vault_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 body TEXT NOT NULL, \
                 secret TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_cascade_parents (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 name TEXT NOT NULL\
             )",
            "CREATE TABLE lg_cascade_children (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 parent_id BIGINT NOT NULL REFERENCES lg_cascade_parents(id), \
                 label TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_secret_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 body TEXT NOT NULL, \
                 internal_note TEXT NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            "CREATE TABLE lg_effective_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 body TEXT NOT NULL, \
                 effective_at TIMESTAMP NOT NULL, \
                 deleted_at TIMESTAMP\
             )",
            VERSION_HISTORY_UP,
            LEDGER_UP,
        ] {
            conn.batch_execute(ddl)
                .await
                .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
        }
    }

    pool
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

/// Rewrite a record's #2323 high-water mark to agree with whatever revisions
/// survive, or remove it when none do.
///
/// The mark turns a deleted tail into permanent evidence, which is the whole
/// point of it — so a test that wants to exercise a *later* layer (the live-row
/// cross-check, or a pinned head) has to model the adversary the threat model
/// actually concedes: one who holds `DELETE` on the revisions table and on the
/// mark table alike, and covers their tracks in both. Everything the ledger can
/// still see after that is what these tests pin.
async fn resync_high_water_mark(pool: &SqlitePool, table: &str, record_id: i64) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "DELETE FROM _autumn_ledger_high_water WHERE table_name = ? AND record_id = ?",
    )
    .bind::<diesel::sql_types::Text, _>(table)
    .bind::<diesel::sql_types::BigInt, _>(record_id)
    .execute(&mut *conn)
    .await
    .expect("clear the mark");
    diesel::sql_query(
        "INSERT INTO _autumn_ledger_high_water \
         (table_name, tenant_key, record_id, high_seq, head_hash, recorded_at) \
         SELECT table_name, COALESCE(tenant_id, ''), record_id, seq, hash, recorded_at \
         FROM _autumn_ledger_revisions \
         WHERE table_name = ? AND record_id = ? ORDER BY seq DESC LIMIT 1",
    )
    .bind::<diesel::sql_types::Text, _>(table)
    .bind::<diesel::sql_types::BigInt, _>(record_id)
    .execute(&mut *conn)
    .await
    .expect("re-establish the mark over the surviving chain");
}

/// Host wall-clock instant, for spacing writes and for building the floors these
/// tests inject. The write path itself reads the *database's* clock (#2323); on
/// this tier that is the same process's clock, so the two stay comparable.
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
// The issue's golden scenario end to end; splitting it would mean re-running the
// whole write sequence per assertion.
#[allow(clippy::too_many_lines)]
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
            amount_rate: 1e16,
            metadata: r#"{"b":1,"a":{"y":2,"x":3}}"#.to_string(),
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
        assert_same_record(
            &reconstructed,
            expected,
            &format!("as-of state at {instant} must equal what a live query returned then"),
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
                amount_rate: 1e16,
                metadata: r#"{"b":1,"a":{"y":2,"x":3}}"#.to_string(),
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
            amount_rate: 1e16,
            metadata: r#"{"b":1,"a":{"y":2,"x":3}}"#.to_string(),
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
                amount_rate: 1e16,
                metadata: r#"{"b":1,"a":{"y":2,"x":3}}"#.to_string(),
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

/// The #2323 mark is monotonic in the *database*, not merely in the code above
/// it: the append's upsert refuses to lower `high_seq`, so a writer that somehow
/// computed a stale sequence number cannot roll the mark back to make a later
/// truncation look clean. It can only fail to raise it — and a mark that no
/// longer describes the head it names is itself reported.
#[tokio::test]
async fn the_high_water_mark_upsert_refuses_to_move_backwards() {
    let pool = boot_pool("lg_mark_monotonic").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        // Byte-for-byte the statement the write path runs, with a stale seq.
        diesel::sql_query(
            "INSERT INTO _autumn_ledger_high_water \
             (table_name, tenant_key, record_id, high_seq, head_hash, recorded_at) \
             VALUES ('lg_invoices', '', ?, 1, 'stale', ?) \
             ON CONFLICT (table_name, tenant_key, record_id) DO UPDATE SET \
             high_seq = excluded.high_seq, \
             head_hash = excluded.head_hash, \
             recorded_at = excluded.recorded_at \
             WHERE _autumn_ledger_high_water.high_seq < excluded.high_seq",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(now())
        .execute(&mut *conn)
        .await
        .expect("the upsert itself must succeed — it simply must not lower the mark");
    }

    let mark = repo
        .ledger_high_water(id)
        .await
        .expect("mark")
        .expect("mark");
    assert_eq!(
        mark.seq, 3,
        "a stale writer must not be able to lower the mark"
    );
    assert_ne!(mark.hash, "stale");
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
}

/// A ledgered write is one transaction: the revision and the mark it raises
/// commit together or not at all.
///
/// Driven from the mark's side, because that is the statement #2323 added: the
/// revision INSERT has already run by the time the upsert does, so if the two
/// were not one transaction a refused upsert would leave a revision behind with
/// no mark naming it — which is itself a state `ledger_verify` calls tampering.
#[tokio::test]
async fn a_failed_mark_upsert_rolls_the_whole_append_back() {
    let pool = boot_pool("lg_mark_rollback").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        conn.batch_execute(
            "CREATE TRIGGER lg_freeze_mark BEFORE UPDATE ON _autumn_ledger_high_water \
             BEGIN SELECT RAISE(ABORT, 'mark frozen'); END",
        )
        .await
        .expect("freeze the mark");
    }

    let result = repo
        .update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(99),
                ..Default::default()
            },
        )
        .await;
    assert!(
        result.is_err(),
        "an append whose mark cannot be raised must fail rather than proceed"
    );

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.len(),
        3,
        "the revision must roll back with the mark, not survive it: {:?}",
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>()
    );
    let mark = repo
        .ledger_high_water(id)
        .await
        .expect("mark")
        .expect("mark");
    assert_eq!(mark.seq, 3);
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());

    // Unfreeze: the next ordinary write advances both together again.
    {
        let mut conn = pool.get().await.expect("conn");
        conn.batch_execute("DROP TRIGGER lg_freeze_mark")
            .await
            .expect("unfreeze the mark");
    }
    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(99),
            ..Default::default()
        },
    )
    .await
    .expect("update");
    let mark = repo
        .ledger_high_water(id)
        .await
        .expect("mark")
        .expect("mark");
    assert_eq!(
        mark.seq, 4,
        "the rolled-back attempt burned no sequence number"
    );
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
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

// ── restore, bulk paths, and the live-state cross-check ──────────────

/// `restore` is the sanctioned inverse of a ledgered delete, so it must record
/// the undelete. Without a revision, `ledger_as_of(now)` would report a deleted
/// row forever while the table shows it live — and `ledger_verify` would call
/// that chain intact.
#[tokio::test]
async fn restore_records_a_revision_and_keeps_as_of_true() {
    let pool = boot_pool("lg_restore").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool);

    let id = write_three_revisions(&repo).await;
    repo.delete_by_id(id).await.expect("soft delete");
    repo.restore(id).await.expect("restore");

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
        vec![
            VersionOp::Insert,
            VersionOp::Update,
            VersionOp::Update,
            VersionOp::Delete,
            VersionOp::Update,
        ],
        "restore appends an update revision"
    );

    let reconstructed = repo
        .ledger_as_of_at(id, LedgerAsOf::default())
        .await
        .expect("as-of")
        .expect("state exists");
    assert!(
        reconstructed.deleted_at.is_none(),
        "the restore revision must snapshot the undeleted row"
    );

    let live = repo
        .find_by_id(id)
        .await
        .expect("live read")
        .expect("live row");
    assert_same_record(&reconstructed, &live, "as-of(now) must equal the live row");
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
}

/// A truncated tail leaves a chain that is internally perfect. Two independent
/// layers still see it: the #2323 high-water mark, which names the sequence
/// number that is gone, and — once the mark has been covered up too — the
/// live-row cross-check.
#[tokio::test]
async fn verify_detects_a_truncated_tail() {
    let pool = boot_pool("lg_truncate").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("lop off the newest revision");
    }

    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a truncated tail must be detected");
    assert_eq!(
        broken.kind,
        LedgerBreak::MissingRevision,
        "the mark outlives the row it names, so the break is the absent revision \
         rather than merely a live row that disagrees"
    );
    assert_eq!(
        broken.seq, 3,
        "reported at the sequence number that is gone"
    );

    // Cover the mark up as well, and the live-row cross-check still holds the line.
    resync_high_water_mark(&pool, "lg_invoices", id).await;
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the live row must still expose the truncation");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
    assert_eq!(broken.seq, 2, "reported at the surviving head");
}

/// The issue's headline case: delete the newest revision, then let an **ordinary
/// application write** land. Before #2323 the append re-used the deleted
/// sequence number, chained cleanly onto its predecessor and matched the live
/// row — so both the chain walk and the live-row cross-check reported intact and
/// the deleted state left no trace. The mark makes the append allocate past the
/// gap instead, and the gap is permanent.
#[tokio::test]
async fn a_post_truncation_append_leaves_a_gap_verify_reports() {
    let pool = boot_pool("lg_truncate_then_append").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("lop off the newest revision");
    }

    // Ordinary traffic — nothing about this write knows a truncation happened.
    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(4_242),
            ..Default::default()
        },
    )
    .await
    .expect("an ordinary write lands after the truncation");

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 4],
        "the append must allocate past the deleted sequence number, not re-use it"
    );

    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the gap the append left must be reported");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 3);

    // And it stays reported: further ordinary writes never fill the hole.
    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(4_343),
            ..Default::default()
        },
    )
    .await
    .expect("more ordinary traffic");
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the gap must not heal");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 3);
}

/// The mark is not a second source of truth to be believed: rolling it back,
/// rewriting it or deleting it is itself what `ledger_verify` reports.
#[tokio::test]
async fn tampering_with_the_high_water_mark_is_itself_detected() {
    let pool = boot_pool("lg_mark_tamper").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let mark = repo
        .ledger_high_water(id)
        .await
        .expect("mark")
        .expect("a written record has a mark");
    assert_eq!(mark.seq, 3, "the mark tracks every append");
    let head = repo
        .ledger_head(id)
        .await
        .expect("head")
        .expect("a written record has a head");
    assert_eq!(mark.hash, head.hash, "the mark names the head revision");

    // Rolled back, so a later truncation would look clean.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET high_seq = 1 \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("roll the mark back");
    }
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a rolled-back mark must be detected");
    assert_eq!(broken.kind, LedgerBreak::HighWaterBehind);

    // Rewritten to name a different revision at the same sequence number.
    resync_high_water_mark(&pool, "lg_invoices", id).await;
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET head_hash = 'deadbeef' \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("rewrite the mark's hash");
    }
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a rewritten mark must be detected");
    assert_eq!(broken.kind, LedgerBreak::HighWaterMismatch);

    // Deleted outright — the obvious way to restore the original attack.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_high_water \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("delete the mark");
    }
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a deleted mark must be detected");
    assert_eq!(broken.kind, LedgerBreak::HighWaterMissing);

    // Restored, and the accusation stops: no false positive once it agrees again.
    resync_high_water_mark(&pool, "lg_invoices", id).await;
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
}

/// A wholly erased chain used to be indistinguishable from a row that predates
/// ledgering, so it could not be reported. The #2323 mark outlives the rows and
/// tells the two apart, so erasure is now an accusation.
#[tokio::test]
async fn a_wholly_erased_chain_is_reported_against_its_surviving_mark() {
    let pool = boot_pool("lg_erased_marked").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("erase the whole chain");
    }

    let report = repo.ledger_verify(id).await.expect("verify");
    assert_eq!(report.revisions_checked, 0);
    let broken = report.broken.expect("an erased chain must be reported");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 1);
}

/// A row with no chain **and no mark** is the documented state of every row that
/// predates the day its model was ledgered, so it must not be reported as
/// tampering. The emptiness is still visible on the report.
#[tokio::test]
async fn a_row_with_no_chain_is_not_reported_as_tampering() {
    let pool = boot_pool("lg_erased").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("leave the record with no chain");
    }
    // No revisions and no mark: exactly the shape of a row written before its
    // model was ledgered.
    resync_high_water_mark(&pool, "lg_invoices", id).await;

    let report = repo.ledger_verify(id).await.expect("verify");
    assert!(report.is_intact(), "{report:?}");
    assert_eq!(
        report.revisions_checked, 0,
        "the empty chain is what the caller inspects, not an accusation"
    );

    // A write after that point opens a fresh chain, exactly as it would for a
    // row that predates ledgering — and that chain verifies.
    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(42),
            ..Default::default()
        },
    )
    .await
    .expect("update");
    let report = repo.ledger_verify(id).await.expect("verify");
    assert!(report.is_intact(), "{report:?}");
    assert_eq!(report.revisions_checked, 1);
}

/// A correctly-hashed appended forgery is caught by the #2323 mark, which the
/// forger did not raise. Cover the mark up too — the adversary the threat model
/// concedes, with write access to both tables — and the forgery becomes
/// undetectable from inside the chain, because the hashing rule is public. A
/// pinned head is the defence there, so prove the pin actually disagrees.
#[tokio::test]
async fn a_pinned_head_detects_a_correctly_hashed_forgery() {
    let pool = boot_pool("lg_pinned_head").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let pinned = repo
        .ledger_head(id)
        .await
        .expect("head")
        .expect("a written record has a head");

    // Mallory appends a well-formed revision, computing the hash the same way
    // the framework does, and rewrites the live row to match so even the
    // live-state cross-check is satisfied.
    let head_revision = repo
        .ledger_revisions(id)
        .await
        .expect("revisions")
        .pop()
        .expect("head revision");
    let mut snapshot = head_revision.snapshot.clone();
    snapshot["amount_cents"] = serde_json::json!(999_999);
    let recorded_at = autumn_web::ledger::truncate_to_micros(now());
    let forged_hash = autumn_web::ledger::revision_hash(&autumn_web::ledger::RevisionHashInput {
        prev_hash: Some(head_revision.hash.as_str()),
        table_name: "lg_invoices",
        tenant_id: None,
        record_id: id,
        seq: head_revision.seq + 1,
        op: VersionOp::Update,
        actor: "mallory",
        request_id: None,
        snapshot: &snapshot,
        valid_from: recorded_at,
        recorded_at,
    });
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO _autumn_ledger_revisions \
             (table_name, tenant_id, record_id, seq, op, actor, request_id, snapshot, \
              valid_from, recorded_at, prev_hash, hash) \
             VALUES ('lg_invoices', NULL, ?, ?, 'update', 'mallory', NULL, ?, ?, ?, ?, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::BigInt, _>(head_revision.seq + 1)
        .bind::<diesel::sql_types::Text, _>(autumn_web::ledger::canonical_json(&snapshot))
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(recorded_at)
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(recorded_at)
        .bind::<diesel::sql_types::Text, _>(head_revision.hash.clone())
        .bind::<diesel::sql_types::Text, _>(forged_hash)
        .execute(&mut *conn)
        .await
        .expect("append a well-formed forgery");
        diesel::sql_query("UPDATE lg_invoices SET amount_cents = 999999 WHERE id = ?")
            .bind::<diesel::sql_types::BigInt, _>(id)
            .execute(&mut *conn)
            .await
            .expect("rewrite the live row to match");
    }

    // The mark still names revision 3, so the appended revision 4 is exposed
    // without any pin at all. Note this is `HighWaterBehind`, which the record's
    // next ordinary write heals — the pin below is what survives that.
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("an append the mark did not authorise must be detected");
    assert_eq!(broken.kind, LedgerBreak::HighWaterBehind);
    assert_eq!(broken.seq, head_revision.seq + 1);

    // Now Mallory raises the mark too. Verification alone cannot see it any
    // more — this is the documented limit the pin exists for.
    resync_high_water_mark(&pool, "lg_invoices", id).await;
    assert!(
        repo.ledger_verify(id).await.expect("verify").is_intact(),
        "a correctly-hashed, live-consistent, mark-consistent append is invisible \
         from inside the database"
    );

    // The pin does.
    let now_head = repo.ledger_head(id).await.expect("head").expect("head");
    assert_ne!(
        now_head.hash, pinned.hash,
        "a head pinned outside the database must disagree after a forgery"
    );
    assert_eq!(now_head.seq, pinned.seq + 1);
}

// ── transaction time comes from the database (#2323) ─────────────────

/// `recorded_at` used to be `Utc::now()` on the writing node, so clock skew
/// across nodes — or a single host clock adjustment — could give a later
/// sequence an earlier instant, and `snapshot_as_of` would then answer a
/// transaction-time query with a revision that was not yet current. The write
/// path now reads the instant from the database and clamps it against the chain
/// it is extending, so a regression cannot be written at all.
#[tokio::test]
async fn transaction_time_never_moves_backwards_along_a_chain() {
    let pool = boot_pool("lg_txn_time_monotonic").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let recorded: Vec<DateTime<Utc>> = repo
        .ledger_revisions(id)
        .await
        .expect("revisions")
        .iter()
        .map(|r| r.recorded_at)
        .collect();
    assert!(
        recorded.windows(2).all(|w| w[0] <= w[1]),
        "transaction time must be non-decreasing along a chain: {recorded:?}"
    );

    // The mark's instant is the floor only where it is the *only* floor: after
    // the revision that carried the real instant is gone. Lop the head off and
    // push the mark forward, as a host whose clock ran fast would have left it.
    let ahead = autumn_web::ledger::truncate_to_micros(now() + Duration::minutes(30));
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("lop off the newest revision");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET recorded_at = ? \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(ahead)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("move the record's transaction-time floor forward");
    }

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(7),
            ..Default::default()
        },
    )
    .await
    .expect("update");

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    let head = revisions.last().expect("head");
    assert!(
        head.recorded_at >= ahead,
        "a write behind the chain's floor must be clamped up to it, not written \
         behind it: {} < {ahead}",
        head.recorded_at
    );
    let recorded: Vec<DateTime<Utc>> = revisions.iter().map(|r| r.recorded_at).collect();
    assert!(
        recorded.windows(2).all(|w| w[0] <= w[1]),
        "still non-decreasing across the gap the deleted revision left: {recorded:?}"
    );
    // The clamped instant is the one that was hashed *and* the one that was
    // stored — the reason the write path truncates to microseconds before doing
    // either. A drift between the two would surface as a `HashMismatch` on an
    // untampered chain.
    assert_eq!(
        head.compute_hash(),
        head.hash,
        "the stored transaction time must be the value that was hashed"
    );
}

/// The floor is a clamp, not a ratchet. The mark's instant carries no hash of
/// its own, so an unbounded `max` would let one out-of-band `UPDATE` push a
/// record's transaction time arbitrarily far forward — every later revision
/// would hash a far-future instant *correctly*, so `ledger_verify` could not
/// object, while `ledger_as_of` quietly stopped returning any of them. Past the
/// tolerated skew the write is refused instead.
#[tokio::test]
async fn a_transaction_time_floor_far_ahead_of_the_database_refuses_the_write() {
    let pool = boot_pool("lg_txn_time_ratchet").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let far = autumn_web::ledger::truncate_to_micros(now() + Duration::days(365 * 900));
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("lop off the newest revision");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET recorded_at = ? \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(far)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("poison the record's transaction-time floor");
    }

    let result = repo
        .update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(7),
                ..Default::default()
            },
        )
        .await;
    assert!(
        result.is_err(),
        "a poisoned floor must refuse the write rather than hash a far-future \
         instant into the chain"
    );

    // Nothing was written, so nothing was destroyed: the surviving revisions
    // still carry real instants and the truncation is still reported.
    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(revisions.len(), 2);
    assert!(revisions.iter().all(|r| r.recorded_at < far));
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the truncation is still reported");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
}

/// Rewriting *only* the mark's instant is a mark rewrite like any other: it is
/// what `ledger_verify` calls `HighWaterMismatch`, so the write path refuses it
/// on exactly the same condition. Anything verify can see must not be something
/// an ordinary write erases.
#[tokio::test]
async fn a_rewritten_mark_instant_is_refused_like_any_other_mark_rewrite() {
    let pool = boot_pool("lg_txn_time_mark_rewrite").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let ahead = autumn_web::ledger::truncate_to_micros(now() + Duration::minutes(45));
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET recorded_at = ? \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(ahead)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("rewrite the mark's instant, leaving seq and hash alone");
    }

    assert_eq!(
        repo.ledger_verify(id)
            .await
            .expect("verify")
            .broken
            .expect("reported")
            .kind,
        LedgerBreak::HighWaterMismatch
    );

    assert!(
        repo.update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(11),
                ..Default::default()
            },
        )
        .await
        .is_err(),
        "the append must refuse rather than overwrite the mark and launder the \
         accusation away"
    );

    // Still reported afterwards, and nothing was written.
    assert_eq!(repo.ledger_revisions(id).await.expect("revisions").len(), 3);
    assert_eq!(
        repo.ledger_verify(id)
            .await
            .expect("verify")
            .broken
            .expect("still reported")
            .kind,
        LedgerBreak::HighWaterMismatch
    );
}

/// While the head revision is present its own instant — which its hash covers —
/// is the floor, and the mark's is ignored. Shown in the one state where a
/// disagreeing mark still writes: a mark *behind* the head, which is what a
/// pre-#2323 node in a mixed-version fleet leaves.
#[tokio::test]
async fn a_mark_behind_the_head_cannot_steer_the_next_revisions_time() {
    let pool = boot_pool("lg_txn_time_mark_ignored").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let ahead = autumn_web::ledger::truncate_to_micros(now() + Duration::minutes(45));
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET high_seq = 2, \
             head_hash = (SELECT hash FROM _autumn_ledger_revisions \
                          WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2), \
             recorded_at = ? \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(ahead)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("leave the mark behind, carrying a bogus instant");
    }

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(11),
            ..Default::default()
        },
    )
    .await
    .expect("a mark behind the head must not block writes");

    let head = repo
        .ledger_revisions(id)
        .await
        .expect("revisions")
        .pop()
        .expect("head");
    assert!(
        head.recorded_at < ahead,
        "the mark's unhashed instant must not become the floor while the head \
         revision that carries a hashed one is still there: {} >= {ahead}",
        head.recorded_at
    );
    assert!(repo.ledger_verify(id).await.expect("verify").is_intact());
}

/// The as-of guarantee that rests on it: a transaction-time query at `t` is
/// answered by the newest revision recorded at or before `t`, and never by one
/// recorded after it.
///
/// Asserted through `ledger_as_of` — the API an auditor actually calls — rather
/// than by re-deriving the answer from the same filter the implementation uses.
#[tokio::test]
async fn an_as_of_query_never_returns_a_revision_recorded_after_the_instant() {
    let pool = boot_pool("lg_txn_time_as_of").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());

    let created = repo
        .save(&NewLgInvoice {
            reference: "INV-AS-OF".to_string(),
            amount_cents: 1,
            amount_rate: 1.5,
            metadata: "{}".to_string(),
        })
        .await
        .expect("insert");
    for step in 2..=4 {
        tick().await;
        repo.update(
            created.id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(step),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }

    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(revisions.len(), 4);
    // The premise the guarantee rests on. Without it the walk below would be
    // asking a question with no well-defined answer.
    assert!(
        revisions
            .windows(2)
            .all(|w| w[0].recorded_at < w[1].recorded_at),
        "the writes are spaced, so each revision has its own instant"
    );

    for (index, revision) in revisions.iter().enumerate() {
        // At a revision's own instant, that revision is the answer.
        let at_instant = repo
            .ledger_as_of(created.id, revision.recorded_at)
            .await
            .expect("as-of")
            .expect("the record existed");
        assert_eq!(
            serde_json::json!(at_instant.amount_cents),
            revision.snapshot["amount_cents"],
            "as-of at revision {}'s own instant must return revision {}",
            revision.seq,
            revision.seq,
        );

        // One microsecond earlier it cannot be: the answer is the *previous*
        // revision, or nothing at all before the insert.
        let just_before = revision.recorded_at - Duration::microseconds(1);
        let earlier = repo
            .ledger_as_of(created.id, just_before)
            .await
            .expect("as-of");
        match index.checked_sub(1).and_then(|prev| revisions.get(prev)) {
            None => assert!(
                earlier.is_none(),
                "before the insert the record did not exist yet"
            ),
            Some(previous) => {
                let earlier = earlier.expect("the record existed");
                assert_eq!(
                    serde_json::json!(earlier.amount_cents),
                    previous.snapshot["amount_cents"],
                    "as-of just before revision {} must return revision {}, never a \
                     revision recorded after the instant asked about",
                    revision.seq,
                    previous.seq,
                );
            }
        }
    }
}

/// A stored chain whose transaction time moves backwards is a forgery, or a
/// chain written before #2323 across a host clock step. Either way it is now
/// reported rather than walked past in `seq` order.
#[tokio::test]
async fn verify_detects_a_transaction_time_that_moves_backwards() {
    let pool = boot_pool("lg_txn_time_regression").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    let head = revisions.last().expect("head");
    let backdated = revisions[1].recorded_at - Duration::seconds(1);
    // Re-hash so the row still hashes to its stored digest: the point is that a
    // regression survives every *other* check and needs one of its own.
    let rehashed = autumn_web::ledger::revision_hash(&autumn_web::ledger::RevisionHashInput {
        prev_hash: head.prev_hash.as_deref(),
        table_name: "lg_invoices",
        tenant_id: None,
        record_id: id,
        seq: head.seq,
        op: head.op,
        actor: head.actor.as_str(),
        request_id: head.request_id.as_deref(),
        snapshot: &head.snapshot,
        valid_from: head.valid_from,
        recorded_at: backdated,
    });
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_revisions SET recorded_at = ?, hash = ? \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = ?",
        )
        .bind::<diesel::sql_types::TimestamptzSqlite, _>(backdated)
        .bind::<diesel::sql_types::Text, _>(rehashed)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::BigInt, _>(head.seq)
        .execute(&mut *conn)
        .await
        .expect("back-date the newest revision");
    }
    // Cover the mark up too, so nothing but the regression check can fire.
    resync_high_water_mark(&pool, "lg_invoices", id).await;

    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("a backwards transaction time must be detected");
    assert_eq!(broken.kind, LedgerBreak::RecordedAtRegression);
    assert_eq!(broken.seq, head.seq);
}

// ── the mark is cross-checked on the write path too (#2323) ──────────

/// The obvious way to restore the pre-#2323 attack is to delete the revision
/// *and* the mark, then wait for ordinary traffic to re-create both. The append
/// refuses instead, so the evidence outlives the attacker's patience.
#[tokio::test]
async fn an_append_refuses_to_re_create_a_deleted_high_water_mark() {
    let pool = boot_pool("lg_mark_recreate").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        for sql in [
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
            "DELETE FROM _autumn_ledger_high_water \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        ] {
            diesel::sql_query(sql)
                .bind::<diesel::sql_types::BigInt, _>(id)
                .execute(&mut *conn)
                .await
                .expect("cover both tables");
        }
    }

    let result = repo
        .update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(1),
                ..Default::default()
            },
        )
        .await;
    assert!(
        result.is_err(),
        "an append over a chain whose mark is gone must refuse rather than \
         re-create the mark and launder the truncation away"
    );

    // Nothing moved, and the accusation stands.
    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(revisions.len(), 2);
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the missing mark is still reported");
    assert_eq!(broken.kind, LedgerBreak::HighWaterMissing);
}

/// A mark that names the head's sequence number but a different revision means
/// one of the two tables was rewritten. Appending would settle that in the
/// writer's favour, so it is refused.
#[tokio::test]
async fn an_append_refuses_when_the_mark_and_the_head_disagree() {
    let pool = boot_pool("lg_mark_disagree").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET head_hash = 'deadbeef' \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("rewrite the mark");
    }

    let result = repo
        .update(
            id,
            &UpdateLgInvoice {
                amount_cents: Patch::Set(1),
                ..Default::default()
            },
        )
        .await;
    assert!(result.is_err(), "the disagreement must stop the write");
    assert_eq!(
        repo.ledger_verify(id)
            .await
            .expect("verify")
            .broken
            .expect("still reported")
            .kind,
        LedgerBreak::HighWaterMismatch
    );
}

/// A wholly erased chain whose mark survived is *not* refused: the append
/// allocates above the mark, so the chain starts past sequence 1 and
/// `ledger_verify` reports the erasure forever. Refusing would brick the record
/// without buying evidence that is not already permanent.
#[tokio::test]
async fn a_write_after_a_wholly_erased_chain_leaves_permanent_evidence() {
    let pool = boot_pool("lg_erased_then_write").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("erase the whole chain");
    }

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(1),
            ..Default::default()
        },
    )
    .await
    .expect("ordinary traffic continues");

    let revisions = repo.ledger_revisions(id).await.expect("revisions");
    assert_eq!(
        revisions.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![4],
        "the append allocates above the surviving mark, not from 1"
    );
    let broken = repo
        .ledger_verify(id)
        .await
        .expect("verify")
        .broken
        .expect("the erasure must still be reported after the write");
    assert_eq!(broken.kind, LedgerBreak::MissingRevision);
    assert_eq!(broken.seq, 1);
}

/// A mark *behind* the head is the mixed-version rolling-deploy case — a
/// pre-#2323 node appends without raising the mark — so it must NOT refuse. It
/// is reported, and the record's next write heals it.
#[tokio::test]
async fn a_mark_behind_the_head_is_reported_but_still_writable() {
    let pool = boot_pool("lg_mark_behind").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "UPDATE _autumn_ledger_high_water SET high_seq = 2, \
             head_hash = (SELECT hash FROM _autumn_ledger_revisions \
                          WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2), \
             recorded_at = (SELECT recorded_at FROM _autumn_ledger_revisions \
                            WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 2) \
             WHERE table_name = 'lg_invoices' AND record_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("leave the mark one behind, as an old writer would");
    }

    assert_eq!(
        repo.ledger_verify(id)
            .await
            .expect("verify")
            .broken
            .expect("reported")
            .kind,
        LedgerBreak::HighWaterBehind
    );

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(1),
            ..Default::default()
        },
    )
    .await
    .expect("a mark behind the head must not block writes");

    assert!(
        repo.ledger_verify(id).await.expect("verify").is_intact(),
        "the record's next write re-establishes the mark above both"
    );
}

/// The residual this change does **not** close, pinned so nobody mistakes the
/// guarantee for a stronger one: an attacker who can DELETE from the revisions
/// table *and* UPDATE the mark to agree with what survives leaves a state that
/// is internally consistent, and ordinary traffic then continues from it.
/// Only a head pinned outside the database catches that.
#[tokio::test]
async fn a_truncation_covered_up_in_both_tables_is_only_caught_by_a_pinned_head() {
    let pool = boot_pool("lg_residual").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());
    let id = write_three_revisions(&repo).await;
    let pinned = repo
        .ledger_head(id)
        .await
        .expect("head")
        .expect("head")
        .hash;

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_invoices' AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(id)
        .execute(&mut *conn)
        .await
        .expect("delete the newest revision");
    }
    resync_high_water_mark(&pool, "lg_invoices", id).await;

    repo.update(
        id,
        &UpdateLgInvoice {
            amount_cents: Patch::Set(1),
            ..Default::default()
        },
    )
    .await
    .expect("ordinary traffic continues");

    assert!(
        repo.ledger_verify(id).await.expect("verify").is_intact(),
        "documented residual: consistent tampering across both tables is not \
         visible from inside the database"
    );
    let now_head = repo.ledger_head(id).await.expect("head").expect("head");
    assert_ne!(
        now_head.hash, pinned,
        "a head pinned outside the database is what still disagrees"
    );
}

// ── adoption: the migration backfills existing chains (#2323) ────────

/// The mark table arrives *after* the revisions table in a real deployment, so
/// its migration has to leave every chain that already exists marked. Without
/// the backfill, `HighWaterMissing` would fire on every record on upgrade day —
/// and the write path would refuse every subsequent write.
#[tokio::test]
async fn the_migration_backfills_a_mark_for_every_chain_that_already_exists() {
    let pool = boot_pool("lg_backfill").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool.clone());

    // Two records with chains of different lengths, written before the mark
    // table exists at all.
    let mut ids = Vec::new();
    for n in 0..2 {
        let created = repo
            .save(&NewLgInvoice {
                reference: format!("PRE-{n}"),
                amount_cents: 1,
                amount_rate: 1.0,
                metadata: "{}".to_string(),
            })
            .await
            .expect("insert");
        for step in 0..=n {
            repo.update(
                created.id,
                &UpdateLgInvoice {
                    amount_cents: Patch::Set(i64::from(step) + 2),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        }
        ids.push(created.id);
    }

    // Drop every mark, leaving exactly what a pre-#2323 ledger looks like: real
    // chains, no marks. Then re-apply the shipped migration — its DDL is
    // `IF NOT EXISTS` and its backfill is `ON CONFLICT DO NOTHING`, so what runs
    // here is the backfill statement itself, over real chains rather than the
    // empty table every other test's boot sees.
    {
        let mut conn = pool.get().await.expect("conn");
        conn.batch_execute("DELETE FROM _autumn_ledger_high_water")
            .await
            .expect("un-mark every chain");
        conn.batch_execute(LEDGER_HIGH_WATER_UP)
            .await
            .expect("apply the high-water migration over an existing ledger");
    }

    for (n, id) in ids.iter().enumerate() {
        let mark = repo
            .ledger_high_water(*id)
            .await
            .expect("mark")
            .expect("the backfill must have marked this chain");
        let head = repo.ledger_head(*id).await.expect("head").expect("head");
        assert_eq!(mark.seq, head.seq, "record {id} ({n})");
        assert_eq!(mark.hash, head.hash);
        assert_eq!(mark.recorded_at, head.recorded_at);
        assert!(
            repo.ledger_verify(*id).await.expect("verify").is_intact(),
            "a backfilled chain must not be accused"
        );
    }

    // And ordinary writes continue from there, without a gap.
    repo.update(
        ids[0],
        &UpdateLgInvoice {
            amount_cents: Patch::Set(99),
            ..Default::default()
        },
    )
    .await
    .expect("update after the backfill");
    assert!(
        repo.ledger_verify(ids[0])
            .await
            .expect("verify")
            .is_intact()
    );
}

// ── tenant isolation of the mark (#2323) ─────────────────────────────

/// Two tenants' rows may share a `record_id`, which is exactly why the mark is
/// keyed on `tenant_key` as well. Prove their marks stay separate — and that a
/// tenant-scoped `ledger_verify` reads its own.
#[tokio::test]
async fn high_water_marks_are_per_tenant() {
    let pool = boot_pool("lg_tenant_mark").await;
    let repo = PgLgTenantInvoiceRepository::with_pool_untracked(pool.clone());

    let a_id = with_tenant("tenant-a".to_string(), async {
        let created = repo
            .save(&NewLgTenantInvoice {
                reference: "A-1".to_string(),
            })
            .await
            .expect("insert as tenant-a");
        for step in 0..2 {
            repo.update(
                created.id,
                &UpdateLgTenantInvoice {
                    reference: Patch::Set(format!("A-1-{step}")),
                    ..Default::default()
                },
            )
            .await
            .expect("update as tenant-a");
        }
        created.id
    })
    .await;

    let b_id = with_tenant("tenant-b".to_string(), async {
        repo.save(&NewLgTenantInvoice {
            reference: "B-1".to_string(),
        })
        .await
        .expect("insert as tenant-b")
        .id
    })
    .await;

    with_tenant("tenant-a".to_string(), async {
        let mark = repo
            .ledger_high_water(a_id)
            .await
            .expect("mark")
            .expect("tenant-a has a mark");
        assert_eq!(mark.seq, 3, "tenant-a's own chain length, not tenant-b's");
        assert!(repo.ledger_verify(a_id).await.expect("verify").is_intact());
    })
    .await;

    with_tenant("tenant-b".to_string(), async {
        let mark = repo
            .ledger_high_water(b_id)
            .await
            .expect("mark")
            .expect("tenant-b has a mark");
        assert_eq!(mark.seq, 1);
        assert!(repo.ledger_verify(b_id).await.expect("verify").is_intact());

        // Tenant b's read of tenant a's record id must not reach tenant a's
        // mark, even where the ids collide.
        if a_id == b_id {
            assert_eq!(
                repo.ledger_high_water(a_id)
                    .await
                    .expect("mark")
                    .unwrap()
                    .seq,
                1
            );
        }
    })
    .await;

    // And a tenant-scoped truncation is reported inside its own scope.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_tenant_invoices' AND tenant_id = 'tenant-a' \
             AND record_id = ? AND seq = 3",
        )
        .bind::<diesel::sql_types::BigInt, _>(a_id)
        .execute(&mut *conn)
        .await
        .expect("truncate tenant-a's chain");
    }
    with_tenant("tenant-a".to_string(), async {
        let broken = repo
            .ledger_verify(a_id)
            .await
            .expect("verify")
            .broken
            .expect("tenant-a's truncation must be reported");
        assert_eq!(broken.kind, LedgerBreak::MissingRevision);
        assert_eq!(broken.seq, 3);
    })
    .await;
    with_tenant("tenant-b".to_string(), async {
        assert!(
            repo.ledger_verify(b_id).await.expect("verify").is_intact(),
            "tenant-b must not inherit tenant-a's accusation"
        );
    })
    .await;
}

/// Bulk writes are where a per-row chain read is most likely to go wrong.
#[tokio::test]
async fn bulk_writes_chain_every_row_independently() {
    let pool = boot_pool("lg_bulk").await;
    let repo = PgLgInvoiceRepository::with_pool_untracked(pool);

    let created = repo
        .save_many(&[
            NewLgInvoice {
                reference: "BULK-1".to_string(),
                amount_cents: 1,
                amount_rate: 1e16,
                metadata: "{}".to_string(),
            },
            NewLgInvoice {
                reference: "BULK-2".to_string(),
                amount_cents: 2,
                amount_rate: 2.5,
                metadata: "{}".to_string(),
            },
        ])
        .await
        .expect("save_many");
    assert_eq!(created.len(), 2);
    let ids: Vec<i64> = created.iter().map(|r| r.id).collect();

    for id in &ids {
        let revisions = repo.ledger_revisions(*id).await.expect("revisions");
        assert_eq!(
            revisions.len(),
            1,
            "each bulk-inserted row opens its own chain"
        );
        assert_eq!(revisions[0].seq, 1);
        assert_eq!(revisions[0].prev_hash, None);
        assert!(repo.ledger_verify(*id).await.expect("verify").is_intact());
    }

    repo.delete_many(&ids).await.expect("delete_many");
    for id in &ids {
        let revisions = repo.ledger_revisions(*id).await.expect("revisions");
        assert_eq!(
            revisions.iter().map(|r| r.op).collect::<Vec<_>>(),
            vec![VersionOp::Insert, VersionOp::Delete],
            "a bulk soft-delete appends one revision per row"
        );
        assert_eq!(revisions[1].seq, 2);
        assert_eq!(
            revisions[1].prev_hash.as_deref(),
            Some(revisions[0].hash.as_str())
        );
        let report = repo.ledger_verify(*id).await.expect("verify");
        assert!(report.is_intact(), "record {id}: {report:?}");
    }
}

// ── encrypted columns stay visible to the cross-check ────────────────

/// The encrypted-column twin of the `#[private]` case: the newest revision
/// changes only an `#[encrypted]` column, then that revision is deleted. If the
/// cross-check compared raw ciphertext it would have to omit the column (a fresh
/// nonce per write makes it incomparable) and the truncation would read as
/// intact. Comparing the plaintext underneath keeps it visible.
#[tokio::test]
async fn a_truncated_tail_is_detected_when_only_an_encrypted_column_changed() {
    let pool = boot_pool("lg_vault_tail").await;
    let repo = PgLgVaultNoteRepository::with_pool_untracked(pool.clone());

    let created = repo
        .save(&NewLgVaultNote {
            body: "public".to_string(),
            secret: "before".to_string(),
        })
        .await
        .expect("insert");
    repo.update(
        created.id,
        &UpdateLgVaultNote {
            secret: Patch::Set("after".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("update an encrypted column only");

    // The stored snapshots hold ciphertext, and two encodings of the *same*
    // plaintext would differ — which is why the comparison decrypts.
    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(revisions.len(), 2);
    let stored_secret = revisions[1].snapshot["secret"]
        .as_str()
        .expect("the snapshot carries the encrypted column");
    assert_ne!(
        stored_secret, "after",
        "the ledger must not store the plaintext of an encrypted column"
    );

    assert!(
        repo.ledger_verify(created.id)
            .await
            .expect("verify")
            .is_intact(),
        "no false positive on an intact chain with an encrypted column"
    );

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_vault_notes' AND record_id = ? AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(created.id)
        .execute(&mut *conn)
        .await
        .expect("lop off the revision that changed only the encrypted column");
    }
    // Cover the #2323 mark up too, so the truncation reaches the live-row
    // cross-check this test exists to exercise rather than stopping at the mark.
    resync_high_water_mark(&pool, "lg_vault_notes", created.id).await;

    let broken = repo
        .ledger_verify(created.id)
        .await
        .expect("verify")
        .broken
        .expect("an encrypted-column-only truncation must still be detected");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
    assert_eq!(broken.seq, 1);
}

/// And the reverse: an untouched chain over an encrypted column must never
/// report tampering, however many times it is verified — the ciphertext differs
/// on every encoding, so only a decrypting comparison can stay quiet.
#[tokio::test]
async fn an_encrypted_column_does_not_cause_a_false_positive() {
    let pool = boot_pool("lg_vault_intact").await;
    let repo = PgLgVaultNoteRepository::with_pool_untracked(pool);

    let created = repo
        .save(&NewLgVaultNote {
            body: "public".to_string(),
            secret: "s1".to_string(),
        })
        .await
        .expect("insert");
    for secret in ["s2", "s3"] {
        repo.update(
            created.id,
            &UpdateLgVaultNote {
                secret: Patch::Set(secret.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }
    repo.delete_by_id(created.id).await.expect("soft delete");
    repo.restore(created.id).await.expect("restore");

    for _ in 0..3 {
        let report = repo.ledger_verify(created.id).await.expect("verify");
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.revisions_checked, 5);
    }

    // As-of reconstruction returns the decrypted value, not ciphertext.
    let reconstructed = repo
        .ledger_as_of_at(created.id, LedgerAsOf::default())
        .await
        .expect("as-of")
        .expect("state");
    assert_eq!(reconstructed.secret, "s3");
}

// ── a hard-deleting parent cannot erase a ledgered child ─────────────

/// Neither outcome is available, so the cascade is refused with a typed error
/// rather than erasing the ledger or dying on a foreign-key violation.
#[tokio::test]
async fn a_hard_parent_delete_is_refused_rather_than_erasing_a_ledgered_child() {
    let pool = boot_pool("lg_cascade").await;
    let parents = PgLgCascadeParentRepository::with_pool_untracked(pool.clone());
    let children = PgLgCascadeChildRepository::with_pool_untracked(pool);

    let parent = parents
        .save(&NewLgCascadeParent {
            name: "p".to_string(),
        })
        .await
        .expect("insert parent");
    let child = children
        .save(&NewLgCascadeChild {
            parent_id: parent.id,
            label: "c".to_string(),
        })
        .await
        .expect("insert ledgered child");

    let err = parents
        .delete_by_id(parent.id)
        .await
        .expect_err("a hard parent delete must not erase a ledgered child");
    let typed = err
        .downcast_chain_ref::<autumn_web::ledger::LedgerError>()
        .expect("the refusal is a typed LedgerError, not a foreign-key violation");
    assert!(
        matches!(
            typed,
            autumn_web::ledger::LedgerError::HardDeleteCascade { record_id, .. }
                if *record_id == child.id
        ),
        "{typed:?}"
    );
    // The message has to name the fix, since the app author cannot see the
    // conflict from either repository's declaration alone.
    let rendered = typed.to_string();
    assert!(rendered.contains("soft_delete"), "{rendered}");

    // Nothing was erased, and the child's chain is untouched.
    let live = children
        .find_by_id(child.id)
        .await
        .expect("read child")
        .expect("the child survives a refused cascade");
    assert!(live.deleted_at.is_none());
    let report = children.ledger_verify(child.id).await.expect("verify");
    assert!(report.is_intact(), "{report:?}");
    assert_eq!(report.revisions_checked, 1);
}

// ── hidden columns are covered by the live-state cross-check ─────────

/// The exact case Codex flagged: the newest revision changes *only* a column the
/// model hides from its public JSON, and an attacker deletes that revision. A
/// serde-shaped cross-check would find the preceding revision and the live row
/// identical and call the truncated chain intact.
#[tokio::test]
async fn a_truncated_tail_is_detected_when_only_a_hidden_column_changed() {
    let pool = boot_pool("lg_hidden_tail").await;
    let repo = PgLgSecretNoteRepository::with_pool_untracked(pool.clone());

    let created = repo
        .save(&NewLgSecretNote {
            body: "public".to_string(),
            internal_note: "before".to_string(),
        })
        .await
        .expect("insert");
    // The public projection of this update is identical to the insert's.
    repo.update(
        created.id,
        &UpdateLgSecretNote {
            internal_note: Patch::Set("after".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("update a hidden column only");

    // Sanity: the two snapshots differ only in the hidden column.
    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(revisions.len(), 2);
    assert_eq!(
        revisions[0].snapshot["body"], revisions[1].snapshot["body"],
        "the public column is unchanged across the two revisions"
    );
    assert_ne!(
        revisions[0].snapshot["internal_note"], revisions[1].snapshot["internal_note"],
        "the hidden column is what changed — and the snapshot carries it"
    );

    assert!(
        repo.ledger_verify(created.id)
            .await
            .expect("verify")
            .is_intact(),
        "no false positive on an intact chain with a hidden column"
    );

    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "DELETE FROM _autumn_ledger_revisions \
             WHERE table_name = 'lg_secret_notes' AND record_id = ? AND seq = 2",
        )
        .bind::<diesel::sql_types::BigInt, _>(created.id)
        .execute(&mut *conn)
        .await
        .expect("lop off the revision that changed only the hidden column");
    }
    // Cover the #2323 mark up too, so the truncation reaches the live-row
    // cross-check this test exists to exercise rather than stopping at the mark.
    resync_high_water_mark(&pool, "lg_secret_notes", created.id).await;

    let broken = repo
        .ledger_verify(created.id)
        .await
        .expect("verify")
        .broken
        .expect("a hidden-column-only truncation must still be detected");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
    assert_eq!(broken.seq, 1);
}

/// The mirror-image bug: comparing a codec-shaped snapshot against a
/// serde-shaped live row would make every model with a hidden column report
/// tampering on history nobody touched.
#[tokio::test]
async fn a_hidden_column_does_not_cause_a_false_positive() {
    let pool = boot_pool("lg_hidden_intact").await;
    let repo = PgLgSecretNoteRepository::with_pool_untracked(pool);

    let created = repo
        .save(&NewLgSecretNote {
            body: "public".to_string(),
            internal_note: "secret".to_string(),
        })
        .await
        .expect("insert");
    for note in ["secret-2", "secret-3"] {
        repo.update(
            created.id,
            &UpdateLgSecretNote {
                internal_note: Patch::Set(note.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    }
    repo.delete_by_id(created.id).await.expect("soft delete");
    repo.restore(created.id).await.expect("restore");

    for _ in 0..3 {
        let report = repo.ledger_verify(created.id).await.expect("verify");
        assert!(report.is_intact(), "{report:?}");
        assert_eq!(report.revisions_checked, 5);
    }

    // And as-of still reconstructs the hidden column exactly.
    let reconstructed = repo
        .ledger_as_of_at(created.id, LedgerAsOf::default())
        .await
        .expect("as-of")
        .expect("state");
    assert_eq!(reconstructed.internal_note, "secret-3");
}

// ── valid time read from a model column ──────────────────────────────

#[tokio::test]
async fn a_declared_valid_time_column_separates_the_two_axes() {
    let pool = boot_pool("lg_valid_time").await;
    let repo = PgLgEffectiveNoteRepository::with_pool_untracked(pool);

    let effective = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .expect("date")
        .and_hms_opt(0, 0, 0)
        .expect("time");
    let created = repo
        .save(&NewLgEffectiveNote {
            body: "v1".to_string(),
            effective_at: effective,
        })
        .await
        .expect("insert");

    let revisions = repo.ledger_revisions(created.id).await.expect("revisions");
    assert_eq!(revisions.len(), 1);
    assert_eq!(
        revisions[0].valid_from,
        effective.and_utc(),
        "valid time comes from the declared column, not the clock"
    );
    assert!(
        revisions[0].recorded_at > revisions[0].valid_from,
        "transaction time is now; valid time is back in January"
    );

    // A back-dated correction: recorded second, valid from *before* the insert.
    let earlier = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
        .expect("date")
        .and_hms_opt(0, 0, 0)
        .expect("time");
    repo.update(
        created.id,
        &UpdateLgEffectiveNote {
            body: Patch::Set("v2".to_string()),
            effective_at: Patch::Set(earlier),
            ..Default::default()
        },
    )
    .await
    .expect("correction");

    // Transaction-time (and unbounded) reads follow the chain: the correction
    // is the live state, even though its valid_from is earlier.
    let live = repo
        .find_by_id(created.id)
        .await
        .expect("live read")
        .expect("live row");
    let latest = repo
        .ledger_as_of(created.id, now() + Duration::seconds(1))
        .await
        .expect("as-of")
        .expect("state");
    assert_eq!(latest, live, "as-of(now) must equal the live row");
    assert_eq!(latest.body, "v2");

    // Valid-time reads walk the valid-time timeline instead.
    let mid_2025 = chrono::NaiveDate::from_ymd_opt(2025, 8, 1)
        .expect("date")
        .and_hms_opt(0, 0, 0)
        .expect("time")
        .and_utc();
    let then = repo
        .ledger_as_of_at(created.id, LedgerAsOf::valid(mid_2025))
        .await
        .expect("as-of")
        .expect("state");
    assert_eq!(
        then.body, "v2",
        "the back-dated correction governs August 2025"
    );

    let mid_2026 = chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
        .expect("date")
        .and_hms_opt(0, 0, 0)
        .expect("time")
        .and_utc();
    let later = repo
        .ledger_as_of_at(created.id, LedgerAsOf::valid(mid_2026))
        .await
        .expect("as-of")
        .expect("state");
    assert_eq!(
        later.body, "v2",
        "a correction supersedes what it corrects from its own effective_at onward"
    );

    assert!(
        repo.ledger_verify(created.id)
            .await
            .expect("verify")
            .is_intact()
    );
}

async fn write_three_revisions(repo: &PgLgInvoiceRepository) -> i64 {
    let created = repo
        .save(&NewLgInvoice {
            reference: "INV-X".to_string(),
            amount_cents: 1,
            amount_rate: 1e16,
            metadata: r#"{"b":1,"a":{"y":2,"x":3}}"#.to_string(),
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
