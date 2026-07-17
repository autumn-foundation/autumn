//! `autumn schema migrate` — apply the generated migrations, then advance the
//! snapshot baseline (slice 6 of tracking issue #1975).
//!
//! This is the *apply* half of the declarative-schema loop. `autumn schema diff
//! --write-migration` (slice 4) authored `migrations/<ts>_<name>/{up,down}.sql`
//! after running the destructive-change guards; this command applies whatever is
//! pending against the configured database and then re-snapshots the declared
//! models so the checked-in baseline advances to the state the database is now
//! in.
//!
//! # What this command deliberately does NOT do
//!
//! It does not re-run the diff guards. The `-- autumn-safety:` advisories a
//! generated migration may carry are inert SQL comments; the destructive-change
//! refusal happened at diff time, not here. Migration files apply verbatim,
//! exactly as `autumn migrate` applies them — this command adds only the
//! provider-lock check and the post-apply snapshot refresh.
//!
//! # Backend handling (Postgres default; `SQLite` behind a feature)
//!
//! The CLI is a Postgres-first, single-backend build. The Postgres apply path is
//! always compiled. The `SQLite` apply path is gated behind the non-default
//! `sqlite` cargo feature (a backend-flip that must never be co-built with the
//! default Postgres backend — see `Cargo.toml`). In a default build a detected
//! `SQLite` backend yields a clear "rebuild with `--features sqlite`" error; no
//! `SQLite` symbol is referenced unless the feature is on.

use std::path::Path;

use autumn_schema_core::Backend;
use diesel_migrations::FileBasedMigrations;

use autumn_web::migrate::{DEFAULT_LOCK_WAIT_TIMEOUT, MigrationResult};

use super::parse::parse_models_path;
use super::snapshot::{
    SNAPSHOT_DEFAULT_PATH, SchemaSnapshot, SnapshotError, load_snapshot, write_snapshot,
};

/// Apply pending migrations against the configured database, then refresh the
/// checked-in schema snapshot.
///
/// Resolves the project root from the current directory; `profile` is the
/// explicit `--profile` flag (else the ambient profile resolution the rest of
/// the CLI uses). Returns a human-readable message on failure — never the
/// database URL or any credential.
///
/// # Errors
///
/// Returns a message when the command is run outside a project, the database URL
/// cannot be resolved, the snapshot's dialect does not match the detected
/// backend (provider-lock), or a migration fails to apply.
pub fn run_migrate(profile: Option<&str>) -> Result<(), String> {
    let project_root = std::env::current_dir()
        .map_err(|e| format!("failed to resolve the current directory: {e}"))?;
    migrate_at(&project_root, profile)
}

/// The body of [`run_migrate`] taking an explicit `project_root` so the wiring is
/// testable without mutating the process CWD (mirrors `diff_at` in the parent
/// module).
fn migrate_at(project_root: &Path, profile: Option<&str>) -> Result<(), String> {
    crate::generate::ensure_project_root(project_root)
        .map_err(|_| crate::generate::GenerateError::NotInProject.to_string())?;

    // Resolve the write/primary database URL exactly as `autumn migrate` does,
    // honoring an explicit `--profile`.
    let url = crate::migrate::resolve_primary_url(profile);

    // The migration backend is derived from the SAME profile-resolved context as
    // the URL: when a URL is configured its scheme is authoritative, so
    // `--profile <name>` selects the correct apply path / provider-lock even when
    // the project's ambient/default backend differs from the selected profile's
    // (using the ambient `detect_backend` here would pick the wrong apply impl).
    // With no URL configured we fall back to the profile-aware project default.
    // This backend also tags the refreshed snapshot below. With `profile == None`
    // the resolution is byte-identical to the previous ambient detection.
    let backend = super::backend_for_url(project_root, profile, url.as_deref());

    // PROVIDER-LOCK: if a snapshot exists, its dialect tag must match the
    // resolved backend before we apply anything cross-dialect. A missing
    // snapshot is not fatal here (the DB may be pre-snapshot), but we note it.
    let snapshot_path = project_root.join(SNAPSHOT_DEFAULT_PATH);
    if !enforce_provider_lock(&snapshot_path, backend)? {
        eprintln!(
            "note: no schema snapshot at {} — applying migrations without a provider-lock check; \
             run `autumn schema snapshot` to establish the baseline.",
            snapshot_path.display()
        );
    }

    // Require the URL now that the provider-lock has been checked.
    let url = url.ok_or_else(|| {
        "no database URL configured — set AUTUMN_DATABASE__URL or DATABASE_URL, or add a \
         [database] primary_url to autumn.toml"
            .to_string()
    })?;

    // No migrations directory (or an empty one) is a clean success, not an error.
    let migrations_dir = project_root.join("migrations");
    if migrations_dir_is_empty(&migrations_dir) {
        println!("No migrations to apply.");
        return Ok(());
    }

    // Apply. The destructive-change guards already ran at diff time; migration
    // files apply verbatim (`-- autumn-safety:` lines are inert SQL comments).
    let result = apply_pending(backend, &url, &migrations_dir)?;
    report_applied(&result);

    // SNAPSHOT REFRESH: the declarative snapshot is the diff baseline. It advances
    // ONLY when migrations were actually applied (`should_refresh_snapshot`) — a
    // real apply means the database moved to a new state, so the baseline follows
    // it to that state (re-parse the models, rewrite the snapshot atomically,
    // dialect-tagged). When nothing was applied (the DB was already up to date)
    // there is no new state to advance to, so the snapshot is left UNTOUCHED —
    // rewriting it from the (possibly newly-edited, still-ungenerated) models
    // would falsely bake those edits into the baseline and hide the drift from
    // the next `schema diff` / `doctor`. A snapshot-write failure AFTER a
    // successful apply is a warning, not an apply failure — the migration really
    // did apply.
    if should_refresh_snapshot(&result) {
        refresh_snapshot_after_apply(project_root, backend);
    }

    Ok(())
}

