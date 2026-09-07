//! `autumn migrate` -- run, inspect, or roll back Diesel database migrations.
//!
//! # Subcommands
//!
//! | Subcommand | Description |
//! |---|---|
//! | *(default)* | Apply all pending user + framework migrations |
//! | `status` | Show applied/pending status per migration plus rollback availability |
//! | `check` | Classify every `up.sql` and `down.sql` for rolling-deploy safety |
//! | `down` | Revert the most recently applied user migration(s) via `down.sql` |
//!
//! # User vs framework migrations
//!
//! **User migrations** live in `./migrations/` and are executed via the
//! `diesel` CLI (`diesel migration run`) or the Rust harness (for `down`).
//!
//! **Framework migrations** are embedded in the `autumn` crate and applied
//! through the Rust `MigrationHarness`. They are **forward-only**: `autumn
//! migrate down` never reverts them. Their forward-only contract is preserved
//! regardless of which user migrations are rolled back.
//!
//! # Database URL resolution
//!
//! Precedence (highest to lowest):
//! 1. `AUTUMN_DATABASE__PRIMARY_URL` env var
//! 2. `AUTUMN_DATABASE__URL` env var
//! 3. `DATABASE_URL` env var
//! 4. `database.primary_url` in `autumn.toml`
//! 5. `database.url` in `autumn.toml`

pub mod safety;

use std::path::Path;
use std::process::Command;

use autumn_web::migrate::{
    AppliedUserMigration, EmbeddedMigrations, FRAMEWORK_MIGRATIONS, MigrationError, MigrationResult,
};

/// Default directory containing Diesel migration files.
const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// Seam message shown when a `sqlite://` database URL is resolved but this CLI
/// build targets Postgres only. The `sqlite` backend-flip is a separate,
/// non-default cargo feature that must never be co-built with the default
/// Postgres backend (see `autumn-cli/Cargo.toml`).
///
/// Only referenced by the `#[cfg(not(feature = "sqlite"))]` seams, so it is gated
/// off the `sqlite` build to avoid a dead-code warning there.
#[cfg(not(feature = "sqlite"))]
const SQLITE_FEATURE_MSG: &str = "`autumn migrate` detected a SQLite database URL, but this CLI build targets Postgres only. \
     Rebuild with `--no-default-features --features sqlite` to run migrations against a \
     `sqlite://` database (the sqlite backend-flip must never be co-built with the default \
     Postgres backend).";

/// Whether `database_url` names a `SQLite` target (`sqlite:` / `file:` scheme).
///
/// A Postgres URL, a libpq keyword/value string, or any unrecognized target is
/// treated as Postgres — i.e. it keeps the historical advisory-locked apply/
/// rollback path — so only an explicit `sqlite://` (or `sqlite:` / `file:`) URL
/// routes to the unlocked `SQLite` harness path (issue #2058). This is
/// backend-detection, independent of the compiled feature: under the default
/// build a detected `SQLite` URL still routes here and then hits the
/// [`SQLITE_FEATURE_MSG`] seam.
fn is_sqlite_target(database_url: &str) -> bool {
    matches!(
        autumn_web::config::DatabaseBackend::detect(database_url),
        Some(autumn_web::config::DatabaseBackend::Sqlite)
    )
}

/// Whether every resolved target is a `SQLite` URL, used to skip the Postgres-only
/// `diesel` CLI preflight (`check_diesel_cli`) — the `SQLite` apply path uses the
/// in-process harness, never the `diesel` subprocess.
fn all_targets_sqlite(targets: &[(String, String)]) -> bool {
    !targets.is_empty() && targets.iter().all(|(_, url)| is_sqlite_target(url))
}

/// Apply pending user migrations against a `SQLite` database through the unlocked
/// diesel harness (no advisory lock, no `diesel` subprocess — issue #1999/#2036
/// precedent). Real only under the `sqlite` feature; the default build returns
/// the [`SQLITE_FEATURE_MSG`] seam.
#[cfg(feature = "sqlite")]
fn apply_pending_sqlite_cli(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<MigrationResult, MigrationError> {
    let migrations =
        diesel_migrations::FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
            MigrationError::Migration(format!(
                "failed to read migrations directory {}: {e}",
                migrations_dir.display()
            ))
        })?;
    autumn_web::migrate::run_pending_sqlite(database_url, migrations)
}

/// `SQLite` apply seam in the default (Postgres-only) build — references no
/// `SQLite` symbol and points the operator at the backend-flip feature.
#[cfg(not(feature = "sqlite"))]
fn apply_pending_sqlite_cli(
    _database_url: &str,
    _migrations_dir: &Path,
) -> Result<MigrationResult, MigrationError> {
    Err(MigrationError::Migration(SQLITE_FEATURE_MSG.to_owned()))
}

/// Return the applied user migrations for a target, routing an explicit
/// `sqlite://` URL to the unlocked `SQLite` harness path and everything else to
/// the historical Postgres path.
fn applied_user_migrations_for_target(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    if is_sqlite_target(database_url) {
        applied_user_migrations_sqlite_cli(database_url, migrations_dir)
    } else {
        autumn_web::migrate::applied_user_migrations(database_url, migrations_dir)
    }
}

#[cfg(feature = "sqlite")]
fn applied_user_migrations_sqlite_cli(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    autumn_web::migrate::applied_user_migrations_sqlite(database_url, migrations_dir)
}

#[cfg(not(feature = "sqlite"))]
fn applied_user_migrations_sqlite_cli(
    _database_url: &str,
    _migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    Err(MigrationError::Migration(SQLITE_FEATURE_MSG.to_owned()))
}

/// Arguments for `autumn migrate down`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownArgs {
    /// Revert this many migrations (default: 1). Mutually exclusive with `to`.
    pub steps: Option<usize>,
    /// Revert until this version is the latest applied. Mutually exclusive with `steps`.
    pub to: Option<String>,
    /// Required when targeting the production profile.
    pub yes_i_mean_prod: bool,
}

/// Arguments for `autumn migrate baseline`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineArgs {
    /// Re-baseline the checksum for a single applied version, overwriting
    /// whatever hash is currently recorded. The **escape hatch** for
    /// deliberate edits to an applied migration — logged loudly, never the
    /// default. When `None`, `baseline` runs in additive mode and records
    /// hashes only for applied migrations that don't already have one.
    pub force_version: Option<String>,
}

/// Subcommands for `autumn migrate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateAction {
    /// Run all pending migrations.
    Run,
    /// Show migration status (pending / applied).
    Status,
    /// Preflight safety check — classifies all migration SQL files and returns
    /// a non-zero exit code if any unsafe or unclassified operations are found.
    Check,
    /// Revert the most recently applied user migration(s).
    Down(DownArgs),
    /// Record content hashes for applied migrations (issue #1203).
    ///
    /// Additive by default (records only applied-but-unrecorded versions —
    /// the legacy backfill). Pass `force_version` to overwrite one version's
    /// stored hash (the escape hatch for a deliberate edit).
    Baseline(BaselineArgs),
}

/// Per-migration safety report returned by [`check_migrations_in_dir_for`].
pub struct MigrationSafetyReport {
    /// Migration directory name (e.g. `"20260101000000_create_posts"`).
    pub name: String,
    /// Findings for `up.sql`.
    pub up: Vec<safety::SafetyFinding>,
    /// Findings for `down.sql` (empty when `down.sql` is absent or empty).
    pub down: Vec<safety::SafetyFinding>,
}

/// Which databases `autumn migrate` operates on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateTarget {
    /// The control database plus every `[[database.shards]]` entry
    /// (control first, shards in declaration order). The default.
    All,
    /// Only the control database (`database.primary_url`/`url`) —
    /// pre-shards behavior.
    ControlOnly,
    /// A single shard, addressed by its configured `name`.
    Shard(String),
}

/// Run the migrate command.
///
/// `wait_override` comes from the `--wait` CLI flag and, when `Some`, takes
/// precedence over `database.startup_wait_secs` from the config / environment.
pub fn run(
    action: &MigrateAction,
    with_maintenance: bool,
    target: &MigrateTarget,
    profile: Option<&str>,
    wait_override: Option<u64>,
) {
    eprintln!("\u{1F342} autumn migrate\n");

    match action {
        MigrateAction::Check => {
            // `check` is an offline, SQL-only preflight that needs no database
            // URL, so it must NOT parse `.env`. Resolving the overlay here would
            // let a malformed/unreadable local `.env` abort the check (breaking
            // offline / CI safety checks), so the `.env` overlay is built lazily
            // only in the database-backed paths below.
            let migrations_dir = resolve_migrations_dir();
            run_safety_check(&migrations_dir, profile);
            return;
        }
        MigrateAction::Down(args) => {
            run_down(args, with_maintenance, target, profile);
            return;
        }
        MigrateAction::Baseline(args) => {
            run_baseline(args, target, profile);
            return;
        }
        _ => {}
    }

    // Overlay a project-root `.env` UNDER the real environment for DB-URL
    // resolution, without mutating the process environment. A real env var of
    // the same key always wins; a malformed `.env` fails loudly here. Built only
    // now — after the non-DB early returns — so `migrate check` never reads it.
    let env = os_env_with_dotenv_or_exit();

    // 1. Resolve migration target databases from autumn.toml (+ profile
    //    overlay) + env.  The merged config table is returned so that
    //    resolve_startup_wait can reuse it without a second filesystem read.
    let (targets, config_table) = resolve_targets(target, profile, &env);

    // 2. Resolve migrations directory
    let migrations_dir = resolve_migrations_dir();

    // 3. Check that diesel CLI is available — but only when a target actually
    //    needs the `diesel` subprocess. The SQLite apply path uses the in-process
    //    harness (issue #2058), so an all-SQLite run must not demand a
    //    Postgres-oriented `diesel` CLI on PATH.
    if !all_targets_sqlite(&targets) {
        check_diesel_cli();
    }

    // 4. Enable maintenance mode if requested
    if with_maintenance && *action == MigrateAction::Run {
        enable_maintenance_for_migrate();
    }

    // 5. Execute the appropriate diesel command per target
    match action {
        MigrateAction::Run => {
            // Resolve the effective startup wait (--wait flag > config > 0).
            let wait = resolve_startup_wait(wait_override, config_table.as_ref());
            run_all_targets(&targets, &migrations_dir, with_maintenance, wait);
        }
        MigrateAction::Status => {
            // `show_status` shells out to `diesel migration list`, and the
            // rollback/framework status readers use the Postgres migration
            // connection — none of which is wired for the SQLite backend (the
            // #2058 port covers `migrate` up/down only). A `sqlite://` target
            // therefore gets a clear provider-appropriate message here instead of
            // reaching the `diesel` subprocess and dying with a raw OS error (the
            // `diesel` CLI preflight was deliberately skipped for all-SQLite runs).
            let mut sqlite_unsupported = false;
            for (label, url) in &targets {
                eprintln!("\u{2500}\u{2500} {label} \u{2500}\u{2500}");
                if is_sqlite_target(url) {
                    eprintln!(
                        "  \u{2717} `autumn migrate status` is not supported under the SQLite \
                         backend. The #2058 port covers `autumn migrate` (up) and \
                         `autumn migrate down` only \u{2014} run those to apply or roll back a \
                         `sqlite://` database."
                    );
                    eprintln!();
                    sqlite_unsupported = true;
                    continue;
                }
                show_status(url, &migrations_dir);
                show_rollback_availability(url, &migrations_dir);
                // Shard targets only require the shard framework migrations, so
                // report against that set instead of the full control-plane one.
                show_framework_status(url, label.starts_with("shard:"));
                eprintln!();
            }
            if sqlite_unsupported {
                std::process::exit(1);
            }
        }
        MigrateAction::Check | MigrateAction::Down(_) | MigrateAction::Baseline(_) => {
            unreachable!("handled above")
        }
    }
}

