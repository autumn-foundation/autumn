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
pub mod commentable;
pub mod config;
pub mod controller;
pub mod counter_cache;
pub mod dsl;
pub mod emit;
pub mod inbound_mail;
pub mod introspect;
pub mod job;
pub mod mailer;
pub mod migration;
pub mod model;
pub mod naming;
mod nested;
pub mod notifications;
pub mod plugin;
pub mod policy;
pub mod prior_index;
pub mod provenance;
pub mod pwa;
pub mod scaffold;
mod scaffold_i18n;
pub mod schema_edit;
pub mod system_test;
pub mod task;
pub mod tauri;
pub mod tauri_mobile;
pub mod teams;
pub mod webhook;
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

    /// `autumn destroy` refuses to remove file(s) whose content matches
    /// neither what the matching `generate` invocation produces now nor the
    /// digest `generate` recorded when it wrote them (issues #1048, #1835) —
    /// pass `--force` to override.
    #[error(
        "refusing to destroy — file(s) match neither the current generator output nor the \
         digest recorded in {}; edit them back, or pass --force to delete them anyway:\n{}",
        crate::generate::provenance::MANIFEST_PATH,
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

/// The generate-time rejection for a UUID primary key (`--id uuid`) on a
/// `SQLite`-backed app (`SQLite` foundation, issue #1614 AC #4).
///
/// Postgres emits `UUID PRIMARY KEY DEFAULT gen_random_uuid()`, so ids are
/// server-generated. `SQLite` has no `uuid` type nor a `gen_random_uuid()`
/// default, and the `#[model]` macro's generated `New*` insert type omits
/// `#[id]` fields — so on `SQLite` an inserted row would get a NULL/omitted id
/// (a non-integer `PRIMARY KEY` is not implicitly `NOT NULL` in `SQLite`).
/// Making it work needs app-side UUID generation in the repository insert
/// codegen, which the `SQLite` runtime slice deferred and #2555 now carries.
/// Rather than emit a `TEXT PRIMARY KEY` column
/// that silently accepts NULL ids, generation fails here with an actionable
/// message (AC #4).
#[must_use]
/// `--id uuid` cannot be combined with `comments:commentable`.
///
/// The shared table stores `commentable_id BIGINT` and every generated helper
/// takes `parent_id: i64`, so a UUID-keyed parent has nowhere to go. Without
/// this the command SUCCEEDS and writes a project that does not compile, which
/// is the worst of both: the user is told it worked and finds out from rustc.
pub fn uuid_pk_commentable_unsupported_error() -> GenerateError {
    GenerateError::Config(
 "`comments:commentable` needs an integer primary key, so it cannot be combined with `--id uuid`. The shared `comments` table keys its rows on `commentable_id BIGINT` — one column serving every commentable model — and the generated helpers take `parent_id: i64`, so a UUID-keyed parent has no representation there. Re-run without `--id uuid` to use the default BIGINT primary key, or without `comments:commentable` and attach comments to a model that has one."
 .to_owned(),
 )
}

pub fn sqlite_uuid_pk_unsupported_error() -> GenerateError {
    GenerateError::Config(
        "UUID primary keys (--id uuid) are not yet supported on SQLite apps; Postgres uses \
         `UUID PRIMARY KEY DEFAULT gen_random_uuid()`, but SQLite has no uuid type nor a \
         gen_random_uuid() default, and the generated `New*` insert type omits `#[id]` \
         fields — so inserted rows would get NULL/omitted ids. App-side UUID generation is \
         tracked in \
         https://github.com/autumn-foundation/autumn/issues/2555 — re-run without `--id uuid` to use \
         the default INTEGER PRIMARY KEY AUTOINCREMENT, or target a Postgres database."
            .to_owned(),
    )
}