/// Whether a completed apply should advance the checked-in snapshot baseline.
///
/// The invariant: the snapshot tracks the APPLIED database state, so it advances
/// **iff** migrations were actually applied. An empty `applied` list means the DB
/// was already up to date — there is no new applied state to snapshot, so the
/// baseline must stay put (otherwise ungenerated model edits would be silently
/// baked into it and never surface as drift). Pure so the decision is testable.
const fn should_refresh_snapshot(result: &MigrationResult) -> bool {
    !result.applied.is_empty()
}

/// Enforce the provider-lock guard against an on-disk snapshot.
///
/// Returns `Ok(true)` when a snapshot was present and its backend matches,
/// `Ok(false)` when no snapshot file exists (caller decides how to note it), and
/// `Err` when the snapshot exists but is dialect-mismatched or unreadable.
fn enforce_provider_lock(snapshot_path: &Path, backend: Backend) -> Result<bool, String> {
    match load_snapshot(snapshot_path) {
        Ok(snapshot) => {
            snapshot
                .ensure_backend_matches(backend)
                .map_err(|e| e.to_string())?;
            Ok(true)
        }
        Err(SnapshotError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(false)
        }
        Err(other) => Err(other.to_string()),
    }
}

/// Apply pending migrations for `backend`. The Postgres path is always compiled;
/// the `SQLite` path is real only under the `sqlite` feature (see
/// [`apply_pending_sqlite`]).
fn apply_pending(
    backend: Backend,
    url: &str,
    migrations_dir: &Path,
) -> Result<MigrationResult, String> {
    match backend {
        Backend::Postgres => apply_pending_pg(url, migrations_dir),
        Backend::Sqlite => apply_pending_sqlite(url, migrations_dir),
    }
}

/// Postgres apply path: advisory-locked (`run_pending_locked`) so concurrent
/// migrators serialize cleanly.
fn apply_pending_pg(url: &str, migrations_dir: &Path) -> Result<MigrationResult, String> {
    let migrations = file_based_migrations(migrations_dir)?;
    autumn_web::migrate::run_pending_locked(url, migrations, Some(DEFAULT_LOCK_WAIT_TIMEOUT))
        .map_err(|e| e.to_string())
}

/// `SQLite` apply path (feature-gated). `SQLite` is a single-writer local database,
/// so — unlike Postgres — there is **no advisory lock** (issue #1999):
/// `run_pending_sqlite` applies directly.
#[cfg(feature = "sqlite")]
fn apply_pending_sqlite(url: &str, migrations_dir: &Path) -> Result<MigrationResult, String> {
    let migrations = file_based_migrations(migrations_dir)?;
    autumn_web::migrate::run_pending_sqlite(url, migrations).map_err(|e| e.to_string())
}

/// `SQLite` apply seam in the default (Postgres-only) build. The `sqlite`
/// backend-flip is a separate, non-default cargo feature that must never be
/// co-built with the default Postgres backend; without it a detected `SQLite`
/// backend cannot be applied here. This arm references **no** `SQLite` symbol.
#[cfg(not(feature = "sqlite"))]
fn apply_pending_sqlite(_url: &str, _migrations_dir: &Path) -> Result<MigrationResult, String> {
    Err(
        "`autumn schema migrate` detected a SQLite backend, but this CLI build targets Postgres \
         only. Rebuild with `--features sqlite` to apply SQLite migrations (the sqlite \
         backend-flip must never be co-built with the default Postgres backend)."
            .to_string(),
    )
}

/// Build a diesel [`FileBasedMigrations`] source rooted at `migrations_dir`,
/// mapping the read failure to a clear message. Left generic over the backend by
/// inference — the Postgres and `SQLite` callers each pin their own `MigrationSource`
/// bound.
fn file_based_migrations(migrations_dir: &Path) -> Result<FileBasedMigrations, String> {
    FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
        format!(
            "failed to read migrations directory {}: {e}",
            migrations_dir.display()
        )
    })
}

/// True when there is nothing to apply: the `migrations/` directory is absent or
/// contains no migration subdirectories.
fn migrations_dir_is_empty(migrations_dir: &Path) -> bool {
    std::fs::read_dir(migrations_dir).map_or(true, |entries| {
        !entries.filter_map(Result::ok).any(|e| e.path().is_dir())
    })
}