/// Build the real OS environment overlaid with a project-root `.env`, exiting
/// loudly (`exit(1)`) if a `.env` file exists but is malformed or unreadable.
///
/// Called only from the database-backed migrate actions (the main `run` flow,
/// `run_down`, and `run_baseline`), right before each `resolve_targets` — so a
/// broken local `.env` fails only where a DB URL is actually resolved, never for
/// the offline `migrate check` preflight.
fn os_env_with_dotenv_or_exit() -> autumn_web::dotenv::DotenvOsEnv {
    match autumn_web::dotenv::os_env_with_dotenv() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("  \u{274C} .env: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve the `(label, database_url)` pairs the command operates on,
/// in apply order (control first, then shards in declaration order).
///
/// Also returns the merged config table so callers can reuse it (e.g. for
/// `resolve_startup_wait_secs_from_sources`) without a second filesystem read.
fn resolve_targets(
    target: &MigrateTarget,
    profile: Option<&str>,
    env: &dyn autumn_web::config::Env,
) -> (Vec<(String, String)>, Option<toml::Table>) {
    // Read autumn.toml once, deep-merging the `autumn-<profile>.toml` overlay
    // when a profile is selected, so control and shard URLs both resolve from
    // the same effective configuration. Environment overrides still win over
    // the merged file (handled inside the `_from_sources` helpers).
    //
    // When no profile is given explicitly (via `--profile` / `AUTUMN_PROFILE`),
    // fall back to `AUTUMN_ENV` — the framework's preferred profile selector —
    // so `autumn migrate` resolves the same overlay the app itself would use.
    //
    // `env` overlays a project-root `.env` under the real environment, so a
    // `.env`-provided DB URL is honored while a real env var still wins.
    let effective = effective_profile(profile);
    let config_table = read_autumn_toml_table_with_profile(Some(&effective));
    let control =
        resolve_primary_database_url_from_sources(|key| env.var(key), config_table.as_ref());
    let shards =
        resolve_shard_database_urls_from_sources(|key| env.var(key), config_table.as_ref());
    // Fail closed BEFORE building targets or applying anything: SQLite anywhere
    // in a sharded topology is rejected at boot by the runtime guards
    // (`autumn/src/config.rs` `database_backend_consistency` + shard-URL
    // Postgres-only validation, and `autumn/src/app.rs`
    // `sqlite_sharding_unsupported_guard`). Without this the new per-target
    // SQLite apply/revert path would create + migrate the control file and/or a
    // SQLite shard file and report success for an app that cannot boot (issue
    // #2058). Mirror the runtime guard's wording.
    if let Err(message) = reject_sqlite_sharding_topology(control.as_deref(), &shards) {
        eprintln!("{message}");
        std::process::exit(1);
    }
    match build_targets(control, shards, target) {
        Ok(targets) => (targets, config_table),
        Err(message) => {
            eprintln!("{message}");
            if matches!(target, MigrateTarget::All | MigrateTarget::ControlOnly) {
                // Reuse the standard missing-URL guidance (prints and exits).
                resolve_database_url();
            }
            std::process::exit(1);
        }
    }
}

/// Reject any `SQLite` target within a sharded topology — horizontal sharding is
/// Postgres-only.
///
/// Mirrors the runtime guard `autumn_web`'s `sqlite_sharding_unsupported_guard`
/// (and the `database_backend_consistency` / shard-URL Postgres-only validators)
/// so `autumn migrate` fails closed BEFORE touching any database, rather than
/// creating + migrating a `SQLite` control or shard file for an app that cannot
/// boot. Two branches, matching the runtime guard:
///
/// * a detected-`SQLite` **control** with shard entries present, and
/// * **any shard** whose resolved `primary_url` is a detected `SQLite` backend
///   (regardless of the control backend — the shard loop would otherwise route it
///   through the Postgres-only harness).
///
/// A non-`SQLite` control with all-Postgres shards (the intended sharded
/// topology), and a control-less non-sharded deployment, both pass. Pure so the
/// decision is unit-testable; `resolve_targets` prints the message and exits on
/// `Err`.
fn reject_sqlite_sharding_topology(
    control: Option<&str>,
    shards: &[(String, String)],
) -> Result<(), String> {
    if !shards.is_empty() && control.is_some_and(is_sqlite_target) {
        return Err(
            "\u{2717} SQLite deployments do not support sharding. The configured sqlite:// \
             control target has shard entries configured, which is a Postgres-only capability \
             \u{2014} remove the shard configuration to run on SQLite, or use a Postgres control \
             database. Tracking: #1614."
                .to_owned(),
        );
    }
    if shards.iter().any(|(_, url)| is_sqlite_target(url)) {
        return Err(
            "\u{2717} SQLite deployments do not support sharding. A configured shard targets a \
             SQLite database, and per-shard migration is a Postgres-only capability \u{2014} \
             remove the SQLite shard configuration to run on SQLite, or use Postgres shard \
             targets. Tracking: #1614."
                .to_owned(),
        );
    }
    Ok(())
}

/// Pure target-selection logic behind [`resolve_targets`], separated so
/// every branch is unit-testable without touching the environment.
fn build_targets(
    control: Option<String>,
    shards: Vec<(String, String)>,
    target: &MigrateTarget,
) -> Result<Vec<(String, String)>, String> {
    match target {
        MigrateTarget::ControlOnly => control
            .map(|url| vec![("control".to_owned(), url)])
            .ok_or_else(|| "\u{2717} No control database URL found.".to_owned()),
        MigrateTarget::Shard(name) => {
            let Some((_, url)) = shards.iter().find(|(shard, _)| shard == name) else {
                let detail = if shards.is_empty() {
                    "No [[database.shards]] entries found in autumn.toml or environment.".to_owned()
                } else {
                    let known: Vec<&str> = shards.iter().map(|(n, _)| n.as_str()).collect();
                    format!("Known shards: {}", known.join(", "))
                };
                return Err(format!("\u{2717} Unknown shard {name:?}.\n  {detail}"));
            };
            Ok(vec![(format!("shard:{name}"), url.clone())])
        }
        MigrateTarget::All => {
            // Shard-only deployments (no control role) are a valid shape:
            // include the control target only when a control URL resolves.
            let mut targets = Vec::new();
            if let Some(control_url) = control {
                targets.push(("control".to_owned(), control_url));
            } else if shards.is_empty() {
                return Err("\u{2717} No database URL found.".to_owned());
            }
            for (name, url) in shards {
                targets.push((format!("shard:{name}"), url));
            }
            reject_duplicate_target_urls(&targets)?;
            Ok(targets)
        }
    }
}

/// Reject distinct target labels that resolve to the same database URL (e.g.
/// profile/env overrides mapping `control` and a shard to one DB). Without this,
/// a multi-target `migrate down` would roll the shared database back once per
/// label, reverting more migrations than requested.
fn reject_duplicate_target_urls(targets: &[(String, String)]) -> Result<(), String> {
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (label, url) in targets {
        if let Some(prev) = seen.insert(url.as_str(), label.as_str()) {
            return Err(format!(
                "\u{2717} Targets {prev:?} and {label:?} resolve to the same database URL. \
                 Check profile / AUTUMN_DATABASE__* overrides; migrating or rolling back the \
                 same database under two labels would apply/revert it twice."
            ));
        }
    }
    Ok(())
}

/// Apply migrations to every target in order, failing fast with a
/// per-target summary.
fn run_all_targets(
    targets: &[(String, String)],
    migrations_dir: &str,
    with_maintenance: bool,
    wait: std::time::Duration,
) {
    let mut completed: Vec<&str> = Vec::new();
    for (label, url) in targets {
        eprintln!("\u{2500}\u{2500} Migrating {label} \u{2500}\u{2500}");
        // Labels starting with "shard:" are shard targets; they receive only
        // the shard-required framework migrations (version history + commit
        // hook queue), not the full control-plane schema.
        let is_shard = label.starts_with("shard:");
        if run_single_target(url, migrations_dir, is_shard, wait) {
            completed.push(label);
            eprintln!();
        } else {
            eprintln!();
            eprintln!("  Summary:");
            for done in &completed {
                eprintln!("    \u{2713} {done}");
            }
            eprintln!("    \u{2717} {label} \u{2014} FAILED (see output above)");
            for (pending, _) in targets.iter().skip(completed.len() + 1) {
                eprintln!("    \u{2022} {pending} \u{2014} not attempted");
            }
            eprintln!();
            eprintln!(
                "  Migrations already applied to earlier targets are skipped \
                 idempotently when you rerun `autumn migrate`."
            );
            if with_maintenance {
                eprintln!(
                    "  \u{26A0}\u{FE0F}  Migration failed — maintenance mode left ON for safety."
                );
                eprintln!("      Fix the migration then run `autumn migrate` to retry.");
                eprintln!("      Run `autumn maintenance off` to re-open traffic manually.");
            }
            std::process::exit(1);
        }
    }

    eprintln!("  Summary:");
    for done in completed {
        eprintln!("    \u{2713} {done}");
    }
    if with_maintenance {
        disable_maintenance_after_migrate();
    }
}

/// Enable maintenance mode before a migrate run.
fn enable_maintenance_for_migrate() {
    use autumn_web::maintenance::{MAINTENANCE_FLAG_FILE, MaintenanceConfig, MaintenanceState};
    let path = std::path::Path::new(MAINTENANCE_FLAG_FILE);
    let config = MaintenanceConfig {
        message: Some("Database migration in progress. Please try again in a moment.".to_owned()),
        ..Default::default()
    };
    match MaintenanceState::save_to_file(path, &config) {
        Ok(()) => eprintln!("  \u{26A0}\u{FE0F}  Maintenance mode ENABLED (--with-maintenance)"),
        Err(e) => {
            eprintln!("\u{274C} Failed to enable maintenance mode: {e}");
            std::process::exit(1);
        }
    }
}

/// Disable maintenance mode after a successful migrate run.
fn disable_maintenance_after_migrate() {
    use autumn_web::maintenance::{MAINTENANCE_FLAG_FILE, MaintenanceState};
    let path = std::path::Path::new(MAINTENANCE_FLAG_FILE);
    match MaintenanceState::remove_flag_file(path) {
        Ok(_) => eprintln!("  \u{2713} Maintenance mode DISABLED — normal traffic resuming"),
        Err(e) => eprintln!("\u{26A0}\u{FE0F}  Could not remove maintenance flag: {e}"),
    }
}

/// Apply app + framework migrations to one database. Returns whether
/// everything succeeded; the caller decides how to fail.
///
/// When `is_shard` is `true`, applies only the shard-required framework
/// migrations (version history + commit-hook queue) instead of the full
/// control-plane `FRAMEWORK_MIGRATIONS` set.
///
/// `wait` controls the startup connectivity wait (AC #2/#6):
///   * `Duration::ZERO` → skip; behaviour is byte-for-byte identical to today.
///   * `> Duration::ZERO` → retry with capped exponential backoff until the DB
///     accepts connections or the window elapses.
fn run_single_target(
    database_url: &str,
    migrations_dir: &str,
    is_shard: bool,
    wait: std::time::Duration,
) -> bool {
    use autumn_web::migrate::{DEFAULT_LOCK_WAIT_TIMEOUT, hold_migration_lock, wait_for_database};

    // SQLite (issue #2058): a single-writer local database has no advisory lock,
    // no `diesel` subprocess, no Postgres content-checksum table, and no
    // control-plane framework migrations — the unlocked harness applies the user
    // migrations directly (mirrors `autumn schema migrate`, #1999/#2036). The
    // Postgres path below is unchanged.
    if is_sqlite_target(database_url) {
        return run_single_target_sqlite(database_url, migrations_dir);
    }

    // Startup wait — only when enabled (startup_wait_secs > 0 or --wait N).
    // When wait == Duration::ZERO we skip entirely so the existing fail-fast
    // path is preserved byte-for-byte (AC #6).
    if wait > std::time::Duration::ZERO {
        eprintln!(
            "  Waiting up to {}s for database to become reachable…",
            wait.as_secs()
        );
        match wait_for_database(database_url, wait, |attempt, delay| {
            eprintln!(
                "  Database not reachable yet (attempt {attempt}); \
                 retrying in {}ms\u{2026}",
                delay.as_millis(),
            );
        }) {
            Ok(()) => eprintln!("  Database reachable."),
            Err(e) => {
                eprintln!("\u{274C} {e}");
                return false;
            }
        }
    }

    // Acquire this target database's Postgres advisory lock before reading
    // the pending-migration list. This serializes concurrent callers
    // (rolling-deploy replicas or parallel `autumn migrate run`
    // invocations): only one process runs migrations against a given
    // database at a time; the rest wait and then find no pending work.
    // The lock is released when `_lock_guard` drops (end of this function
    // or process exit — both are safe because PostgreSQL releases
    // session-level advisory locks on connection close).
    //
    // Known limitation: the advisory lock lives on the parent process's
    // connection, not inside the child `diesel` subprocess. If the parent is
    // killed (SIGKILL or SIGTERM) while the child is still running, Postgres
    // releases the session lock and a second caller can acquire it before the
    // child finishes. SIGKILL is not fixable at the Rust level (no destructors
    // run); for SIGTERM a kill-on-drop child guard would close the window but
    // could abort an in-progress transaction. In practice most orchestrators
    // kill the whole cgroup, and Postgres's transaction isolation prevents
    // concurrent dirty writes either way.
    let _lock_guard = match hold_migration_lock(database_url, DEFAULT_LOCK_WAIT_TIMEOUT) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("\u{274C} Failed to acquire migration lock: {e}");
            return false;
        }
    };

    // Content-checksum guard (issue #1203): before applying any pending
    // migrations, validate that every already-applied migration's recorded
    // hash still matches its on-disk `up.sql`. A mismatch means the migration
    // was edited after being applied — the schema in this database silently
    // forks from what a fresh build would produce. Fail fast rather than
    // compounding the drift.
    let dir = std::path::Path::new(migrations_dir);
    if !validate_checksums_before_apply(database_url, dir) {
        return false;
    }

    eprintln!("  Running pending migrations...\n");
    let status = Command::new("diesel")
        .args(["migration", "run", "--migration-dir"])
        .arg(dir)
        .env("DATABASE_URL", database_url)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("\n\u{2713} Migrations applied successfully.");
        }
        Ok(_) => {
            eprintln!(
                "\n\u{274C} Migration failed in {}. Check the error output above.",
                dir.display()
            );
            return false;
        }
        Err(e) => {
            eprintln!("\u{274C} Failed to run diesel migration run: {e}");
            return false;
        }
    }

    // Record the USER-migration checksums NOW — immediately after the user
    // `diesel migration run` succeeded and BEFORE the framework / shard
    // framework migrations run (issue #1203 review, P2-B). Those later steps
    // can fail and early-return; if we deferred recording until after them, the
    // just-applied user versions would be left with no stored hash. On the next
    // retry they would classify as `Unrecorded`, so an edit to a user `up.sql`
    // made while fixing the framework failure would be silently accepted as the
    // baseline instead of tripping the drift guard. `record_checksums_from_dir`
    // resolves against the user `dir`, so it only ever records user versions
    // (framework versions live in the embedded set and are recorded by
    // `run_pending`); it is idempotent (ON CONFLICT DO NOTHING), so recording
    // again after the framework step below is harmless.
    record_checksums_after_apply(database_url, dir);

    let framework_ok = if is_shard {
        run_shard_framework_migrations(database_url)
    } else {
        run_framework_migrations(database_url)
    };
    if !framework_ok {
        return false;
    }

    true
}

/// Apply pending user migrations to a single `SQLite` target through the unlocked
/// harness (issue #2058). Returns whether it succeeded.
///
/// Deliberately mirrors `autumn schema migrate`'s `SQLite` path: no advisory lock
/// (`SQLite` is single-writer, #1999), no `diesel` subprocess, no Postgres
/// content-checksum bookkeeping, and no control-plane / shard framework
/// migrations (their DDL is Postgres-specific and is never applied to a `SQLite`
/// database). Only the project's `migrations/` user set is applied.
fn run_single_target_sqlite(database_url: &str, migrations_dir: &str) -> bool {
    let dir = std::path::Path::new(migrations_dir);
    eprintln!("  Running pending migrations (SQLite, unlocked harness)...\n");
    match apply_pending_sqlite_cli(database_url, dir) {
        Ok(result) if result.applied.is_empty() => {
            eprintln!("\u{2713} Migrations are already up to date.");
            true
        }
        Ok(result) => {
            for migration in &result.applied {
                eprintln!("  Applied {migration}");
            }
            eprintln!("\n\u{2713} Migrations applied successfully.");
            true
        }
        Err(e) => {
            eprintln!("\u{2717} {e}");
            false
        }
    }
}

/// Fail-fast validation of every applied migration's checksum against its
/// on-disk `up.sql` (issue #1203). Prints a helpful error and returns
/// `false` on mismatch so `run_single_target` can propagate the failure.
///
/// Returns `true` when validation passes OR when the checksum table doesn't
/// yet exist — a fresh database that hasn't run the framework migration
/// which creates the table is not itself an error.
fn validate_checksums_before_apply(database_url: &str, migrations_dir: &std::path::Path) -> bool {
    match autumn_web::migrate::validate_recorded_checksums_against_dir(database_url, migrations_dir)
    {
        Ok(()) => true,
        Err(e) => {
            eprintln!("\u{274C} {e}");
            false
        }
    }
}

/// After a successful apply, record content hashes for every applied
/// migration that doesn't yet have a stored checksum (issue #1203).
/// Silent when the migrations dir is unreadable — this backfills the CLI
/// path when the framework's checksum table wasn't present before.
fn record_checksums_after_apply(database_url: &str, migrations_dir: &std::path::Path) {
    match autumn_web::migrate::record_checksums_from_dir(database_url, migrations_dir) {
        Ok(0) => {}
        Ok(n) => {
            eprintln!(
                "  \u{2713} Recorded content checksum(s) for {n} newly-applied migration(s)."
            );
        }
        Err(e) => {
            eprintln!(
                "  \u{26A0}\u{FE0F}  Could not record migration checksums: {e}. Applied \
                 migrations will show as `unrecorded` until `autumn migrate baseline` is run."
            );
        }
    }
}

/// Run `autumn migrate baseline` against every configured target.
///
/// * Default (additive) — records on-disk `up.sql` hashes for every applied
///   migration that does not yet have a stored checksum. Idempotent; safe to
///   re-run. This is the answer to "we adopted the checksum feature on a live
///   database with already-applied migrations".
///
/// * `--force <version>` — overwrites the stored checksum for one applied
///   version with the current on-disk hash. The **escape hatch** for a
///   deliberate re-baseline (an operator edited an applied migration, accepts
///   the fork risk, and wants future runs to compare against the new bytes).
///   Emits a WARN log.
///
/// Both paths run their applied-versions read and checksum write under the same
/// advisory migration lock that `run`/`down` use (via the `*_locked` helpers),
/// so a concurrent `autumn migrate down` cannot revert a version between the
/// read and the write (issue #1203 review).
fn run_baseline(args: &BaselineArgs, target: &MigrateTarget, profile: Option<&str>) {
    // Overlay `.env` under the real environment only now that we are about to
    // resolve a DB URL (a malformed `.env` fails loudly here).
    let env = os_env_with_dotenv_or_exit();
    let (targets, _) = resolve_targets(target, profile, &env);
    let migrations_dir = resolve_migrations_dir();
    let dir = std::path::Path::new(&migrations_dir);

    for (label, url) in &targets {
        eprintln!("\u{2500}\u{2500} Baselining {label} \u{2500}\u{2500}");
        if let Some(version) = args.force_version.as_deref() {
            eprintln!(
                "  \u{26A0}\u{FE0F}  Re-baselining checksum for {version} \u{2014} the new on-disk \
                 hash will replace the previously recorded one. Other environments running the \
                 previous content will now report a mismatch."
            );
            match autumn_web::migrate::rebaseline_checksum_from_dir_locked(url, dir, version, None)
            {
                Ok(()) => eprintln!("  \u{2713} Re-baselined {version}."),
                Err(e) => {
                    eprintln!("\u{274C} Re-baseline failed for {label}: {e}");
                    std::process::exit(1);
                }
            }
        } else {
            match autumn_web::migrate::record_checksums_from_dir_locked(url, dir, None) {
                Ok(0) => eprintln!("  All applied migrations already have recorded checksums."),
                Ok(n) => eprintln!("  \u{2713} Recorded checksum(s) for {n} migration(s)."),
                Err(e) => {
                    eprintln!("\u{274C} Baseline failed for {label}: {e}");
                    std::process::exit(1);
                }
            }
        }
        eprintln!();
    }
}

/// Print the safety findings for one migration direction (`up.sql`/`down.sql`).
fn print_findings(label: &str, name: &str, findings: &[safety::SafetyFinding]) {
    if safety::is_safe(findings) {
        eprintln!("  \u{2713} {name}  [{label}]");
    } else {
        eprintln!("  \u{2717} {name}  [{label}]");
        for f in findings {
            eprintln!("      \u{2022} {} [{}]", f.operation, f.risk);
            eprintln!("        Why:  {}", f.why);
            eprintln!("        Next: {}", f.next_action);
        }
    }
}

