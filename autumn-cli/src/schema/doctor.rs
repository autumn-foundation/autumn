//! `autumn schema doctor` — read-only diagnosis of the declarative-schema state
//! (slice 6 of tracking issue #1975).
//!
//! Doctor answers "is my declarative-schema setup healthy?" without mutating
//! anything: it reports a table of named checks — filesystem, snapshot, model
//! drift, backend provider-lock, and pending migrations — each with an
//! `OK`/`WARN`/`ERROR` status and a one-line detail. It exits non-zero when any
//! check is `ERROR` (an actionable misconfiguration); a `WARN` alone (e.g.
//! uncommitted model changes, or an unreachable database) never fails the
//! command, so doctor stays runnable offline.
//!
//! # Testability
//!
//! The check computation is a pure function ([`compute_checks`]) over gathered
//! facts ([`LocalFacts`] + a [`PendingState`]); all filesystem/DB/env I/O lives
//! in the thin shell ([`run_doctor`] / [`gather_local_facts`]). Unit tests
//! exercise the pure layer directly with no database.
//!
//! # Backend note
//!
//! The pending-migrations check is Postgres-specific (it consumes the pg
//! `pending_migrations` status API). A non-Postgres backend is reported as a
//! skipped WARN rather than failing. The database-schema-drift check introspects
//! the live database: on a default (Postgres-only) build a `SQLite` backend is
//! skipped, while a `--features sqlite` build additionally references
//! `introspect_sqlite` so it can diff a live
//! `SQLite` database too.

use std::path::Path;

use autumn_schema_core::{Backend, ColumnType, Table};
use diesel_migrations::FileBasedMigrations;
use serde::Serialize;

use super::diff::{DiffOptions, MigrationPlan, SchemaChange, diff_schema};
use super::introspect::introspect_postgres;
#[cfg(feature = "sqlite")]
use super::introspect::introspect_sqlite;
use super::parse::{ParsedSchema, parse_models_path};
use super::schema_rs::{SCHEMA_RS_PATH, SchemaRsCheck, check_tables};
use super::snapshot::{SNAPSHOT_DEFAULT_PATH, SnapshotError, load_snapshot};

/// A single diagnostic check outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Stable check identifier (e.g. `project-root`, `snapshot-drift`).
    pub name: String,
    /// The severity of the outcome.
    pub status: Status,
    /// A one-line, human-readable detail (remediation when actionable).
    pub detail: String,
}

/// Check severity. Serializes as the upper-case token used in the report and the
/// `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    /// The check passed.
    Ok,
    /// A heads-up that does not fail the command (drift, unreachable DB, skips).
    Warn,
    /// An actionable problem — the command exits non-zero.
    Error,
}

impl Status {
    /// The bracketed label used in the plain-text report.
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "[OK]   ",
            Self::Warn => "[WARN] ",
            Self::Error => "[ERROR]",
        }
    }
}

/// Snapshot load outcome plus, when loaded, its dialect tag and the model-drift
/// result computed against it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotState {
    /// Loaded and parsed; carries its backend tag and the drift verdict.
    Loaded { backend: Backend, drift: DriftState },
    /// No snapshot file at the default path.
    Missing,
    /// The file exists but could not be read/parsed (IO, JSON, or bad version).
    Unreadable(String),
}

/// Model-vs-snapshot drift verdict (only meaningful when the snapshot loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DriftState {
    /// The declared models match the snapshot baseline.
    Clean,
    /// The models diverge from the baseline by this many changes.
    Changed(usize),
    /// The declared models could not be parsed to compute drift.
    ModelsError(String),
}

/// Pending-migrations probe outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingState {
    /// No database URL is configured — the check is skipped.
    NotConfigured,
    /// The backend/build cannot run this check (e.g. non-Postgres backend).
    Skipped(String),
    /// The database was reached; carries the pending migration names.
    Reachable(Vec<String>),
    /// The database could not be reached (secret-free reason).
    Unreachable(String),
}

/// Database-vs-snapshot drift probe outcome (the live database introspected and
/// diffed against the checked-in snapshot baseline). Offline-safe: an
/// unreachable database is a distinct, non-failing state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DbSchemaState {
    /// No database URL is configured — the check is skipped.
    NotConfigured,
    /// The backend/build cannot run this check (a `SQLite` backend on a default
    /// Postgres-only build). Constructed only off the `sqlite` lane — on a `sqlite`
    /// build both backends introspect, so this state is unreachable there (the
    /// variant stays part of the type's vocabulary and is still matched in the check
    /// renderer).
    #[cfg_attr(feature = "sqlite", allow(dead_code))]
    Skipped(String),
    /// No readable snapshot baseline to diff the database against.
    SnapshotMissing,
    /// The database could not be reached/introspected (secret-free reason).
    Unreachable(String),
    /// The live database matches the snapshot baseline.
    Clean,
    /// The database diverges from the snapshot baseline by this many changes.
    Drifted(usize),
}

/// `src/schema.rs` against the managed models (offline).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SchemaRsState {
    /// The project has no `src/schema.rs`, and a model is managed.
    NoFile,
    /// `src/schema.rs` exists but cannot be read.
    Unreadable(String),
    /// No model is managed.
    NoManaged,
    /// The result of the comparison.
    Checked(SchemaRsCheck),
    /// The models could not be parsed.
    ModelsError(String),
}

/// Unmanaged models against the live database.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnmanagedState {
    /// No database URL is configured.
    NotConfigured,
    /// The build cannot read this backend. A `sqlite` build does not use it.
    #[cfg_attr(feature = "sqlite", allow(dead_code))]
    Skipped(String),
    /// No model is unmanaged.
    NoUnmanaged,
    /// The models could not be parsed.
    ModelsError(String),
    /// The database could not be read (secret-free reason).
    Unreachable(String),
    /// The result of the comparison.
    Checked(UnmanagedDrift),
}

/// The result of [`compute_unmanaged_drift`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct UnmanagedDrift {
    /// Tables that differ from their models: `(table, difference count)`.
    drifted: Vec<(String, usize)>,
    /// Tables whose model has a field that the parser skipped. The check
    /// cannot compare that column.
    unchecked: Vec<String>,
}

/// Filesystem/snapshot/backend facts gathered without touching the database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFacts {
    cargo_toml_present: bool,
    autumn_dir_present: bool,
    snapshot: SnapshotState,
    detected_backend: Backend,
    /// Backend implied by the resolved DB URL scheme (`None` if no URL / unknown).
    url_backend: Option<Backend>,
    /// `src/schema.rs` against the managed models.
    schema_rs: SchemaRsState,
}

/// Run the doctor: gather facts, compute checks, print the report (table or
/// JSON), and return `Err(summary)` when any check is `ERROR` so the dispatch
/// tail exits non-zero.
///
/// # Errors
///
/// Returns a one-line summary when one or more checks are `ERROR`.
pub fn run_doctor(profile: Option<&str>, json: bool) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;
    let local = gather_local_facts(&project_root, profile);
    let pending = gather_pending(&project_root, profile, &local);
    let (db_schema, unmanaged) = gather_db_facts(&project_root, profile, &local);
    let checks = compute_checks(&local, &pending, &db_schema, &unmanaged);
    render(&checks, json)?;
    finish(&checks)
}

