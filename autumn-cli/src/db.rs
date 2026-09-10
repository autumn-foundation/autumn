//! `autumn db` — database lifecycle commands: `create`, `drop`, and `reset`.
//!
//! These are thin orchestration over Autumn's existing config resolution and the
//! `migrate` / `seed` runners. They resolve the primary/write database URL the
//! exact same way `autumn migrate` does (defaults → `autumn.toml` →
//! `autumn-{profile}.toml` → `AUTUMN_*` / `DATABASE_URL` / `primary_url`) and
//! operate only on the single primary role. `CREATE`/`DROP DATABASE` are issued
//! from a connection to the server's maintenance database (`postgres`) because
//! they cannot run while connected to the target database.
//!
//! Production safety: the destructive operations (`drop`, `reset`) refuse to run
//! against a production profile unless `--force` is passed, and credentials are
//! never printed in output or error messages.

use std::path::Path;
use std::process::Command;

use diesel::connection::SimpleConnection as _;
use diesel::{Connection as _, PgConnection, RunQueryDsl as _, sql_query};

use crate::migrate;

/// `autumn db backup` / `autumn db restore` — logical dump/restore (issue #1595).
/// A submodule so it can reuse this module's production guard
/// ([`guard_destructive`]) verbatim.
pub mod backup;

/// Minimal synchronous S3 SigV4 client for offsite backup transfer (issue #1619).
pub mod s3;

/// `autumn db scrub` — anonymize a database (or a backup artifact) for
/// non-production use (issue #1602). A submodule so it can reuse this module's
/// production guard ([`guard_destructive`]) and identifier-quoting verbatim.
pub mod scrub;

/// `autumn db replica` — restore, inspect and verify a continuously replicated
/// SQLite database (issue #1628). A submodule so it can reuse this module's
/// production guard ([`guard_destructive`]) verbatim.
pub mod replica;

/// Online-safe `SQLite` snapshot/restore behind `autumn db backup` /
/// `autumn db restore` on the `SQLite` tier (issue #1909). A submodule so it
/// sits beside the Postgres [`backup`] path it dispatches from.
pub mod sqlite_snapshot;

/// `autumn db retention` — report, dry-run, and on-demand purge of the
/// unified data-retention policy for framework-owned data (issue #1605).
/// A submodule for namespacing only: it drives the app binary rather than
/// connecting to Postgres itself, so it shares no helpers with this module.
pub mod retention;

/// The lifecycle subcommands of `autumn db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbCommand {
    /// Create the database named in the resolved connection config (idempotent).
    Create,
    /// Drop the database (idempotent; refuses prod without `force`).
    Drop { force: bool },
    /// Drop → create → migrate → seed in one shot (refuses prod without `force`).
    Reset { force: bool },
}

/// Failure modes for the `db` commands. `Display` is deliberately
/// credential-safe: no variant ever embeds the resolved URL, only the parsed
/// host/port and database name, consistent with `autumn doctor --strict`.
#[derive(Debug)]
enum DbError {
    /// No database URL could be resolved from config or environment.
    NoUrl,
    /// The resolved URL could not be parsed or names no database.
    UnparsableUrl,
    /// Could not connect to the maintenance database. Carries only the parsed
    /// host/port/db — never the credentials from the URL.
    Connection { host: String, port: u16, db: String },
    /// A SQL statement failed. The message comes from the server (statement
    /// errors like "database is being accessed by other users"), never the URL.
    Sql(String),
    /// A destructive op was refused because the active profile is production
    /// and `--force` was not supplied.
    ProductionRefused { profile: String },
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoUrl => write!(
                f,
                "No database URL found.\n  Set database.primary_url (or database.url) in autumn.toml, \
                 or set AUTUMN_DATABASE__PRIMARY_URL / AUTUMN_DATABASE__URL / DATABASE_URL."
            ),
            Self::UnparsableUrl => write!(
                f,
                "The resolved database URL could not be parsed or does not name a database."
            ),
            Self::Connection { host, port, db } => write!(
                f,
                "Could not connect to the Postgres maintenance database for {db:?} at {host}:{port}.\n  \
                 Is the server running and reachable?"
            ),
            Self::Sql(message) => write!(f, "{message}"),
            Self::ProductionRefused { profile } => write!(
                f,
                "Refusing to run a destructive database operation against the {profile:?} profile.\n  \
                 Re-run with --force if you really mean it."
            ),
        }
    }
}