/// Run the migration safety preflight check against all SQL files in `migrations_dir`.
///
/// Prints a human-readable report to stderr and exits with code 1 if any
/// unsafe or potentially-blocking operations are detected in either direction.
fn run_safety_check(migrations_dir: &str, profile: Option<&str>) {
    // The app's own backend decides the dialect (issue #1906): a SQLite app must
    // not be told to use `CREATE INDEX CONCURRENTLY`. `detect_backend_offline`
    // honors `--profile`, reads no `.env`, and never exits, so `check` stays the
    // offline, URL-free preflight documented above.
    let backend = crate::generate::detect_backend_offline(Path::new("."), profile);
    let reports = match check_migrations_in_dir_for(backend, Path::new(migrations_dir)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\u{2717} Migration safety check failed: {e}");
            std::process::exit(1);
        }
    };

    if reports.is_empty() {
        eprintln!("\u{2713} No migrations found in {migrations_dir}/");
        return;
    }

    let total = reports.len();
    eprintln!("  Scanning {total} migration(s) in {migrations_dir}/...\n");

    for report in &reports {
        print_findings("up.sql", &report.name, &report.up);
        if !report.down.is_empty() {
            print_findings("down.sql", &report.name, &report.down);
        }
    }

    let any_unsafe = rolling_deploy_blocked(&reports);
    let any_down_unsafe = reports.iter().any(|r| safety::has_unsafe_findings(&r.down));

    eprintln!();
    if any_unsafe {
        eprintln!(
            "\u{2717} One or more migrations contain operations that are unsafe for a live \
             rolling deploy."
        );
        eprintln!("  Review the findings above, apply the expand/contract pattern where needed,");
        eprintln!("  or coordinate a maintenance window before deploying these migrations.");
        std::process::exit(1);
    }
    eprintln!("\u{2713} All {total} migration(s) are safe for a rolling deploy.");
    if any_down_unsafe {
        eprintln!(
            "  Note: some down.sql reverts contain destructive or blocking operations \
             (reported above). These only run on `autumn migrate down`, not on deploy."
        );
    }
}

/// Whether any migration is unsafe for a forward rolling deploy.
///
/// Keys off `up.sql` only — `down.sql` runs on `autumn migrate down`, not during
/// a forward deploy, so its (often inherently destructive) findings do not block
/// the deploy gate.
fn rolling_deploy_blocked(reports: &[MigrationSafetyReport]) -> bool {
    reports.iter().any(|r| safety::has_unsafe_findings(&r.up))
}

/// Read every migration directory in `dir`, classify both `up.sql` and
/// `down.sql`, and return a sorted list of [`MigrationSafetyReport`]s.
///
/// Migration directories that have no `up.sql` are silently skipped.
/// `down.sql` is optional — its findings are empty when the file is absent.
#[cfg(test)]
pub fn check_migrations_in_dir(dir: &Path) -> Result<Vec<MigrationSafetyReport>, String> {
    check_migrations_in_dir_for(autumn_web::config::DatabaseBackend::Postgres, dir)
}

/// Read every migration directory in `dir` and classify both `up.sql` and
/// `down.sql` against a specific database `backend` (issue #1906).
///
/// The Postgres path is unchanged. On `SQLite` the SQL is classified against
/// `SQLite`'s dialect ([`safety::classify_sql_for`]) and the
/// `CREATE INDEX CONCURRENTLY` transaction opt-out check is skipped — `SQLite`
/// has no `CONCURRENTLY`, so the classifier already reports the keyword itself
/// as unsupported.
pub fn check_migrations_in_dir_for(
    backend: autumn_web::config::DatabaseBackend,
    dir: &Path,
) -> Result<Vec<MigrationSafetyReport>, String> {
    let postgres = backend == autumn_web::config::DatabaseBackend::Postgres;
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect();

    // Sort by directory name (which starts with a timestamp) for stable output.
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut results = Vec::new();
    for entry in entries {
        let migration_name = entry.file_name().to_string_lossy().into_owned();
        let up_sql_path = entry.path().join("up.sql");
        if !up_sql_path.exists() {
            continue;
        }
        let up_sql = std::fs::read_to_string(&up_sql_path)
            .map_err(|e| format!("cannot read {}: {e}", up_sql_path.display()))?;
        let mut up_findings = safety::classify_sql_for(backend, &up_sql);
        if postgres {
            check_concurrent_index_transaction_opt_out(&up_sql, &entry.path(), &mut up_findings);
        }

        let down_sql_path = entry.path().join("down.sql");
        let down_findings = if down_sql_path.exists() {
            let down_sql = std::fs::read_to_string(&down_sql_path)
                .map_err(|e| format!("cannot read {}: {e}", down_sql_path.display()))?;
            let mut down_findings = safety::classify_sql_for(backend, &down_sql);
            if postgres {
                check_concurrent_index_transaction_opt_out(
                    &down_sql,
                    &entry.path(),
                    &mut down_findings,
                );
            }
            down_findings
        } else {
            Vec::new()
        };

        results.push(MigrationSafetyReport {
            name: migration_name,
            up: up_findings,
            down: down_findings,
        });
    }

    Ok(results)
}

/// If the SQL uses `CREATE INDEX CONCURRENTLY` but the migration directory does not
/// opt out of Diesel's default transaction wrapping via `metadata.toml`, add a
/// `PotentiallyBlocking` finding.
///
/// `PostgreSQL` rejects `CREATE INDEX CONCURRENTLY` inside a transaction block.
/// Without `run_in_transaction = false` in `metadata.toml`, Diesel wraps the
/// migration in a transaction and the deployment job will fail.
fn check_concurrent_index_transaction_opt_out(
    sql: &str,
    migration_dir: &Path,
    findings: &mut Vec<safety::SafetyFinding>,
) {
    if !safety::contains_concurrent_index(sql) {
        return;
    }

    if !migration_opts_out_of_transaction(migration_dir) {
        findings.push(safety::SafetyFinding {
            operation: "CONCURRENTLY index operation (missing transaction opt-out)".to_owned(),
            risk: safety::RiskLevel::PotentiallyBlocking,
            why: "`PostgreSQL` rejects `CREATE INDEX CONCURRENTLY` and `DROP INDEX CONCURRENTLY` \
                  inside a transaction block. Diesel wraps migrations in a transaction by default, \
                  so this migration will fail at deploy time unless the transaction is disabled.",
            next_action: "Add `run_in_transaction = false` to the migration's `metadata.toml` \
                          (create the file if absent). Example: \
                          echo 'run_in_transaction = false' > migrations/<name>/metadata.toml",
        });
    }
}

/// Whether the migration directory's `metadata.toml` sets
/// `run_in_transaction = false`, opting out of Diesel's default per-migration
/// transaction wrapping (required for `CONCURRENTLY` index operations).
fn migration_opts_out_of_transaction(migration_dir: &Path) -> bool {
    std::fs::read_to_string(migration_dir.join("metadata.toml"))
        .ok()
        .and_then(|content| toml::from_str::<toml::Table>(&content).ok())
        .and_then(|table| {
            table
                .get("run_in_transaction")
                .and_then(toml::Value::as_bool)
        })
        .is_some_and(|v| !v)
}

/// Resolve the primary/write database URL from autumn.toml and environment variables.
///
/// Precedence (highest to lowest):
/// 1. `AUTUMN_DATABASE__PRIMARY_URL` environment variable
/// 2. `AUTUMN_DATABASE__URL` environment variable
/// 3. `DATABASE_URL` environment variable
/// 4. `database.primary_url` from `autumn.toml`
/// 5. `database.url` from `autumn.toml`
fn resolve_database_url() -> String {
    resolve_database_url_with_env(|key| std::env::var(key))
}

fn resolve_database_url_with_env<F>(env_var: F) -> String
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    let config_table = read_autumn_toml_table();
    if let Some(url) = resolve_primary_database_url_from_sources(env_var, config_table.as_ref()) {
        return url;
    }

    eprintln!("\u{2717} No database URL found.");
    eprintln!(
        "  Set database.primary_url (or database.url) in autumn.toml, or set AUTUMN_DATABASE__PRIMARY_URL / AUTUMN_DATABASE__URL / DATABASE_URL."
    );
    std::process::exit(1);
}

pub fn read_autumn_toml_table() -> Option<toml::Table> {
    let config_path = Path::new("autumn.toml");
    if !config_path.exists() {
        return None;
    }
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|contents| toml::from_str::<toml::Table>(&contents).ok())
}

/// The profile actually in effect for CLI config resolution, mirroring the
/// runtime loader's precedence (`autumn_web::config::resolve_profile_input`) so
/// the CLI resolves the same overlay/URLs the running app would: `AUTUMN_ENV`
/// (preferred), then legacy `AUTUMN_PROFILE`, then an explicit `--profile` flag,
/// then release build-mode (`AUTUMN_IS_DEBUG=0` resolves to `prod`), and finally
/// `dev` — the runtime's default — so a local `autumn migrate` applies the same
/// `[profile.dev]` / `autumn-dev.toml` overlay the app would rather than the
/// bare base config.
///
/// `explicit` must be the real `--profile` flag value only — the clap arg no
/// longer auto-fills it from `AUTUMN_PROFILE`, so env vars keep their documented
/// precedence over the flag. Always resolves to a profile (worst case `"dev"`).
pub fn effective_profile(explicit: Option<&str>) -> String {
    let read = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    read("AUTUMN_ENV")
        .or_else(|| read("AUTUMN_PROFILE"))
        .or_else(|| {
            explicit
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            if std::env::var("AUTUMN_IS_DEBUG").ok().as_deref() == Some("0") {
                "prod".to_owned()
            } else {
                "dev".to_owned()
            }
        })
}

/// Whether a resolved profile name is production (`prod`/`production`).
pub fn is_production_profile_name(profile: &str) -> bool {
    let normalized = profile.trim().to_ascii_lowercase();
    normalized == "prod" || normalized == "production"
}

/// Normalize a profile name to its canonical spelling — the SAME mapping the
/// runtime config loader applies (`autumn_web::config`'s `normalize_profile_name`):
/// `production`/`prod` (any case) → `prod`, `development`/`dev` (any case) → `dev`,
/// custom names preserved (trimmed, case kept). Reused so an offsite
/// `.env.<profile>` / `[profile.<p>]` selection matches the app's resolved profile
/// even when the operator writes `--profile development` / `AUTUMN_ENV=PROD`
/// (issue #1619 P2 #20).
#[must_use]
pub fn canonical_profile(profile: &str) -> String {
    let trimmed = profile.trim();
    if trimmed.eq_ignore_ascii_case("production") || trimmed.eq_ignore_ascii_case("prod") {
        "prod".to_owned()
    } else if trimmed.eq_ignore_ascii_case("development") || trimmed.eq_ignore_ascii_case("dev") {
        "dev".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Profile name spellings to probe for inline `[profile.<name>]` sections,
/// mirroring `autumn_web::config::profile_lookup_names`'s alias handling so
/// `prod`/`production` and `dev`/`development` are interchangeable. Matching is
/// case-insensitive; custom profile names are used verbatim.
fn profile_lookup_names(profile: &str) -> Vec<String> {
    match profile.trim().to_ascii_lowercase().as_str() {
        "prod" | "production" => vec!["production".to_owned(), "prod".to_owned()],
        "dev" | "development" => vec!["development".to_owned(), "dev".to_owned()],
        _ => vec![profile.trim().to_owned()],
    }
}

/// Overlay-FILE lookup names, **selected-spelling first** — mirrors the runtime
/// `autumn_web::config::profile_override_file_lookup_names`. Only one overlay
/// file is loaded (the first that exists), so when both `autumn-prod.toml` and
/// `autumn-production.toml` are present the operator's selected `--profile`
/// spelling wins, resolving the same file the running app would.
fn profile_file_lookup_names(profile: &str) -> Vec<String> {
    let trimmed = profile.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "prod" | "production" => {
            if trimmed.eq_ignore_ascii_case("production") {
                vec!["production".to_owned(), "prod".to_owned()]
            } else {
                vec!["prod".to_owned(), "production".to_owned()]
            }
        }
        "dev" | "development" => {
            if trimmed.eq_ignore_ascii_case("development") {
                vec!["development".to_owned(), "dev".to_owned()]
            } else {
                vec!["dev".to_owned(), "development".to_owned()]
            }
        }
        _ => vec![trimmed.to_owned()],
    }
}

/// Read `autumn.toml`, layering profile overrides the same way the runtime
/// loader does (see `autumn/src/config.rs`):
///
///   `autumn.toml` ← `[profile.{name}]` (inline) ← `autumn-{profile}.toml`
///
/// so `autumn migrate --profile prod` resolves the same control + shard URLs
/// the running app would under that profile — whether the deployment keeps
/// production URLs in an inline `[profile.prod.database]` section or a separate
/// `autumn-prod.toml` file. With no profile (or no overrides), the base table
/// is returned unchanged.
pub fn read_autumn_toml_table_with_profile(profile: Option<&str>) -> Option<toml::Table> {
    read_autumn_toml_table_with_profile_in(Path::new("."), profile)
}

/// Like [`read_autumn_toml_table_with_profile`], but resolves `autumn.toml` from
/// the SAME base directory the full `AutumnConfig::load` uses
/// (`find_config_file_named`): `AUTUMN_MANIFEST_DIR` when it holds an
/// `autumn.toml`, otherwise the process CWD. Installed/daemon launches (config in
/// the manifest/config dir, a different CWD) can therefore resolve offsite
/// settings that a bare CWD read would miss — matching where the full loader and
/// the profile-aware dotenv overlay already look (issue #1619 P2 #15).
pub fn read_autumn_toml_table_with_profile_from_config_dir(
    profile: Option<&str>,
) -> Option<toml::Table> {
    read_autumn_toml_table_with_profile_in(&autumn_toml_base_dir(), profile)
}

/// The directory to read `autumn.toml` from, mirroring the config loader's
/// `find_config_file_named`: `AUTUMN_MANIFEST_DIR` when that directory actually
/// contains an `autumn.toml`, otherwise `.` (the process CWD).
fn autumn_toml_base_dir() -> std::path::PathBuf {
    autumn_toml_base_dir_from(std::env::var("AUTUMN_MANIFEST_DIR").ok().as_deref())
}

/// Directory-parameterized core of [`autumn_toml_base_dir`], separated so the
/// `AUTUMN_MANIFEST_DIR` resolution is unit-testable without mutating process env.
fn autumn_toml_base_dir_from(manifest_dir: Option<&str>) -> std::path::PathBuf {
    if let Some(dir) = manifest_dir.map(str::trim).filter(|d| !d.is_empty()) {
        let dir = Path::new(dir);
        if dir.join("autumn.toml").exists() {
            return dir.to_path_buf();
        }
    }
    std::path::PathBuf::from(".")
}

/// Directory-parameterized core of [`read_autumn_toml_table_with_profile`],
/// separated so the overlay-merge behavior is unit-testable without mutating
/// the process-global current directory.
pub fn read_autumn_toml_table_with_profile_in(
    dir: &Path,
    profile: Option<&str>,
) -> Option<toml::Table> {
    // An absent file is a no-op (fall back to the next layer), but a file that
    // EXISTS yet can't be read or parsed is a hard error — silently ignoring it
    // would resolve different URLs than the running app (which the runtime
    // loader rejects), risking migrations/row-moves against the wrong database.
    read_autumn_toml_table_with_profile_in_using(dir, profile, |path| {
        if !path.exists() {
            return None;
        }
        let contents = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to read {}: {e}", path.display());
            std::process::exit(1);
        });
        match toml::from_str::<toml::Table>(&contents) {
            Ok(table) => Some(table),
            Err(e) => {
                eprintln!("\u{2717} Failed to parse {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    })
}

/// Reads an existing `autumn.toml`, tolerating a missing, unreadable or
/// malformed file by returning `None` for it.
///
/// For the OFFLINE preflights only (issue #1906 review): they analyze SQL and
/// must never abort on local config the running app is not being pointed at.
/// Every path that resolves a real database URL uses the hard-error reader in
/// [`read_autumn_toml_table_with_profile_in`] instead.
pub fn read_optional_toml_table(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
}

/// [`read_autumn_toml_table_with_profile_in`] with a caller-supplied file
/// reader, so the offline preflights can substitute a tolerant one.
pub fn read_autumn_toml_table_with_profile_in_using(
    dir: &Path,
    profile: Option<&str>,
    read_table: impl Fn(&Path) -> Option<toml::Table>,
) -> Option<toml::Table> {
    let base = read_table(&dir.join("autumn.toml"));
    let Some(profile) = profile.filter(|p| !p.is_empty()) else {
        return base;
    };

    // Probe overlay files across canonical alias spellings (e.g. the operator
    // sets `AUTUMN_ENV=production` but the file is the common `autumn-prod.toml`)
    // and load the first that exists. File lookup prefers the *selected* spelling
    // (mirroring the runtime `profile_override_file_lookup_names`), so a repo with
    // both `autumn-prod.toml` and `autumn-production.toml` resolves the file the
    // app would under the same profile.
    let lookup_names = profile_lookup_names(profile);
    let overlay = profile_file_lookup_names(profile)
        .iter()
        .find_map(|name| read_table(&dir.join(format!("autumn-{name}.toml"))));
    if base.is_none() && overlay.is_none() {
        return None;
    }
    let mut merged = base.unwrap_or_default();

    // Layer the inline `[profile.<name>]` section(s) from autumn.toml on top of
    // the base, matching the runtime loader's precedence. Co-located profile
    // config (e.g. `[profile.prod.database]`) must be honored, not just a
    // separate `autumn-<profile>.toml` file.
    for name in &lookup_names {
        if let Some(inline) = merged
            .get("profile")
            .and_then(toml::Value::as_table)
            .and_then(|profiles| profiles.get(name))
            .and_then(toml::Value::as_table)
            .cloned()
        {
            deep_merge_toml(&mut merged, inline);
        }
    }

    // Finally, the legacy `autumn-<profile>.toml` overlay wins over both.
    if let Some(overlay) = overlay {
        deep_merge_toml(&mut merged, overlay);
    }

    Some(merged)
}

/// Deep-merge `overlay` into `base`: nested tables are merged recursively,
/// every other value (scalars, arrays) in `overlay` replaces the matching key
/// in `base`. Keys present only in `base` are preserved. This matches the
/// framework's profile-overlay merge semantics (`autumn/src/config.rs`).
fn deep_merge_toml(base: &mut toml::Table, overlay: toml::Table) {
    for (key, overlay_val) in overlay {
        match (base.get_mut(&key), overlay_val) {
            (Some(toml::Value::Table(base_child)), toml::Value::Table(overlay_child)) => {
                deep_merge_toml(base_child, overlay_child);
            }
            (_, overlay_val) => {
                base.insert(key, overlay_val);
            }
        }
    }
}

/// Resolve the primary/write database URL for the given profile using the
/// exact same layering as `autumn migrate` (defaults → `autumn.toml` →
/// `autumn-{profile}.toml` → `AUTUMN_*` / `DATABASE_URL` / `primary_url`).
///
/// Returns `None` when no URL can be resolved, leaving the caller to decide how
/// to report the failure (the `autumn db` commands surface their own message).
pub fn resolve_primary_url(profile: Option<&str>) -> Option<String> {
    use autumn_web::config::Env as _;

    let effective = effective_profile(profile);
    let config_table = read_autumn_toml_table_with_profile(Some(&effective));
    // Overlay a project-root `.env` UNDER the real environment so a
    // `.env`-provided AUTUMN_DATABASE__* / DATABASE_URL is honored for the
    // `autumn db` lifecycle commands (create/drop/reset) and `db pull`, exactly
    // as `autumn migrate` resolves its target URLs via `resolve_targets`. A real
    // env var of the same key still wins; a malformed `.env` fails loudly here
    // (with a `path:line` location), where a DB URL is actually needed.
    let env = os_env_with_dotenv_or_exit();
    resolve_primary_database_url_from_sources(|key| env.var(key), config_table.as_ref())
}

pub fn resolve_primary_database_url_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> Option<String>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    for var in [
        "AUTUMN_DATABASE__PRIMARY_URL",
        "AUTUMN_DATABASE__URL",
        "DATABASE_URL",
    ] {
        if let Ok(url) = env_var(var)
            && !url.is_empty()
        {
            return Some(url);
        }
    }

    let database = table?.get("database").and_then(toml::Value::as_table)?;
    for key in ["primary_url", "url"] {
        if let Some(url) = database
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|url| !url.is_empty())
        {
            return Some(url.to_owned());
        }
    }
    None
}

/// Resolve `(name, primary_url)` for every `[[database.shards]]` entry,
/// in declaration order.
///
/// Mirrors the framework's positional environment override scheme:
/// `AUTUMN_DATABASE__SHARDS__{i}__NAME` / `__PRIMARY_URL` override entry
/// `i` of the TOML declaration (or append a new entry when both are set
/// for the next free index); probing stops at the first absent index.
pub fn resolve_shard_database_urls_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> Vec<(String, String)>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    // Thin wrapper over the fallible core: preserve the historical
    // print-and-exit behavior for command paths (`autumn migrate`, `db backup`,
    // …) that expect a `Vec` and abort on a malformed shard. Callers that must
    // NOT exit the process (e.g. `autumn doctor`, which needs to emit a JSON
    // `CheckResult` instead) call `try_resolve_shard_database_urls_from_sources`
    // directly and map the `Err` to a failing check.
    try_resolve_shard_database_urls_from_sources(env_var, table).unwrap_or_else(|msg| {
        eprintln!("\u{2717} {msg}");
        std::process::exit(1);
    })
}

/// Fallible core of [`resolve_shard_database_urls_from_sources`].
///
/// Resolves `(name, primary_url)` for every `[[database.shards]]` entry (TOML +
/// `AUTUMN_DATABASE__SHARDS__*` env overrides), returning `Err(message)` on a
/// malformed shard (non-table entry, missing `name`/`primary_url`, or a
/// duplicate shard name) instead of printing and exiting the process. The
/// message carries no leading `✗` marker so callers can render it however they
/// like; the exiting wrapper prepends the marker to keep its stderr output
/// byte-identical.
pub fn try_resolve_shard_database_urls_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> Result<Vec<(String, String)>, String>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    const MAX_ENV_SHARDS: usize = 64;

    let mut shards: Vec<(String, String)> = Vec::new();
    if let Some(entries) = table
        .and_then(|table| table.get("database"))
        .and_then(toml::Value::as_table)
        .and_then(|database| database.get("shards"))
        .and_then(toml::Value::as_array)
    {
        // Fail on a malformed `[[database.shards]]` entry rather than silently
        // dropping it: the runtime config loader rejects the same config, so
        // skipping a misspelled shard here would let `autumn migrate` "succeed"
        // while leaving that shard unmigrated.
        for (i, entry) in entries.iter().enumerate() {
            let shard = entry
                .as_table()
                .ok_or_else(|| format!("[[database.shards]] entry {i} is not a table."))?;
            let name = shard
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    format!("[[database.shards]] entry {i} is missing a string `name`.")
                })?;
            let url = shard
                .get("primary_url")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "[[database.shards]] entry {i} ({name:?}) is missing a string \
                         `primary_url`."
                    )
                })?;
            shards.push((name.to_owned(), url.to_owned()));
        }
    }

    for i in 0..MAX_ENV_SHARDS {
        let name_var = format!("AUTUMN_DATABASE__SHARDS__{i}__NAME");
        let url_var = format!("AUTUMN_DATABASE__SHARDS__{i}__PRIMARY_URL");
        if i >= shards.len() {
            let (Ok(name), Ok(url)) = (env_var(&name_var), env_var(&url_var)) else {
                break;
            };
            shards.push((name, url));
            continue;
        }
        if let Ok(name) = env_var(&name_var) {
            shards[i].0 = name;
        }
        if let Ok(url) = env_var(&url_var) {
            shards[i].1 = url;
        }
    }

    // Reject duplicate shard names (the runtime config validator does too), so
    // `--shard foo` / `move-slot --from foo` can't silently resolve only the
    // first of two entries and target a different shard set than the app.
    let mut seen = std::collections::HashSet::new();
    for (name, _) in &shards {
        if !seen.insert(name.as_str()) {
            return Err(format!(
                "Duplicate shard name {name:?} in [[database.shards]] / \
                 AUTUMN_DATABASE__SHARDS__* — shard names must be unique."
            ));
        }
    }

    Ok(shards)
}