/// Gather the non-DB facts (filesystem, snapshot, drift, backends).
fn gather_local_facts(project_root: &Path, profile: Option<&str>) -> LocalFacts {
    let cargo_toml_present = project_root.join("Cargo.toml").is_file();
    let autumn_dir_present = project_root.join(".autumn").is_dir();

    // Resolve the URL once for the requested `--profile`; both the URL-implied
    // backend (dialect-vs-DB check) and the project/selected backend (provider-
    // lock + pending-path selection) derive from this SAME profile-resolved
    // context, so `schema doctor --profile <name>` diagnoses the profile it
    // claims to (rather than mixing the requested profile's URL with the ambient
    // profile's detected backend).
    let url = crate::migrate::resolve_primary_url(profile);
    let url_backend = url
        .as_deref()
        .and_then(autumn_web::config::DatabaseBackend::detect)
        .map(super::map_detected_backend);
    let detected_backend = super::backend_for_url(project_root, profile, url.as_deref());
    let snapshot = probe_snapshot(project_root, detected_backend);

    let schema_rs = probe_schema_rs(project_root, detected_backend);

    LocalFacts {
        cargo_toml_present,
        autumn_dir_present,
        snapshot,
        detected_backend,
        url_backend,
        schema_rs,
    }
}

/// Compare `src/schema.rs` with the managed models.
fn probe_schema_rs(project_root: &Path, backend: Backend) -> SchemaRsState {
    let Some(models_path) = super::existing_models_path(project_root) else {
        return SchemaRsState::NoManaged;
    };
    let desired = match parse_models_path(&models_path, backend) {
        Ok(desired) => desired,
        Err(e) => return SchemaRsState::ModelsError(e.to_string()),
    };
    if !desired.tables.iter().any(|t| t.managed) {
        return SchemaRsState::NoManaged;
    }
    match std::fs::read_to_string(project_root.join(SCHEMA_RS_PATH)) {
        Ok(existing) => SchemaRsState::Checked(check_tables(&existing, &desired)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SchemaRsState::NoFile,
        Err(e) => SchemaRsState::Unreadable(e.to_string()),
    }
}

/// The unmanaged models whose table in `db` is missing or has other column
/// shapes: a model column the table does not have, another primary key,
/// another type, or another `NULL` rule. Columns that only the table has, indexes, defaults and
/// constraints are not compared.
fn compute_unmanaged_drift(models: &ParsedSchema, db: &[Table]) -> UnmanagedDrift {
    let mut unchecked: Vec<String> = models
        .tables
        .iter()
        .filter(|t| !t.managed && models.diagnostics.iter().any(|d| d.table == t.name))
        .map(|t| t.name.clone())
        .collect();
    unchecked.dedup();
    let drifted = models
        .tables
        .iter()
        .filter(|t| !t.managed)
        .filter_map(|model| {
            // Mark both sides managed so that the engine compares them. Diff
            // one table at a time so that no other table shows as a drop.
            let mut want = model.clone();
            want.managed = true;
            // The model does not read a column that the parser adds.
            want.columns.retain(|c| {
                !models
                    .implicit_columns
                    .iter()
                    .any(|(t, col)| *t == model.name && *col == c.name)
            });
            let base: Vec<Table> = db
                .iter()
                .filter(|t| t.name == model.name)
                .map(|t| Table {
                    managed: true,
                    ..t.clone()
                })
                .collect();
            let plan = diff_schema(
                &base,
                &ParsedSchema::from_tables(vec![want]),
                DiffOptions::default(),
            );
            // A missing table counts once. An `Opaque` database type
            // (`VARCHAR(40)`, SQLite `BOOLEAN`) cannot be compared.
            let missing = plan.changes.iter().any(|c| {
                matches!(
                    c,
                    SchemaChange::CreateTable(_)
                        | SchemaChange::CreateTableBlockedBySkippedField { .. }
                )
            });
            let count = if missing {
                1
            } else {
                plan.changes
                    .iter()
                    .filter(|c| match c {
                        SchemaChange::AlterColumnType { from, .. } => {
                            !matches!(from, ColumnType::Opaque { .. })
                        }
                        SchemaChange::AddColumn { .. }
                        | SchemaChange::PrimaryKeyChange { .. }
                        | SchemaChange::SetNotNull { .. }
                        | SchemaChange::DropNotNull { .. } => true,
                        _ => false,
                    })
                    .count()
            };
            (count > 0).then(|| (model.name.clone(), count))
        })
        .collect();
    UnmanagedDrift { drifted, unchecked }
}

/// Load the snapshot and, if it loads, compute model drift against it using the
/// snapshot's own dialect tag.
fn probe_snapshot(project_root: &Path, _detected: Backend) -> SnapshotState {
    let snapshot_path = project_root.join(SNAPSHOT_DEFAULT_PATH);
    match load_snapshot(&snapshot_path) {
        Ok(snapshot) => {
            let drift = compute_drift(project_root, snapshot.backend, &snapshot.tables);
            SnapshotState::Loaded {
                backend: snapshot.backend,
                drift,
            }
        }
        Err(SnapshotError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            SnapshotState::Missing
        }
        Err(other) => SnapshotState::Unreadable(other.to_string()),
    }
}

/// Diff the declared models (parsed with the snapshot's backend) against the
/// baseline tables to decide the drift verdict.
fn compute_drift(project_root: &Path, backend: Backend, baseline: &[Table]) -> DriftState {
    let Some(models_path) = super::existing_models_path(project_root) else {
        return DriftState::ModelsError(
            "no declarative models found under src/ to compare against the snapshot".to_string(),
        );
    };
    match parse_models_path(&models_path, backend) {
        Ok(desired) => {
            let plan = diff_schema(baseline, &desired, DiffOptions::default());
            if plan.is_empty() {
                DriftState::Clean
            } else {
                DriftState::Changed(plan.changes.len())
            }
        }
        Err(e) => DriftState::ModelsError(e.to_string()),
    }
}

/// Probe pending migrations for the detected backend. Postgres consults the live
/// `pending_migrations` status API; any other backend (and a build without the
/// database URL) is skipped with a note. Never references a `SQLite` symbol.
fn gather_pending(project_root: &Path, profile: Option<&str>, local: &LocalFacts) -> PendingState {
    let Some(url) = crate::migrate::resolve_primary_url(profile) else {
        return PendingState::NotConfigured;
    };
    if local.detected_backend != Backend::Postgres {
        return PendingState::Skipped(format!(
            "pending-migration check is Postgres-only; detected {:?} backend",
            local.detected_backend
        ));
    }
    probe_pending(&url, &project_root.join("migrations"))
}

/// Probe pending migrations against `url` using the project's `migrations_dir`.
///
/// A present, readable dir is the migration source. When it is missing/unreadable
/// we must NOT short-circuit to "up to date" — that would let a missing migrations
/// directory mask an offline or misconfigured database (a false OK). Instead we
/// probe connectivity with a freshly-created, guaranteed-empty temp directory:
/// with zero available migrations `pending_migrations` still connects, returning
/// an empty set on a reachable DB (correctly "up to date") and an error on an
/// unreachable one (surfaced as [`PendingState::Unreachable`], i.e. a WARN).
fn probe_pending(url: &str, migrations_dir: &Path) -> PendingState {
    // Deferred-init tempdir guard: bound in this scope so, when used, it outlives
    // the `pending_migrations` call below.
    let fallback_dir;
    let migrations = if let Ok(source) = FileBasedMigrations::from_path(migrations_dir) {
        source
    } else {
        fallback_dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(e) => {
                return PendingState::Unreachable(format!(
                    "could not create a temp directory to probe the database: {e}"
                ));
            }
        };
        match FileBasedMigrations::from_path(fallback_dir.path()) {
            Ok(source) => source,
            Err(e) => {
                return PendingState::Unreachable(format!(
                    "could not build an empty migration source to probe the database: {e}"
                ));
            }
        }
    };
    match autumn_web::migrate::pending_migrations(url, migrations) {
        Ok(names) => PendingState::Reachable(names),
        // MigrationError Display is secret-free.
        Err(e) => PendingState::Unreachable(e.to_string()),
    }
}