/// The generate-time rejection for a sharded model (`--sharded`) on a
/// `SQLite`-backed app (`SQLite` foundation, issue #1614 AC #4).
///
/// `--sharded` emits a `#[shard_key]` model plus (via `generate scaffold`)
/// `ShardedDb` routes and shard-aware migrations, all of which require a
/// `[[database.shards]]` topology. But `DatabaseConfig::validate_backend_consistency`
/// rejects *any* `database.shards` against a `SQLite` primary, so no valid
/// `SQLite` config can ever use a generated sharded resource. Unlike UUID ids
/// (#2555) — and unlike FTS, now supported on `SQLite` via FTS5 (#1910), and
/// every field kind, now converted (#1924) — this is **not** a deferred slice:
/// `SQLite` is single-host / single-writer, so
/// horizontal sharding is Postgres-only and permanently out of scope for
/// `SQLite` (see `docs/guide/sqlite-in-production.md`). Rather than emit a
/// resource that no `SQLite` app can boot, generation fails here with an
/// actionable message (AC #4).
#[must_use]
pub fn sqlite_sharded_unsupported_error() -> GenerateError {
    GenerateError::Config(
        "sharded models (--sharded) require the Postgres backend: SQLite is single-host, \
         single-writer, so horizontal sharding is Postgres-only and permanently out of scope \
         for SQLite. A sharded resource needs a `[[database.shards]]` topology, which config \
         validation rejects for a SQLite primary — so no SQLite app could use the generated \
         resource. Drop `--sharded`, or target a Postgres database. See \
         docs/guide/sqlite-in-production.md for SQLite's supported single-host deployment model."
            .to_owned(),
    )
}