/// Resolve `database.startup_wait_secs` from env and config, mirroring the
/// same source precedence as other database config knobs:
///
/// 1. `AUTUMN_DATABASE__STARTUP_WAIT_SECS` env var (highest)
/// 2. `database.startup_wait_secs` in the merged `autumn.toml` table
/// 3. `0` (default, fail-fast — no wait)
pub fn resolve_startup_wait_secs_from_sources<F>(env_var: F, table: Option<&toml::Table>) -> u64
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    if let Ok(val) = env_var("AUTUMN_DATABASE__STARTUP_WAIT_SECS") {
        match val.trim().parse::<u64>() {
            Ok(n) => return n,
            Err(_) => {
                eprintln!(
                    "  Warning: AUTUMN_DATABASE__STARTUP_WAIT_SECS={:?} is not a valid \
                     non-negative integer; falling back to config file / default (0).",
                    val.trim()
                );
            }
        }
    }

    table
        .and_then(|t| t.get("database"))
        .and_then(toml::Value::as_table)
        .and_then(|db| db.get("startup_wait_secs"))
        .and_then(toml::Value::as_integer)
        .map_or(0, |n| {
            u64::try_from(n).unwrap_or_else(|_| {
                eprintln!(
                    "  Warning: `database.startup_wait_secs = {n}` in autumn.toml is not a \
                     valid non-negative integer; falling back to 0 (fail-fast)."
                );
                0
            })
        })
}

/// Resolve the effective startup wait: the `--wait` CLI flag (if given) wins;
/// otherwise fall back to the merged config table (env > toml > 0).
///
/// `config_table` should be the already-merged table returned by
/// `resolve_targets` so this function does not need a second filesystem read.
fn resolve_startup_wait(
    flag: Option<u64>,
    config_table: Option<&toml::Table>,
) -> std::time::Duration {
    let secs = flag.unwrap_or_else(|| {
        resolve_startup_wait_secs_from_sources(|key| std::env::var(key), config_table)
    });
    std::time::Duration::from_secs(secs)
}

/// Resolve the migrations directory (default: `./migrations/`).
fn resolve_migrations_dir() -> String {
    let dir = Path::new(DEFAULT_MIGRATIONS_DIR);
    if !dir.exists() {
        eprintln!("\u{2717} Migrations directory not found: {DEFAULT_MIGRATIONS_DIR}/");
        eprintln!("  Create it with `diesel setup` or `diesel migration generate <name>`.");
        std::process::exit(1);
    }
    DEFAULT_MIGRATIONS_DIR.to_string()
}

/// Check that the `diesel` CLI is installed and available on PATH.
fn check_diesel_cli() {
    match Command::new("diesel").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            eprintln!("  Using {}", version.trim());
        }
        _ => {
            eprintln!("\u{2717} diesel CLI not found on PATH.");
            eprintln!(
                "  Install it with: cargo install diesel_cli --no-default-features --features postgres"
            );
            std::process::exit(1);
        }
    }
}

fn run_framework_migrations(database_url: &str) -> bool {
    eprintln!("  Running pending Autumn framework migrations...\n");

    match run_framework_migrations_inner(database_url, autumn_web::migrate::run_pending) {
        Ok(result) if result.applied.is_empty() => {
            eprintln!("\n\u{2713} Framework migrations are up to date.");
            true
        }
        Ok(result) => {
            for migration in &result.applied {
                eprintln!("  Applied {migration}");
            }
            eprintln!("\n\u{2713} Framework migrations applied successfully.");
            true
        }
        Err(e) => {
            eprintln!("\n\u{2717} Framework migration failed: {e}");
            false
        }
    }
}

fn run_framework_migrations_inner<F>(
    database_url: &str,
    run_pending: F,
) -> Result<MigrationResult, MigrationError>
where
    F: FnOnce(&str, EmbeddedMigrations) -> Result<MigrationResult, MigrationError>,
{
    run_pending(database_url, FRAMEWORK_MIGRATIONS)
}

/// Apply only the shard-required framework migrations to a shard target.
///
/// Shards need version-history and commit-hook queue tables but do **not**
/// host the full control-plane schema (API tokens, sessions, job queues, …).
/// In production this delegates to
/// [`autumn_web::migrate::run_pending_shard_framework_migrations`]; the inner
/// helper takes a closure so the dispatch can be tested without a live database.
fn run_shard_framework_migrations(database_url: &str) -> bool {
    eprintln!("  Running pending Autumn shard framework migrations...\n");

    match run_shard_framework_migrations_inner(
        database_url,
        autumn_web::migrate::run_pending_shard_framework_migrations,
    ) {
        Ok(result) if result.applied.is_empty() => {
            eprintln!("\n\u{2713} Shard framework migrations are up to date.");
            true
        }
        Ok(result) => {
            for migration in &result.applied {
                eprintln!("  Applied {migration}");
            }
            eprintln!("\n\u{2713} Shard framework migrations applied successfully.");
            true
        }
        Err(e) => {
            eprintln!("\n\u{2717} Shard framework migration failed: {e}");
            false
        }
    }
}

fn run_shard_framework_migrations_inner<F>(
    database_url: &str,
    run_shard: F,
) -> Result<MigrationResult, MigrationError>
where
    F: FnOnce(&str) -> Result<MigrationResult, MigrationError>,
{
    run_shard(database_url)
}

/// True iff the [effective profile](effective_profile) resolves to
/// `prod`/`production`.
///
/// Takes the same explicit `--profile` value as the rest of CLI resolution and
/// runs it through [`effective_profile`], so the rollback guard agrees with the
/// databases the command will actually target: `AUTUMN_ENV` (preferred), then
/// legacy `AUTUMN_PROFILE`, then the explicit flag, then the release build-mode
/// signal (`AUTUMN_IS_DEBUG=0`). Pass `None` to test the env/build-mode signals
/// alone.
fn is_production_profile(explicit: Option<&str>) -> bool {
    // Delegate to the shared effective-profile resolution (env precedence +
    // explicit `--profile` + build-mode signal) so the rollback guard and config
    // resolution agree on which profile (and thus which databases) is targeted.
    is_production_profile_name(&effective_profile(explicit))
}

/// Whether an applied user migration has a usable `down.sql` on disk.
///
/// Returns `false` when the migration is not present locally (`dir` is `None`)
/// or its `down.sql` is missing/empty/comment-only.
fn has_revertable_down_sql(m: &AppliedUserMigration) -> bool {
    m.dir.as_ref().is_some_and(|d| {
        std::fs::read_to_string(d.join("down.sql"))
            .is_ok_and(|sql| safety::has_executable_sql(&sql))
    })
}

/// Whether reverting `m` would fail mid-rollback because its `down.sql` runs a
/// `CREATE`/`DROP INDEX CONCURRENTLY` but the migration does not opt out of
/// Diesel's default per-migration transaction via `metadata.toml`.
///
/// `PostgreSQL` rejects `CONCURRENTLY` index operations inside a transaction
/// block, so such a revert fails at execution time. In a multi-step rollback
/// that failure lands *after* earlier (newer) migrations have already been
/// reverted and committed, leaving a partial rollback — so it is surfaced in
/// preflight, before anything is mutated. Migrations missing locally (`dir`
/// is `None`) are reported by the separate `has_revertable_down_sql` preflight.
fn down_sql_concurrent_without_opt_out(m: &AppliedUserMigration) -> bool {
    m.dir.as_ref().is_some_and(|d| {
        std::fs::read_to_string(d.join("down.sql"))
            .is_ok_and(|sql| safety::contains_concurrent_index(&sql))
            && !migration_opts_out_of_transaction(d)
    })
}

/// Build the newest-first list of user-migration versions to revert.
///
/// With `--to VERSION`, every applied user migration strictly newer than
/// `VERSION` is reverted (exiting non-zero if `VERSION` is not a currently
/// applied *user* migration). `VERSION` must be a user migration version —
/// framework migrations are forward-only and cannot serve as a boundary.
/// Otherwise the most recently applied `--steps N` (default 1) versions are
/// reverted.
///
/// `applied` is ascending by version, so the newest-first plan is its reverse.
fn build_rollback_plan(args: &DownArgs, applied: &[AppliedUserMigration]) -> Vec<String> {
    let Some(target_version) = args.to.as_deref() else {
        let n = args.steps.unwrap_or(1);
        return applied
            .iter()
            .rev()
            .take(n)
            .map(|m| m.version.clone())
            .collect();
    };

    if !applied.iter().any(|m| m.version == target_version) {
        eprintln!("\u{2717} Version {target_version} is not a currently applied user migration.");
        eprintln!("  Check `autumn migrate status` to see the applied user migrations.");
        std::process::exit(1);
    }
    applied
        .iter()
        .filter(|m| m.version.as_str() > target_version)
        .rev()
        .map(|m| m.version.clone())
        .collect()
}