/// Probe the live database once, for the `database-schema-drift` and
/// `unmanaged-drift` rows.
///
/// Offline-safe by construction (mirrors [`gather_pending`]): no URL →
/// `NotConfigured`; a `SQLite` backend on a default (Postgres-only) build →
/// `Skipped` (the `SQLite` introspector is compiled only under the `sqlite`
/// feature); a connect/query failure → `Unreachable` (a WARN, never an error —
/// doctor must stay runnable offline).
fn gather_db_facts(
    project_root: &Path,
    profile: Option<&str>,
    local: &LocalFacts,
) -> (DbSchemaState, UnmanagedState) {
    let Some(url) = crate::migrate::resolve_primary_url(profile) else {
        return (DbSchemaState::NotConfigured, UnmanagedState::NotConfigured);
    };
    let backend = local.detected_backend;
    match backend {
        Backend::Postgres => gather_db_facts_with(project_root, &url, backend, introspect_postgres),
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => gather_db_facts_with(project_root, &url, backend, introspect_sqlite),
        // On a default (Postgres-only) build the SQLite arm is not compiled, so a
        // SQLite backend is skipped rather than failing doctor — introspection is
        // Postgres-only on this build.
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => {
            let reason = format!(
                "database checks are Postgres-only on this build; detected {backend:?} backend \
                 (rebuild with `--features sqlite` to check SQLite drift)"
            );
            (
                DbSchemaState::Skipped(reason.clone()),
                UnmanagedState::Skipped(reason),
            )
        }
    }
}

/// [`gather_db_facts`] with the backend-specific `introspect` as a parameter.
///
/// The snapshot is the baseline of `database-schema-drift`: without one that
/// row is `SnapshotMissing`. The unmanaged models (parsed for `backend`) are
/// the input of `unmanaged-drift`. With neither, the database is not read.
fn gather_db_facts_with(
    project_root: &Path,
    url: &str,
    backend: Backend,
    introspect: impl Fn(&str) -> Result<Vec<Table>, super::introspect::IntrospectError>,
) -> (DbSchemaState, UnmanagedState) {
    let snapshot = load_snapshot(&project_root.join(SNAPSHOT_DEFAULT_PATH)).ok();
    let (models, models_error) = match super::existing_models_path(project_root)
        .map(|path| parse_models_path(&path, backend))
    {
        Some(Ok(models)) if models.tables.iter().any(|t| !t.managed) => (Some(models), None),
        Some(Err(e)) => (None, Some(UnmanagedState::ModelsError(e.to_string()))),
        _ => (None, None),
    };
    if snapshot.is_none() && models.is_none() {
        return (
            DbSchemaState::SnapshotMissing,
            models_error.unwrap_or(UnmanagedState::NoUnmanaged),
        );
    }
    let read = introspect(url).map_err(|e| e.to_string()); // credential-safe
    let schema = match (&snapshot, &read) {
        (None, _) => DbSchemaState::SnapshotMissing,
        (Some(_), Err(e)) => DbSchemaState::Unreachable(e.clone()),
        (Some(snapshot), Ok(tables)) => {
            let drift = compute_db_schema_drift(&snapshot.tables, tables);
            if drift.is_clean() {
                DbSchemaState::Clean
            } else {
                DbSchemaState::Drifted(drift.reported_count())
            }
        }
    };
    let unmanaged = match (models, read) {
        (None, _) => models_error.unwrap_or(UnmanagedState::NoUnmanaged),
        (Some(_), Err(e)) => UnmanagedState::Unreachable(e),
        (Some(models), Ok(tables)) => {
            UnmanagedState::Checked(compute_unmanaged_drift(&models, &tables))
        }
    };
    (schema, unmanaged)
}

/// Bidirectional database-vs-snapshot drift, computed purely from two table sets
/// (no I/O) so both `schema doctor` and `schema pull --dry-run` share one engine.
///
/// The **forward** diff (`snapshot` baseline → introspected `db` desired) is what a
/// `schema pull` would change in the snapshot: it catches column/index/table
/// adds+drops and type changes. But because [`diff_column`](super::diff) /
/// `diff_checks` treat a desired-side `None` as "unknown, retained" (never a
/// `DropDefault` / `DropForeignKey` / `DropCheck`), the forward pass MISSES a
/// manually-dropped default, foreign key, or CHECK in the live DB. Running the diff
/// in the **reverse** direction (`db` baseline → `snapshot` desired) surfaces
/// exactly those dropped-facet cases, so drift is the union of the two passes.
pub struct DbSchemaDrift {
    /// What a `schema pull` would change in the snapshot baseline.
    pub forward: MigrationPlan,
    /// The reverse diff — catches the dropped default/FK/CHECK cases the forward
    /// pass misses.
    pub reverse: MigrationPlan,
}

impl DbSchemaDrift {
    /// True when neither direction reports any change.
    pub const fn is_clean(&self) -> bool {
        self.forward.is_empty() && self.reverse.is_empty()
    }

    /// The advisory change count to report. Prefers the forward plan's length when
    /// it is non-empty (the common single-direction case), else the reverse plan's
    /// — avoiding the worst double-counting without over-engineering dedup.
    pub const fn reported_count(&self) -> usize {
        if self.forward.is_empty() {
            self.reverse.changes.len()
        } else {
            self.forward.changes.len()
        }
    }
}

/// Compute [`DbSchemaDrift`] between the checked-in `snapshot_tables` baseline and
/// the live `db_tables` introspected from the database. Pure and unit-testable —
/// runs [`diff_schema`] in both directions (see [`DbSchemaDrift`]).
pub fn compute_db_schema_drift(snapshot_tables: &[Table], db_tables: &[Table]) -> DbSchemaDrift {
    // Both sides are complete introspections, so expression/partial-index
    // definitions ARE authoritative here — a dropped/changed one is real drift and
    // must still be reported (unlike a model diff, which retains what the DSL
    // cannot express). This flag governs BOTH directions and also serves
    // `pull --dry-run`, which computes drift through this function.
    let opts = DiffOptions {
        definitions_authoritative: true,
        ..DiffOptions::default()
    };
    // Forward: what pulling the DB would change in the snapshot baseline.
    let forward = diff_schema(
        snapshot_tables,
        &ParsedSchema::from_tables(db_tables.to_vec()),
        opts,
    );
    // Reverse: catches the dropped-default/FK/CHECK cases the forward pass misses.
    let reverse = diff_schema(
        db_tables,
        &ParsedSchema::from_tables(snapshot_tables.to_vec()),
        opts,
    );
    DbSchemaDrift { forward, reverse }
}

/// Pure check computation over gathered facts. Ordered filesystem → snapshot →
/// drift → backend (provider-lock, then dialect-vs-DB) → database
/// (pending-migrations, then database-schema drift).
fn compute_checks(
    local: &LocalFacts,
    pending: &PendingState,
    db_schema: &DbSchemaState,
    unmanaged: &UnmanagedState,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. project-root
    checks.push(if !local.cargo_toml_present {
        Check {
            name: "project-root".to_string(),
            status: Status::Error,
            detail: "not in an Autumn project (no Cargo.toml) — run from your project root"
                .to_string(),
        }
    } else if !local.autumn_dir_present {
        Check {
            name: "project-root".to_string(),
            status: Status::Error,
            detail: "missing .autumn/ state directory — run `autumn schema snapshot` first"
                .to_string(),
        }
    } else {
        Check {
            name: "project-root".to_string(),
            status: Status::Ok,
            detail: "Cargo.toml and .autumn/ present".to_string(),
        }
    });

    // 2. snapshot-present
    checks.push(match &local.snapshot {
        SnapshotState::Loaded { backend, .. } => Check {
            name: "snapshot-present".to_string(),
            status: Status::Ok,
            detail: format!("snapshot loaded (backend {backend:?})"),
        },
        SnapshotState::Missing => Check {
            name: "snapshot-present".to_string(),
            status: Status::Error,
            detail: format!(
                "no snapshot at {SNAPSHOT_DEFAULT_PATH} — run `autumn schema snapshot`"
            ),
        },
        SnapshotState::Unreadable(reason) => Check {
            name: "snapshot-present".to_string(),
            status: Status::Error,
            detail: format!("snapshot is unreadable: {reason}"),
        },
    });

    // 3. snapshot-drift, then schema-rs-drift
    checks.push(drift_check(&local.snapshot));
    checks.push(schema_rs_check(&local.schema_rs));

    // 4. provider-lock (snapshot backend vs detected backend)
    checks.push(provider_lock_check(&local.snapshot, local.detected_backend));

    // 5. snapshot-dialect vs DB (snapshot backend vs resolved-URL backend)
    checks.push(dialect_vs_db_check(&local.snapshot, local.url_backend));

    // 6. pending-migrations
    checks.push(pending_check(pending));

    // 7. database-schema-drift (live DB introspected vs the snapshot baseline)
    checks.push(db_schema_drift_check(db_schema));

    // 8. unmanaged-drift (unmanaged models vs the live DB)
    checks.push(unmanaged_check(unmanaged));

    checks
}