/// Entry point dispatched from `main`. Prints a credential-safe message and
/// exits non-zero on failure.
pub fn run(command: &DbCommand, profile: Option<&str>) {
    eprintln!("\u{1F342} autumn db\n");
    let result = match command {
        DbCommand::Create => create(profile),
        DbCommand::Drop { force } => drop(profile, *force),
        DbCommand::Reset { force } => reset(profile, *force),
    };
    if let Err(e) = result {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

/// `autumn db create` — create the configured database, idempotently.
fn create(profile: Option<&str>) -> Result<(), DbError> {
    let url = resolve_url(profile)?;
    let MaintenanceTarget {
        db_name,
        maintenance_url,
        host,
        port,
    } = maintenance_target(&url)?;
    let mut conn = connect(&maintenance_url, &host, port, &db_name)?;

    if database_exists(&mut conn, &db_name)? {
        eprintln!("  \u{2139} Database {db_name:?} already exists \u{2014} nothing to do.");
        return Ok(());
    }
    conn.batch_execute(&format!("CREATE DATABASE {}", quote_ident(&db_name)))
        .map_err(|e| DbError::Sql(e.to_string()))?;
    eprintln!("  \u{2713} Created database {db_name:?}.");
    Ok(())
}

/// `autumn db drop` — drop the configured database, idempotently. Refuses to run
/// outside the `dev`/`test` profile unless `force` is set.
fn drop(profile: Option<&str>, force: bool) -> Result<(), DbError> {
    let resolved = migrate::effective_profile(profile);
    guard_destructive(&resolved, force)?;

    let url = resolve_url(profile)?;
    let MaintenanceTarget {
        db_name,
        maintenance_url,
        host,
        port,
    } = maintenance_target(&url)?;
    let mut conn = connect(&maintenance_url, &host, port, &db_name)?;

    if !database_exists(&mut conn, &db_name)? {
        eprintln!("  \u{2139} Database {db_name:?} does not exist \u{2014} nothing to do.");
        return Ok(());
    }
    conn.batch_execute(&format!("DROP DATABASE {}", quote_ident(&db_name)))
        .map_err(|e| DbError::Sql(e.to_string()))?;
    eprintln!("  \u{2713} Dropped database {db_name:?}.");
    Ok(())
}

/// `autumn db reset` — drop → create → migrate → seed, as a single command.
///
/// The four steps are run by self-invoking this same binary so each reuses the
/// existing runners verbatim and reports its own exit status; reset stops at the
/// first failure and names the step that failed. The seed step is skipped (with
/// a notice) when `src/bin/seed.rs` is absent.
fn reset(profile: Option<&str>, force: bool) -> Result<(), DbError> {
    let resolved = migrate::effective_profile(profile);
    // Apply the production guard once, up front, before any destructive work.
    guard_destructive(&resolved, force)?;

    // `db drop` is invoked with --force because reset has already cleared the
    // production guard above; otherwise the child would re-refuse.
    run_step("drop", &["db", "drop", "--force"], profile)?;
    run_step("create", &["db", "create"], profile)?;
    run_step("migrate", &["migrate"], profile)?;

    if seed_binary_present() {
        run_step("seed", &["seed"], profile)?;
    } else {
        eprintln!("  \u{2139} No src/bin/seed.rs found \u{2014} skipping the seed step.");
    }

    eprintln!(
        "\n\u{2713} Database reset complete (drop \u{2192} create \u{2192} migrate \u{2192} seed)."
    );
    Ok(())
}

/// Run one reset step by self-invoking the `autumn` binary, forwarding the
/// profile. On a non-zero exit, fail with a message naming the step.
fn run_step(name: &str, args: &[&str], profile: Option<&str>) -> Result<(), DbError> {
    eprintln!("\u{2500}\u{2500} reset: {name} \u{2500}\u{2500}");
    let exe = std::env::current_exe()
        .map_err(|e| DbError::Sql(format!("could not locate the autumn executable: {e}")))?;
    let mut command = Command::new(exe);
    command.args(args);
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    let status = command
        .status()
        .map_err(|e| DbError::Sql(format!("failed to spawn `autumn {}`: {e}", args.join(" "))))?;
    if status.success() {
        Ok(())
    } else {
        Err(DbError::Sql(format!(
            "reset failed at the {name:?} step (`autumn {}` exited {}).",
            args.join(" "),
            status
                .code()
                .map_or_else(|| "with a signal".to_owned(), |c| format!("with code {c}")),
        )))
    }
}

/// Whether a seed binary exists at the conventional `src/bin/seed.rs` path.
fn seed_binary_present() -> bool {
    Path::new("src/bin/seed.rs").is_file()
}

/// Resolve the primary/write URL, mapping a missing URL to [`DbError::NoUrl`].
fn resolve_url(profile: Option<&str>) -> Result<String, DbError> {
    migrate::resolve_primary_url(profile).ok_or(DbError::NoUrl)
}

/// Refuse destructive operations against a production profile unless forced.
/// `dev`/`test` (and their aliases) are always allowed; any other profile —
/// including custom ones — requires `--force`, matching the issue's
/// "refuse-by-default outside dev/test" posture.
/// The production guard as a plain predicate, for submodules that carry their
/// own error type ([`replica`]). `Err(())` means "refused; needs `--force`".
pub fn guard_destructive_public(profile: &str, force: bool) -> Result<(), ()> {
    guard_destructive(profile, force).map_err(|_| ())
}

fn guard_destructive(profile: &str, force: bool) -> Result<(), DbError> {
    if force || is_safe_destructive_profile(profile) {
        Ok(())
    } else {
        Err(DbError::ProductionRefused {
            profile: profile.to_owned(),
        })
    }
}

/// Whether a profile is one the destructive ops may run against without
/// `--force` (the local-development profiles `dev`/`test`).
fn is_safe_destructive_profile(profile: &str) -> bool {
    matches!(
        profile.trim().to_ascii_lowercase().as_str(),
        "dev" | "development" | "test"
    )
}

/// The parsed pieces needed to issue `CREATE`/`DROP DATABASE` from the server's
/// maintenance database.
struct MaintenanceTarget {
    /// The target database name (decoded).
    db_name: String,
    /// A connection URL pointing at the `postgres` maintenance database on the
    /// same server, carrying the original credentials.
    maintenance_url: String,
    /// Parsed host, for credential-safe error reporting.
    host: String,
    /// Parsed port, for credential-safe error reporting.
    port: u16,
}

/// Derive the maintenance-database connection from a resolved primary URL.
///
/// `CREATE DATABASE` / `DROP DATABASE` cannot run while connected to the target
/// database (or inside a transaction), so we connect to the per-server
/// maintenance database `postgres` and keep the original credentials/host.
fn maintenance_target(url: &str) -> Result<MaintenanceTarget, DbError> {
    let parsed = url::Url::parse(url).map_err(|_| DbError::UnparsableUrl)?;

    // The database name is the first (and only meaningful) path segment.
    let db_name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next())
        .map(decode_percent)
        .filter(|name| !name.is_empty())
        .ok_or(DbError::UnparsableUrl)?;

    let host = parsed.host_str().unwrap_or("localhost").to_owned();
    let port = parsed.port().unwrap_or(5432);

    let mut maintenance = parsed;
    maintenance.set_path("/postgres");
    Ok(MaintenanceTarget {
        db_name,
        maintenance_url: maintenance.to_string(),
        host,
        port,
    })
}

/// Establish a synchronous Postgres connection to the maintenance database,
/// mapping any failure to a [`DbError::Connection`] that omits credentials.
fn connect(
    maintenance_url: &str,
    host: &str,
    port: u16,
    db_name: &str,
) -> Result<PgConnection, DbError> {
    PgConnection::establish(maintenance_url).map_err(|_| DbError::Connection {
        host: host.to_owned(),
        port,
        db: db_name.to_owned(),
    })
}

/// A single boolean column produced by the existence probe.
#[derive(diesel::QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    exists: bool,
}

