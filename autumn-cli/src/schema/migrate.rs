//! `autumn schema migrate` — apply the generated migrations (slice 6 of tracking
//! issue #1975).
//!
//! This is the *apply* half of the declarative-schema loop. `autumn schema diff
//! --write-migration` (slice 4) authored `migrations/<ts>_<name>/{up,down}.sql`
//! after running the destructive-change guards AND advanced the checked-in
//! snapshot baseline to the generated plan's target state; this command applies
//! whatever is pending against the configured database. It does NOT touch the
//! snapshot — the baseline already moved at generation time, so re-snapshotting
//! here (from possibly-newer, still-ungenerated models) could only bake un-drift
//! into the baseline and hide it from the next `schema diff` / `doctor`.
//!
//! # What this command deliberately does NOT do
//!
//! It does not re-run the diff guards. The `-- autumn-safety:` advisories a
//! generated migration may carry are inert SQL comments; the destructive-change
//! refusal happened at diff time, not here. Migration files apply verbatim,
//! exactly as `autumn migrate` applies them — this command adds only the
//! provider-lock check. It also does not advance the snapshot (see above).
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

use super::snapshot::{SNAPSHOT_DEFAULT_PATH, SnapshotError, load_snapshot};

/// Apply pending migrations against the configured database.
///
/// The checked-in schema snapshot is NOT touched here — it already advanced at
/// `schema diff --write-migration` (generation) time.
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

    // Resolve the primary URL exactly as `autumn migrate` does.
    let url = crate::migrate::resolve_primary_url(profile);

    // Derive the backend from the same profile-resolved URL, whose scheme is authoritative, so
    // `--profile` picks the right apply path and provider-lock even when the project's default
    // backend differs. With no URL, fall back to the profile-aware project default.
    let backend = super::backend_for_url(project_root, profile, url.as_deref());

    // Provider lock: an existing snapshot's dialect must match the backend. A missing snapshot is
    // not fatal (the DB may predate snapshots).
    let snapshot_path = project_root.join(SNAPSHOT_DEFAULT_PATH);
    if !enforce_provider_lock(&snapshot_path, backend)? {
        eprintln!(
            "note: no schema snapshot at {} — applying migrations without a provider-lock check; \
             run `autumn schema snapshot` to establish the baseline.",
            snapshot_path.display()
        );
    }

    // Checked after the provider lock so a dialect mismatch is reported first.
    let url = url.ok_or_else(|| {
        "no database URL configured — set AUTUMN_DATABASE__URL or DATABASE_URL, or add a \
         [database] primary_url to autumn.toml"
            .to_string()
    })?;

    // An unreadable `migrations/` is an error, never "nothing to apply": skipping it would let a
    // deployment boot against an outdated schema.
    let migrations_dir = project_root.join("migrations");
    match classify_migrations_dir(&migrations_dir) {
        MigrationsDir::Absent | MigrationsDir::Empty => {
            println!("No migrations to apply.");
            return Ok(());
        }
        MigrationsDir::Unreadable(e) => {
            return Err(format!(
                "failed to read migrations directory {}: {e}",
                migrations_dir.display()
            ));
        }
        MigrationsDir::HasMigrations => {}
    }

    // Destructive-change guards ran at diff time and the snapshot advanced at
    // `schema diff --write-migration`, so files apply verbatim and nothing is re-snapshotted.
    // Same table-lock policy as `autumn migrate` (#3057).
    let lock_policy = crate::migrate::resolve_migration_lock_policy(profile);
    let result = apply_pending(backend, &url, &migrations_dir, lock_policy)?;
    report_applied(&result);

    Ok(())
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
    lock_policy: autumn_web::migrate::MigrationLockPolicy,
) -> Result<MigrationResult, String> {
    match backend {
        Backend::Postgres => apply_pending_pg(url, migrations_dir, lock_policy),
        Backend::Sqlite => apply_pending_sqlite(url, migrations_dir),
    }
}