/// The generate-time rejection for an `ALTER TABLE … ADD COLUMN … NOT NULL`
/// with no `DEFAULT` on a `SQLite`-backed app (`SQLite` foundation, issue #1614
/// AC #4).
///
/// `SQLite` rejects that statement at DDL time once the target table already
/// has rows (`Cannot add a NOT NULL column with default value NULL`), so a
/// generated `Add<Field>To<Table>` migration would fail to apply. The
/// `generate migration Add…To…` command has no way to attach a column
/// `DEFAULT`, so the only safe shapes on `SQLite` are a nullable column or a
/// column with a default — neither of which this invocation produced. Rather
/// than emit DDL that breaks on `SQLite`, generation fails here with an
/// actionable message (AC #4).
///
/// Note: this limit is specific to `ALTER TABLE ADD COLUMN`. A `NOT NULL`
/// column inside a `CREATE TABLE` (the `generate model` path) is fine on
/// `SQLite` and is unaffected.
#[must_use]
pub fn sqlite_add_not_null_without_default_error(table: &str, column: &str) -> GenerateError {
    GenerateError::Config(format!(
        "cannot add NOT NULL column `{column}` to table `{table}` on a SQLite app: SQLite \
         rejects `ALTER TABLE {table} ADD COLUMN {column} … NOT NULL` without a DEFAULT once \
         the table has rows (\"Cannot add a NOT NULL column with default value NULL\"). Make \
         the field nullable (e.g. `{column}:Option<…>`) or give it a default so the column \
         can be added safely. (A NOT NULL column is fine inside CREATE TABLE — this limit is \
         specific to ALTER TABLE ADD COLUMN.)"
    ))
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
/// / `DATABASE_URL` environment variables take precedence, then the merged,
/// **profile-aware** `autumn.toml` (base ← inline `[profile.<env>]` ←
/// `autumn-<env>.toml` overlay) `[database].primary_url` / `url` — and
/// classifies it via [`autumn_web::config::DatabaseBackend::detect`].
///
/// The active profile is resolved through
/// [`crate::migrate::effective_profile`] (`AUTUMN_ENV` preferred, then
/// `AUTUMN_PROFILE`, then release build-mode, else `dev`), so a database URL
/// that lives only in an active profile overlay (e.g. `[profile.prod.database]`
/// or `autumn-prod.toml` under `AUTUMN_ENV=prod`) is honored — matching the
/// URLs `autumn migrate` and the running app resolve. Reading only the base
/// `autumn.toml` would miss such an overlay and wrongly fall back to Postgres,
/// emitting Postgres DDL (`BIGSERIAL`, `TIMESTAMPTZ`, `NOW()`) for a `SQLite`
/// app.
///
/// Defaults to [`DatabaseBackend::Postgres`](autumn_web::config::DatabaseBackend::Postgres) when no URL can be resolved or its
/// shape is unrecognized, preserving today's Postgres-only generator behavior
/// with no regression. The emitted DDL / diesel schema is then made
/// backend-aware so no generator output compiles against Postgres but breaks on
/// `SQLite` (AC #4).
#[must_use]
pub fn detect_backend(project_root: &Path) -> autumn_web::config::DatabaseBackend {
    detect_backend_for_profile(project_root, None)
}

/// Profile-aware variant of [`detect_backend`]: resolves the project's database
/// backend for an explicit `--profile` (else the ambient profile resolution when
/// `profile` is `None`).
///
/// Schema commands that honor an explicit `--profile` (`schema migrate`,
/// `schema doctor`) must detect the backend of the SAME profile whose database
/// URL they act against — otherwise a project whose default backend differs from
/// the selected profile's would pick the wrong apply path / provider-lock. This
/// resolves the effective profile through [`crate::migrate::effective_profile`]
/// (so `profile == None` reproduces [`detect_backend`] byte-for-byte) and reuses
/// the same profile-aware config/`.env` resolution as [`detect_backend_with`].
#[must_use]
pub fn detect_backend_for_profile(
    project_root: &Path,
    profile: Option<&str>,
) -> autumn_web::config::DatabaseBackend {
    let effective = crate::migrate::effective_profile(profile);
    detect_backend_with(project_root, Some(&effective), |k| std::env::var(k))
}

/// Backend detection for the OFFLINE preflights — `migrate check`, `deploy
/// check`, `doctor` (issue #1906 review).
///
/// Resolves the URL from the same layers [`detect_backend`] does — real env,
/// the project `.env`/`.env.<profile>`, and the profile-merged `autumn.toml` —
/// but **never exits**. The hard-error `autumn.toml` reader is right for a
/// command about to migrate a real database; a malformed local config must not
/// abort an offline SQL check, nor truncate `autumn doctor --json` to zero bytes
/// mid-report.
///
/// `migrate check`'s documented contract is that a bad `.env` cannot abort it,
/// not that the file goes unread — and a `SQLite` URL living only in `.env` is a
/// shape both the runtime and the generator support, so skipping it would grade
/// valid `SQLite` SQL under Postgres rules.
///
/// Anything it cannot determine resolves to `Postgres`, the historical default.
#[must_use]
pub fn detect_backend_offline(
    project_root: &Path,
    profile: Option<&str>,
) -> autumn_web::config::DatabaseBackend {
    use autumn_web::config::DatabaseBackend;

    let effective = crate::migrate::effective_profile(profile);
    let table = crate::migrate::read_autumn_toml_table_with_profile_in_using(
        project_root,
        Some(&effective),
        crate::migrate::read_optional_toml_table,
    );
    // Same precedence as `detect_backend_with`: the real environment wins, and
    // `.env` only fills keys it does not define. A malformed `.env` is dropped
    // rather than surfaced — this is a best-effort hint, and the commands that
    // act on a real URL report it loudly themselves.
    let base = FnEnv(&|k: &str| std::env::var(k));
    let dotenv: std::collections::BTreeMap<String, String> =
        autumn_web::dotenv::resolve_dotenv_vars_in(project_root, &effective, &base)
            .unwrap_or_default()
            .into_iter()
            .collect();
    let env = |key: &str| {
        std::env::var(key).or_else(|_| {
            dotenv
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    };
    crate::migrate::resolve_primary_database_url_from_sources(env, table.as_ref())
        .as_deref()
        .and_then(DatabaseBackend::detect)
        .unwrap_or(DatabaseBackend::Postgres)
}

/// Directory-, profile-, and env-parameterized core of [`detect_backend`],
/// separated so the profile-overlay and `.env` resolution is unit-testable
/// without mutating the process-global environment.
///
/// Loads the same merged, profile-aware config table `autumn migrate` builds
/// (via [`crate::migrate::read_autumn_toml_table_with_profile_in`]), overlays
/// the project `.env` the same way (via [`autumn_web::dotenv`]), and applies the
/// same env-var precedence
/// ([`crate::migrate::resolve_primary_database_url_from_sources`]).
fn detect_backend_with<F>(
    project_root: &Path,
    profile: Option<&str>,
    env_var: F,
) -> autumn_web::config::DatabaseBackend
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    use autumn_web::config::DatabaseBackend;

    // Mirror `autumn migrate`: overlay a project `.env` UNDER the real
    // environment before resolving the DB URL, so a URL that lives only in
    // `.env` (e.g. `DATABASE_URL=sqlite://app.db`) is honored. Without this a
    // SQLite app whose URL is only in `.env` mis-detects as Postgres and the
    // generator emits Postgres DDL (`BIGSERIAL`/`NOW()`) even though migrate and
    // the running app resolve the SQLite URL. `.env` is read from
    // `project_root` (the same dir the profile-merged `autumn.toml` is read
    // from) for the resolved profile, honoring the framework's dotenv gating
    // (only the `dev`/`test` profiles auto-load unless `AUTUMN_DOTENV=1`) and
    // profile-selector exclusion. Precedence matches migrate's `DotenvOsEnv`
    // exactly: the real env (`env_var`) wins, and `.env` only fills keys it does
    // not define. A malformed `.env` is ignored for detection (a best-effort
    // hint) — `autumn migrate` still surfaces it loudly at migration time.
    let base = FnEnv(&env_var);
    let dotenv_overlay: std::collections::BTreeMap<String, String> =
        autumn_web::dotenv::resolve_dotenv_vars_in(project_root, profile.unwrap_or("dev"), &base)
            .unwrap_or_default()
            .into_iter()
            .collect();
    let env_with_dotenv = |key: &str| {
        env_var(key).or_else(|_| {
            dotenv_overlay
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    };

    let table = crate::migrate::read_autumn_toml_table_with_profile_in(project_root, profile);
    crate::migrate::resolve_primary_database_url_from_sources(env_with_dotenv, table.as_ref())
        .as_deref()
        .and_then(DatabaseBackend::detect)
        .unwrap_or(DatabaseBackend::Postgres)
}

/// Adapter turning an `Fn(&str) -> Result<String, VarError>` env-var lookup into
/// an [`autumn_web::config::Env`], so [`detect_backend_with`] can feed its
/// test-injectable env closure to the `.env` resolver without mutating (or
/// depending on) the real process environment.
struct FnEnv<F>(F);

impl<F> autumn_web::config::Env for FnEnv<F>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        (self.0)(key)
    }
}

/// The generate-time rejection for a DSL field kind whose rendered Rust model
/// type has no working diesel `SQLite` `FromSql`/`ToSql` (`SQLite` foundation,
/// issue #1614 AC #4; conversions extended in #1924).
///
/// The `SQLite` column/schema mapping ([`dsl::FieldKind::sqlite_schema_type`])
/// changes the DDL and diesel sql-type, but the `#[model]` struct field still
/// renders as a Rust type that needs a working conversion. As of #1924 every
/// DSL kind has one, so this never fires today — it stays as the actionable
/// message a NEW kind gets if it returns `false` from
/// [`dsl::FieldKind::sqlite_has_diesel_conversion`] rather than emit code that
/// cannot compile (AC #4).
#[must_use]
pub fn sqlite_field_kind_unsupported_error(field: &str, rust_type: &str) -> GenerateError {
    GenerateError::Config(format!(
        "field `{field}` has no working diesel SQLite conversion for its Rust type \
         `{rust_type}`: diesel implements no FromSql/ToSql for that type on its SQLite \
         backend in a generated app's feature set, so a generated SQLite app using this \
         field would fail to compile. Supported SQLite field kinds are: {kinds}. \
         Use a supported field kind, or target a Postgres database.",
        kinds = dsl::SQLITE_SUPPORTED_KINDS,
    ))
}

/// Reject any field whose kind lacks a working diesel `SQLite` conversion
/// (issue #1614 AC #4; #1924) before any DDL / model / schema is emitted for a
/// `SQLite`-backed app. Bubbles the first offending field as a
/// [`sqlite_field_kind_unsupported_error`]. Postgres callers never invoke this,
/// so their output is unaffected.
///
/// Every kind converts as of #1924, so this is a standing guard rather than an
/// active gate — see [`dsl::FieldKind::sqlite_has_diesel_conversion`].
///
/// # Errors
/// Returns [`GenerateError::Config`] for the first field whose kind has no
/// working diesel `SQLite` conversion.
pub fn reject_sqlite_unsupported_field_kinds(fields: &[dsl::Field]) -> Result<(), GenerateError> {
    for f in fields {
        if !f.kind.sqlite_has_diesel_conversion() {
            // `Field::rust_type` (not `FieldKind::rust_type`) so an `Enum`
            // field reports its generated enum type name rather than the bare
            // `String` storage-representation fallback.
            return Err(sqlite_field_kind_unsupported_error(&f.name, &f.rust_type()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first spelling in `sql` that does not belong in `SQLite` DDL, if any.
    ///
    /// The type names are the silent half of the problem: `SQLite` parses an
    /// unknown type rather than rejecting it, so only a scan catches them. That
    /// cuts both ways, so the list covers two kinds of leak:
    ///
    /// - Spellings `SQLite` has no notion of at all (`BIGSERIAL`, `TIMESTAMPTZ`,
    ///   `JSONB`) — silently the wrong storage class.
    /// - Spellings `SQLite` DOES recognise but which resolve to the wrong
    ///   affinity for the column's diesel sql-type. `TIMESTAMP` is the trap:
    ///   `SQLite` gives it NUMERIC affinity, so an ISO-8601 string stored in it
    ///   is not what the column claims to hold. Timestamps must be `TEXT` here,
    ///   which is the convention `auth.rs` and `dsl.rs` already follow.
    ///
    /// `BIGINT` is deliberately NOT listed: it takes integer affinity, the same
    /// as `INTEGER`, so it reads back without drift (see `counter_cache`, which
    /// emits it on both backends on purpose).
    ///
    /// `--` comments are ignored wherever they start on a line, since they
    /// discuss both dialects. Matches on word boundaries rather than bare
    /// substrings, so a column named `serial_number` does not trip `SERIAL`.
    fn postgres_only_leak(sql: &str) -> Option<&'static str> {
        const NOT_SQLITE: &[&str] = &[
            "BIGSERIAL",
            "SERIAL",
            "TIMESTAMPTZ",
            "TIMESTAMP",
            "JSONB",
            "BYTEA",
            "DOUBLE PRECISION",
            "NOW()",
            "GEN_RANDOM_UUID",
            "TSVECTOR",
            "USING GIN",
            "USING GIST",
        ];
        let statements = sql
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .to_uppercase();
        NOT_SQLITE
            .iter()
            .copied()
            .find(|token| contains_word(&statements, token))
    }

    /// The ban list rejects what it exists for and tolerates what it must.
    ///
    /// Without this, [`postgres_only_leak`]'s list could be trimmed to nothing
    /// and every caller would still pass.
    #[test]
    fn postgres_only_leak_flags_leaks_and_spares_lookalikes() {
        for bad in [
            "id BIGSERIAL PRIMARY KEY",
            "id SERIAL PRIMARY KEY",
            "at TIMESTAMPTZ NOT NULL",
            "at TIMESTAMP NOT NULL",
            "meta JSONB NOT NULL",
            "blob BYTEA",
            "x DOUBLE PRECISION",
            "at TEXT NOT NULL DEFAULT NOW()",
            "id TEXT DEFAULT gen_random_uuid()",
            "CREATE INDEX i ON t USING GIN (v)",
        ] {
            assert!(
                postgres_only_leak(bad).is_some(),
                "must be flagged as non-SQLite: {bad}"
            );
        }
        for good in [
            // Word-boundary cases: identifiers that merely CONTAIN a banned word.
            "serial_number TEXT NOT NULL",
            "bigserial_legacy_id INTEGER",
            "timestamp_source TEXT",
            // Affinity-equivalent, deliberately allowed.
            "comment_count BIGINT NOT NULL DEFAULT 0",
            // The dialect we actually want.
            "id INTEGER PRIMARY KEY AUTOINCREMENT",
            "created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
            // Comments discuss both dialects, wherever they start on the line.
            "-- was BIGSERIAL and DEFAULT NOW() on Postgres",
            "id INTEGER PRIMARY KEY, -- BIGSERIAL on Postgres",
        ] {
            assert_eq!(
                postgres_only_leak(good),
                None,
                "must be accepted as valid SQLite DDL: {good}"
            );
        }
    }

    /// Fail if `sql` carries a spelling that does not belong in `SQLite` DDL.
    fn assert_no_postgres_only_sql(label: &str, dir: &std::path::Path, sql: &str) {
        if let Some(token) = postgres_only_leak(sql) {
            panic!(
                "`generate {label}` leaked `{token}`, which is not valid SQLite DDL, into \
                 the SQLite migration at {}:\n{sql}",
                dir.display()
            );
        }
    }

    /// Whether `haystack` contains `needle` bounded by non-identifier characters
    /// on both sides, so `SERIAL` does not match inside `serial_number`.
    ///
    /// A trailing `)` or `(` in the needle (`NOW()`) is already a boundary, so
    /// only the leading edge is checked in that case.
    fn contains_word(haystack: &str, needle: &str) -> bool {
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let bounded_right = !needle.ends_with(ident);
        haystack.match_indices(needle).any(|(at, _)| {
            let before_ok = haystack[..at].chars().next_back().is_none_or(|c| !ident(c));
            let after_ok = bounded_right
                || haystack[at + needle.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !ident(c));
            before_ok && after_ok
        })
    }

    /// A minimal Autumn project configured for `SQLite`, for the audit below.
    ///
    /// `src/main.rs` carries a shared 4-arg `pub fn layout` — what `autumn new`
    /// emits, and what the `auth` and `pwa` preflights both require.
    fn sqlite_project() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.7\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "use autumn_web::prelude::*;\n\n\
             pub fn layout(title: &str, current_path: &str, flash: maud::Markup, \
             content: maud::Markup) -> maud::Markup {\n\
             \x20   let _ = (current_path, flash);\n\
             \x20   maud::html! { title { (title) } (content) }\n\
             }\n\n\
             #[autumn_web::main]\n\
             async fn main() {\n\
             \x20   autumn_web::app().routes(routes![]).run().await;\n\
             }\n",
        )
        .unwrap();
        tmp
    }

    /// Fail if any generated `.rs` file under `root` names a diesel construct or
    /// type that has no `SQLite` implementation.
    ///
    /// The DDL is only half of "works on `SQLite`". `generate teams` shipped 19
    /// `.for_update()` calls, and diesel implements the locking clause for
    /// Postgres and `MySQL` only — so the migration applied while the generated
    /// crate would not compile (issue #1927). Applying SQL cannot see that, and
    /// building a generated crate per generator is far too slow for a unit test,
    /// so the constructs whose absence on `SQLite` is known are scanned for
    /// instead. Use `::autumn_web::maybe_for_update!` (a plain read on `SQLite`)
    /// rather than `.for_update()`, and `::autumn_web::RuntimeConnection` /
    /// `RuntimeBackend` rather than naming the Postgres ones.
    fn assert_no_postgres_only_rust(label: &str, root: &std::path::Path) {
        const POSTGRES_ONLY: &[(&str, &str)] = &[
            (
                ".for_update()",
                "use `::autumn_web::maybe_for_update!` instead",
            ),
            (".skip_locked()", "SQLite has no locking clause"),
            (".no_wait()", "SQLite has no locking clause"),
            ("AsyncPgConnection", "use `::autumn_web::RuntimeConnection`"),
            ("diesel::pg::", "use `::autumn_web::RuntimeBackend`"),
            (
                "on_constraint(",
                "SQLite has no `ON CONFLICT ON CONSTRAINT`",
            ),
        ];
        let mut stack = vec![root.join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for (token, remedy) in POSTGRES_ONLY {
                        assert!(
                            !source.contains(token),
                            "`generate {label}` emitted `{token}`, which has no SQLite \
                             implementation, into {} — {remedy}",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    /// Plan one generator against the `SQLite` project at `root`, then apply and
    /// roll back every migration it emits on a real in-memory `SQLite`.
    fn audit_one_generator(
        label: &str,
        plan: fn(&std::path::Path) -> Result<crate::generate::emit::Plan, GenerateError>,
        root: &std::path::Path,
    ) {
        use diesel::connection::SimpleConnection as _;
        use diesel::prelude::*;

        plan(root)
            .unwrap_or_else(|e| panic!("`generate {label}` must plan on a SQLite app: {e}"))
            .execute(Flags::default())
            .unwrap_or_else(|e| panic!("`generate {label}` plan must execute: {e}"));

        assert_no_postgres_only_rust(label, root);

        // Diesel applies migration directories in version order, so the audit
        // does too — and rolls back in the reverse.
        let mut dirs: Vec<_> = std::fs::read_dir(root.join("migrations"))
            .unwrap_or_else(|e| panic!("`generate {label}` must emit a migrations dir: {e}"))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("up.sql").is_file())
            .collect();
        dirs.sort();
        assert!(
            !dirs.is_empty(),
            "`generate {label}` emitted no migration to audit"
        );

        let mut conn = diesel::SqliteConnection::establish(":memory:").expect("in-memory sqlite");
        for dir in &dirs {
            let up = std::fs::read_to_string(dir.join("up.sql")).unwrap();
            conn.batch_execute(&up).unwrap_or_else(|e| {
                panic!(
                    "`generate {label}` emitted up.sql that SQLite refuses ({}): {e}\n{up}",
                    dir.display()
                )
            });
            assert_no_postgres_only_sql(label, dir, &up);
        }
        for dir in dirs.iter().rev() {
            let down = std::fs::read_to_string(dir.join("down.sql")).unwrap();
            assert_no_postgres_only_sql(label, dir, &down);
            conn.batch_execute(&down).unwrap_or_else(|e| {
                panic!(
                    "`generate {label}` emitted down.sql that SQLite refuses ({}): {e}\n{down}",
                    dir.display()
                )
            });
        }
    }

    /// The issue-#1927 audit, kept as a test rather than a one-time sweep.
    ///
    /// `auth`, `mailer --list-unsubscribe`, `teams` and `commentable` hand-write
    /// their `CREATE TABLE` literals rather than deriving them from a model's
    /// fields, and that is the shape that shipped Postgres-only SQL:
    /// `BIGSERIAL`, `NOW()`, `TIMESTAMPTZ`. `notifications` and `pwa` derive
    /// theirs through `schema_edit`, so they are backend-aware by construction;
    /// they ride along here because a singleton table is cheap to check and the
    /// roster is then simply "every generator that emits a migration of its own".
    ///
    /// Each is planned against a `SQLite` app; every migration it emits is
    /// applied — and rolled back — on a real in-memory `SQLite`, and the Rust it
    /// emits is scanned for constructs `SQLite` has no diesel implementation
    /// for. A new hand-written table cannot regress the audit unnoticed.
    ///
    /// Applying the DDL is necessary but NOT sufficient, so the emitted SQL is
    /// also scanned for Postgres-only spellings. `SQLite` accepts any unknown
    /// type name (it falls back to BLOB affinity), so `id BIGSERIAL PRIMARY
    /// KEY` parses cleanly there and simply stops auto-incrementing — the exact
    /// silent breakage this issue is about. Only the Postgres-only *syntax*
    /// (`NOW()`, `USING GIN`) fails loudly on its own.
    ///
    /// A generator added to this family belongs in this list. `counter_cache` is
    /// deliberately absent: its `ALTER TABLE … ADD COLUMN … BIGINT NOT NULL
    /// DEFAULT 0` / `DROP COLUMN` is already portable (`SQLite` gives `BIGINT`
    /// integer affinity, so it reads back without drift) and it emits no
    /// `CREATE TABLE` of its own.
    #[test]
    fn hand_written_ddl_generators_emit_applicable_sqlite_ddl() {
        const TS: &str = "20260101000000";

        // The roster is the audit's scope, so losing an entry must not be
        // silent — deleting one below fails the equality further down rather
        // than quietly shrinking what gets checked. Adding a generator that
        // emits its own migration means adding it to BOTH lists.
        const AUDITED: &[&str] = &[
            "auth",
            "mailer --list-unsubscribe",
            "notifications",
            "pwa",
            "teams",
            "model … comments:commentable",
        ];

        type Planner = fn(&std::path::Path) -> Result<crate::generate::emit::Plan, GenerateError>;

        let generators: &[(&str, Planner)] = &[
            ("auth", |root| {
                // Every optional table too: recovery codes, magic-link tokens,
                // OAuth identities, WebAuthn credentials.
                crate::generate::auth::plan_auth_full_ex2(
                    root,
                    "User",
                    TS,
                    &crate::generate::auth::AuthOAuthOptions {
                        providers: vec!["github".to_owned()],
                    },
                    true,
                    true,
                    true,
                )
            }),
            ("mailer --list-unsubscribe", |root| {
                crate::generate::mailer::plan_mailer(root, "Welcome", Some("news"), false)
            }),
            (
                "notifications",
                crate::generate::notifications::plan_notifications,
            ),
            ("pwa", crate::generate::pwa::plan_pwa),
            ("teams", |root| {
                crate::generate::teams::plan_teams(root, TS, false)
            }),
            // `comments:commentable` brings the shared `comments` table and its
            // parent-cleanup TRIGGER alongside the model's own migration — the
            // only generated DDL in this family that is not a bare table.
            ("model … comments:commentable", |root| {
                crate::generate::model::plan_model(
                    root,
                    "Post",
                    &["title:String".to_owned(), "comments:commentable".to_owned()],
                    TS,
                )
            }),
        ];

        assert_eq!(
            generators
                .iter()
                .map(|(label, _)| *label)
                .collect::<Vec<_>>(),
            AUDITED,
            "the audited-generator roster changed; update AUDITED alongside it"
        );

        for (label, plan) in generators {
            temp_env::with_vars(
                [
                    ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                    ("AUTUMN_DATABASE__URL", None::<&str>),
                    ("DATABASE_URL", None::<&str>),
                ],
                || {
                    let tmp = sqlite_project();
                    audit_one_generator(label, *plan, tmp.path());
                },
            );
        }
    }

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

    // ── Profile-aware backend detection (issue #1614) ──────────────────────

    /// An empty environment — no `AUTUMN_DATABASE__*` / `DATABASE_URL` set — so
    /// `detect_backend_with` resolves purely from the merged config table.
    fn no_env(_key: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    /// A `SQLite` URL that lives only in an active profile overlay
    /// (`[profile.prod.database]`, selected by `AUTUMN_ENV=prod`) must be
    /// detected as `SQLite` — the same merged, profile-aware config `autumn
    /// migrate` resolves — so generation emits `SQLite` DDL, not Postgres.
    #[test]
    fn detect_backend_honors_sqlite_url_in_active_profile_overlay() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n\n\
             [profile.prod.database]\nprimary_url = \"sqlite://prod.db\"\n",
        )
        .unwrap();
        // With the prod profile active, the overlay's SQLite URL wins.
        assert_eq!(
            detect_backend_with(tmp.path(), Some("prod"), no_env),
            DatabaseBackend::Sqlite
        );
        // Without the prod profile, the base Postgres URL is used.
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Postgres
        );
    }

    /// A `SQLite` URL in a separate `autumn-<profile>.toml` overlay file is
    /// detected as `SQLite` when that profile is active.
    #[test]
    fn detect_backend_honors_sqlite_url_in_profile_overlay_file() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("autumn-prod.toml"),
            "[database]\nprimary_url = \"sqlite://prod.db\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("prod"), no_env),
            DatabaseBackend::Sqlite
        );
    }

    /// A base-file Postgres URL (no profile overlay) still resolves to Postgres.
    #[test]
    fn detect_backend_base_file_postgres_stays_postgres() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Postgres
        );
    }

    /// No config and no env → default to Postgres (no regression to today's
    /// Postgres-only generator behavior).
    #[test]
    fn detect_backend_no_config_defaults_to_postgres() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Postgres
        );
    }

    /// Environment variables keep their documented precedence over the merged
    /// config: a `DATABASE_URL=sqlite://…` wins over a base-file Postgres URL.
    #[test]
    fn detect_backend_env_var_overrides_config() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        let env = |key: &str| {
            if key == "DATABASE_URL" {
                Ok("sqlite://override.db".to_owned())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        };
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), env),
            DatabaseBackend::Sqlite
        );
    }

    // ── dotenv-backed backend detection (issue #1614 finding 8) ────────────

    /// A DB URL that lives ONLY in the project `.env` (no `autumn.toml` url, no
    /// process env) must be detected — `autumn migrate` overlays `.env` before
    /// resolving the URL, so the generator must too, or a `SQLite` app mis-detects
    /// as Postgres and emits `BIGSERIAL`/`NOW()` DDL.
    #[test]
    fn detect_backend_honors_sqlite_url_in_dotenv() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=sqlite://app.db\n").unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Sqlite
        );
    }

    /// A real process env var of the same key wins over `.env` — the exact
    /// precedence migrate's `DotenvOsEnv` uses (real env wins, `.env` only fills
    /// gaps). A `.env` `SQLite` URL must NOT override a Postgres `DATABASE_URL` in
    /// the (test-injected) real environment.
    #[test]
    fn detect_backend_process_env_wins_over_dotenv() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=sqlite://app.db\n").unwrap();
        let env = |key: &str| {
            if key == "DATABASE_URL" {
                Ok("postgres://localhost/app".to_owned())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        };
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), env),
            DatabaseBackend::Postgres
        );
    }

    /// The `.env`-fed env-var layer sits ABOVE the merged `autumn.toml` (matching
    /// migrate's `resolve_primary_database_url_from_sources` order): a `.env`
    /// `DATABASE_URL=sqlite://…` wins over a base-file Postgres URL.
    #[test]
    fn detect_backend_dotenv_overlays_above_autumn_toml() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=sqlite://app.db\n").unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Sqlite
        );
    }

    /// No `.env` and a base-file Postgres URL still resolves to Postgres — the
    /// dotenv overlay is empty and changes nothing.
    #[test]
    fn detect_backend_no_dotenv_stays_postgres() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("dev"), no_env),
            DatabaseBackend::Postgres
        );
    }

    /// `.env` auto-loads only for the `dev`/`test` profiles (unless
    /// `AUTUMN_DOTENV=1`), matching the framework's gating: a `.env` `SQLite` URL
    /// under an ungated `prod` profile is NOT loaded, so detection falls back to
    /// Postgres.
    #[test]
    fn detect_backend_dotenv_gated_off_for_prod_profile() {
        use autumn_web::config::DatabaseBackend;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=sqlite://app.db\n").unwrap();
        assert_eq!(
            detect_backend_with(tmp.path(), Some("prod"), no_env),
            DatabaseBackend::Postgres
        );
    }
}