/// The `snapshot-drift` row.
fn drift_check(snapshot: &SnapshotState) -> Check {
    let name = "snapshot-drift".to_string();
    match snapshot {
        SnapshotState::Loaded {
            drift: DriftState::Clean,
            ..
        } => Check {
            name,
            status: Status::Ok,
            detail: "models match the snapshot baseline".to_string(),
        },
        SnapshotState::Loaded {
            drift: DriftState::Changed(n),
            ..
        } => Check {
            name,
            status: Status::Warn,
            detail: format!(
                "{n} pending model change(s) — run `autumn schema diff --write-migration`"
            ),
        },
        SnapshotState::Loaded {
            drift: DriftState::ModelsError(reason),
            ..
        } => Check {
            name,
            status: Status::Warn,
            detail: format!("could not evaluate drift: {reason}"),
        },
        SnapshotState::Missing | SnapshotState::Unreadable(_) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot baseline".to_string(),
        },
    }
}

/// The `schema-rs-drift` row.
fn schema_rs_check(state: &SchemaRsState) -> Check {
    let (status, detail) = match state {
        SchemaRsState::Checked(check) => {
            let mut parts = Vec::new();
            if !check.stale.is_empty() {
                parts.push(format!(
                    "missing or stale block for {}",
                    check.stale.join(", ")
                ));
            }
            if check.stale_macros {
                parts.push("a stale `joinable!` or table list".to_owned());
            }
            if !parts.is_empty() {
                parts.push("run `autumn schema diff --write-migration`".to_owned());
            }
            if !check.unchecked.is_empty() {
                let unchecked: Vec<String> = check
                    .unchecked
                    .iter()
                    .map(|(table, reason)| format!("{table} ({reason})"))
                    .collect();
                parts.push(format!("not checked: {}", unchecked.join(", ")));
            }
            if parts.is_empty() {
                (Status::Ok, "the managed blocks match".to_owned())
            } else {
                (Status::Warn, parts.join("; "))
            }
        }
        SchemaRsState::NoManaged => (
            Status::Ok,
            "no managed model — nothing to compare".to_owned(),
        ),
        SchemaRsState::NoFile => (Status::Warn, format!("no {SCHEMA_RS_PATH} — skipped")),
        SchemaRsState::Unreadable(reason) => (
            Status::Warn,
            format!("could not read {SCHEMA_RS_PATH}: {reason}"),
        ),
        SchemaRsState::ModelsError(reason) => {
            (Status::Warn, format!("could not read the models: {reason}"))
        }
    };
    Check {
        name: "schema-rs-drift".to_owned(),
        status,
        detail,
    }
}

/// The `unmanaged-drift` row.
fn unmanaged_check(state: &UnmanagedState) -> Check {
    let (status, detail) = match state {
        UnmanagedState::Checked(drift) => {
            let note = if drift.unchecked.is_empty() {
                String::new()
            } else {
                format!(
                    "; not checked: {} (a field the parser cannot read)",
                    drift.unchecked.join(", ")
                )
            };
            if drift.drifted.is_empty() && drift.unchecked.is_empty() {
                (
                    Status::Ok,
                    "the unmanaged models match their tables".to_owned(),
                )
            } else if drift.drifted.is_empty() {
                (
                    Status::Warn,
                    format!("the other unmanaged models match their tables{note}"),
                )
            } else {
                let list: Vec<String> = drift
                    .drifted
                    .iter()
                    .map(|(t, n)| format!("{t} ({n})"))
                    .collect();
                (
                    Status::Warn,
                    format!(
                        "unmanaged model(s) differ from the database: {} — write a migration \
                         with `autumn generate migration`, or change the model{note}",
                        list.join(", ")
                    ),
                )
            }
        }
        UnmanagedState::NoUnmanaged => (Status::Ok, "no unmanaged model".to_owned()),
        UnmanagedState::NotConfigured => {
            (Status::Warn, "no database configured — skipped".to_owned())
        }
        UnmanagedState::Unreachable(reason) => (
            Status::Warn,
            format!("could not reach the database: {reason}"),
        ),
        UnmanagedState::Skipped(reason) => (Status::Warn, reason.clone()),
        UnmanagedState::ModelsError(reason) => {
            (Status::Warn, format!("could not read the models: {reason}"))
        }
    };
    Check {
        name: "unmanaged-drift".to_owned(),
        status,
        detail,
    }
}

/// The `provider-lock` row (snapshot dialect vs the detected backend).
fn provider_lock_check(snapshot: &SnapshotState, detected: Backend) -> Check {
    let name = "provider-lock".to_string();
    match snapshot {
        SnapshotState::Loaded { backend, .. } if *backend == detected => Check {
            name,
            status: Status::Ok,
            detail: format!("snapshot and detected backend agree ({detected:?})"),
        },
        SnapshotState::Loaded { backend, .. } => Check {
            name,
            status: Status::Error,
            detail: format!(
                "snapshot backend {backend:?} does not match the detected backend {detected:?}"
            ),
        },
        SnapshotState::Missing | SnapshotState::Unreadable(_) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot".to_string(),
        },
    }
}

/// The `snapshot-dialect-vs-db` row (snapshot dialect vs the resolved DB URL's
/// backend). Distinct from provider-lock: the configured backend and the URL
/// scheme can diverge.
fn dialect_vs_db_check(snapshot: &SnapshotState, url_backend: Option<Backend>) -> Check {
    let name = "snapshot-dialect-vs-db".to_string();
    match (snapshot, url_backend) {
        (SnapshotState::Missing | SnapshotState::Unreadable(_), _) => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot".to_string(),
        },
        (SnapshotState::Loaded { .. }, None) => Check {
            name,
            status: Status::Warn,
            detail: "no database URL configured — skipped".to_string(),
        },
        (SnapshotState::Loaded { backend, .. }, Some(url_backend)) if *backend == url_backend => {
            Check {
                name,
                status: Status::Ok,
                detail: format!("snapshot dialect matches the database URL ({url_backend:?})"),
            }
        }
        (SnapshotState::Loaded { backend, .. }, Some(url_backend)) => Check {
            name,
            status: Status::Error,
            detail: format!(
                "snapshot backend {backend:?} does not match the database URL backend {url_backend:?}"
            ),
        },
    }
}

/// The `pending-migrations` row.
fn pending_check(pending: &PendingState) -> Check {
    let name = "pending-migrations".to_string();
    match pending {
        PendingState::NotConfigured => Check {
            name,
            status: Status::Warn,
            detail: "no database configured — skipped".to_string(),
        },
        PendingState::Skipped(reason) => Check {
            name,
            status: Status::Warn,
            detail: reason.clone(),
        },
        PendingState::Unreachable(reason) => Check {
            name,
            status: Status::Warn,
            detail: format!("could not reach database: {reason}"),
        },
        PendingState::Reachable(names) if names.is_empty() => Check {
            name,
            status: Status::Ok,
            detail: "database is up to date".to_string(),
        },
        PendingState::Reachable(names) => Check {
            name,
            status: Status::Warn,
            detail: format!(
                "{} pending migration(s): {} — run `autumn schema migrate`",
                names.len(),
                names.join(", ")
            ),
        },
    }
}