/// Postgres apply path: advisory-locked (`run_pending_locked_with_policy`) so
/// concurrent migrators serialize cleanly, with the configured table-lock
/// policy (#3057).
fn apply_pending_pg(
    url: &str,
    migrations_dir: &Path,
    lock_policy: autumn_web::migrate::MigrationLockPolicy,
) -> Result<MigrationResult, String> {
    let migrations = file_based_migrations(migrations_dir)?;
    autumn_web::migrate::run_pending_locked_with_policy(
        url,
        migrations,
        Some(DEFAULT_LOCK_WAIT_TIMEOUT),
        lock_policy,
    )
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

/// Classification of the `migrations/` path for the apply decision.
///
/// Distinguishes the three cases the apply path must treat differently: an
/// absent path and a readable-but-empty directory are both clean no-ops, but an
/// unreadable path (an I/O error, or a regular file where a directory was
/// expected) must be surfaced as an error so a deployment can never silently
/// skip required migrations and boot against an outdated schema.
#[derive(Debug)]
enum MigrationsDir {
    /// The path does not exist — nothing to apply, clean no-op.
    Absent,
    /// A readable directory with no migration subdirectories — clean no-op.
    Empty,
    /// A readable directory containing at least one migration subdirectory.
    HasMigrations,
    /// The path exists but could not be read as a directory (an I/O error, or it
    /// is a regular file, not a directory) — the caller must error.
    Unreadable(std::io::Error),
}

/// Classify the `migrations/` path into [`MigrationsDir`].
///
/// An absent path is [`MigrationsDir::Absent`]. Otherwise the path must be a
/// readable directory: a `read_dir` failure — including the "path is a regular
/// file" case, on which `read_dir` errors — is [`MigrationsDir::Unreadable`],
/// never silently treated as empty. A readable directory is
/// [`MigrationsDir::HasMigrations`] when it contains at least one migration
/// subdirectory (the diesel layout) and [`MigrationsDir::Empty`] otherwise. Pure
/// over the filesystem so the decision is testable without a database.
///
/// Per-entry iteration errors are propagated, NOT swallowed: a `read_dir`
/// iterator can yield `Err` mid-scan (an I/O failure after the directory opened
/// cleanly). The whole iterator is consumed and ANY per-entry `Err` yields
/// [`MigrationsDir::Unreadable`] — otherwise a mid-scan failure could be
/// misclassified `Empty` and silently skip a required deploy.
fn classify_migrations_dir(migrations_dir: &Path) -> MigrationsDir {
    match migrations_dir.try_exists() {
        Ok(false) => return MigrationsDir::Absent,
        Ok(true) => {}
        // E.g. a permission error on a parent: unreadable, not absent.
        Err(e) => return MigrationsDir::Unreadable(e),
    }

    let entries = match std::fs::read_dir(migrations_dir) {
        Ok(entries) => entries,
        Err(e) => return MigrationsDir::Unreadable(e),
    };
    // Surface a per-entry error rather than filtering it out, which would report `Empty`.
    match entries.collect::<Result<Vec<_>, _>>() {
        Ok(entries) => {
            if entries.iter().any(|e| e.path().is_dir()) {
                MigrationsDir::HasMigrations
            } else {
                MigrationsDir::Empty
            }
        }
        Err(e) => MigrationsDir::Unreadable(e),
    }
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

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;
    // Only the provider-lock tests write a snapshot.
    use super::super::snapshot::{SchemaSnapshot, write_snapshot};

    #[test]
    fn classify_absent_path_is_a_no_op() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            classify_migrations_dir(&root.path().join("migrations")),
            MigrationsDir::Absent
        ));
    }

    #[test]
    fn classify_readable_empty_dir_is_a_no_op() {
        let root = tempfile::tempdir().expect("tempdir");
        let migrations = root.path().join("migrations");
        std::fs::create_dir_all(&migrations).expect("mkdir");
        assert!(matches!(
            classify_migrations_dir(&migrations),
            MigrationsDir::Empty
        ));
        // A stray file is not a migration.
        std::fs::write(migrations.join("README.md"), "x").expect("write");
        assert!(matches!(
            classify_migrations_dir(&migrations),
            MigrationsDir::Empty
        ));
    }

    #[test]
    fn classify_dir_with_migration_subdir_proceeds() {
        let root = tempfile::tempdir().expect("tempdir");
        let migrations = root.path().join("migrations");
        std::fs::create_dir_all(migrations.join("20260101000000_init")).expect("mkdir");
        assert!(matches!(
            classify_migrations_dir(&migrations),
            MigrationsDir::HasMigrations
        ));
    }

    #[test]
    fn classify_regular_file_is_unreadable_error() {
        let root = tempfile::tempdir().expect("tempdir");
        // A regular file makes `read_dir` fail: an error, not an empty no-op.
        let migrations = root.path().join("migrations");
        std::fs::write(&migrations, "not a directory").expect("write");
        assert!(matches!(
            classify_migrations_dir(&migrations),
            MigrationsDir::Unreadable(_)
        ));
    }

    // A mid-scan per-entry `Err` also classifies as `Unreadable`, but no portable primitive can
    // inject an I/O failure after the directory opens, so only the open-error path is tested.
    #[cfg(unix)]
    #[test]
    fn classify_unreadable_dir_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        // Skipped when the process bypasses permission bits (root ignores chmod 000).
        let root = tempfile::tempdir().expect("tempdir");
        let migrations = root.path().join("migrations");
        std::fs::create_dir_all(&migrations).expect("mkdir");
        std::fs::set_permissions(&migrations, std::fs::Permissions::from_mode(0o000))
            .expect("chmod");

        let can_still_read = std::fs::read_dir(&migrations).is_ok();
        let classified = classify_migrations_dir(&migrations);
        // Restore so the tempdir can be cleaned up.
        std::fs::set_permissions(&migrations, std::fs::Permissions::from_mode(0o755))
            .expect("restore chmod");

        if can_still_read {
            return;
        }
        assert!(matches!(classified, MigrationsDir::Unreadable(_)));
    }

    #[test]
    fn provider_lock_missing_snapshot_is_not_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join(SNAPSHOT_DEFAULT_PATH);
        assert_eq!(enforce_provider_lock(&path, Backend::Postgres), Ok(false));
    }

    #[test]
    fn provider_lock_mismatch_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join(SNAPSHOT_DEFAULT_PATH);
        let snap = SchemaSnapshot::new(Backend::Sqlite, Vec::new());
        write_snapshot(&path, &snap).expect("write snapshot");
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
        // No Cargo.toml.
        let err = migrate_at(root.path(), None).unwrap_err();
        assert!(
            err.contains("Autumn project"),
            "friendly not-in-project error: {err}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn sqlite_apply_seam_names_the_feature_in_default_build() {
        // Without the `sqlite` feature the SQLite apply path names the feature and never touches
        // the DB.
        let root = tempfile::tempdir().expect("tempdir");
        let err = apply_pending_sqlite("sqlite:///tmp/x.db", &root.path().join("migrations"))
            .unwrap_err();
        assert!(
            err.contains("--features sqlite") && err.contains("SQLite"),
            "seam error names the feature: {err}"
        );
    }
}