/// Print the applied-migration summary (mirrors `autumn migrate` reporting).
fn report_applied(result: &MigrationResult) {
    if result.applied.is_empty() {
        println!("Database already up to date.");
        return;
    }
    for name in &result.applied {
        println!("  applied {name}");
    }
    println!("Applied {} migration(s).", result.applied.len());
}

/// Re-parse the declared models and rewrite the snapshot at
/// [`SNAPSHOT_DEFAULT_PATH`], tagged with `backend`. Best-effort: after a
/// successful DB apply, a failure to refresh the snapshot is reported as a
/// warning (never surfaced as an apply failure), and a missing models source is
/// a benign note.
fn refresh_snapshot_after_apply(project_root: &Path, backend: Backend) {
    let Some(models_path) = super::existing_models_path(project_root) else {
        eprintln!(
            "note: no declarative models found under src/ — migrations applied, but the schema \
             snapshot was not refreshed (nothing to snapshot)."
        );
        return;
    };

    let desired = match parse_models_path(&models_path, backend) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!(
                "warning: migrations applied, but re-parsing models to refresh the snapshot \
                 failed: {e}"
            );
            return;
        }
    };

    let snapshot = SchemaSnapshot::new(backend, desired.tables);
    let out = project_root.join(SNAPSHOT_DEFAULT_PATH);
    match write_snapshot(&out, &snapshot) {
        Ok(()) => println!("refreshed schema snapshot at {}", out.display()),
        Err(e) => eprintln!(
            "warning: migrations applied, but refreshing the schema snapshot at {} failed: {e}",
            out.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    #[test]
    fn migrations_dir_is_empty_true_when_missing_or_no_subdirs() {
        let root = tempfile::tempdir().expect("tempdir");
        // Missing entirely.
        assert!(migrations_dir_is_empty(&root.path().join("migrations")));
        // Present but empty.
        let migrations = root.path().join("migrations");
        std::fs::create_dir_all(&migrations).expect("mkdir");
        assert!(migrations_dir_is_empty(&migrations));
        // A stray file (not a migration dir) still counts as empty.
        std::fs::write(migrations.join("README.md"), "x").expect("write");
        assert!(migrations_dir_is_empty(&migrations));
        // A real migration subdir flips it to non-empty.
        std::fs::create_dir_all(migrations.join("20260101000000_init")).expect("mkdir");
        assert!(!migrations_dir_is_empty(&migrations));
    }

    #[test]
    fn should_refresh_snapshot_only_when_something_was_applied() {
        // A real apply (non-empty `applied`) advances the baseline.
        let applied = MigrationResult {
            applied: vec!["20260101000000_init".to_string()],
        };
        assert!(should_refresh_snapshot(&applied));
        // A no-op apply (DB already up to date) must NOT advance it — otherwise
        // ungenerated model edits would be silently baked into the baseline.
        let noop = MigrationResult {
            applied: Vec::new(),
        };
        assert!(!should_refresh_snapshot(&noop));
    }

    #[test]
    fn provider_lock_missing_snapshot_is_not_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join(SNAPSHOT_DEFAULT_PATH);
        // No snapshot → Ok(false): caller notes it, does not fail.
        assert_eq!(enforce_provider_lock(&path, Backend::Postgres), Ok(false));
    }

    #[test]
    fn provider_lock_mismatch_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join(SNAPSHOT_DEFAULT_PATH);
        let snap = SchemaSnapshot::new(Backend::Sqlite, Vec::new());
        write_snapshot(&path, &snap).expect("write snapshot");
        // Detected Postgres vs a Sqlite-tagged snapshot → refused.
        let err = enforce_provider_lock(&path, Backend::Postgres).unwrap_err();
        assert!(
            err.to_lowercase().contains("backend") && err.contains("does not match"),
            "provider-lock refusal: {err}"
        );
    }

    #[test]
    fn provider_lock_match_is_ok() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join(SNAPSHOT_DEFAULT_PATH);
        let snap = SchemaSnapshot::new(Backend::Postgres, Vec::new());
        write_snapshot(&path, &snap).expect("write snapshot");
        assert_eq!(enforce_provider_lock(&path, Backend::Postgres), Ok(true));
    }

    #[test]
    fn migrate_outside_project_is_friendly_error() {
        let root = tempfile::tempdir().expect("tempdir");
        // No Cargo.toml → not a project.
        let err = migrate_at(root.path(), None).unwrap_err();
        assert!(
            err.contains("Autumn project"),
            "friendly not-in-project error: {err}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn sqlite_apply_seam_names_the_feature_in_default_build() {
        // In the default (Postgres-only) build the SQLite apply path is the seam:
        // it must point the user at `--features sqlite` and never touch the DB.
        let root = tempfile::tempdir().expect("tempdir");
        let err = apply_pending_sqlite("sqlite:///tmp/x.db", &root.path().join("migrations"))
            .unwrap_err();
        assert!(
            err.contains("--features sqlite") && err.contains("SQLite"),
            "seam error names the feature: {err}"
        );
    }
}