/// The `database-schema-drift` row (live database introspected vs the snapshot
/// baseline). Never an `Error`: an unreachable/misconfigured/non-Postgres
/// database is a WARN so doctor stays runnable offline (mirroring
/// `pending-migrations` / `snapshot-dialect-vs-db`).
fn db_schema_drift_check(state: &DbSchemaState) -> Check {
    let name = "database-schema-drift".to_string();
    match state {
        DbSchemaState::Clean => Check {
            name,
            status: Status::Ok,
            detail: "database schema matches the snapshot baseline".to_string(),
        },
        DbSchemaState::Drifted(n) => Check {
            name,
            status: Status::Warn,
            detail: format!(
                "database schema differs from the snapshot baseline ({n} difference(s)); \
                 run `autumn schema pull` to update the baseline or generate a migration"
            ),
        },
        DbSchemaState::Unreachable(reason) => Check {
            name,
            status: Status::Warn,
            detail: format!("could not reach the database: {reason}"),
        },
        DbSchemaState::NotConfigured => Check {
            name,
            status: Status::Warn,
            detail: "no database configured — skipped".to_string(),
        },
        DbSchemaState::Skipped(reason) => Check {
            name,
            status: Status::Warn,
            detail: reason.clone(),
        },
        DbSchemaState::SnapshotMissing => Check {
            name,
            status: Status::Warn,
            detail: "skipped — no readable snapshot baseline".to_string(),
        },
    }
}

/// Print the report as an aligned table, or as JSON when `json` is set.
fn render(checks: &[Check], json: bool) -> Result<(), String> {
    if json {
        let out = serde_json::to_string_pretty(checks)
            .map_err(|e| format!("failed to serialize doctor report: {e}"))?;
        println!("{out}");
        return Ok(());
    }
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    println!("Schema doctor:");
    for check in checks {
        println!(
            "  {} {:<width$}  {}",
            check.status.label(),
            check.name,
            check.detail,
            width = width
        );
    }
    Ok(())
}

