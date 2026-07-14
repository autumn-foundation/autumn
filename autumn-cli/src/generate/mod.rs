//! `autumn generate` — code scaffolding for models, migrations, and CRUD.
//!
//! Generators emit idiomatic Autumn code (`#[model]`, `#[repository]`, route
//! handlers, Maud templates, Diesel migrations) so users do not hand-write
//! the same five files every time they add a resource.
//!
//! Three subcommands live here:
//! - [`model::plan_model_with_options`] — model + migration + schema entry
//! - [`migration::plan_migration_with_options`] — migration only (with optional add/remove DSL)
//! - [`scaffold::plan_scaffold_with_options`] — model + repository + HTML routes + smoke test +
//!   `routes![]` registration

pub mod admin;
pub mod auth;
pub mod channel;
pub mod config;
pub mod controller;
pub mod dsl;
pub mod emit;
pub mod inbound_mail;
pub mod introspect;
pub mod job;
pub mod mailer;
pub mod migration;
pub mod model;
pub mod naming;
pub mod plugin;
pub mod policy;
pub mod pwa;
pub mod scaffold;
pub mod schema_edit;
pub mod system_test;
pub mod task;
pub mod tauri;
pub mod tauri_mobile;
pub mod wizard;

use std::path::{Path, PathBuf};

/// Errors that can occur during code generation.
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// One or more files would be overwritten and `--force` was not given.
    #[error("would overwrite existing file(s):\n{}", format_collisions(.0))]
    Collisions(Vec<PathBuf>),

    /// The resource name is not a valid Rust identifier.
    #[error("invalid resource name '{0}': {1}")]
    InvalidName(String, String),

    /// A field-DSL token (`name:Type`) failed to parse.
    #[error("invalid field '{token}': {reason}")]
    InvalidField {
        /// The original `field:Type` token from the command line.
        token: String,
        /// Why parsing failed.
        reason: String,
    },

    /// The current working directory is not an Autumn project root.
    #[error("not inside an Autumn project (no Cargo.toml found in current directory)")]
    NotInProject,

    /// Filesystem error during code emission.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// Generator config file is invalid or missing a required section.
    #[error("{0}")]
    Config(String),

    /// `autumn destroy` refuses to remove file(s) whose content has diverged
    /// from what the matching `generate` invocation would have produced
    /// (issue #1048) — pass `--force` to override.
    #[error(
        "refusing to destroy — file(s) diverged from generated content, pass --force to override:\n{}",
        format_collisions(.0)
    )]
    Diverged(Vec<PathBuf>),
}

/// ⚡ Bolt optimization: Formats collision paths directly into a pre-allocated
/// String buffer to avoid multiple intermediate `String` and `Vec` allocations.
fn format_collisions(paths: &[PathBuf]) -> String {
    use std::fmt::Write;
    // Estimate ~60 bytes per path
    let mut out = String::with_capacity(paths.len() * 60);
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write!(out, "  {}", p.display().to_string().replace('\\', "/")).unwrap();
    }
    out
}

/// The generate-time rejection for Postgres full-text search (`--search` /
/// `#[searchable]`) on a `SQLite`-backed app (`SQLite` foundation, issue #1614).
///
/// Postgres FTS is emitted as a `tsvector` generated column plus a GIN index,
/// neither of which `SQLite` has. `SQLite`'s own full-text search (FTS5) is a
/// later slice tracked in issue #1910, so rather than emit Postgres-only DDL
/// that would break on `SQLite`, generation fails here with an actionable
/// message (AC #4).
#[must_use]
pub fn sqlite_search_unsupported_error() -> GenerateError {
    GenerateError::Config(
        "full-text search (--search / #[searchable]) is not yet supported on SQLite apps; \
         Postgres FTS uses a tsvector generated column and a GIN index, which SQLite lacks. \
         SQLite FTS5 support is tracked in https://github.com/madmax983/autumn/issues/1910 — \
         re-run without the search flag, or target a Postgres database."
            .to_owned(),
    )
}