/// Run `autumn migrate down`.
fn run_down(
    args: &DownArgs,
    with_maintenance: bool,
    target: &MigrateTarget,
    profile: Option<&str>,
) {
    // 1. Production guard. Derive prod-ness from the SAME effective profile the
    //    rollback will target (env precedence + build-mode + explicit flag), so
    //    e.g. an `AUTUMN_ENV=prod` overlay still trips the guard even when an
    //    explicit `--profile dev` is passed, and the guard can't be bypassed by
    //    resolving prod URLs without `--yes-i-mean-prod`.
    let is_prod = is_production_profile(profile);
    if is_prod && !args.yes_i_mean_prod {
        eprintln!("\u{2717} Production profile detected.");
        eprintln!("  Rolling back migrations in production requires explicit confirmation.");
        eprintln!("  Re-run with --yes-i-mean-prod to proceed.");
        std::process::exit(1);
    }

    // 2. Resolve target databases (control + shards / a single shard /
    //    control-only) and the migrations dir. `down` honors --shard /
    //    --control-only exactly like `migrate run`. Overlay `.env` under the
    //    real environment only now that we are about to resolve a DB URL (a
    //    malformed `.env` fails loudly here).
    let env = os_env_with_dotenv_or_exit();
    let (targets, _) = resolve_targets(target, profile, &env);
    let migrations_dir = resolve_migrations_dir();
    let dir = Path::new(&migrations_dir);

    // Preflight EVERY target before mutating any of them: build and validate
    // each target's rollback plan (down.sql present, CONCURRENTLY opt-out) up
    // front, so a divergent shard whose plan can't be reverted fails before the
    // control DB (or an earlier shard) has already been rolled back — which
    // would leave control and shards at different schema versions. Each target
    // re-validates under its own lock in run_down_target as well.
    let plans: Vec<(String, Vec<String>)> = targets
        .iter()
        .map(|(label, url)| {
            (
                label.clone(),
                preflight_rollback_target(args, url, dir, label),
            )
        })
        .collect();

    // A multi-target `down` must revert the SAME version list on every target.
    // If they diverge (e.g. a shard lagged behind control after a partial
    // migrate), refuse rather than roll back different versions per target.
    // Scoping with --shard / --control-only resolves to a single target, so this
    // never blocks an intentional single-target rollback.
    reject_divergent_rollback_plans(&plans);

    // Targets are reverted in order; each plans + executes under its own
    // advisory lock. To roll a single database back in isolation, scope the
    // command with --shard / --control-only.
    //
    // Maintenance mode is a global flag, so enable it at most once (the first
    // time any target actually has work) and disable it after all targets
    // complete. Tracked across targets so an empty plan never toggles it.
    let mut maintenance_enabled = false;
    let mut total_reverted = 0usize;
    let multi = targets.len() > 1;

    for ((label, url), (_, preflighted_plan)) in targets.iter().zip(plans.iter()) {
        if multi {
            eprintln!("\u{2500}\u{2500} Rolling back {label} \u{2500}\u{2500}");
        }
        match run_down_target(
            args,
            url,
            dir,
            with_maintenance,
            &mut maintenance_enabled,
            preflighted_plan,
        ) {
            Ok(n) => {
                total_reverted += n;
                if multi {
                    eprintln!();
                }
            }
            Err(e) => {
                eprintln!("\n\u{2717} Rollback failed for {label}: {e}");
                if maintenance_enabled {
                    eprintln!(
                        "  \u{26A0}\u{FE0F}  Maintenance mode left ON for safety. Investigate, then run \
                         `autumn maintenance off` to re-open traffic."
                    );
                }
                std::process::exit(1);
            }
        }
    }

    if total_reverted > 0 {
        eprintln!("\n\u{2713} {total_reverted} migration(s) rolled back.");
    }
    if maintenance_enabled {
        disable_maintenance_after_migrate();
    }
}

/// Validate that every planned rollback migration can be reverted: it must have
/// an executable `down.sql`, and a `CONCURRENTLY` index revert must opt out of
/// the per-migration transaction. Prints the offending migrations and exits the
/// process on failure (before any schema is mutated).
fn check_rollback_plan_revertable(applied: &[AppliedUserMigration], plan: &[String]) {
    let missing: Vec<&str> = plan
        .iter()
        .filter_map(|version| {
            let m = applied.iter().find(|m| &m.version == version)?;
            (!has_revertable_down_sql(m)).then_some(m.name.as_str())
        })
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "\u{2717} The following migration(s) have no executable down.sql and cannot be reverted:"
        );
        for name in &missing {
            eprintln!("    \u{2022} {name}");
        }
        eprintln!("  Add a down.sql to each migration before running `autumn migrate down`.");
        std::process::exit(1);
    }

    let needs_opt_out: Vec<&str> = plan
        .iter()
        .filter_map(|version| {
            let m = applied.iter().find(|m| &m.version == version)?;
            down_sql_concurrent_without_opt_out(m).then_some(m.name.as_str())
        })
        .collect();
    if !needs_opt_out.is_empty() {
        eprintln!(
            "\u{2717} The following migration(s) revert a `CONCURRENTLY` index but do not set \
             `run_in_transaction = false` and cannot be reverted safely:"
        );
        for name in &needs_opt_out {
            eprintln!("    \u{2022} {name}");
        }
        eprintln!(
            "  `PostgreSQL` rejects `CONCURRENTLY` index operations inside a transaction, so \
             the revert would fail partway through. Add `run_in_transaction = false` to each \
             migration's metadata.toml before running `autumn migrate down`."
        );
        std::process::exit(1);
    }
}

/// Up-front (read-only, no lock) preflight of one target's rollback plan, run
/// for every target before any is mutated. Exits on a plan that cannot be
/// reverted, naming the target. Returns the planned version list so the caller
/// can compare plans across targets before mutating any of them.
fn preflight_rollback_target(
    args: &DownArgs,
    database_url: &str,
    dir: &Path,
    label: &str,
) -> Vec<String> {
    let applied = applied_user_migrations_for_target(database_url, dir).unwrap_or_else(|e| {
        eprintln!("\u{2717} Could not preflight rollback for {label}: {e}");
        std::process::exit(1);
    });
    let plan = build_rollback_plan(args, &applied);
    check_rollback_plan_revertable(&applied, &plan);
    plan
}

/// Whether the targets' rollback plans diverge — i.e. not every target would
/// revert the exact same ordered version list. Plans are computed from each
/// target's own applied history, so divergence means a `down` would revert
/// different versions per target.
fn rollback_plans_diverge(plans: &[(String, Vec<String>)]) -> bool {
    match plans.first() {
        Some((_, first)) => plans.iter().any(|(_, plan)| plan != first),
        None => false,
    }
}

/// Refuse a multi-target rollback when the targets' plans diverge.
///
/// `down` computes each target's plan from that target's own applied history, so
/// if a previous multi-target `migrate` failed partway (leaving, say, control at
/// `A+B` but a shard at only `A`), a `--steps`/`--to` rollback would revert
/// different versions per target — e.g. `B` on control but `A` on a lagging shard
/// — leaving the fleet at mismatched schemas or dropping a still-needed
/// migration's objects from a shard. Require the operator to reconcile and scope
/// the command (`--shard`/`--control-only`) rather than diverge further.
fn reject_divergent_rollback_plans(plans: &[(String, Vec<String>)]) {
    if !rollback_plans_diverge(plans) {
        return;
    }
    eprintln!("\u{2717} Refusing to roll back: target rollback plans diverge.");
    eprintln!(
        "  Each target's plan is computed from its own applied history, and they differ \u{2014}\n  \
         likely because a previous multi-target migrate failed partway. Rolling back now\n  \
         would leave targets at different schema versions (or drop a migration's objects\n  \
         from a lagging target while keeping them on others)."
    );
    for (label, plan) in plans {
        if plan.is_empty() {
            eprintln!("    \u{2022} {label}: nothing to roll back");
        } else {
            eprintln!("    \u{2022} {label}: {}", plan.join(", "));
        }
    }
    eprintln!(
        "  Reconcile the targets first (`autumn migrate status` per target), then re-run\n  \
         scoped with --shard <name> or --control-only to roll back one target at a time."
    );
    std::process::exit(1);
}

/// Roll back the planned user migrations on a single target database. Returns the
/// number of migrations reverted.
///
/// Postgres runs under the migration advisory lock; a `sqlite://` target uses the
/// unlocked harness (`SQLite` is single-writer — no cross-process serialization is
/// needed, issue #2058). Listing applied migrations, building the plan, and
/// preflighting `down.sql` all happen inside the `plan` closure (on the same
/// connection) so the plan cannot go stale between read and execute (e.g. two
/// concurrent Postgres `down` runs).
/// `maintenance_enabled` is shared across targets so maintenance mode is
/// enabled at most once, the first time any target has work to do.
///
/// `preflighted_plan` is the version list computed for this target during the
/// up-front (unlocked) preflight, which the cross-target divergence check
/// verified was uniform across all targets. Under the lock we recompute the plan
/// and compare: if a concurrent migrate/down changed this target's applied
/// history since preflight, the plans differ and we abort *before mutating this
/// target* — otherwise a multi-target rollback could revert a different version
/// list here than on the targets already rolled back, diverging the fleet.
fn run_down_target(
    args: &DownArgs,
    database_url: &str,
    dir: &Path,
    with_maintenance: bool,
    maintenance_enabled: &mut bool,
    preflighted_plan: &[String],
) -> Result<usize, autumn_web::migrate::MigrationError> {
    use autumn_web::migrate::MigrationError;

    // The plan/execute closures are backend-agnostic; only the revert engine
    // differs — Postgres runs under the advisory lock, SQLite through the
    // unlocked harness (issue #2058, single-writer #1999). The plan is still
    // rebuilt and re-validated under the same connection in both.
    let plan = |applied: &[AppliedUserMigration]| -> Result<Vec<String>, MigrationError> {
        let plan = build_rollback_plan(args, applied);

        // Recheck under the lock that this target's plan still matches what
        // preflight saw (and verified uniform across targets). A concurrent
        // migrate/down may have changed the applied history in the window
        // between preflight and acquiring this lock; rolling back a now-
        // different version list would diverge this target from the ones
        // already reverted.
        if plan != preflighted_plan {
            return Err(MigrationError::Migration(format!(
                "rollback plan for this target changed under the lock since preflight \
                     (a concurrent migrate/down likely altered its applied history). \
                     Preflighted [{}] but now [{}]. Aborting before mutating this target; \
                     re-run `autumn migrate down` once migrations are quiesced.",
                preflighted_plan.join(", "),
                plan.join(", "),
            )));
        }

        if plan.is_empty() {
            eprintln!("  \u{2713} Nothing to roll back.");
            return Ok(plan);
        }

        // Re-validate under the lock (defense-in-depth against the plan
        // going stale between the up-front preflight and here).
        check_rollback_plan_revertable(applied, &plan);

        // Preflight passed: enable maintenance mode (if requested and not
        // already enabled for an earlier target) before we mutate schema,
        // then stream the rollback.
        if with_maintenance && !*maintenance_enabled {
            enable_maintenance_for_migrate();
            *maintenance_enabled = true;
        }
        eprintln!("  Rolling back {} migration(s)...\n", plan.len());
        Ok(plan)
    };
    let on_reverted = |r: &autumn_web::migrate::RevertedMigration| {
        eprintln!(
            "  \u{2713} Rolled back {}  ({}ms)",
            r.name,
            r.duration.as_millis()
        );
    };

    if is_sqlite_target(database_url) {
        revert_user_migrations_sqlite_cli(database_url, dir, plan, on_reverted)
    } else {
        autumn_web::migrate::revert_user_migrations_locked(
            database_url,
            dir,
            None,
            plan,
            on_reverted,
        )
    }
}

/// Revert user migrations on a `SQLite` target through the unlocked harness
/// (issue #2058). Real only under the `sqlite` feature; the default build returns
/// the [`SQLITE_FEATURE_MSG`] seam.
#[cfg(feature = "sqlite")]
fn revert_user_migrations_sqlite_cli<P, F>(
    database_url: &str,
    migrations_dir: &Path,
    plan: P,
    on_reverted: F,
) -> Result<usize, autumn_web::migrate::MigrationError>
where
    P: FnOnce(&[AppliedUserMigration]) -> Result<Vec<String>, autumn_web::migrate::MigrationError>,
    F: FnMut(&autumn_web::migrate::RevertedMigration),
{
    autumn_web::migrate::revert_user_migrations_sqlite(
        database_url,
        migrations_dir,
        plan,
        on_reverted,
    )
}

#[cfg(not(feature = "sqlite"))]
fn revert_user_migrations_sqlite_cli<P, F>(
    _database_url: &str,
    _migrations_dir: &Path,
    _plan: P,
    _on_reverted: F,
) -> Result<usize, autumn_web::migrate::MigrationError>
where
    P: FnOnce(&[AppliedUserMigration]) -> Result<Vec<String>, autumn_web::migrate::MigrationError>,
    F: FnMut(&autumn_web::migrate::RevertedMigration),
{
    Err(autumn_web::migrate::MigrationError::Migration(
        SQLITE_FEATURE_MSG.to_owned(),
    ))
}

/// Show rollback availability for all applied user migrations.
fn show_rollback_availability(database_url: &str, migrations_dir: &str) {
    use autumn_web::migrate::applied_user_migrations;

    let dir = Path::new(migrations_dir);
    let applied = match applied_user_migrations(database_url, dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  \u{26A0}\u{FE0F}  Could not check rollback availability: {e}");
            return;
        }
    };

    eprintln!("  Rollback availability (user migrations):\n");

    if applied.is_empty() {
        eprintln!("  No applied user migrations.");
        return;
    }

    for m in &applied {
        if m.dir.is_none() {
            eprintln!(
                "  \u{2717} {}  (applied but missing locally — not revertable)",
                m.name
            );
        } else if !has_revertable_down_sql(m) {
            eprintln!(
                "  \u{2717} {}  (no executable down.sql — not revertable)",
                m.name
            );
        } else if down_sql_concurrent_without_opt_out(m) {
            // `migrate down` refuses these (the CONCURRENTLY revert would fail
            // inside Diesel's transaction), so don't advertise them as available.
            eprintln!(
                "  \u{2717} {}  (down.sql uses CONCURRENTLY without `run_in_transaction = false` — \
                 not revertable as-is)",
                m.name
            );
        } else {
            eprintln!("  \u{2713} {}", m.name);
        }
    }
    eprintln!();
}

/// Show migration status via `diesel migration pending`.
fn show_status(database_url: &str, migrations_dir: &str) {
    eprintln!("  Checking migration status...\n");
    show_diesel_migration_status(database_url, Path::new(migrations_dir));
    show_checksum_status(database_url, Path::new(migrations_dir));
}

/// Print each applied migration's content-hash state (issue #1203).
///
/// * `ok` — the on-disk `up.sql` still hashes to the recorded value.
/// * `changed` — an applied migration's content was edited after being applied;
///   running `autumn migrate` would refuse to continue with the same message.
/// * `missing` — an applied migration that had a recorded checksum but whose
///   `up.sql` is now gone (deleted or renamed after being applied); this is
///   drift and `autumn migrate` would refuse to continue.
/// * `unrecorded` — legacy applied migration with no recorded checksum; run
///   `autumn migrate baseline` to record the current hash.
fn show_checksum_status(database_url: &str, migrations_dir: &Path) {
    eprintln!("\n  Checking migration checksum status...\n");
    match autumn_web::migrate::checksum_status(database_url, migrations_dir) {
        Ok(entries) if entries.is_empty() => {
            eprintln!("  No applied migrations.");
        }
        Ok(entries) => {
            let mut changed = 0;
            let mut missing = 0;
            let mut unrecorded = 0;
            for (version, state) in &entries {
                match state {
                    autumn_web::migrate::ChecksumState::Ok => {
                        eprintln!("  \u{2713} {version}  [ok]");
                    }
                    autumn_web::migrate::ChecksumState::Changed { recorded, actual } => {
                        changed += 1;
                        eprintln!(
                            "  \u{2717} {version}  [changed]  recorded={recorded} \
                             actual={actual}"
                        );
                    }
                    autumn_web::migrate::ChecksumState::Missing { recorded } => {
                        missing += 1;
                        eprintln!(
                            "  \u{2717} {version}  [missing]  recorded={recorded} \
                             (recorded as applied but up.sql is gone \u{2014} possible drift)"
                        );
                    }
                    autumn_web::migrate::ChecksumState::Unrecorded => {
                        unrecorded += 1;
                        eprintln!("  \u{2022} {version}  [unrecorded]");
                    }
                }
            }
            if changed > 0 {
                eprintln!(
                    "\n  \u{2717} {changed} migration(s) were EDITED after being applied. \
                     `autumn migrate` will refuse to run until this is resolved. Never edit \
                     an applied migration \u{2014} add a new one, or re-baseline deliberately \
                     with `autumn migrate baseline --force <version>`."
                );
            }
            if missing > 0 {
                eprintln!(
                    "\n  \u{2717} {missing} migration(s) are recorded as applied but their \
                     up.sql is missing from the source tree (deleted or renamed after being \
                     applied). `autumn migrate` will refuse to run until this is resolved. \
                     Never delete or rename an applied migration \u{2014} add a new one instead."
                );
            }
            if unrecorded > 0 {
                eprintln!(
                    "  {unrecorded} migration(s) have no recorded checksum (applied before \
                     content-hash tracking existed). Run `autumn migrate baseline` to record \
                     their current hashes."
                );
            }
        }
        Err(e) => {
            eprintln!("  \u{26A0}\u{FE0F}  Could not read checksum status: {e}");
        }
    }
    eprintln!();
}

fn show_framework_status(database_url: &str, is_shard: bool) {
    eprintln!("  Checking Autumn framework migration status...\n");

    // Shard targets require only the shard framework migrations (version
    // history + commit-hook queue); the control plane requires the full set.
    let pending = if is_shard {
        autumn_web::migrate::pending_shard_framework_migrations(database_url)
    } else {
        pending_framework_migrations_inner(database_url, autumn_web::migrate::pending_migrations)
    };
    match pending {
        Ok(pending) if pending.is_empty() => {
            eprintln!("  Framework migrations are up to date.");
        }
        Ok(pending) => {
            eprintln!("  Pending Autumn framework migrations:");
            for migration in pending {
                eprintln!("    {migration}");
            }
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to check framework migration status: {e}");
            std::process::exit(1);
        }
    }
}

fn pending_framework_migrations_inner<F>(
    database_url: &str,
    pending_migrations: F,
) -> Result<Vec<String>, MigrationError>
where
    F: FnOnce(&str, EmbeddedMigrations) -> Result<Vec<String>, MigrationError>,
{
    pending_migrations(database_url, FRAMEWORK_MIGRATIONS)
}