/// Turn an `ERROR`-containing report into a non-zero exit via `Err`, else `Ok`.
fn finish(checks: &[Check]) -> Result<(), String> {
    let errors: Vec<&str> = checks
        .iter()
        .filter(|c| c.status == Status::Error)
        .map(|c| c.name.as_str())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "schema doctor found {} error check(s): {}",
            errors.len(),
            errors.join(", ")
        ))
    }
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    const POST_MODEL: &str = r#"
        #[autumn_web::model(managed)]
        pub struct Post {
            #[id]
            pub id: i64,
            pub title: String,
        }
    "#;

    /// A version-1 snapshot for a `posts(id, title, created_at)` table, matching
    /// `POST_MODEL` (the parser adds the implicit `created_at`).
    fn posts_snapshot(backend: &str) -> String {
        format!(
            r#"{{
  "snapshot_version": 1,
  "backend": "{backend}",
  "tables": [
    {{
      "name": "posts",
      "columns": [
        {{ "name": "id", "ty": "Int64", "nullable": false, "primary_key": true, "unique": false, "default": null, "references": null, "serial": "BigSerial" }},
        {{ "name": "title", "ty": "Text", "nullable": false, "primary_key": false, "unique": false, "default": null, "references": null }},
        {{ "name": "created_at", "ty": "Timestamp", "nullable": false, "primary_key": false, "unique": false, "default": "Now", "references": null }}
      ],
      "primary_key": ["id"],
      "indexes": [],
      "checks": [],
      "backend": "{backend}",
      "managed": true
    }}
  ]
}}
"#
        )
    }

    /// Write a project scaffold: `Cargo.toml`, `src/models.rs`, and a snapshot at
    /// the default path. Returns the tempdir (kept alive by the caller).
    fn scaffold(models: &str, snapshot_json: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n")
            .expect("Cargo.toml");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(src.join("models.rs"), models).expect("models.rs");
        let autumn = root.path().join(".autumn");
        std::fs::create_dir_all(&autumn).expect("mkdir .autumn");
        if let Some(json) = snapshot_json {
            std::fs::write(autumn.join("schema-snapshot.json"), json).expect("snapshot");
        }
        root
    }

    #[test]
    fn missing_project_root_is_error() {
        let root = tempfile::tempdir().expect("tempdir"); // no Cargo.toml
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let pr = checks.iter().find(|c| c.name == "project-root").unwrap();
        assert_eq!(pr.status, Status::Error, "{pr:?}");
        // finish() turns it into a non-zero exit.
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn matching_models_report_drift_ok() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Ok, "{drift:?}");
        // No ERROR checks (detected Postgres matches the Postgres snapshot).
        assert!(finish(&checks).is_ok(), "checks: {checks:?}");
    }

    #[test]
    fn diverging_models_report_drift_warn() {
        // Model adds a `body` column the snapshot lacks → drift WARN.
        let models = r#"
            #[autumn_web::model(managed)]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub body: Option<String>,
            }
        "#;
        let root = scaffold(models, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Warn, "{drift:?}");
        assert!(drift.detail.contains("pending model change"), "{drift:?}");
        // A WARN alone does not fail the command.
        assert!(finish(&checks).is_ok());
    }

    /// Finding 3 (#2036): the SELECTED profile's backend must flow into the
    /// checks. When `gather_local_facts` resolves the requested profile's backend
    /// to Sqlite (from its database URL), a Sqlite-tagged snapshot must NOT trip a
    /// false `provider-lock` ERROR — even though the ambient project default is
    /// Postgres (see the contrasting `snapshot_backend_mismatch_is_provider_lock_error`).
    /// Synthesizes the post-resolution facts directly so the pure check layer is
    /// exercised without a database.
    #[test]
    fn selected_profile_backend_flows_into_provider_lock() {
        let local = LocalFacts {
            cargo_toml_present: true,
            autumn_dir_present: true,
            snapshot: SnapshotState::Loaded {
                backend: Backend::Sqlite,
                drift: DriftState::Clean,
            },
            detected_backend: Backend::Sqlite,
            url_backend: Some(Backend::Sqlite),
            schema_rs: SchemaRsState::NoFile,
        };
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let lock = checks.iter().find(|c| c.name == "provider-lock").unwrap();
        assert_eq!(lock.status, Status::Ok, "{lock:?}");
        // No ERROR overall: the selected Sqlite backend agrees with the snapshot.
        assert!(finish(&checks).is_ok(), "checks: {checks:?}");
    }

    #[test]
    fn snapshot_backend_mismatch_is_provider_lock_error() {
        // Sqlite-tagged snapshot but the tempdir project detects Postgres.
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Sqlite")));
        let local = gather_local_facts(root.path(), None);
        assert_eq!(local.detected_backend, Backend::Postgres);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let lock = checks.iter().find(|c| c.name == "provider-lock").unwrap();
        assert_eq!(lock.status, Status::Error, "{lock:?}");
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn missing_snapshot_is_snapshot_present_error() {
        let root = scaffold(POST_MODEL, None); // .autumn present, no snapshot file
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let snap = checks
            .iter()
            .find(|c| c.name == "snapshot-present")
            .unwrap();
        assert_eq!(snap.status, Status::Error, "{snap:?}");
        // Drift/provider-lock degrade to WARN skips when there's no snapshot.
        let drift = checks.iter().find(|c| c.name == "snapshot-drift").unwrap();
        assert_eq!(drift.status, Status::Warn);
        assert!(finish(&checks).is_err());
    }

    #[test]
    fn unreadable_snapshot_is_snapshot_present_error() {
        let root = scaffold(POST_MODEL, Some("{ not valid json"));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let snap = checks
            .iter()
            .find(|c| c.name == "snapshot-present")
            .unwrap();
        assert_eq!(snap.status, Status::Error, "{snap:?}");
    }

    /// Finding 1 (connectivity false-positive): a MISSING migrations dir must not
    /// short-circuit `probe_pending` to `Reachable(empty)` — with a configured URL
    /// the probe must still attempt a connection, so an offline/misconfigured
    /// database surfaces as `Unreachable` (a WARN) rather than a false "database is
    /// up to date". Uses a loopback port with nothing listening (immediate
    /// connection-refused — fast and deterministic, no network egress). Gated to
    /// the default Postgres lane since the pending probe is Postgres-only.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn missing_migrations_dir_still_probes_the_database() {
        let root = tempfile::tempdir().expect("tempdir"); // no migrations/ dir
        let state = probe_pending(
            "postgres://user:pw@127.0.0.1:1/db",
            &root.path().join("migrations"),
        );
        assert!(
            matches!(state, PendingState::Unreachable(_)),
            "missing dir + unreachable DB must be Unreachable, got {state:?}"
        );
    }

    #[test]
    fn pending_reachable_empty_is_ok_and_nonempty_is_warn() {
        let clean = pending_check(&PendingState::Reachable(Vec::new()));
        assert_eq!(clean.status, Status::Ok);
        let dirty = pending_check(&PendingState::Reachable(vec!["20260101_init".to_string()]));
        assert_eq!(dirty.status, Status::Warn);
        assert!(dirty.detail.contains("autumn schema migrate"));
        // Unreachable is a WARN, never a hard fail (doctor runs offline).
        let down = pending_check(&PendingState::Unreachable("connection refused".to_string()));
        assert_eq!(down.status, Status::Warn);
    }

    #[test]
    fn dialect_vs_db_mismatch_is_error() {
        let snapshot = SnapshotState::Loaded {
            backend: Backend::Postgres,
            drift: DriftState::Clean,
        };
        let check = dialect_vs_db_check(&snapshot, Some(Backend::Sqlite));
        assert_eq!(check.status, Status::Error, "{check:?}");
        // Matching URL backend is OK.
        let ok = dialect_vs_db_check(&snapshot, Some(Backend::Postgres));
        assert_eq!(ok.status, Status::Ok);
        // No URL → skipped WARN.
        let skip = dialect_vs_db_check(&snapshot, None);
        assert_eq!(skip.status, Status::Warn);
    }

    #[test]
    fn status_serializes_uppercase_in_json() {
        let checks = vec![Check {
            name: "x".to_string(),
            status: Status::Warn,
            detail: "y".to_string(),
        }];
        let json = serde_json::to_string(&checks).unwrap();
        assert!(json.contains("\"status\":\"WARN\""), "{json}");
        assert!(json.contains("\"name\":\"x\""));
    }

    #[test]
    fn report_ordering_is_filesystem_then_snapshot_then_db() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        let checks = compute_checks(
            &local,
            &PendingState::NotConfigured,
            &DbSchemaState::NotConfigured,
            &UnmanagedState::NotConfigured,
        );
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "project-root",
                "snapshot-present",
                "snapshot-drift",
                "schema-rs-drift",
                "provider-lock",
                "snapshot-dialect-vs-db",
                "pending-migrations",
                "database-schema-drift",
                "unmanaged-drift",
            ]
        );
    }

    #[test]
    fn db_schema_drift_states_map_to_expected_status() {
        // Clean → OK.
        assert_eq!(
            db_schema_drift_check(&DbSchemaState::Clean).status,
            Status::Ok
        );
        // Drifted → WARN, names the remediation.
        let drifted = db_schema_drift_check(&DbSchemaState::Drifted(3));
        assert_eq!(drifted.status, Status::Warn);
        assert!(drifted.detail.contains("3 difference(s)"), "{drifted:?}");
        assert!(drifted.detail.contains("autumn schema pull"), "{drifted:?}");
        // Unreachable → WARN (never an error; doctor stays offline-runnable).
        let down = db_schema_drift_check(&DbSchemaState::Unreachable("refused".to_string()));
        assert_eq!(down.status, Status::Warn);
        assert!(down.detail.contains("could not reach the database"));
        // Non-Postgres skip / not-configured / no-snapshot → WARN, never error.
        assert_eq!(
            db_schema_drift_check(&DbSchemaState::Skipped("sqlite".to_string())).status,
            Status::Warn
        );
        assert_eq!(
            db_schema_drift_check(&DbSchemaState::NotConfigured).status,
            Status::Warn
        );
        assert_eq!(
            db_schema_drift_check(&DbSchemaState::SnapshotMissing).status,
            Status::Warn
        );
    }

    #[test]
    fn db_schema_drift_never_fails_the_command() {
        // Even a drifted/unreachable database keeps the whole report non-failing
        // (a WARN alone never trips `finish`) — doctor must run clean offline.
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let local = gather_local_facts(root.path(), None);
        for state in [
            DbSchemaState::Drifted(5),
            DbSchemaState::Unreachable("connection refused".to_string()),
            DbSchemaState::Clean,
        ] {
            let checks = compute_checks(
                &local,
                &PendingState::NotConfigured,
                &state,
                &UnmanagedState::NotConfigured,
            );
            let db = checks
                .iter()
                .find(|c| c.name == "database-schema-drift")
                .unwrap();
            assert_ne!(db.status, Status::Error, "{db:?}");
            assert!(finish(&checks).is_ok(), "checks: {checks:?}");
        }
    }

    // --- Bidirectional drift (`compute_db_schema_drift`) --------------------
    //
    // The forward pass alone (`diff_column` treats a desired `None` as "unknown,
    // retained") silently MISSES a manually-dropped default / FK / CHECK in the
    // live DB. These pure tests pin that the union of the two directions catches
    // those cases, while the forward-caught cases (adds/type changes) still count.

    /// A Postgres `posts(id BIGINT PK, created_at TIMESTAMP DEFAULT now())`-shaped
    /// table, built with the schema-core constructors, so drift tests can vary a
    /// single facet (a default, an FK, an extra column) against a clone.
    fn drift_posts_table(created_at_default: Option<autumn_schema_core::ColumnDefault>) -> Table {
        use autumn_schema_core::{Column, ColumnType};
        let mut id = Column::new("id".to_string(), ColumnType::Int64);
        id.primary_key = true;
        let mut created_at = Column::new("created_at".to_string(), ColumnType::Timestamp);
        created_at.default = created_at_default;
        let mut table = Table::new("posts", Backend::Postgres);
        table.managed = true;
        table.primary_key = vec!["id".to_string()];
        table.columns = vec![id, created_at];
        table
    }

    #[test]
    fn compute_db_schema_drift_identical_is_clean() {
        let snapshot = vec![drift_posts_table(Some(
            autumn_schema_core::ColumnDefault::Now,
        ))];
        let db = snapshot.clone();
        let drift = compute_db_schema_drift(&snapshot, &db);
        assert!(drift.is_clean(), "identical table sets must be clean");
        assert_eq!(drift.reported_count(), 0);
    }

    #[test]
    fn compute_db_schema_drift_catches_dropped_default() {
        // Snapshot keeps `created_at DEFAULT now()`; the live DB dropped the
        // default (default: None). The FORWARD pass misses this (a desired `None`
        // is "retained"); the reverse pass catches it, so the union is drift.
        let snapshot = vec![drift_posts_table(Some(
            autumn_schema_core::ColumnDefault::Now,
        ))];
        let db = vec![drift_posts_table(None)];
        let drift = compute_db_schema_drift(&snapshot, &db);
        assert!(
            drift.forward.is_empty(),
            "forward pass alone must miss the dropped default: {:?}",
            drift.forward.changes
        );
        assert!(
            !drift.reverse.is_empty(),
            "reverse pass must catch the dropped default"
        );
        assert!(!drift.is_clean(), "dropped default is drift");
        assert!(drift.reported_count() >= 1);
    }

    #[test]
    fn compute_db_schema_drift_catches_dropped_foreign_key() {
        use autumn_schema_core::{Column, ColumnType, ForeignKey};
        // Both tables share the same columns; the snapshot's `author_id` carries an
        // FK the live DB dropped. Forward misses it (desired `None` retained);
        // reverse catches it.
        let build = |with_fk: bool| {
            let mut id = Column::new("id".to_string(), ColumnType::Int64);
            id.primary_key = true;
            let mut author_id = Column::new("author_id".to_string(), ColumnType::Int64);
            if with_fk {
                author_id.references = Some(ForeignKey::new("authors", "id"));
            }
            let mut table = Table::new("posts", Backend::Postgres);
            table.managed = true;
            table.primary_key = vec!["id".to_string()];
            table.columns = vec![id, author_id];
            table
        };
        let snapshot = vec![build(true)];
        let db = vec![build(false)];
        let drift = compute_db_schema_drift(&snapshot, &db);
        assert!(
            drift.forward.is_empty(),
            "forward pass alone must miss the dropped FK: {:?}",
            drift.forward.changes
        );
        assert!(
            !drift.reverse.is_empty(),
            "reverse pass must catch the dropped FK"
        );
        assert!(!drift.is_clean(), "dropped FK is drift");
    }

    #[test]
    fn compute_db_schema_drift_catches_added_column() {
        use autumn_schema_core::{Column, ColumnType};
        // The live DB gained a column the snapshot lacks — the FORWARD pass catches
        // this as an AddColumn, so the reported count comes from the forward plan.
        let snapshot = vec![drift_posts_table(Some(
            autumn_schema_core::ColumnDefault::Now,
        ))];
        let mut db_table = drift_posts_table(Some(autumn_schema_core::ColumnDefault::Now));
        db_table
            .columns
            .push(Column::new("extra".to_string(), ColumnType::Text));
        let db = vec![db_table];
        let drift = compute_db_schema_drift(&snapshot, &db);
        assert!(
            !drift.forward.is_empty(),
            "forward pass must catch the added column"
        );
        assert!(!drift.is_clean(), "added column is drift");
        assert_eq!(
            drift.reported_count(),
            drift.forward.changes.len(),
            "reported count prefers the non-empty forward plan"
        );
    }

    /// A dropped expression/partial index IS real drift for the introspection
    /// diff: `compute_db_schema_drift` runs with `definitions_authoritative: true`,
    /// so a snapshot table carrying a `definition` index the live DB is missing
    /// reports Drifted (proving the authoritative, bidirectional path still catches
    /// a dropped expression index — the model-diff retention does NOT leak here).
    #[test]
    fn compute_db_schema_drift_catches_dropped_expression_index() {
        use autumn_schema_core::Index;
        let mut snapshot_table = drift_posts_table(None);
        snapshot_table.indexes.push(Index {
            name: "idx_posts_lower_created".to_owned(),
            columns: vec!["created_at".to_owned()],
            unique: false,
            definition: Some(
                "CREATE INDEX idx_posts_lower_created ON posts (lower(created_at::text))"
                    .to_owned(),
            ),
            is_partial: false,
            key_columns: Vec::new(),
        });
        let snapshot = vec![snapshot_table];
        // Live DB lacks the expression index entirely.
        let db = vec![drift_posts_table(None)];
        let drift = compute_db_schema_drift(&snapshot, &db);
        assert!(
            !drift.is_clean(),
            "a dropped expression index must be reported as drift by the authoritative path"
        );
        assert!(
            drift
                .forward
                .changes
                .iter()
                .any(|c| matches!(c, crate::schema::diff::SchemaChange::DropIndex { .. })),
            "forward introspection pass must emit a DropIndex for the missing expression index: {:?}",
            drift.forward.changes
        );
    }

    // --- `schema-rs-drift` (Decision 1) --------------------------------------

    const POSTS_BLOCK: &str = "diesel::table! {
    posts (id) {
        id -> Int8,
        title -> Text,
        created_at -> Timestamp,
    }
}
";

    fn schema_rs_row(root: &Path) -> Check {
        let local = gather_local_facts(root, None);
        schema_rs_check(&local.schema_rs)
    }

    #[test]
    fn schema_rs_drift_is_ok_when_each_managed_block_matches() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        std::fs::write(root.path().join("src/schema.rs"), POSTS_BLOCK).unwrap();
        let row = schema_rs_row(root.path());
        assert_eq!(row.name, "schema-rs-drift");
        assert_eq!(row.status, Status::Ok, "{row:?}");
    }

    #[test]
    fn schema_rs_drift_warns_on_a_stale_or_missing_block() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        std::fs::write(
            root.path().join("src/schema.rs"),
            "diesel::table! {\n    posts (id) {\n        id -> Int8,\n    }\n}\n",
        )
        .unwrap();
        let row = schema_rs_row(root.path());
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(row.detail.contains("posts"), "{row:?}");
        assert!(
            row.detail.contains("autumn schema diff --write-migration"),
            "{row:?}"
        );
    }

    #[test]
    fn schema_rs_drift_skips_without_a_file_and_passes_without_managed_models() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        let row = schema_rs_row(root.path());
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(row.detail.contains("no src/schema.rs"), "{row:?}");

        // Without a managed model, a missing file is not a problem.
        let unmanaged = POST_MODEL.replace("model(managed)", "model");
        let root = scaffold(&unmanaged, Some(&posts_snapshot("Postgres")));
        let row = schema_rs_row(root.path());
        assert_eq!(row.status, Status::Ok, "{row:?}");
        assert!(row.detail.contains("no managed model"), "{row:?}");
    }

    // --- `unmanaged-drift` (Decision 4) --------------------------------------

    fn unmanaged_posts() -> ParsedSchema {
        let unmanaged = POST_MODEL.replace("model(managed)", "model");
        crate::schema::parse::parse_model_source(&unmanaged, Backend::Postgres).expect("parse")
    }

    /// The `posts` table as the database has it, for `POST_MODEL`.
    fn db_posts() -> Table {
        let mut t = unmanaged_posts().tables.remove(0);
        t.managed = true;
        t
    }

    #[test]
    fn unmanaged_drift_is_empty_when_the_table_matches() {
        assert!(
            compute_unmanaged_drift(&unmanaged_posts(), &[db_posts()])
                .drifted
                .is_empty()
        );
    }

    #[test]
    fn unmanaged_drift_reports_a_missing_table_column_type_or_null_rule() {
        let models = unmanaged_posts();
        assert_eq!(
            compute_unmanaged_drift(&models, &[]).drifted,
            vec![("posts".to_owned(), 1)]
        );

        let mut no_title = db_posts();
        no_title.columns.retain(|c| c.name != "title");
        assert_eq!(
            compute_unmanaged_drift(&models, &[no_title]).drifted,
            vec![("posts".to_owned(), 1)]
        );

        let mut other = db_posts();
        let title = other
            .columns
            .iter_mut()
            .find(|c| c.name == "title")
            .unwrap();
        title.ty = autumn_schema_core::ColumnType::Int32;
        title.nullable = true;
        assert_eq!(
            compute_unmanaged_drift(&models, &[other]).drifted,
            vec![("posts".to_owned(), 2)]
        );
    }

    #[test]
    fn unmanaged_drift_ignores_table_only_columns_indexes_and_managed_models() {
        let models = unmanaged_posts();
        let mut wider = db_posts();
        wider.columns.push(autumn_schema_core::Column::new(
            "search_vector",
            autumn_schema_core::ColumnType::Text,
        ));
        wider.indexes.push(autumn_schema_core::Index::new(
            "idx_posts_title",
            vec!["title".to_owned()],
            false,
        ));
        assert!(
            compute_unmanaged_drift(&models, &[wider])
                .drifted
                .is_empty()
        );

        let managed =
            crate::schema::parse::parse_model_source(POST_MODEL, Backend::Postgres).expect("parse");
        assert!(compute_unmanaged_drift(&managed, &[]).drifted.is_empty());
    }

    #[test]
    fn unmanaged_drift_states_map_to_expected_status() {
        let row = unmanaged_check(&UnmanagedState::Checked(UnmanagedDrift::default()));
        assert_eq!(
            (row.name.as_str(), row.status),
            ("unmanaged-drift", Status::Ok)
        );
        let row = unmanaged_check(&UnmanagedState::Checked(UnmanagedDrift {
            drifted: vec![("posts".to_owned(), 2)],
            unchecked: Vec::new(),
        }));
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.contains("posts (2)"), "{row:?}");
        assert!(row.detail.contains("autumn generate migration"), "{row:?}");
        for state in [
            UnmanagedState::NotConfigured,
            UnmanagedState::Unreachable("refused".to_owned()),
            UnmanagedState::Skipped("sqlite".to_owned()),
        ] {
            assert_eq!(unmanaged_check(&state).status, Status::Warn, "{state:?}");
        }
        assert_eq!(
            unmanaged_check(&UnmanagedState::NoUnmanaged).status,
            Status::Ok
        );
    }

    /// One introspection feeds both database rows. A missing snapshot skips
    /// only `database-schema-drift`.
    #[test]
    fn one_introspection_feeds_both_database_rows() {
        let unmanaged = POST_MODEL.replace("model(managed)", "model");
        let root = scaffold(&unmanaged, None);
        let (schema, models) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                Ok(Vec::new())
            });
        assert_eq!(schema, DbSchemaState::SnapshotMissing);
        assert_eq!(
            models,
            UnmanagedState::Checked(UnmanagedDrift {
                drifted: vec![("posts".to_owned(), 1)],
                unchecked: Vec::new(),
            })
        );

        let root = scaffold(&unmanaged, Some(&posts_snapshot("Postgres")));
        let (schema, models) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                Ok(vec![db_posts()])
            });
        assert_eq!(schema, DbSchemaState::Clean);
        assert_eq!(models, UnmanagedState::Checked(UnmanagedDrift::default()));

        let (schema, models) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                Err(crate::schema::introspect::IntrospectError::Query(
                    "refused".to_owned(),
                ))
            });
        assert!(
            matches!(schema, DbSchemaState::Unreachable(_)),
            "{schema:?}"
        );
        assert!(
            matches!(models, UnmanagedState::Unreachable(_)),
            "{models:?}"
        );

        // No snapshot and no unmanaged model: no connection at all.
        let root = scaffold(POST_MODEL, None);
        let (schema, models) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                panic!("must not connect")
            });
        assert_eq!(schema, DbSchemaState::SnapshotMissing);
        assert_eq!(models, UnmanagedState::NoUnmanaged);
    }

    /// The parser adds `created_at` to a model that omits it. A hand-made
    /// table without that column is not drift: the model does not read it.
    #[test]
    fn unmanaged_drift_ignores_the_implicit_created_at() {
        let models = unmanaged_posts();
        let mut db = db_posts();
        db.columns.retain(|c| c.name != "created_at");
        assert!(compute_unmanaged_drift(&models, &[db]).drifted.is_empty());

        // A declared `created_at` still counts.
        let declared = crate::schema::parse::parse_model_source(
            "#[model] pub struct Post { #[id] pub id: i64, pub created_at: chrono::NaiveDateTime }",
            Backend::Postgres,
        )
        .expect("parse");
        let mut db = db_posts();
        db.columns.retain(|c| c.name != "created_at");
        assert_eq!(
            compute_unmanaged_drift(&declared, &[db]).drifted,
            vec![("posts".to_owned(), 1)]
        );
    }

    /// A table that the check cannot compare is named, as a WARN.
    #[test]
    fn schema_rs_drift_names_unchecked_tables() {
        let check = SchemaRsCheck {
            stale: Vec::new(),
            unchecked: vec![("posts".to_owned(), "enum field".to_owned())],
            stale_macros: false,
        };
        let row = schema_rs_check(&SchemaRsState::Checked(check));
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(
            row.detail.contains("not checked: posts (enum field)"),
            "{row:?}"
        );
    }

    /// A database type that the IR keeps as `Opaque` (`VARCHAR(40)`, `SQLite`
    /// `BOOLEAN`) is not drift: the engine cannot compare it.
    #[test]
    fn unmanaged_drift_ignores_an_opaque_database_type() {
        let mut db = db_posts();
        let title = db.columns.iter_mut().find(|c| c.name == "title").unwrap();
        title.ty = autumn_schema_core::ColumnType::Opaque {
            pg_type: "varchar(40)".to_owned(),
        };
        assert!(
            compute_unmanaged_drift(&unmanaged_posts(), &[db])
                .drifted
                .is_empty()
        );
    }

    #[test]
    fn database_rows_keep_their_reason_when_the_database_is_down() {
        // No snapshot: the snapshot row keeps its hint, even offline.
        let unmanaged = POST_MODEL.replace("model(managed)", "model");
        let root = scaffold(&unmanaged, None);
        let (schema, _) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                Err(crate::schema::introspect::IntrospectError::Query(
                    "refused".to_owned(),
                ))
            });
        assert_eq!(schema, DbSchemaState::SnapshotMissing);

        // Models that do not parse are a WARN, not an OK.
        let root = scaffold("pub struct {", Some(&posts_snapshot("Postgres")));
        let (_, models) =
            gather_db_facts_with(root.path(), "postgres://db", Backend::Postgres, |_| {
                Ok(vec![db_posts()])
            });
        assert!(
            matches!(models, UnmanagedState::ModelsError(_)),
            "{models:?}"
        );
        assert_eq!(unmanaged_check(&models).status, Status::Warn);
    }

    /// A model with a field that the parser skipped is named as not checked,
    /// not reported as clean.
    #[test]
    fn unmanaged_drift_names_a_model_with_a_skipped_field() {
        let models = crate::schema::parse::parse_model_source(
            "#[model] pub struct Post { #[id] pub id: i64, pub title: String, pub status: PostStatus }",
            Backend::Postgres,
        )
        .expect("parse");
        let drift = compute_unmanaged_drift(&models, &[db_posts()]);
        assert!(drift.drifted.is_empty(), "{drift:?}");
        assert_eq!(drift.unchecked, vec!["posts".to_owned()]);
        let row = unmanaged_check(&UnmanagedState::Checked(drift));
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(row.detail.contains("not checked: posts"), "{row:?}");
    }

    /// A `src/schema.rs` that exists but cannot be read is not "no file".
    #[test]
    fn schema_rs_drift_reports_an_unreadable_file() {
        let root = scaffold(POST_MODEL, Some(&posts_snapshot("Postgres")));
        std::fs::create_dir_all(root.path().join("src/schema.rs")).unwrap();
        let row = schema_rs_row(root.path());
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(
            row.detail.contains("could not read src/schema.rs"),
            "{row:?}"
        );
    }

    /// A stale `joinable!` alone is a WARN.
    #[test]
    fn schema_rs_drift_warns_on_stale_macros_alone() {
        let check = SchemaRsCheck {
            stale_macros: true,
            ..SchemaRsCheck::default()
        };
        let row = schema_rs_check(&SchemaRsState::Checked(check));
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(row.detail.contains("joinable!"), "{row:?}");
    }

    /// A different primary key is drift.
    #[test]
    fn unmanaged_drift_counts_a_primary_key_change() {
        let mut db = db_posts();
        db.primary_key = vec!["title".to_owned()];
        for c in &mut db.columns {
            c.primary_key = c.name == "title";
            c.serial = None;
        }
        assert_eq!(
            compute_unmanaged_drift(&unmanaged_posts(), &[db]).drifted,
            vec![("posts".to_owned(), 1)]
        );
    }
}