/// Common flags shared by every `generate` subcommand.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    /// Print the list of files that would be created/modified, then exit.
    pub dry_run: bool,
    /// Overwrite existing files instead of erroring on collision.
    pub force: bool,
}

/// Verify we are at an Autumn project root by checking for `Cargo.toml`.
pub fn ensure_project_root(dir: &Path) -> Result<(), GenerateError> {
    if dir.join("Cargo.toml").is_file() {
        Ok(())
    } else {
        Err(GenerateError::NotInProject)
    }
}

/// Generate a 14-digit UTC timestamp prefix, matching Diesel's convention
/// (`YYYYMMDDHHMMSS`).
pub fn timestamp_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    timestamp_from_unix(secs)
}

/// Convert a Unix timestamp (seconds) to a `YYYYMMDDHHMMSS` string.
///
/// Pure function — extracted so tests can pin a deterministic timestamp
/// without mocking the system clock.
#[must_use]
pub fn timestamp_from_unix(unix_secs: u64) -> String {
    // Days since 1970-01-01.
    let days = unix_secs / 86_400;
    let rem = unix_secs % 86_400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    let (y, m, d) = ymd_from_days(days);
    format!("{y:04}{m:02}{d:02}{hour:02}{minute:02}{second:02}")
}

/// Civil-date conversion (days-since-epoch → year/month/day) using Howard
/// Hinnant's algorithm — fully self-contained, no chrono dependency.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "the input range (Unix seconds within Diesel's reasonable lifetime) is far\
              below i64::MAX/2, so the i64↔u64 round-trip stays within bounds."
)]
const fn ymd_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    let z = days_since_epoch as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

/// Read a file to `String`, returning an empty string if the file does not exist.
pub fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Determine the target app's database backend at generate time (`SQLite`
/// foundation, issue #1614).
///
/// Resolves the primary database URL for `project_root` the same way `autumn
/// migrate` does — the `AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL`
/// / `DATABASE_URL` environment variables take precedence, then
/// `<project_root>/autumn.toml`'s `[database].primary_url` / `url` — and
/// classifies it via [`autumn_web::config::DatabaseBackend::detect`].
///
/// Defaults to [`DatabaseBackend::Postgres`] when no URL can be resolved or its
/// shape is unrecognized, preserving today's Postgres-only generator behavior
/// with no regression. The emitted DDL / diesel schema is then made
/// backend-aware so no generator output compiles against Postgres but breaks on
/// `SQLite` (AC #4).
#[must_use]
pub fn detect_backend(project_root: &Path) -> autumn_web::config::DatabaseBackend {
    use autumn_web::config::DatabaseBackend;

    let table = std::fs::read_to_string(project_root.join("autumn.toml"))
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok());
    crate::migrate::resolve_primary_database_url_from_sources(|k| std::env::var(k), table.as_ref())
        .as_deref()
        .and_then(DatabaseBackend::detect)
        .unwrap_or(DatabaseBackend::Postgres)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format_is_14_digits() {
        let ts = timestamp_now();
        assert_eq!(ts.len(), 14);
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn timestamp_2026_04_27_known_value() {
        // 2026-04-27T00:00:00Z = 1_777_248_000.
        let ts = timestamp_from_unix(1_777_248_000);
        assert_eq!(ts, "20260427000000");
    }

    #[test]
    fn timestamp_1970_epoch() {
        assert_eq!(timestamp_from_unix(0), "19700101000000");
    }

    #[test]
    fn timestamp_handles_leap_year() {
        // 2024-02-29T12:34:56Z = 1709210096
        assert_eq!(timestamp_from_unix(1_709_210_096), "20240229123456");
    }

    #[test]
    fn ensure_project_root_succeeds_with_cargo_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        assert!(ensure_project_root(tmp.path()).is_ok());
    }

    #[test]
    fn ensure_project_root_fails_without_cargo_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            ensure_project_root(tmp.path()).unwrap_err(),
            GenerateError::NotInProject
        ));
    }
}