fn show_diesel_migration_status(database_url: &str, migrations_dir: &Path) {
    // `diesel migration list` shows all migrations and their status
    let status = Command::new("diesel")
        .args(["migration", "list", "--migration-dir"])
        .arg(migrations_dir)
        .env("DATABASE_URL", database_url)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(_) => {
            eprintln!(
                "\n\u{2717} Failed to check migration status for {}.",
                migrations_dir.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "\u{2717} Failed to run diesel migration list for {}: {e}",
                migrations_dir.display()
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Advisory-lock API accessibility ───────────────────────────────────

    #[test]
    fn migration_lock_key_exported_from_autumn_web() {
        let key = autumn_web::migrate::MIGRATION_ADVISORY_LOCK_KEY;
        assert!(key > 0, "lock key must be a positive i64");
    }

    #[test]
    fn default_lock_wait_timeout_is_sixty_seconds() {
        let timeout = autumn_web::migrate::DEFAULT_LOCK_WAIT_TIMEOUT;
        assert_eq!(timeout.as_secs(), 60);
    }

    #[test]
    fn hold_migration_lock_returns_connection_error_on_bad_url() {
        let result = autumn_web::migrate::hold_migration_lock(
            "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db",
            std::time::Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert!(
            matches!(
                result.unwrap_err(),
                autumn_web::migrate::MigrationError::Connection(_)
            ),
            "unreachable host must produce Connection error"
        );
    }

    // ── Existing tests ────────────────────────────────────────────────────

    #[test]
    fn migrate_action_eq() {
        assert_eq!(MigrateAction::Run, MigrateAction::Run);
        assert_eq!(MigrateAction::Status, MigrateAction::Status);
        assert_eq!(MigrateAction::Check, MigrateAction::Check);
        assert_ne!(MigrateAction::Run, MigrateAction::Status);
        assert_ne!(MigrateAction::Run, MigrateAction::Check);
    }

    // ── Baseline args (issue #1203) ──────────────────────────────────────

    #[test]
    fn baseline_action_default_records_all_unrecorded() {
        // No force_version → additive baseline (backfill hashes for legacy
        // applied migrations only). This is the safe default and must never
        // silently overwrite an existing recorded hash.
        let action = MigrateAction::Baseline(BaselineArgs {
            force_version: None,
        });
        assert!(matches!(
            action,
            MigrateAction::Baseline(BaselineArgs {
                force_version: None
            })
        ));
    }

    #[test]
    fn baseline_action_force_targets_one_version() {
        // With force_version → escape hatch that overwrites a single stored
        // hash. Must carry the explicit version.
        let action = MigrateAction::Baseline(BaselineArgs {
            force_version: Some("20260101000000".to_owned()),
        });
        match action {
            MigrateAction::Baseline(args) => {
                assert_eq!(args.force_version.as_deref(), Some("20260101000000"));
            }
            _ => panic!("expected Baseline"),
        }
    }

    // ── check_migrations_in_dir_for: SQLite dialect (#1906) ────────────────

    /// A migration dir with the given `up.sql`.
    fn migrations_dir_with_up(up: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2026_01_01_000000_m");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), up).unwrap();
        tmp
    }

    #[test]
    fn sqlite_dir_scan_does_not_flag_the_drop_index_before_drop_column() {
        // The generated SQLite remove-column migration must pass the gate: its
        // DROP INDEX is a precondition of the DROP COLUMN, not a risk.
        let tmp = migrations_dir_with_up(
            "DROP INDEX IF EXISTS idx_posts_title;\nALTER TABLE posts DROP COLUMN title;\n",
        );
        let reports =
            check_migrations_in_dir_for(autumn_web::config::DatabaseBackend::Sqlite, tmp.path())
                .unwrap();
        let ops: Vec<&str> = reports[0].up.iter().map(|f| f.operation.as_str()).collect();
        assert_eq!(ops, vec!["DROP COLUMN"], "got {ops:?}");
    }

    #[test]
    fn sqlite_dir_scan_skips_the_concurrently_transaction_opt_out_check() {
        // On SQLite the CONCURRENTLY keyword is itself unsupported; the
        // Postgres-only `metadata.toml` opt-out finding must not also fire.
        let tmp = migrations_dir_with_up("CREATE INDEX CONCURRENTLY i ON posts (title);\n");
        let reports =
            check_migrations_in_dir_for(autumn_web::config::DatabaseBackend::Sqlite, tmp.path())
                .unwrap();
        let ops: Vec<&str> = reports[0].up.iter().map(|f| f.operation.as_str()).collect();
        assert_eq!(
            ops,
            vec!["CREATE INDEX CONCURRENTLY (unsupported on SQLite)"],
            "got {ops:?}"
        );
    }

    #[test]
    fn sqlite_dir_scan_classifies_down_sql_too() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2026_01_01_000000_m");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), "CREATE INDEX i ON posts (title);\n").unwrap();
        std::fs::write(
            dir.join("down.sql"),
            "DROP INDEX i;\nALTER TABLE posts ALTER COLUMN title TYPE TEXT;\n",
        )
        .unwrap();
        let reports =
            check_migrations_in_dir_for(autumn_web::config::DatabaseBackend::Sqlite, tmp.path())
                .unwrap();
        let down: Vec<&str> = reports[0]
            .down
            .iter()
            .map(|f| f.operation.as_str())
            .collect();
        assert_eq!(
            down,
            vec!["ALTER COLUMN (unsupported on SQLite)"],
            "the DROP INDEX must not be flagged, the ALTER COLUMN must be: {down:?}"
        );
    }

    #[test]
    fn grade_migrate_check_for_grades_against_the_given_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2026_01_01_000000_m");
        std::fs::create_dir_all(&dir).unwrap();
        // The generated SQLite remove-column shape: safe on SQLite, blocking on
        // Postgres (which wants DROP INDEX CONCURRENTLY).
        std::fs::write(
            dir.join("up.sql"),
            "DROP INDEX IF EXISTS idx_posts_title;\n",
        )
        .unwrap();
        assert!(
            crate::deploy::grade_migrate_check_for(
                autumn_web::config::DatabaseBackend::Sqlite,
                tmp.path()
            )
            .passed
        );
        assert!(
            !crate::deploy::grade_migrate_check_for(
                autumn_web::config::DatabaseBackend::Postgres,
                tmp.path()
            )
            .passed
        );
    }

    #[test]
    fn check_migrations_in_dir_still_classifies_as_postgres() {
        let tmp = migrations_dir_with_up("DROP INDEX idx_posts_title;\n");
        let bare = check_migrations_in_dir(tmp.path()).unwrap();
        let explicit =
            check_migrations_in_dir_for(autumn_web::config::DatabaseBackend::Postgres, tmp.path())
                .unwrap();
        assert_eq!(bare[0].up.len(), explicit[0].up.len());
        assert_eq!(bare[0].up[0].operation, "DROP INDEX (non-concurrent)");
    }

    // ── check_migrations_in_dir ────────────────────────────────────────────

    fn write_migration(dir: &std::path::Path, name: &str, up_sql: &str) {
        let migration_dir = dir.join(name);
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(migration_dir.join("up.sql"), up_sql).unwrap();
        std::fs::write(migration_dir.join("down.sql"), "").unwrap();
    }

    fn write_migration_with_down(dir: &std::path::Path, name: &str, up_sql: &str, down_sql: &str) {
        let migration_dir = dir.join(name);
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(migration_dir.join("up.sql"), up_sql).unwrap();
        std::fs::write(migration_dir.join("down.sql"), down_sql).unwrap();
    }

    #[test]
    fn check_empty_migrations_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn check_safe_migration_produces_no_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration(
            tmp.path(),
            "20260101000000_create_posts",
            "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);",
        );

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "20260101000000_create_posts");
        assert!(
            results[0].up.is_empty(),
            "CREATE TABLE should produce no up findings"
        );
        assert!(
            results[0].down.is_empty(),
            "empty down.sql should produce no findings"
        );
    }

    #[test]
    fn check_down_sql_findings_are_included_in_report() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration_with_down(
            tmp.path(),
            "20260101000000_add_column",
            "ALTER TABLE posts ADD COLUMN body TEXT;",
            "ALTER TABLE posts DROP COLUMN body;",
        );

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].up.is_empty(), "ADD COLUMN should be safe in up");
        assert_eq!(
            results[0].down.len(),
            1,
            "DROP COLUMN in down.sql should have 1 finding"
        );
        assert_eq!(results[0].down[0].risk, safety::RiskLevel::Destructive);
    }

    #[test]
    fn check_down_sql_absent_produces_empty_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260101000000_create_posts");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("up.sql"),
            "CREATE TABLE posts (id BIGSERIAL);",
        )
        .unwrap();

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].down.is_empty(),
            "absent down.sql should produce empty findings"
        );
    }

    #[test]
    fn rolling_deploy_gate_ignores_destructive_down_sql() {
        // A forward-safe migration whose down.sql is inherently destructive
        // (ADD COLUMN up / DROP COLUMN down) must NOT block the forward deploy.
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration_with_down(
            tmp.path(),
            "20260101000000_add_column",
            "ALTER TABLE posts ADD COLUMN body TEXT;",
            "ALTER TABLE posts DROP COLUMN body;",
        );
        let reports = check_migrations_in_dir(tmp.path()).unwrap();
        assert!(
            !reports[0].down.is_empty(),
            "precondition: down.sql has a destructive finding"
        );
        assert!(
            !rolling_deploy_blocked(&reports),
            "destructive down.sql must not block a forward rolling deploy"
        );
    }

    #[test]
    fn rolling_deploy_gate_blocks_on_unsafe_up_sql() {
        // An unsafe forward operation (DROP COLUMN in up.sql) must block.
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration_with_down(
            tmp.path(),
            "20260101000000_drop_column",
            "ALTER TABLE posts DROP COLUMN body;",
            "ALTER TABLE posts ADD COLUMN body TEXT;",
        );
        let reports = check_migrations_in_dir(tmp.path()).unwrap();
        assert!(
            rolling_deploy_blocked(&reports),
            "unsafe up.sql must block the forward rolling deploy"
        );
    }

    #[test]
    fn check_destructive_migration_produces_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration(
            tmp.path(),
            "20260102000000_remove_body_from_posts",
            "ALTER TABLE posts DROP COLUMN body;",
        );

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].up.len(), 1);
        assert_eq!(results[0].up[0].risk, safety::RiskLevel::Destructive);
    }

    #[test]
    fn check_results_are_sorted_by_migration_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration(tmp.path(), "20260103000000_third", "SELECT 1;");
        write_migration(tmp.path(), "20260101000000_first", "SELECT 1;");
        write_migration(tmp.path(), "20260102000000_second", "SELECT 1;");

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        let names: Vec<_> = results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "20260101000000_first",
                "20260102000000_second",
                "20260103000000_third"
            ]
        );
    }

    #[test]
    fn check_directories_without_up_sql_are_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("incomplete_migration")).unwrap();
        write_migration(tmp.path(), "20260101000000_valid", "SELECT 1;");

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "20260101000000_valid");
    }

    #[test]
    fn check_multiple_migrations_reports_each() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration(
            tmp.path(),
            "20260101000000_create_posts",
            "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);",
        );
        write_migration(
            tmp.path(),
            "20260102000000_remove_body",
            "ALTER TABLE posts DROP COLUMN body;",
        );

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].up.is_empty(), "first migration should be safe");
        assert!(
            !results[1].up.is_empty(),
            "second migration should have findings"
        );
    }

    #[test]
    fn check_non_concurrent_index_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_migration(
            tmp.path(),
            "20260103000000_add_index",
            "CREATE INDEX idx_posts_title ON posts (title);",
        );

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results[0].up.len(), 1);
        assert_eq!(
            results[0].up[0].risk,
            safety::RiskLevel::PotentiallyBlocking
        );
    }

    // ── effective_profile ─────────────────────────────────────────────────────

    #[test]
    fn effective_profile_prefers_autumn_env_over_legacy_and_flag() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", Some("prod")),
                ("AUTUMN_PROFILE", Some("dev")),
                ("AUTUMN_IS_DEBUG", None),
            ],
            || {
                // AUTUMN_ENV wins over a stale legacy AUTUMN_PROFILE and an
                // explicit --profile flag, mirroring the runtime loader.
                assert_eq!(effective_profile(Some("staging")), "prod");
            },
        );
    }

    #[test]
    fn effective_profile_uses_legacy_when_no_autumn_env() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None),
                ("AUTUMN_PROFILE", Some("prod")),
                ("AUTUMN_IS_DEBUG", None),
            ],
            || {
                assert_eq!(effective_profile(None), "prod");
            },
        );
    }

    #[test]
    fn effective_profile_uses_explicit_flag_when_no_env() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None::<&str>),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", None),
            ],
            || {
                assert_eq!(effective_profile(Some("staging")), "staging");
            },
        );
    }

    #[test]
    fn effective_profile_defaults_to_dev() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None::<&str>),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", None),
            ],
            || {
                // No env, no flag, debug build → dev (the runtime default), so the
                // CLI applies the same [profile.dev]/autumn-dev.toml overlay.
                assert_eq!(effective_profile(None), "dev");
            },
        );
    }

    #[test]
    fn effective_profile_release_mode_defaults_to_prod() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", Some("0")),
            ],
            || {
                assert_eq!(effective_profile(None), "prod");
            },
        );
    }

    #[test]
    fn canonical_profile_normalizes_aliases_and_case() {
        // P2 #20: offsite `.env.<profile>` selection must use the app's canonical
        // profile, so alias/case spellings collapse the same way the loader does.
        for input in ["development", "Development", "DEV", "dev", "  dev  "] {
            assert_eq!(canonical_profile(input), "dev", "{input:?}");
        }
        for input in ["production", "PROD", "Prod", "prod"] {
            assert_eq!(canonical_profile(input), "prod", "{input:?}");
        }
        // Custom profiles are preserved verbatim (trimmed, case kept).
        assert_eq!(canonical_profile("staging"), "staging");
        assert_eq!(canonical_profile("  QA  "), "QA");
    }

    #[test]
    fn profile_file_lookup_prefers_selected_spelling() {
        // Overlay file probe order prefers the spelling the operator selected.
        assert_eq!(
            profile_file_lookup_names("prod"),
            vec!["prod", "production"]
        );
        assert_eq!(
            profile_file_lookup_names("production"),
            vec!["production", "prod"]
        );
        assert_eq!(profile_file_lookup_names("dev"), vec!["dev", "development"]);
        assert_eq!(
            profile_file_lookup_names("development"),
            vec!["development", "dev"]
        );
        // Custom profiles are used verbatim.
        assert_eq!(profile_file_lookup_names("staging"), vec!["staging"]);
    }

    // ── is_production_profile ─────────────────────────────────────────────────

    #[test]
    fn is_production_profile_detects_prod() {
        temp_env::with_var("AUTUMN_ENV", Some("prod"), || {
            assert!(is_production_profile(None));
        });
    }

    #[test]
    fn is_production_profile_detects_production() {
        temp_env::with_var("AUTUMN_ENV", Some("production"), || {
            assert!(is_production_profile(None));
        });
    }

    #[test]
    fn is_production_profile_case_insensitive() {
        temp_env::with_var("AUTUMN_ENV", Some("PROD"), || {
            assert!(is_production_profile(None));
        });
    }

    #[test]
    fn is_production_profile_false_for_dev() {
        temp_env::with_var("AUTUMN_ENV", Some("dev"), || {
            assert!(!is_production_profile(None));
        });
    }

    #[test]
    fn is_production_profile_reads_autumn_profile_legacy() {
        temp_env::with_vars(
            [("AUTUMN_ENV", None), ("AUTUMN_PROFILE", Some("production"))],
            || {
                assert!(is_production_profile(None));
            },
        );
    }

    #[test]
    fn is_production_profile_empty_autumn_env_falls_back_to_legacy() {
        // An empty/whitespace AUTUMN_ENV must be treated as absent so the
        // legacy AUTUMN_PROFILE still triggers the production guard.
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", Some("   ")),
                ("AUTUMN_PROFILE", Some("prod")),
            ],
            || {
                assert!(is_production_profile(None));
            },
        );
    }

    #[test]
    fn is_production_profile_debug_zero_without_env_is_prod() {
        // No explicit profile: `AUTUMN_IS_DEBUG=0` (release build signal) must
        // resolve to prod so the rollback guard still trips.
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", Some("0")),
            ],
            || {
                assert!(is_production_profile(None));
            },
        );
    }

    #[test]
    fn is_production_profile_debug_one_without_env_is_not_prod() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", Some("1")),
            ],
            || {
                assert!(!is_production_profile(None));
            },
        );
    }

    #[test]
    fn is_production_profile_explicit_env_overrides_debug_signal() {
        // An explicit dev profile wins over `AUTUMN_IS_DEBUG=0`.
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", Some("dev")),
                ("AUTUMN_PROFILE", None),
                ("AUTUMN_IS_DEBUG", Some("0")),
            ],
            || {
                assert!(!is_production_profile(None));
            },
        );
    }

    #[test]
    fn default_migrations_dir_is_migrations() {
        assert_eq!(DEFAULT_MIGRATIONS_DIR, "migrations");
    }

    // ── rollback planning ────────────────────────────────────────────────────

    fn applied(version: &str) -> AppliedUserMigration {
        AppliedUserMigration {
            version: version.to_string(),
            name: format!("{version}_m"),
            dir: Some(std::path::PathBuf::from(format!("migrations/{version}_m"))),
        }
    }

    #[test]
    fn rollback_plans_diverge_detects_mismatched_targets() {
        let plan = |label: &str, versions: &[&str]| {
            (
                label.to_owned(),
                versions.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
            )
        };

        // Identical plans across targets: a healthy uniform rollback.
        assert!(!rollback_plans_diverge(&[
            plan("control", &["20260102000000"]),
            plan("shard:s0", &["20260102000000"]),
        ]));

        // A lagging shard whose newest applied differs reverts a different
        // version — must be flagged.
        assert!(rollback_plans_diverge(&[
            plan("control", &["20260102000000"]),
            plan("shard:s0", &["20260101000000"]),
        ]));

        // A target that has nothing to roll back while others do also diverges.
        assert!(rollback_plans_diverge(&[
            plan("control", &["20260102000000"]),
            plan("shard:s0", &[]),
        ]));

        // A single target (scoped with --shard / --control-only) never diverges.
        assert!(!rollback_plans_diverge(&[plan(
            "shard:s0",
            &["20260102000000"]
        )]));
        assert!(!rollback_plans_diverge(&[]));
    }

    #[test]
    fn build_rollback_plan_default_reverts_single_newest() {
        // `applied` is ascending; default plan reverts only the newest.
        let applied = [applied("20260101000000"), applied("20260102000000")];
        let args = DownArgs {
            steps: None,
            to: None,
            yes_i_mean_prod: false,
        };
        assert_eq!(build_rollback_plan(&args, &applied), ["20260102000000"]);
    }

    #[test]
    fn build_rollback_plan_steps_reverts_n_newest_first() {
        let applied = [
            applied("20260101000000"),
            applied("20260102000000"),
            applied("20260103000000"),
        ];
        let args = DownArgs {
            steps: Some(2),
            to: None,
            yes_i_mean_prod: false,
        };
        // Newest-first so dependent migrations revert before their dependencies.
        assert_eq!(
            build_rollback_plan(&args, &applied),
            ["20260103000000", "20260102000000"]
        );
    }

    #[test]
    fn build_rollback_plan_to_reverts_strictly_newer_newest_first() {
        let applied = [
            applied("20260101000000"),
            applied("20260102000000"),
            applied("20260103000000"),
        ];
        let args = DownArgs {
            steps: None,
            to: Some("20260101000000".to_string()),
            yes_i_mean_prod: false,
        };
        assert_eq!(
            build_rollback_plan(&args, &applied),
            ["20260103000000", "20260102000000"]
        );
    }

    #[test]
    fn has_revertable_down_sql_false_when_missing_locally() {
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000".to_string(),
            dir: None,
        };
        assert!(!has_revertable_down_sql(&m));
    }

    #[test]
    fn has_revertable_down_sql_true_for_executable_down() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("20260101000000_create_posts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("down.sql"), "DROP TABLE posts;").unwrap();
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_create_posts".to_string(),
            dir: Some(dir),
        };
        assert!(has_revertable_down_sql(&m));
    }

    #[test]
    fn has_revertable_down_sql_false_for_comment_only_down() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("20260101000000_create_posts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("down.sql"), "-- nothing to do\n").unwrap();
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_create_posts".to_string(),
            dir: Some(dir),
        };
        assert!(!has_revertable_down_sql(&m));
    }

    /// Build an `AppliedUserMigration` backed by a temp dir containing the given
    /// `down.sql` (and optional `metadata.toml`). Returns the migration plus the
    /// `TempDir` guard, which must be kept alive for the files to exist.
    fn migration_with_down(
        down_sql: &str,
        metadata: Option<&str>,
    ) -> (AppliedUserMigration, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("20260101000000_add_index");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("down.sql"), down_sql).unwrap();
        if let Some(meta) = metadata {
            std::fs::write(dir.join("metadata.toml"), meta).unwrap();
        }
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_add_index".to_string(),
            dir: Some(dir),
        };
        (m, tmp)
    }

    #[test]
    fn down_concurrent_without_metadata_needs_opt_out() {
        let (m, _tmp) = migration_with_down("DROP INDEX CONCURRENTLY idx_posts_title;", None);
        assert!(down_sql_concurrent_without_opt_out(&m));
    }

    #[test]
    fn down_concurrent_with_opt_out_is_safe() {
        let (m, _tmp) = migration_with_down(
            "DROP INDEX CONCURRENTLY idx_posts_title;",
            Some("run_in_transaction = false\n"),
        );
        assert!(!down_sql_concurrent_without_opt_out(&m));
    }

    #[test]
    fn down_without_concurrent_index_is_safe() {
        let (m, _tmp) = migration_with_down("DROP TABLE posts;", None);
        assert!(!down_sql_concurrent_without_opt_out(&m));
    }

    #[test]
    fn down_concurrent_missing_locally_is_not_flagged_here() {
        // Reported by the separate missing-down.sql preflight, not this one.
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_add_index".to_string(),
            dir: None,
        };
        assert!(!down_sql_concurrent_without_opt_out(&m));
    }

    #[test]
    fn framework_api_token_migrations_run_from_embedded_set() {
        let mut called = false;

        let result = run_framework_migrations_inner(
            "postgres://primary/app",
            |database_url, embedded_migrations| {
                assert_eq!(database_url, "postgres://primary/app");
                let _ = embedded_migrations;
                called = true;
                Ok(autumn_web::migrate::MigrationResult {
                    applied: vec!["20260512000000_create_api_tokens".to_string()],
                })
            },
        )
        .unwrap();

        assert!(called);
        assert_eq!(
            result.applied,
            vec!["20260512000000_create_api_tokens".to_string()]
        );
    }

    #[test]
    fn framework_api_token_status_uses_embedded_set() {
        let mut called = false;

        let pending = pending_framework_migrations_inner(
            "postgres://primary/app",
            |database_url, embedded_migrations| {
                assert_eq!(database_url, "postgres://primary/app");
                let _ = embedded_migrations;
                called = true;
                Ok(vec!["20260512000000_create_api_tokens".to_string()])
            },
        )
        .unwrap();

        assert!(called);
        assert_eq!(
            pending,
            vec!["20260512000000_create_api_tokens".to_string()]
        );
    }

    // ── Shard-specific framework migrations (§5a) ──────────────────────────────
    //
    // Shards hold tenant data and need only the version-history and
    // commit-hook queue tables — not the full control-plane schema (API tokens,
    // sessions, jobs, …).  `run_shard_framework_migrations_inner` is the
    // testable core; in production it delegates to
    // `autumn_web::migrate::run_pending_shard_framework_migrations`.

    #[test]
    fn shard_target_applies_only_shard_framework_migrations() {
        let mut called = false;

        let result =
            run_shard_framework_migrations_inner("postgres://shard0/app", |database_url| {
                assert_eq!(database_url, "postgres://shard0/app");
                called = true;
                Ok(autumn_web::migrate::MigrationResult {
                    applied: vec![
                        "vh_migration".to_string(),
                        "commit_hook_migration".to_string(),
                    ],
                })
            })
            .unwrap();

        assert!(called, "shard framework migration helper must be called");
        assert_eq!(
            result.applied,
            vec!["vh_migration", "commit_hook_migration"]
        );
    }

    #[test]
    fn control_target_still_uses_full_framework_migrations() {
        let mut called_with_url = String::new();
        let mut called = false;

        let result =
            run_framework_migrations_inner("postgres://control/app", |database_url, _embedded| {
                called_with_url = database_url.to_owned();
                called = true;
                Ok(autumn_web::migrate::MigrationResult {
                    applied: vec!["20260512000000_create_api_tokens".to_string()],
                })
            })
            .unwrap();

        assert!(
            called,
            "run_framework_migrations_inner must call the closure"
        );
        assert_eq!(called_with_url, "postgres://control/app");
        assert_eq!(
            result.applied,
            vec!["20260512000000_create_api_tokens".to_string()]
        );
    }

    #[test]
    fn embedded_framework_migrations_include_durable_hook_queue() {
        use autumn_web::reexports::diesel::migration::{Migration, MigrationSource};
        use autumn_web::reexports::diesel::pg::Pg;

        let migrations: Vec<Box<dyn Migration<Pg>>> = autumn_web::migrate::FRAMEWORK_MIGRATIONS
            .migrations()
            .unwrap();
        let names: Vec<_> = migrations
            .iter()
            .map(|migration| migration.name().to_string())
            .collect();

        assert!(
            names
                .iter()
                .any(|name| name == "20260512000000_create_api_tokens"),
            "framework migrations must include the timestamped API token schema migration: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name == "20260515000000_create_repository_commit_hook_queue"),
            "framework migrations must include the durable repository commit hook queue: {names:?}"
        );
    }

    #[test]
    fn resolve_database_url_from_env() {
        // AUTUMN_DATABASE__URL takes priority
        let env_var = |key: &str| -> Result<String, std::env::VarError> {
            if key == "AUTUMN_DATABASE__URL" {
                Ok("postgres://test:5432/mydb".to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        };
        let url = resolve_database_url_with_env(env_var);
        assert_eq!(url, "postgres://test:5432/mydb");
    }

    #[test]
    fn resolve_database_url_from_database_url_env() {
        // Make sure AUTUMN_DATABASE__URL is not set, but DATABASE_URL is
        let env_var = |key: &str| -> Result<String, std::env::VarError> {
            if key == "DATABASE_URL" {
                Ok("postgres://fallback:5432/db".to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        };
        let url = resolve_database_url_with_env(env_var);
        assert_eq!(url, "postgres://fallback:5432/db");
    }

    #[test]
    fn database_topology_primary_env_wins_for_migrations() {
        let env_var = |key: &str| -> Result<String, std::env::VarError> {
            match key {
                "AUTUMN_DATABASE__PRIMARY_URL" => Ok("postgres://primary:5432/app".to_string()),
                "AUTUMN_DATABASE__URL" => Ok("postgres://legacy:5432/app".to_string()),
                "DATABASE_URL" => Ok("postgres://database-url:5432/app".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            }
        };

        let url = resolve_primary_database_url_from_sources(env_var, None).unwrap();

        assert_eq!(url, "postgres://primary:5432/app");
    }

    #[test]
    fn database_topology_toml_primary_wins_over_legacy_url() {
        let table = toml::from_str::<toml::Table>(
            r#"
[database]
url = "postgres://legacy:5432/app"
primary_url = "postgres://primary:5432/app"
replica_url = "postgres://replica:5432/app"
"#,
        )
        .unwrap();
        let env_var = |_key: &str| -> Result<String, std::env::VarError> {
            Err(std::env::VarError::NotPresent)
        };

        let url = resolve_primary_database_url_from_sources(env_var, Some(&table)).unwrap();

        assert_eq!(url, "postgres://primary:5432/app");
    }

    #[test]
    fn resolve_primary_url_reads_env_for_active_profile() {
        // `resolve_primary_url` is the convenience entry the `autumn db`
        // commands use; it must resolve the same primary URL `autumn migrate`
        // would for the active profile. The env var wins over any file layer.
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", Some("dev")),
                (
                    "AUTUMN_DATABASE__PRIMARY_URL",
                    Some("postgres://primary:5432/app"),
                ),
            ],
            || {
                assert_eq!(
                    resolve_primary_url(None).as_deref(),
                    Some("postgres://primary:5432/app")
                );
            },
        );
    }

    #[test]
    fn resolve_primary_url_none_when_unset() {
        // No env URL and no autumn.toml in the test's working directory → None,
        // leaving the caller to report the missing-URL error.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None),
                ("DATABASE_URL", None),
            ],
            || {
                assert!(resolve_primary_url(Some("dev")).is_none());
            },
        );
    }

    // ── build_targets ──────────────────────────────────────────────────────

    fn two_shards() -> Vec<(String, String)> {
        vec![
            ("shard0".to_owned(), "postgres://s0/app".to_owned()),
            ("shard1".to_owned(), "postgres://s1/app".to_owned()),
        ]
    }

    #[test]
    fn build_targets_all_orders_control_first_then_shards() {
        let targets = build_targets(
            Some("postgres://control/app".to_owned()),
            two_shards(),
            &MigrateTarget::All,
        )
        .unwrap();

        let labels: Vec<&str> = targets.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["control", "shard:shard0", "shard:shard1"]);
        assert_eq!(targets[0].1, "postgres://control/app");
    }

    #[test]
    fn build_targets_all_supports_shard_only_deployments() {
        let targets = build_targets(None, two_shards(), &MigrateTarget::All).unwrap();
        let labels: Vec<&str> = targets.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["shard:shard0", "shard:shard1"]);
    }

    #[test]
    fn build_targets_all_errors_with_no_databases_at_all() {
        let error = build_targets(None, Vec::new(), &MigrateTarget::All).unwrap_err();
        assert!(error.contains("No database URL"));
    }

    fn sqlite_shard() -> Vec<(String, String)> {
        vec![("shard0".to_owned(), "sqlite:///tmp/shard0.db".to_owned())]
    }

    #[test]
    fn reject_sqlite_sharding_rejects_sqlite_control_and_shards() {
        // SQLite control + shard entries is a boot-invalid topology (sharding is
        // Postgres-only); the guard mirrors the runtime guard's wording (#1614).
        let err = reject_sqlite_sharding_topology(Some("sqlite:///tmp/app.db"), &two_shards())
            .unwrap_err();
        assert!(
            err.contains("do not support sharding") && err.contains("#1614"),
            "guard names the SQLite sharding situation clearly: {err}"
        );
    }

    #[test]
    fn reject_sqlite_sharding_rejects_postgres_control_with_sqlite_shard() {
        // The sibling case: a Postgres (or any non-sqlite) control with a shard
        // whose primary_url is sqlite:// — the shard loop would migrate that
        // sqlite file. Must be rejected regardless of the control backend.
        let err = reject_sqlite_sharding_topology(Some("postgres://control/app"), &sqlite_shard())
            .unwrap_err();
        assert!(
            err.contains("do not support sharding")
                && err.contains("shard")
                && err.contains("#1614"),
            "guard rejects a sqlite shard target: {err}"
        );
    }

    #[test]
    fn reject_sqlite_sharding_rejects_no_control_with_sqlite_shard() {
        // No control role, but a sqlite shard target: still rejected (the shard
        // loop would route it through the Postgres-only harness).
        let err = reject_sqlite_sharding_topology(None, &sqlite_shard()).unwrap_err();
        assert!(
            err.contains("do not support sharding") && err.contains("#1614"),
            "guard rejects a sqlite shard even without a control: {err}"
        );
    }

    #[test]
    fn reject_sqlite_sharding_allows_sqlite_without_shards() {
        // A plain SQLite deployment (no shards) is the normal, supported case.
        assert_eq!(
            reject_sqlite_sharding_topology(Some("sqlite:///tmp/app.db"), &[]),
            Ok(())
        );
    }

    #[test]
    fn reject_sqlite_sharding_allows_postgres_with_postgres_shards() {
        // Postgres control + all-Postgres shards is the intended sharded topology
        // — never rejected.
        assert_eq!(
            reject_sqlite_sharding_topology(Some("postgres://control/app"), &two_shards()),
            Ok(())
        );
    }

    #[test]
    fn reject_sqlite_sharding_allows_no_control_no_shards() {
        // No primary role and no shards: nothing to reject.
        assert_eq!(reject_sqlite_sharding_topology(None, &[]), Ok(()));
    }

    #[test]
    fn build_targets_control_only_requires_control() {
        let targets = build_targets(
            Some("postgres://control/app".to_owned()),
            two_shards(),
            &MigrateTarget::ControlOnly,
        )
        .unwrap();
        assert_eq!(
            targets,
            vec![("control".to_owned(), "postgres://control/app".to_owned())]
        );

        let error = build_targets(None, two_shards(), &MigrateTarget::ControlOnly).unwrap_err();
        assert!(error.contains("control database"));
    }

    #[test]
    fn build_targets_selects_single_shard_by_name() {
        let targets = build_targets(
            None,
            two_shards(),
            &MigrateTarget::Shard("shard1".to_owned()),
        )
        .unwrap();
        assert_eq!(
            targets,
            vec![("shard:shard1".to_owned(), "postgres://s1/app".to_owned())]
        );
    }

    #[test]
    fn build_targets_unknown_shard_lists_known_names() {
        let error = build_targets(None, two_shards(), &MigrateTarget::Shard("nope".to_owned()))
            .unwrap_err();
        assert!(error.contains("Unknown shard"));
        assert!(error.contains("shard0, shard1"));

        let error =
            build_targets(None, Vec::new(), &MigrateTarget::Shard("nope".to_owned())).unwrap_err();
        assert!(error.contains("No [[database.shards]] entries"));
    }

    // ── resolve_shard_database_urls ────────────────────────────────────────

    fn no_env(_key: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    #[test]
    fn shard_urls_resolve_from_toml_in_declaration_order() {
        let table = toml::from_str::<toml::Table>(
            r#"
[database]
primary_url = "postgres://control:5432/app"

[[database.shards]]
name = "shard0"
primary_url = "postgres://shard0:5432/app"

[[database.shards]]
name = "shard1"
primary_url = "postgres://shard1:5432/app"
"#,
        )
        .unwrap();

        let shards = resolve_shard_database_urls_from_sources(no_env, Some(&table));

        assert_eq!(
            shards,
            vec![
                ("shard0".to_owned(), "postgres://shard0:5432/app".to_owned()),
                ("shard1".to_owned(), "postgres://shard1:5432/app".to_owned()),
            ]
        );
    }

    #[test]
    fn shard_urls_env_overrides_and_appends() {
        let table = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "shard0"
primary_url = "postgres://toml:5432/app"
"#,
        )
        .unwrap();
        let env_var = |key: &str| -> Result<String, std::env::VarError> {
            match key {
                "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL" => {
                    Ok("postgres://env:5432/app".to_owned())
                }
                "AUTUMN_DATABASE__SHARDS__1__NAME" => Ok("shard1".to_owned()),
                "AUTUMN_DATABASE__SHARDS__1__PRIMARY_URL" => {
                    Ok("postgres://env1:5432/app".to_owned())
                }
                _ => Err(std::env::VarError::NotPresent),
            }
        };

        let shards = resolve_shard_database_urls_from_sources(env_var, Some(&table));

        assert_eq!(
            shards,
            vec![
                ("shard0".to_owned(), "postgres://env:5432/app".to_owned()),
                ("shard1".to_owned(), "postgres://env1:5432/app".to_owned()),
            ]
        );
    }

    #[test]
    fn shard_urls_empty_without_shards() {
        assert!(resolve_shard_database_urls_from_sources(no_env, None).is_empty());
    }

    #[test]
    fn try_shard_urls_ok_matches_exiting_variant() {
        let table = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "shard0"
primary_url = "postgres://shard0:5432/app"
"#,
        )
        .unwrap();
        // The fallible core returns the same successful result the exiting wrapper
        // does, so command paths keep identical behavior.
        assert_eq!(
            try_resolve_shard_database_urls_from_sources(no_env, Some(&table)).unwrap(),
            vec![("shard0".to_owned(), "postgres://shard0:5432/app".to_owned())],
        );
    }

    #[test]
    fn try_shard_urls_missing_primary_url_returns_err_not_exit() {
        // A malformed `[[database.shards]]` entry (missing `primary_url`) makes the
        // exiting variant `std::process::exit(1)`. The fallible core must instead
        // return `Err` so `autumn doctor` can render a failing check rather than
        // terminating before emitting JSON.
        let table = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "shard0"
"#,
        )
        .unwrap();
        let err = try_resolve_shard_database_urls_from_sources(no_env, Some(&table))
            .expect_err("a shard missing primary_url must be an Err, not a process exit");
        assert!(
            err.contains("primary_url"),
            "error must name the missing field: {err}"
        );
    }

    #[test]
    fn try_shard_urls_duplicate_name_returns_err() {
        let table = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "dup"
primary_url = "postgres://a:5432/app"

[[database.shards]]
name = "dup"
primary_url = "postgres://b:5432/app"
"#,
        )
        .unwrap();
        let err = try_resolve_shard_database_urls_from_sources(no_env, Some(&table))
            .expect_err("duplicate shard names must be an Err");
        assert!(
            err.contains("Duplicate shard name"),
            "error must flag the duplicate: {err}"
        );
    }

    // ── deep_merge_toml / profile overlay ──────────────────────────────────

    #[test]
    fn deep_merge_toml_overlay_scalar_wins() {
        let mut base = toml::from_str::<toml::Table>(
            r#"
[database]
primary_url = "postgres://base:5432/app"
pool_size = 5
"#,
        )
        .unwrap();
        let overlay = toml::from_str::<toml::Table>(
            r#"
[database]
primary_url = "postgres://prod:5432/app"
"#,
        )
        .unwrap();

        deep_merge_toml(&mut base, overlay);

        let database = base
            .get("database")
            .and_then(toml::Value::as_table)
            .unwrap();
        // Overlay scalar replaces the base value...
        assert_eq!(
            database.get("primary_url").and_then(toml::Value::as_str),
            Some("postgres://prod:5432/app")
        );
        // ...while base-only keys in the same table are preserved.
        assert_eq!(
            database.get("pool_size").and_then(toml::Value::as_integer),
            Some(5)
        );
    }

    #[test]
    fn deep_merge_toml_arrays_replaced_wholesale() {
        // Shard arrays must be replaced, not concatenated, so a profile can
        // point at an entirely different set of shard databases.
        let mut base = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "s0"
primary_url = "postgres://base-s0:5432/app"
"#,
        )
        .unwrap();
        let overlay = toml::from_str::<toml::Table>(
            r#"
[[database.shards]]
name = "s0"
primary_url = "postgres://prod-s0:5432/app"
"#,
        )
        .unwrap();

        deep_merge_toml(&mut base, overlay);

        let shards = resolve_shard_database_urls_from_sources(no_env, Some(&base));
        assert_eq!(
            shards,
            vec![("s0".to_owned(), "postgres://prod-s0:5432/app".to_owned())]
        );
    }

    #[test]
    fn profile_overlay_overrides_shard_url() {
        // End-to-end through the file loader: a base autumn.toml plus an
        // autumn-prod.toml overlay; --profile prod resolves the prod URLs.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            r#"
[database]
primary_url = "postgres://base-control:5432/app"

[[database.shards]]
name = "s0"
primary_url = "postgres://base-s0:5432/app"
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("autumn-prod.toml"),
            r#"
[database]
primary_url = "postgres://prod-control:5432/app"

[[database.shards]]
name = "s0"
primary_url = "postgres://prod-s0:5432/app"
"#,
        )
        .unwrap();

        let with_profile = read_autumn_toml_table_with_profile_in(tmp.path(), Some("prod"));
        let without_profile = read_autumn_toml_table_with_profile_in(tmp.path(), None);

        let merged = with_profile.unwrap();
        assert_eq!(
            resolve_primary_database_url_from_sources(no_env, Some(&merged)).as_deref(),
            Some("postgres://prod-control:5432/app")
        );
        assert_eq!(
            resolve_shard_database_urls_from_sources(no_env, Some(&merged)),
            vec![("s0".to_owned(), "postgres://prod-s0:5432/app".to_owned())]
        );

        // Without a profile, the base file is returned unchanged.
        let base = without_profile.unwrap();
        assert_eq!(
            resolve_primary_database_url_from_sources(no_env, Some(&base)).as_deref(),
            Some("postgres://base-control:5432/app")
        );
    }

    #[test]
    fn autumn_toml_base_dir_prefers_manifest_dir_with_config() {
        // P2 #15: when AUTUMN_MANIFEST_DIR holds an autumn.toml, offsite TOML
        // reads must resolve from THAT directory (like the full loader), not CWD.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("autumn.toml"), "[backup.offsite]\n").unwrap();
        assert_eq!(
            autumn_toml_base_dir_from(Some(tmp.path().to_str().unwrap())),
            tmp.path()
        );
    }

    #[test]
    fn autumn_toml_base_dir_falls_back_to_cwd() {
        // No manifest dir, or a manifest dir WITHOUT an autumn.toml → CWD (".").
        assert_eq!(
            autumn_toml_base_dir_from(None),
            std::path::PathBuf::from(".")
        );
        assert_eq!(
            autumn_toml_base_dir_from(Some("   ")),
            std::path::PathBuf::from(".")
        );
        let empty = tempfile::TempDir::new().unwrap();
        assert_eq!(
            autumn_toml_base_dir_from(Some(empty.path().to_str().unwrap())),
            std::path::PathBuf::from(".")
        );
    }

    #[test]
    fn config_dir_read_surfaces_offsite_section() {
        // The manifest-dir read must carry the [backup.offsite] section (with the
        // profile overlay applied) so offsite resolution + auto_upload work when
        // the config lives outside CWD.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[backup.offsite]\nauto_upload = true\nprefix = \"db\"\n",
        )
        .unwrap();
        let table = read_autumn_toml_table_with_profile_in(tmp.path(), Some("dev"))
            .expect("manifest-dir autumn.toml should read");
        let auto_upload = table
            .get("backup")
            .and_then(|v| v.get("offsite"))
            .and_then(|v| v.get("auto_upload"))
            .and_then(toml::Value::as_bool);
        assert_eq!(auto_upload, Some(true));
    }

    #[test]
    fn overlay_file_resolved_via_profile_alias() {
        // Operator selects `production`, but the overlay file uses the common
        // `autumn-prod.toml` spelling — the alias must still resolve it, just
        // like the runtime loader.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://base:5432/app\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("autumn-prod.toml"),
            "[database]\nprimary_url = \"postgres://prod:5432/app\"\n",
        )
        .unwrap();

        let merged =
            read_autumn_toml_table_with_profile_in(tmp.path(), Some("production")).unwrap();
        assert_eq!(
            resolve_primary_database_url_from_sources(no_env, Some(&merged)).as_deref(),
            Some("postgres://prod:5432/app")
        );
    }

    #[test]
    fn inline_profile_section_overrides_base_url() {
        // A deployment that keeps prod URLs in an inline `[profile.prod.*]`
        // section of autumn.toml (no separate autumn-prod.toml) must still
        // resolve the prod URLs under `--profile prod`, matching the runtime
        // loader. `production` is honored as an alias for `prod`.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            r#"
[database]
primary_url = "postgres://base-control:5432/app"

[[database.shards]]
name = "s0"
primary_url = "postgres://base-s0:5432/app"

[profile.production.database]
primary_url = "postgres://prod-control:5432/app"

[[profile.production.database.shards]]
name = "s0"
primary_url = "postgres://prod-s0:5432/app"
"#,
        )
        .unwrap();

        let merged = read_autumn_toml_table_with_profile_in(tmp.path(), Some("prod")).unwrap();
        assert_eq!(
            resolve_primary_database_url_from_sources(no_env, Some(&merged)).as_deref(),
            Some("postgres://prod-control:5432/app")
        );
        assert_eq!(
            resolve_shard_database_urls_from_sources(no_env, Some(&merged)),
            vec![("s0".to_owned(), "postgres://prod-s0:5432/app".to_owned())]
        );
    }

    // ── check_concurrent_index_transaction_opt_out ────────────────────────

    #[test]
    fn concurrent_index_without_metadata_toml_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
        assert!(
            findings[0].operation.contains("CONCURRENTLY"),
            "finding should mention CONCURRENTLY"
        );
        assert!(
            findings[0]
                .next_action
                .contains("run_in_transaction = false"),
            "next_action should guide user to metadata.toml"
        );
    }

    #[test]
    fn concurrent_index_with_run_in_transaction_false_is_not_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("metadata.toml"),
            "run_in_transaction = false\n",
        )
        .unwrap();

        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert!(
            findings.is_empty(),
            "correctly opted-out CONCURRENTLY should produce no additional findings"
        );
    }

    #[test]
    fn concurrent_index_with_run_in_transaction_false_no_spaces_is_not_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("metadata.toml"),
            "run_in_transaction=false\n",
        )
        .unwrap();

        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert!(
            findings.is_empty(),
            "TOML `run_in_transaction=false` (no spaces) should also suppress the finding"
        );
    }

    #[test]
    fn concurrent_index_with_commented_out_flag_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("metadata.toml"),
            "# run_in_transaction = false\n",
        )
        .unwrap();

        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(
            findings.len(),
            1,
            "a commented-out opt-out should NOT suppress the finding"
        );
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn concurrent_index_with_metadata_toml_missing_flag_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("metadata.toml"),
            "# Diesel migration metadata\n",
        )
        .unwrap();

        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn non_concurrent_index_is_not_flagged_by_opt_out_check() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        let sql = "CREATE INDEX idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert!(
            findings.is_empty(),
            "non-CONCURRENTLY index should not be flagged by opt-out check"
        );
    }

    #[test]
    fn concurrent_index_in_sql_comment_is_not_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        // The concurrent index is only mentioned in a comment; the actual
        // statement is a plain (non-concurrent) CREATE INDEX.
        let sql = "-- TODO: switch to CREATE INDEX CONCURRENTLY once traffic drops\n\
                   CREATE INDEX idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert!(
            findings.is_empty(),
            "a CONCURRENTLY reference inside a SQL comment must not trigger the opt-out check"
        );
    }

    #[test]
    fn concurrent_unique_index_without_metadata_toml_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_unique_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        let sql = "CREATE UNIQUE INDEX CONCURRENTLY idx_posts_slug ON posts (slug);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn concurrent_index_multiline_without_metadata_toml_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        let sql = "CREATE INDEX\n  CONCURRENTLY idx_posts_title ON posts (title);";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(
            findings.len(),
            1,
            "multi-line CREATE INDEX CONCURRENTLY should be flagged when metadata.toml is absent"
        );
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn check_migrations_in_dir_flags_concurrent_index_without_metadata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("up.sql"),
            "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);",
        )
        .unwrap();
        std::fs::write(migration_dir.join("down.sql"), "").unwrap();

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .up
                .iter()
                .any(|f| f.operation.contains("CONCURRENTLY")),
            "missing metadata.toml should produce a CONCURRENTLY finding"
        );
    }

    #[test]
    fn drop_index_concurrently_without_metadata_toml_is_flagged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_drop_index");
        std::fs::create_dir_all(&migration_dir).unwrap();

        let sql = "DROP INDEX CONCURRENTLY idx_posts_title;";
        let mut findings = Vec::new();
        check_concurrent_index_transaction_opt_out(sql, &migration_dir, &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, safety::RiskLevel::PotentiallyBlocking);
        assert!(
            findings[0].operation.contains("CONCURRENTLY"),
            "finding should mention CONCURRENTLY"
        );
    }

    // ── startup wait resolver (red phase) ────────────────────────────────────

    #[test]
    fn resolve_startup_wait_secs_defaults_to_zero() {
        let secs =
            resolve_startup_wait_secs_from_sources(|_| Err(std::env::VarError::NotPresent), None);
        assert_eq!(secs, 0);
    }

    #[test]
    fn resolve_startup_wait_secs_from_toml() {
        let mut table = toml::Table::new();
        let mut db = toml::Table::new();
        db.insert("startup_wait_secs".to_owned(), toml::Value::Integer(45));
        table.insert("database".to_owned(), toml::Value::Table(db));
        let secs = resolve_startup_wait_secs_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            Some(&table),
        );
        assert_eq!(secs, 45);
    }

    #[test]
    fn resolve_startup_wait_secs_env_overrides_toml() {
        let mut table = toml::Table::new();
        let mut db = toml::Table::new();
        db.insert("startup_wait_secs".to_owned(), toml::Value::Integer(10));
        table.insert("database".to_owned(), toml::Value::Table(db));
        let secs = resolve_startup_wait_secs_from_sources(
            |key| {
                if key == "AUTUMN_DATABASE__STARTUP_WAIT_SECS" {
                    Ok("90".to_owned())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            },
            Some(&table),
        );
        assert_eq!(secs, 90);
    }

    #[test]
    fn resolve_startup_wait_secs_bad_env_falls_back_to_toml() {
        let mut table = toml::Table::new();
        let mut db = toml::Table::new();
        db.insert("startup_wait_secs".to_owned(), toml::Value::Integer(30));
        table.insert("database".to_owned(), toml::Value::Table(db));
        let secs = resolve_startup_wait_secs_from_sources(
            |key| {
                if key == "AUTUMN_DATABASE__STARTUP_WAIT_SECS" {
                    Ok("not_a_number".to_owned())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            },
            Some(&table),
        );
        assert_eq!(secs, 30, "bad env value should fall back to toml");
    }

    #[test]
    fn check_migrations_in_dir_concurrent_index_with_metadata_is_safe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migration_dir = tmp.path().join("20260104000000_add_index");
        std::fs::create_dir_all(&migration_dir).unwrap();
        std::fs::write(
            migration_dir.join("up.sql"),
            "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);",
        )
        .unwrap();
        std::fs::write(migration_dir.join("down.sql"), "").unwrap();
        std::fs::write(
            migration_dir.join("metadata.toml"),
            "run_in_transaction = false\n",
        )
        .unwrap();

        let results = check_migrations_in_dir(tmp.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            !results[0]
                .up
                .iter()
                .any(|f| f.operation.contains("CONCURRENTLY")),
            "opted-out CONCURRENTLY should not produce a transaction opt-out finding"
        );
    }
}