/// Whether a database with `db_name` exists, via `pg_database`.
fn database_exists(conn: &mut PgConnection, db_name: &str) -> Result<bool, DbError> {
    let query = format!(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = {}) AS exists",
        quote_literal(db_name)
    );
    let row: ExistsRow = sql_query(query)
        .get_result(conn)
        .map_err(|e| DbError::Sql(e.to_string()))?;
    Ok(row.exists)
}

/// Quote a Postgres identifier (database name) for safe interpolation, doubling
/// any embedded double quotes.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a value as a single-quoted SQL string literal, doubling embedded
/// single quotes. Shared with `db_pull` (catalog/identifier values only — the
/// connection URL is never interpolated).
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Minimal percent-decoding for a URL path segment (database name).
fn decode_percent(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let Ok(byte) = u8::try_from(hi * 16 + lo)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_target_extracts_name_and_rewrites_to_postgres() {
        let target = maintenance_target("postgres://user:pw@db.example.com:6543/my_app").unwrap();
        assert_eq!(target.db_name, "my_app");
        assert_eq!(target.host, "db.example.com");
        assert_eq!(target.port, 6543);
        assert!(
            target.maintenance_url.ends_with("/postgres"),
            "maintenance url should point at the postgres database: {}",
            target.maintenance_url
        );
    }

    #[test]
    fn maintenance_target_defaults_port_and_handles_no_credentials() {
        let target = maintenance_target("postgres://localhost/my_app").unwrap();
        assert_eq!(target.db_name, "my_app");
        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 5432);
    }

    #[test]
    fn maintenance_target_decodes_percent_encoded_name() {
        let target = maintenance_target("postgres://localhost/my%20app").unwrap();
        assert_eq!(target.db_name, "my app");
    }

    #[test]
    fn maintenance_target_rejects_url_without_database_name() {
        assert!(matches!(
            maintenance_target("postgres://localhost/"),
            Err(DbError::UnparsableUrl)
        ));
        assert!(matches!(
            maintenance_target("not a url"),
            Err(DbError::UnparsableUrl)
        ));
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("my_app"), "\"my_app\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn quote_literal_doubles_embedded_single_quotes() {
        assert_eq!(quote_literal("my_app"), "'my_app'");
        assert_eq!(quote_literal("o'brien"), "'o''brien'");
    }

    #[test]
    fn guard_allows_dev_and_test_without_force() {
        for profile in ["dev", "development", "DEV", "test", "Test"] {
            assert!(
                guard_destructive(profile, false).is_ok(),
                "{profile} should be allowed"
            );
        }
    }

    #[test]
    fn guard_refuses_prod_and_custom_without_force() {
        for profile in ["prod", "production", "staging", "anything-else"] {
            assert!(
                matches!(
                    guard_destructive(profile, false),
                    Err(DbError::ProductionRefused { .. })
                ),
                "{profile} should be refused without --force"
            );
            assert!(
                guard_destructive(profile, true).is_ok(),
                "{profile} should pass with --force"
            );
        }
    }

    #[test]
    fn errors_never_leak_credentials() {
        // The connection error carries only host/port/db, never the password.
        let err = DbError::Connection {
            host: "db.example.com".to_owned(),
            port: 5432,
            db: "my_app".to_owned(),
        };
        let rendered = err.to_string();
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("db.example.com"));
        assert!(rendered.contains("my_app"));

        // The production-refusal message names the profile but no URL.
        let refused = DbError::ProductionRefused {
            profile: "prod".to_owned(),
        };
        assert!(refused.to_string().contains("prod"));
        assert!(!refused.to_string().contains("postgres://"));
    }
}
