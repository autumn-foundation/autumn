//! `autumn db scrub` — turn a production database (or an `autumn db backup`
//! artifact) into an anonymized copy that is safe on a laptop or a shared
//! staging box (issue #1602).
//!
//! # Why this exists
//!
//! The moment logical backups ship (#1595), a production copy is one command
//! away from a non-production machine — PII and all. Every peer tool for this
//! job (Greenmask, pgsync + obfuscation config, the PostgreSQL Anonymizer
//! extension, django-scrubber) is **schema-blind**: the developer hand-maintains
//! a column list that silently rots the first time someone adds an `email`
//! column, which is exactly the failure mode that leaks PII.
//!
//! Autumn is not schema-blind. This command classifies columns from three
//! sources, in precedence order:
//!
//! 1. **`[tables.<t>.pii]` in `scrub.toml`** — the developer's explicit
//!    declaration, including the replacement strategy.
//! 2. **`#[encrypted]` model columns** — machine-readable PII semantics the
//!    framework already holds ([`crate::schema::parse::parse_encrypted_columns`]).
//!    A `safe` declaration may **not** override these.
//! 3. **GDPR `ModelRegistration::anonymize("<table>")` registrations** — a
//!    table-level signal, so every non-key column of that table is classified
//!    PII unless explicitly declared `safe`.
//!
//! Everything left over is **unclassified**, and an unclassified column is a
//! hard failure ([`ScrubError::Unclassified`]) — never a silent pass-through.
//! Because the column universe comes from **introspecting the live database**
//! (not from the config file), a column added yesterday cannot be missing from
//! that universe: adding a column without declaring it breaks the scrub, which
//! is the whole point.
//!
//! # Safety properties
//!
//! - **Fail-closed.** Unclassified, stale (naming a column that no longer
//!   exists), and self-contradictory declarations all refuse before a single row
//!   is touched. The one exception is `--artifact`: the restore must run before
//!   the classification can read the schema it creates, so a refusal after a
//!   restore leaves unscrubbed data in the target — and says so, loudly.
//! - **Production guard.** Writing refuses outside `dev`/`test` without
//!   `--force`, the identical protocol as `autumn db drop`
//!   ([`crate::db::guard_destructive`]).
//! - **Constraint-preserving.** PII on a primary- or foreign-key column is
//!   refused outright (so referential integrity is untouched), `NULL` is refused
//!   on a `NOT NULL` column, a constant replacement is refused on a `UNIQUE`
//!   column, and a `varchar(n)` bound narrows the generated value or refuses.
//! - **Atomic.** Every target is classified before any target is written (so an
//!   undeclared column on one shard cannot leave the topology half anonymized),
//!   and every statement for one database runs in a single transaction: a
//!   half-scrubbed database is never left behind.
//! - **Framework-aware.** Introspection excludes `autumn_*` tables from the
//!   classified universe, so the ones that carry app-supplied payloads (queued
//!   jobs, offline-sync rows, `api_tokens`) are reported separately and emptied
//!   when the app opts in with `[framework] purge`.
//! - **Credential-safe.** No error or report ever embeds a resolved URL.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use autumn_schema_core::{Column, ColumnType, Table};
use diesel::{Connection as _, PgConnection, RunQueryDsl as _, sql_query};
use serde::Deserialize;

use crate::migrate;
use crate::schema::introspect;

use super::{quote_ident, quote_literal};

/// The per-app PII declaration file, read from the project root unless
/// `--config` points elsewhere.
pub const SCRUB_CONFIG_FILE: &str = "scrub.toml";

/// Width of the per-row `md5` token, in hex characters.
const TOKEN_HEX_LEN: usize = 32;

/// Narrowest token a length-bounded column may carry and still be treated as
/// per-row unique. Below this the column is refused rather than silently
/// truncated into collisions.
const MIN_TOKEN_WIDTH: usize = 8;

/// The reserved, permanently undeliverable domain scrubbed addresses use
/// (RFC 6761 reserves `.invalid`).
const SCRUB_EMAIL_DOMAIN: &str = "@example.invalid";

// ─── Arguments ──────────────────────────────────────────────────────────────

/// Arguments for `autumn db scrub`.
#[derive(Debug, Clone, Default)]
pub struct ScrubArgs {
    /// Profile overlay to resolve the connection under (see `db create`).
    pub profile: Option<String>,
    /// Restore this backup run directory (or artifact file) into the resolved
    /// database(s) before scrubbing, closing the backup → scrub → restore loop.
    pub artifact: Option<PathBuf>,
    /// After a successful scrub, write a fresh backup run into this directory —
    /// a scrubbed artifact that can be handed to a teammate.
    pub output: Option<PathBuf>,
    /// Path to the PII declaration file (default: `./scrub.toml`).
    pub config: Option<PathBuf>,
    /// Classify only: report the plan (or the unclassified columns) and write
    /// nothing.
    pub check: bool,
    /// Print the exact SQL the scrub would run and write nothing.
    pub dry_run: bool,
    /// Bypass the production guard (mirrors `autumn db drop --force`).
    pub force: bool,
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Failure modes for `autumn db scrub`. `Display` is credential-safe: no variant
/// ever embeds a resolved URL (only a parsed host/port/db), matching the rest of
/// the `db` command family.
#[derive(Debug)]
pub enum ScrubError {
    /// One or more columns are neither PII-classified nor explicitly declared
    /// safe. The scrub refuses rather than let real data through (AC #3).
    Unclassified {
        /// `table.column`, sorted.
        columns: Vec<String>,
    },
    /// The declaration names a table or column the database does not have —
    /// the config has rotted away from the schema.
    StaleConfig {
        /// `table` or `table.column`, sorted.
        entries: Vec<String>,
    },
    /// A column is declared both `safe` and PII in the same table.
    Contradiction {
        /// `table.column`, sorted.
        columns: Vec<String>,
    },
    /// A `safe` declaration tried to un-classify an `#[encrypted]` column.
    SafeOverridesEncrypted {
        /// `table.column`, sorted.
        columns: Vec<String>,
    },
    /// PII was declared on a primary- or foreign-key column, which a scrub may
    /// never rewrite without breaking referential integrity.
    PiiOnKeyColumn {
        /// `table.column`, sorted.
        columns: Vec<String>,
    },
    /// No replacement can be derived from the column's type alone.
    NoAutoStrategy {
        /// The column name.
        column: String,
        /// The unsupported type, rendered for humans.
        detail: String,
    },
    /// The `null` strategy was declared on a `NOT NULL` column.
    NullOnNotNull {
        /// `table.column`.
        column: String,
    },
    /// A strategy that yields the same value for every row was declared on a
    /// `UNIQUE` column.
    NonUniqueStrategy {
        /// `table.column`.
        column: String,
        /// The offending strategy name.
        strategy: &'static str,
    },
    /// A strategy cannot produce a value of the column's type.
    StrategyTypeMismatch {
        /// The column name.
        column: String,
        /// The offending strategy name.
        strategy: &'static str,
        /// The column type, rendered for humans.
        detail: String,
    },
    /// A length-bounded column is too narrow to hold a per-row-unique fake.
    ColumnTooNarrow {
        /// The column name.
        column: String,
        /// The column's character limit.
        limit: usize,
        /// Characters the strategy's fixed affixes already consume.
        overhead: usize,
    },
    /// The declaration file could not be read or parsed.
    Config {
        /// The path that failed.
        path: String,
        /// A human-readable reason.
        detail: String,
    },
    /// An app source file could not be read or parsed while scanning for
    /// `#[encrypted]` columns / GDPR registrations.
    SourceScan {
        /// A human-readable reason (carries the offending path).
        detail: String,
    },
    /// A `ModelRegistration::anonymize(...)` call was found whose table name is
    /// not a string literal, so the scanner cannot classify it. Refused rather
    /// than ignored — an unreadable registration must not look like an absent
    /// one.
    UnresolvableAnonymize {
        /// The call as written, for the developer to find it.
        detail: String,
    },
    /// The database could not be introspected or connected to. Carries only the
    /// parsed host/port/db, never the credentials.
    Introspect {
        /// The target label (`control` / `shard:<name>`).
        label: String,
        /// A credential-safe reason.
        detail: String,
    },
    /// A scrub statement failed. The message comes from the server.
    Sql(String),
    /// The scrub was refused because the active profile is production and
    /// `--force` was not supplied.
    ProductionRefused {
        /// The effective profile name.
        profile: String,
    },
    /// The write target is the database a profile's **config file** declares —
    /// the artifact's own source — so the scrub would overwrite it.
    OverwritesConfiguredTarget {
        /// The profile whose config names this database.
        profile: String,
        /// The database name (never a URL).
        database: String,
    },
    /// `[framework] purge` names a table that is not framework-owned. Emptying a
    /// user table is never something a scrub does implicitly.
    PurgeNotFrameworkTable {
        /// The offending table names, sorted.
        tables: Vec<String>,
    },
    /// A backup/restore step (artifact restore, `--output` re-dump) failed.
    Backup(Box<super::backup::BackupError>),
}

impl std::fmt::Display for ScrubError {
    // One arm per variant, each a single multi-line, actionable message; splitting
    // the match would scatter the error copy across helpers for no reader benefit.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unclassified { columns } => write!(
                f,
                "{} column(s) are neither PII-classified nor declared safe, so the scrub \
                 cannot prove they carry no real data:\n{}\n  \
                 Declare each one in {SCRUB_CONFIG_FILE} — under [tables.<table>.pii] to \
                 replace it, or in `safe` to keep it verbatim. Run `autumn db scrub --check` \
                 for a paste-ready starting point.",
                columns.len(),
                bullet_list(columns),
            ),
            Self::StaleConfig { entries } => write!(
                f,
                "{SCRUB_CONFIG_FILE} names {} table(s)/column(s) the database does not have:\n{}\n  \
                 The declaration has drifted from the schema — remove or rename the stale \
                 entries (a renamed column must be re-declared under its new name).",
                entries.len(),
                bullet_list(entries),
            ),
            Self::Contradiction { columns } => write!(
                f,
                "{} column(s) are declared both `safe` and PII in {SCRUB_CONFIG_FILE}:\n{}\n  \
                 Pick one.",
                columns.len(),
                bullet_list(columns),
            ),
            Self::SafeOverridesEncrypted { columns } => write!(
                f,
                "{} column(s) carry #[encrypted] in the model but are declared `safe` in \
                 {SCRUB_CONFIG_FILE}:\n{}\n  \
                 An at-rest-encrypted column is PII by construction and cannot be declared \
                 safe. Remove the `safe` entry (or drop #[encrypted] from the model if the \
                 column really is not sensitive).",
                columns.len(),
                bullet_list(columns),
            ),
            Self::PiiOnKeyColumn { columns } => write!(
                f,
                "{} column(s) are primary or foreign keys but declared PII:\n{}\n  \
                 Rewriting a key column would break referential integrity. Scrub the \
                 referenced table's own PII columns instead, and declare these `safe`.",
                columns.len(),
                bullet_list(columns),
            ),
            Self::NoAutoStrategy { column, detail } => write!(
                f,
                "No replacement can be derived for {column:?} from its type ({detail}).\n  \
                 Declare an explicit strategy for it under [tables.<table>.pii] in \
                 {SCRUB_CONFIG_FILE} (for example `= \"redact\"`), or declare the column \
                 `safe`."
            ),
            Self::NullOnNotNull { column } => write!(
                f,
                "{column:?} is NOT NULL, so the `null` strategy would violate the column \
                 constraint.\n  Use `redact` (or another value-producing strategy) instead."
            ),
            Self::NonUniqueStrategy { column, strategy } => write!(
                f,
                "{column:?} is UNIQUE, but the `{strategy}` strategy writes the same value \
                 into every row and would violate the unique constraint.\n  \
                 Use a per-row-unique strategy (`redact`, `email`, `name`, `uuid`, `bytes`)."
            ),
            Self::StrategyTypeMismatch {
                column,
                strategy,
                detail,
            } => write!(
                f,
                "The `{strategy}` strategy cannot produce a value for {column:?} ({detail}).\n  \
                 Pick a strategy that matches the column type."
            ),
            Self::ColumnTooNarrow {
                column,
                limit,
                overhead,
            } => write!(
                f,
                "{column:?} holds at most {limit} characters, but the chosen strategy needs \
                 {overhead} for its fixed text plus at least {MIN_TOKEN_WIDTH} more for a \
                 per-row-unique token.\n  Use a shorter strategy (`redact`), widen the \
                 column, or declare it `safe`."
            ),
            Self::Config { path, detail } => write!(f, "Cannot read {path}: {detail}"),
            Self::SourceScan { detail } => write!(
                f,
                "Could not scan the app source for PII annotations: {detail}"
            ),
            Self::UnresolvableAnonymize { detail } => write!(
                f,
                "A GDPR anonymize registration names a table this scanner cannot resolve: \
                 {detail}\n  `autumn db scrub` reads `ModelRegistration::anonymize(\"...\")` \
                 with a string-literal table name. Use a literal there, or declare the \
                 table's columns explicitly in {SCRUB_CONFIG_FILE}."
            ),
            Self::Introspect { label, detail } => {
                write!(f, "Could not read the schema of {label}: {detail}")
            }
            Self::Sql(message) => write!(f, "{message}"),
            Self::ProductionRefused { profile } => write!(
                f,
                "Refusing to scrub the {profile:?} profile database.\n  \
                 A scrub REWRITES data in place — running it against production would \
                 destroy the real values. Point --profile at your staging/dev target, or \
                 re-run with --force if you really mean it."
            ),
            Self::OverwritesConfiguredTarget { profile, database } => write!(
                f,
                "Refusing to scrub {database:?}: it is the database the {profile:?} profile's \
                 config file declares.\n  \
                 The scrub would overwrite the source it was taken from. Point the target at \
                 a separate staging database, or re-run with --force."
            ),
            Self::PurgeNotFrameworkTable { tables } => write!(
                f,
                "[framework] purge in {SCRUB_CONFIG_FILE} names {} table(s) that are not \
                 framework-owned:\n{}\n  \
                 `purge` empties a table outright and only accepts framework-owned names \
                 (`autumn_*` / `_autumn*`, plus the framework's unprefixed tables). Declare \
                 a user table's columns under [tables.<table>.pii] instead.",
                tables.len(),
                bullet_list(tables),
            ),
            Self::Backup(e) => write!(f, "{e}"),
        }
    }
}

impl From<super::backup::BackupError> for ScrubError {
    fn from(e: super::backup::BackupError) -> Self {
        Self::Backup(Box::new(e))
    }
}

/// Render a sorted list as indented bullets for a multi-line error message.
fn bullet_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("    - {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Declaration file ───────────────────────────────────────────────────────

/// How a PII column's value is replaced.
///
/// Every strategy derives from an `md5` token over the row's primary key salted
/// with the column name, so two columns of one row never receive the same fake
/// value and a `UNIQUE` column keeps its uniqueness. For a table with a primary
/// key the result is also **stable across runs** (the same row always scrubs to
/// the same value); a table with no primary key falls back to the physical
/// `ctid`, which is unique within the statement but not stable between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Derive the strategy from the column's type (the default for
    /// automatically-classified columns).
    Auto,
    /// A syntactically valid, permanently undeliverable address.
    Email,
    /// A human-shaped placeholder name.
    Name,
    /// A `+1555`-prefixed placeholder number.
    Phone,
    /// An obviously-fake bracketed marker.
    Redact,
    /// `NULL` (refused on a `NOT NULL` column).
    Null,
    /// A deterministic replacement UUID.
    Uuid,
    /// Deterministic replacement bytes.
    Bytes,
    /// A constant `{"scrubbed": true}` document.
    Json,
    /// Numeric zero / boolean false.
    Zero,
    /// The Unix epoch.
    Epoch,
}

impl Strategy {
    /// The name as written in `scrub.toml`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Email => "email",
            Self::Name => "name",
            Self::Phone => "phone",
            Self::Redact => "redact",
            Self::Null => "null",
            Self::Uuid => "uuid",
            Self::Bytes => "bytes",
            Self::Json => "json",
            Self::Zero => "zero",
            Self::Epoch => "epoch",
        }
    }

    /// Whether this strategy may be used on a `UNIQUE` column.
    ///
    /// `Null` qualifies despite writing one value: Postgres permits any number
    /// of `NULL`s in a unique index. `Phone` does not — its digits are a lossy
    /// projection of the token, so collisions are possible.
    const fn allowed_on_unique(self) -> bool {
        matches!(
            self,
            Self::Auto
                | Self::Email
                | Self::Name
                | Self::Redact
                | Self::Null
                | Self::Uuid
                | Self::Bytes
        )
    }
}

/// Declarations that apply to every table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubDefaults {
    /// Column names that are safe in **any** table — the one-line escape from
    /// declaring `id` / `created_at` / `updated_at` in every stanza.
    #[serde(default)]
    pub safe_columns: Vec<String>,
}

/// One table's declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableRule {
    /// Columns reviewed and deliberately kept verbatim.
    #[serde(default)]
    pub safe: Vec<String>,
    /// PII columns and how each is replaced.
    #[serde(default)]
    pub pii: BTreeMap<String, Strategy>,
}

/// How framework-owned tables are handled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkRule {
    /// Framework-owned tables to empty during the scrub. Opt-in: by default the
    /// scrub only *warns* about the ones that carry app-supplied payloads.
    #[serde(default)]
    pub purge: Vec<String>,
}

/// The parsed `scrub.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubConfig {
    /// Cross-table declarations.
    #[serde(default)]
    pub defaults: ScrubDefaults,
    /// Per-table declarations, keyed by table name.
    #[serde(default)]
    pub tables: BTreeMap<String, TableRule>,
    /// Framework-owned table handling.
    #[serde(default)]
    pub framework: FrameworkRule,
}

/// Framework-owned tables whose rows carry **app-supplied** payloads, and can
/// therefore hold PII the column-level classification never sees: introspection
/// deliberately skips `autumn_*` / `_autumn*` tables (mirroring `autumn db pull`
/// and `autumn schema pull`), so their columns are not part of the classified
/// universe at all.
///
/// A scrub warns when one of these is present, and empties it when the app opts
/// in with `[framework] purge = [...]`. Every entry here is transient
/// operational state that a staging copy has no reason to inherit — a queued job
/// payload, an offline-sync row buffer, an experiment assignment — never
/// schema-bearing bookkeeping like `autumn_migration_checksums`.
const FRAMEWORK_PAYLOAD_TABLES: &[&str] = &[
    // Hashed API tokens minted in production. A staging copy that inherits them
    // is a live credential leak, not merely a PII one.
    "api_tokens",
    "autumn_experiment_assignments",
    "autumn_job_tracking",
    "autumn_jobs",
    "autumn_sync_applied",
    "autumn_sync_pending",
    "autumn_sync_rows",
];

/// Framework-owned tables whose names do not carry the `autumn_` / `_autumn`
/// prefix. Kept in lock-step with `crate::schema::introspect`'s
/// `is_framework_table`, which is what decides they are excluded from the
/// classified universe in the first place.
const UNPREFIXED_FRAMEWORK_TABLES: &[&str] = &[
    "api_tokens",
    "feature_flag_changes",
    "__diesel_schema_migrations",
];

/// Whether a table name is framework-owned — i.e. one introspection filters out
/// of the classified universe, so the column-level classification never sees it.
fn is_framework_table(table: &str) -> bool {
    table.starts_with("autumn_")
        || table.starts_with("_autumn")
        || UNPREFIXED_FRAMEWORK_TABLES.contains(&table)
}

/// Validate a `[framework] purge` list: it may only name framework-owned tables.
/// A user table listed there would be silently emptied, which is never what a
/// scrub should do behind a one-word config key.
fn check_purge_list(config: &ScrubConfig) -> Result<(), ScrubError> {
    let mut offenders: Vec<String> = config
        .framework
        .purge
        .iter()
        .filter(|t| !is_framework_table(t))
        .cloned()
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    offenders.sort();
    Err(ScrubError::PurgeNotFrameworkTable { tables: offenders })
}

/// Parse a `scrub.toml` document.
///
/// # Errors
///
/// Returns [`ScrubError::Config`] when the document is not valid TOML, names an
/// unknown key, or names an unknown strategy.
#[cfg(test)]
fn parse_config_str(src: &str) -> Result<ScrubConfig, ScrubError> {
    parse_config_at(src, Path::new(SCRUB_CONFIG_FILE))
}

/// [`parse_config_str`], attributing any error to the file it came from.
fn parse_config_at(src: &str, path: &Path) -> Result<ScrubConfig, ScrubError> {
    toml::from_str(src).map_err(|e| ScrubError::Config {
        path: path.display().to_string(),
        detail: e.to_string(),
    })
}

/// Load the declaration file, defaulting to an empty declaration when the
/// conventional path is simply absent (every column is then unclassified, which
/// is the fail-closed outcome the developer is told how to fix).
fn load_config(explicit: Option<&Path>) -> Result<ScrubConfig, ScrubError> {
    load_config_at(explicit, Path::new(SCRUB_CONFIG_FILE))
}

/// [`load_config`] against an explicit conventional path, so the "a missing
/// default is fine, a missing explicit path is not" rule is testable without
/// mutating the process working directory.
fn load_config_at(explicit: Option<&Path>, default: &Path) -> Result<ScrubConfig, ScrubError> {
    let path = explicit.map_or_else(|| default.to_path_buf(), Path::to_path_buf);
    match std::fs::read_to_string(&path) {
        Ok(src) => parse_config_at(&src, &path),
        // An explicitly-named file that is missing is an error; the conventional
        // default simply may not exist yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
            Ok(ScrubConfig::default())
        }
        Err(e) => Err(ScrubError::Config {
            path: path.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

// ─── Classification ─────────────────────────────────────────────────────────

/// Where a column's classification came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassSource {
    /// An explicit `[tables.<t>.pii]` entry.
    Config,
    /// An `#[encrypted]` model column.
    Encrypted,
    /// A table registered with the GDPR anonymize strategy.
    GdprAnonymize,
}

impl ClassSource {
    /// A short label for the scrub report.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "declared",
            Self::Encrypted => "#[encrypted]",
            Self::GdprAnonymize => "gdpr:anonymize",
        }
    }
}

/// Everything the classifier reads. Grouped into one struct so the pure
/// classification step stays testable without a database.
pub struct ClassificationInputs<'a> {
    /// The live schema (framework-owned tables already excluded).
    pub tables: &'a [Table],
    /// The developer's declaration.
    pub config: &'a ScrubConfig,
    /// `#[encrypted]` columns, keyed by table.
    pub encrypted: &'a BTreeMap<String, BTreeSet<String>>,
    /// Tables registered with the GDPR anonymize strategy.
    pub anonymize_tables: &'a BTreeSet<String>,
}

/// One column the scrub will rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPlan {
    /// The column name.
    pub column: String,
    /// The resolved (never [`Strategy::Auto`]) replacement strategy.
    pub strategy: Strategy,
    /// What classified it.
    pub source: ClassSource,
}

/// One table's scrub statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePlan {
    /// The table name.
    pub table: String,
    /// The columns rewritten, in table column order.
    pub columns: Vec<ColumnPlan>,
    /// The single `UPDATE` that rewrites them all.
    pub sql: String,
}

/// The full set of statements a scrub will run against one database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubPlan {
    /// One entry per table with at least one PII column, in table order.
    pub tables: Vec<TablePlan>,
}

impl ScrubPlan {
    /// Look up one column's decision, if the scrub rewrites it.
    #[cfg(test)]
    fn column(&self, table: &str, column: &str) -> Option<&ColumnPlan> {
        self.tables
            .iter()
            .find(|t| t.table == table)?
            .columns
            .iter()
            .find(|c| c.column == column)
    }

    /// Total number of columns the scrub rewrites.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.tables.iter().map(|t| t.columns.len()).sum()
    }
}

/// Classify every column of every table and build the statements, or refuse.
///
/// The checks run in a fixed order so one run reports the most fundamental
/// problem rather than a cascade: declaration rot first (stale, contradictory,
/// overriding `#[encrypted]`), then structural refusals (PII on a key column),
/// then the fail-closed sweep, then per-column strategy validation.
///
/// # Errors
///
/// Returns the corresponding [`ScrubError`] variant for each refusal above.
pub fn build_plan(inputs: &ClassificationInputs<'_>) -> Result<ScrubPlan, ScrubError> {
    let by_name: BTreeMap<&str, &Table> =
        inputs.tables.iter().map(|t| (t.name.as_str(), t)).collect();

    check_config_freshness(inputs, &by_name)?;
    check_contradictions(inputs)?;
    check_safe_overrides_encrypted(inputs, &by_name)?;

    let mut unclassified = Vec::new();
    let mut key_pii = Vec::new();
    let mut planned: Vec<(&Table, Vec<(ColumnPlan, &Column)>)> = Vec::new();

    for table in inputs.tables {
        let rule = inputs.config.tables.get(&table.name);
        let anonymized = inputs.anonymize_tables.contains(&table.name);
        let encrypted = inputs.encrypted.get(&table.name);
        let mut columns = Vec::new();

        for column in &table.columns {
            let qualified = format!("{}.{}", table.name, column.name);
            let is_key = is_key_column(table, column);
            let declared_pii = rule.and_then(|r| r.pii.get(&column.name)).copied();
            let is_encrypted = encrypted.is_some_and(|e| e.contains(&column.name));
            let declared_safe = rule.is_some_and(|r| r.safe.contains(&column.name))
                || inputs.config.defaults.safe_columns.contains(&column.name);

            let (spec, source) = if let Some(strategy) = declared_pii {
                (strategy, ClassSource::Config)
            } else if is_encrypted {
                (Strategy::Auto, ClassSource::Encrypted)
            } else if declared_safe {
                continue;
            } else if anonymized {
                // A table-level inference never claims the structural columns:
                // rewriting a key would break referential integrity, and the
                // registration says nothing about them.
                if is_key {
                    continue;
                }
                (Strategy::Auto, ClassSource::GdprAnonymize)
            } else {
                unclassified.push(qualified);
                continue;
            };

            if is_key {
                key_pii.push(qualified);
                continue;
            }
            columns.push((
                ColumnPlan {
                    column: column.name.clone(),
                    strategy: spec,
                    source,
                },
                column,
            ));
        }
        planned.push((table, columns));
    }

    if !key_pii.is_empty() {
        key_pii.sort();
        return Err(ScrubError::PiiOnKeyColumn { columns: key_pii });
    }
    if !unclassified.is_empty() {
        unclassified.sort();
        return Err(ScrubError::Unclassified {
            columns: unclassified,
        });
    }

    let mut out = ScrubPlan::default();
    for (table, columns) in planned {
        if columns.is_empty() {
            continue;
        }
        let mut resolved = Vec::with_capacity(columns.len());
        let mut assignments = Vec::with_capacity(columns.len());
        for (mut plan, column) in columns {
            let qualified = format!("{}.{}", table.name, column.name);
            if plan.strategy == Strategy::Auto {
                plan.strategy = auto_strategy(column).map_err(|e| qualify(e, &qualified))?;
            }
            if plan.strategy == Strategy::Null && !column.nullable {
                return Err(ScrubError::NullOnNotNull { column: qualified });
            }
            if is_unique_column(table, column) && !plan.strategy.allowed_on_unique() {
                return Err(ScrubError::NonUniqueStrategy {
                    column: qualified,
                    strategy: plan.strategy.as_str(),
                });
            }
            let token = token_expr(table, &column.name);
            let value = replacement_expr(plan.strategy, column, &token)
                .map_err(|e| qualify(e, &qualified))?;
            assignments.push(assignment(column, &value));
            resolved.push(plan);
        }
        out.tables.push(TablePlan {
            table: table.name.clone(),
            columns: resolved,
            sql: format!(
                "UPDATE {} SET {}",
                quote_ident(&table.name),
                assignments.join(", ")
            ),
        });
    }
    Ok(out)
}

/// Re-label a per-column error with its `table.column` name.
///
/// The expression builders work from a bare [`Column`] and cannot know which
/// table it came from, but a bare column name is ambiguous in a report (three
/// tables can each have an `email`). The classifier knows both, so it qualifies
/// the name on the way out.
fn qualify(error: ScrubError, qualified: &str) -> ScrubError {
    let column = qualified.to_owned();
    match error {
        ScrubError::NoAutoStrategy { detail, .. } => ScrubError::NoAutoStrategy { column, detail },
        ScrubError::StrategyTypeMismatch {
            strategy, detail, ..
        } => ScrubError::StrategyTypeMismatch {
            column,
            strategy,
            detail,
        },
        ScrubError::ColumnTooNarrow {
            limit, overhead, ..
        } => ScrubError::ColumnTooNarrow {
            column,
            limit,
            overhead,
        },
        other => other,
    }
}

/// Refuse a declaration that names a table or column the database no longer has
/// — the exact rot that lets a renamed column leak.
fn check_config_freshness(
    inputs: &ClassificationInputs<'_>,
    by_name: &BTreeMap<&str, &Table>,
) -> Result<(), ScrubError> {
    let mut stale = Vec::new();
    for (name, rule) in &inputs.config.tables {
        let Some(table) = by_name.get(name.as_str()) else {
            stale.push(name.clone());
            continue;
        };
        let columns: BTreeSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        for column in rule.safe.iter().chain(rule.pii.keys()) {
            if !columns.contains(column.as_str()) {
                stale.push(format!("{name}.{column}"));
            }
        }
    }
    if stale.is_empty() {
        return Ok(());
    }
    stale.sort();
    stale.dedup();
    Err(ScrubError::StaleConfig { entries: stale })
}

/// Refuse a column declared both `safe` and PII.
fn check_contradictions(inputs: &ClassificationInputs<'_>) -> Result<(), ScrubError> {
    let mut columns = Vec::new();
    for (name, rule) in &inputs.config.tables {
        for column in &rule.safe {
            if rule.pii.contains_key(column) {
                columns.push(format!("{name}.{column}"));
            }
        }
    }
    if columns.is_empty() {
        return Ok(());
    }
    columns.sort();
    Err(ScrubError::Contradiction { columns })
}

/// Refuse a `safe` declaration that would un-classify an `#[encrypted]` column.
/// An at-rest-encrypted column is PII by construction; letting a declaration
/// override it would reintroduce exactly the silent-passthrough this command
/// exists to prevent.
fn check_safe_overrides_encrypted(
    inputs: &ClassificationInputs<'_>,
    by_name: &BTreeMap<&str, &Table>,
) -> Result<(), ScrubError> {
    let mut columns = Vec::new();
    for (table, encrypted) in inputs.encrypted {
        if !by_name.contains_key(table.as_str()) {
            continue;
        }
        let rule = inputs.config.tables.get(table);
        for column in encrypted {
            // An explicit PII entry is not an override — it only picks the
            // strategy — so only `safe` declarations conflict.
            if rule.is_some_and(|r| r.pii.contains_key(column)) {
                continue;
            }
            if rule.is_some_and(|r| r.safe.contains(column))
                || inputs.config.defaults.safe_columns.contains(column)
            {
                columns.push(format!("{table}.{column}"));
            }
        }
    }
    if columns.is_empty() {
        return Ok(());
    }
    columns.sort();
    Err(ScrubError::SafeOverridesEncrypted { columns })
}

/// Whether a column participates in the table's primary key or is a foreign key.
fn is_key_column(table: &Table, column: &Column) -> bool {
    column.primary_key || table.primary_key.contains(&column.name) || column.references.is_some()
}

/// Whether a column is constrained unique on its own — either the column flag
/// introspection sets for a single-column unique index, or such an index listed
/// on the table. A **partial** unique index does not count: it only constrains
/// the rows matching its predicate.
fn is_unique_column(table: &Table, column: &Column) -> bool {
    if column.unique {
        return true;
    }
    table.indexes.iter().any(|index| {
        if !index.unique || index.is_partial {
            return false;
        }
        let keys = if index.key_columns.is_empty() {
            &index.columns
        } else {
            &index.key_columns
        };
        keys.len() == 1 && keys[0] == column.name
    })
}

// ─── Replacement expressions ────────────────────────────────────────────────

/// The SQL expression identifying a row for deterministic replacement: its
/// primary key when it has one, else its physical `ctid` (unique within the
/// statement, which is all a single `UPDATE` needs).
fn row_key_expr(table: &Table) -> String {
    let mut keys: Vec<&str> = table.primary_key.iter().map(String::as_str).collect();
    if keys.is_empty() {
        keys = table
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect();
    }
    if keys.is_empty() {
        return "ctid::text".to_owned();
    }
    keys.iter()
        .map(|key| format!("coalesce({}::text, '')", quote_ident(key)))
        .collect::<Vec<_>>()
        .join(" || '|' || ")
}

/// The per-row, per-column token every replacement is derived from. Salting with
/// the column name keeps two PII columns of one row from receiving identical
/// fake values.
fn token_expr(table: &Table, column: &str) -> String {
    token_expr_from(&row_key_expr(table), column)
}

/// [`token_expr`] against an already-computed row key.
fn token_expr_from(row_key: &str, column: &str) -> String {
    format!("md5({} || '|' || {row_key})", quote_literal(column))
}

/// The character limit a length-bounded Postgres type imposes, if any.
///
/// Introspection preserves `varchar(n)` / `char(n)` verbatim as
/// [`ColumnType::Opaque`] rather than collapsing them to `Text`, so the limit is
/// readable straight off the type.
fn char_max_len(ty: &ColumnType) -> Option<usize> {
    let ColumnType::Opaque { pg_type } = ty else {
        return None;
    };
    let rest = pg_type
        .strip_prefix("varchar(")
        .or_else(|| pg_type.strip_prefix("char("))?;
    rest.strip_suffix(')')?.parse().ok()
}

/// Whether a column stores character data a text-shaped replacement can be
/// written into.
fn is_texty(ty: &ColumnType) -> bool {
    match ty {
        ColumnType::Text => true,
        ColumnType::Opaque { pg_type } => {
            char_max_len(ty).is_some() || matches!(pg_type.as_str(), "citext" | "name")
        }
        _ => false,
    }
}

/// Derive a replacement strategy from the column type alone — what an
/// automatically-classified column (`#[encrypted]`, GDPR anonymize) uses.
///
/// A closed-set ([`ColumnType::Enum`]) or otherwise exotic type has no safe
/// generic fake (a fabricated value would violate its `CHECK`), so the developer
/// is asked for an explicit strategy rather than guessed at.
///
/// # Errors
///
/// Returns [`ScrubError::NoAutoStrategy`] for a type with no generic fake.
fn auto_strategy(column: &Column) -> Result<Strategy, ScrubError> {
    Ok(match &column.ty {
        ColumnType::Text => email_shaped(&column.name),
        ColumnType::Uuid => Strategy::Uuid,
        ColumnType::Bytes => Strategy::Bytes,
        ColumnType::Json | ColumnType::Attachment => Strategy::Json,
        ColumnType::Int32
        | ColumnType::Int64
        | ColumnType::Float32
        | ColumnType::Float64
        | ColumnType::Bool
        | ColumnType::Decimal { .. } => Strategy::Zero,
        ColumnType::Timestamp | ColumnType::TimestampTz => Strategy::Epoch,
        ty @ ColumnType::Opaque { .. } if is_texty(ty) => email_shaped(&column.name),
        other => {
            return Err(ScrubError::NoAutoStrategy {
                column: column.name.clone(),
                detail: format!("{other:?}"),
            });
        }
    })
}

/// Text columns whose name says "email" get a syntactically valid address, so a
/// scrubbed copy still satisfies format `CHECK`s and app-level parsing. Every
/// other text column is redacted — the name is only ever used to pick a *shape*,
/// never to decide whether a column is PII.
fn email_shaped(name: &str) -> Strategy {
    if name.to_ascii_lowercase().contains("email") {
        Strategy::Email
    } else {
        Strategy::Redact
    }
}

/// Build the replacement expression for one column.
///
/// # Errors
///
/// Returns [`ScrubError::ColumnTooNarrow`] when a length-bounded column cannot
/// hold a per-row-unique value, [`ScrubError::StrategyTypeMismatch`] when the
/// strategy cannot produce the column's type, or [`ScrubError::NoAutoStrategy`]
/// when [`Strategy::Auto`] cannot be resolved.
fn replacement_expr(
    strategy: Strategy,
    column: &Column,
    token: &str,
) -> Result<String, ScrubError> {
    let limit = char_max_len(&column.ty);
    let narrow = |overhead: usize| -> Result<String, ScrubError> {
        bounded_token(token, limit, overhead, &column.name)
    };
    Ok(match strategy {
        Strategy::Auto => replacement_expr(auto_strategy(column)?, column, token)?,
        Strategy::Email => {
            let tok = narrow("scrubbed+".len() + SCRUB_EMAIL_DOMAIN.len())?;
            format!("'scrubbed+' || {tok} || '{SCRUB_EMAIL_DOMAIN}'")
        }
        Strategy::Name => {
            let tok = narrow("Scrubbed ".len())?;
            format!("'Scrubbed ' || {tok}")
        }
        Strategy::Redact => {
            let tok = narrow("[scrubbed:]".len())?;
            format!("'[scrubbed:' || {tok} || ']'")
        }
        Strategy::Phone => {
            // 10 hex characters mapped onto digits: `translate` is lossy, which
            // is why `allowed_on_unique` excludes this strategy.
            const PHONE_DIGITS: usize = 10;
            let overhead = "+1555".len();
            if let Some(limit) = limit
                && limit < overhead + PHONE_DIGITS
            {
                return Err(ScrubError::ColumnTooNarrow {
                    column: column.name.clone(),
                    limit,
                    overhead,
                });
            }
            format!("'+1555' || translate(substr({token}, 1, {PHONE_DIGITS}), 'abcdef', '0123456')")
        }
        Strategy::Null => "NULL".to_owned(),
        Strategy::Uuid => {
            require_type(column, strategy, matches!(column.ty, ColumnType::Uuid))?;
            format!("({token})::uuid")
        }
        Strategy::Bytes => {
            require_type(column, strategy, matches!(column.ty, ColumnType::Bytes))?;
            format!("decode({token}, 'hex')")
        }
        Strategy::Json => {
            let json = "'{\"scrubbed\": true}'";
            match &column.ty {
                ColumnType::Json | ColumnType::Attachment => format!("{json}::jsonb"),
                ColumnType::Opaque { pg_type } if pg_type == "json" => format!("{json}::json"),
                ty if is_texty(ty) => json.to_owned(),
                other => {
                    return Err(ScrubError::StrategyTypeMismatch {
                        column: column.name.clone(),
                        strategy: strategy.as_str(),
                        detail: format!("{other:?}"),
                    });
                }
            }
        }
        Strategy::Zero => match &column.ty {
            ColumnType::Bool => "false".to_owned(),
            ColumnType::Int32
            | ColumnType::Int64
            | ColumnType::Float32
            | ColumnType::Float64
            | ColumnType::Decimal { .. } => "0".to_owned(),
            ColumnType::Opaque { pg_type } if pg_type.starts_with("numeric") => "0".to_owned(),
            other => {
                return Err(ScrubError::StrategyTypeMismatch {
                    column: column.name.clone(),
                    strategy: strategy.as_str(),
                    detail: format!("{other:?}"),
                });
            }
        },
        Strategy::Epoch => match &column.ty {
            ColumnType::Timestamp => "'1970-01-01 00:00:00'::timestamp".to_owned(),
            ColumnType::TimestampTz => "'1970-01-01 00:00:00+00'::timestamptz".to_owned(),
            other => {
                return Err(ScrubError::StrategyTypeMismatch {
                    column: column.name.clone(),
                    strategy: strategy.as_str(),
                    detail: format!("{other:?}"),
                });
            }
        },
    })
}

/// Refuse a strategy whose output type cannot be stored in the column.
fn require_type(column: &Column, strategy: Strategy, ok: bool) -> Result<(), ScrubError> {
    if ok {
        return Ok(());
    }
    Err(ScrubError::StrategyTypeMismatch {
        column: column.name.clone(),
        strategy: strategy.as_str(),
        detail: format!("{:?}", column.ty),
    })
}

/// Narrow the token so `overhead` fixed characters plus the token fit inside a
/// length-bounded column, or refuse when what is left cannot stay unique.
fn bounded_token(
    token: &str,
    limit: Option<usize>,
    overhead: usize,
    column: &str,
) -> Result<String, ScrubError> {
    let Some(limit) = limit else {
        return Ok(token.to_owned());
    };
    let available = limit.saturating_sub(overhead);
    if available >= TOKEN_HEX_LEN {
        Ok(token.to_owned())
    } else if available >= MIN_TOKEN_WIDTH {
        Ok(format!("substr({token}, 1, {available})"))
    } else {
        Err(ScrubError::ColumnTooNarrow {
            column: column.to_owned(),
            limit,
            overhead,
        })
    }
}

/// One `SET` clause. A nullable column keeps its `NULL`s (a scrub anonymizes
/// values, it does not invent them), so it is wrapped in a `CASE`; a `NOT NULL`
/// column needs no guard.
fn assignment(column: &Column, value: &str) -> String {
    let ident = quote_ident(&column.name);
    if column.nullable {
        format!("{ident} = CASE WHEN {ident} IS NULL THEN NULL ELSE {value} END")
    } else {
        format!("{ident} = {value}")
    }
}

// ─── GDPR anonymize registrations ───────────────────────────────────────────

/// Extract the tables registered with the GDPR anonymize strategy from one Rust
/// source file.
///
/// The registry is built at runtime (`GdprRegistry::new().register(...)`) so it
/// is not readable without booting the app; the registrations themselves are
/// plain calls, and reading them with `syn` keeps the scrub usable against a
/// dump without a compiled binary. A call whose argument is not a string literal
/// is **refused**, never skipped — an unreadable registration must not look like
/// an absent one.
///
/// # Errors
///
/// Returns [`ScrubError::SourceScan`] if the source is not valid Rust, or
/// [`ScrubError::UnresolvableAnonymize`] for a non-literal table name.
fn extract_anonymize_tables(src: &str) -> Result<BTreeSet<String>, ScrubError> {
    use syn::visit::Visit as _;

    let file = syn::parse_file(src).map_err(|e| ScrubError::SourceScan {
        detail: e.to_string(),
    })?;
    let mut scan = AnonymizeScan::default();
    scan.visit_file(&file);
    if let Some(detail) = scan.unresolved.into_iter().next() {
        return Err(ScrubError::UnresolvableAnonymize { detail });
    }
    Ok(scan.tables)
}

/// `syn` visitor collecting `ModelRegistration::anonymize("<table>")` calls.
#[derive(Default)]
struct AnonymizeScan {
    tables: BTreeSet<String>,
    unresolved: Vec<String>,
}

impl<'ast> syn::visit::Visit<'ast> for AnonymizeScan {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*node.func {
            let segments = &path.path.segments;
            let is_anonymize = segments.len() >= 2
                && segments[segments.len() - 1].ident == "anonymize"
                && segments[segments.len() - 2].ident == "ModelRegistration";
            if is_anonymize {
                match node.args.first() {
                    Some(syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(table),
                        ..
                    })) => {
                        self.tables.insert(table.value());
                    }
                    _ => self.unresolved.push(quote::quote!(#node).to_string()),
                }
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

/// Scan every `.rs` file under `root` (recursively) for anonymize registrations.
fn scan_anonymize_tables(root: &Path) -> Result<BTreeSet<String>, ScrubError> {
    let mut out = BTreeSet::new();
    if !root.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| ScrubError::SourceScan {
            detail: format!("{}: {e}", dir.display()),
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| ScrubError::SourceScan {
                detail: format!("{}: {e}", dir.display()),
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| ScrubError::SourceScan {
                detail: format!("{}: {e}", path.display()),
            })?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                let src = std::fs::read_to_string(&path).map_err(|e| ScrubError::SourceScan {
                    detail: format!("{}: {e}", path.display()),
                })?;
                let found = extract_anonymize_tables(&src).map_err(|e| match e {
                    ScrubError::SourceScan { detail } => ScrubError::SourceScan {
                        detail: format!("{}: {detail}", path.display()),
                    },
                    ScrubError::UnresolvableAnonymize { detail } => {
                        ScrubError::UnresolvableAnonymize {
                            detail: format!("{} — {detail}", path.display()),
                        }
                    }
                    other => other,
                })?;
                out.extend(found);
            }
        }
    }
    Ok(out)
}

/// Read the `#[encrypted]` column set from the project's models, degrading to an
/// empty map when the project has no models directory at all.
fn encrypted_columns(
    project_root: &Path,
) -> Result<BTreeMap<String, BTreeSet<String>>, ScrubError> {
    let Some(path) = crate::schema::existing_models_path(project_root) else {
        return Ok(BTreeMap::new());
    };
    crate::schema::parse::parse_encrypted_columns_path(&path).map_err(|e| ScrubError::SourceScan {
        detail: e.to_string(),
    })
}

// ─── Guards ─────────────────────────────────────────────────────────────────

/// Refuse a scrub against a production profile without `--force` — the identical
/// protocol as `autumn db drop` (AC #5).
///
/// # Errors
///
/// Returns [`ScrubError::ProductionRefused`] for any profile outside
/// `dev`/`test` when `force` is not set.
fn guard_scrub_target(profile: &str, force: bool) -> Result<(), ScrubError> {
    super::guard_destructive(profile, force).map_err(|_| ScrubError::ProductionRefused {
        profile: profile.to_owned(),
    })
}

/// Whether two connection strings address the same database: same host, same
/// port, same database name. Credentials are deliberately ignored — a read-only
/// role pointed at production is still production.
///
/// An unparsable URL never claims a match (the guard errs toward "different",
/// leaving the profile guard as the enforcement).
fn same_database(a: &str, b: &str) -> bool {
    let parts = |raw: &str| -> Option<(String, u16, String)> {
        let parsed = url::Url::parse(raw).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let port = parsed.port().unwrap_or(5432);
        let name = parsed
            .path_segments()
            .and_then(|mut s| s.next())
            .map(str::to_owned)
            .filter(|n| !n.is_empty())?;
        Some((host, port, name))
    };
    match (parts(a), parts(b)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Refuse to scrub the database a **config file** declares for the artifact's
/// own (non-dev/test) profile — the "I pointed staging at the production URL"
/// mistake the profile guard alone cannot see.
///
/// Deliberately reads only `autumn.toml` / `autumn-<profile>.toml`, never the
/// environment: an env-provided `DATABASE_URL` is shared by every profile
/// resolution, so consulting it would make this guard fire on legitimate scrubs.
/// It is defence in depth on top of [`guard_scrub_target`], not a replacement.
fn guard_configured_source(
    source_profile: &str,
    targets: &[(String, String)],
    force: bool,
) -> Result<(), ScrubError> {
    if force || super::is_safe_destructive_profile(source_profile) {
        return Ok(());
    }
    let table = migrate::read_autumn_toml_table_with_profile(Some(source_profile));
    let Some(declared) = migrate::resolve_primary_database_url_from_sources(
        |_| Err(std::env::VarError::NotPresent),
        table.as_ref(),
    ) else {
        return Ok(());
    };
    for (_, url) in targets {
        if same_database(&declared, url) {
            return Err(ScrubError::OverwritesConfiguredTarget {
                profile: source_profile.to_owned(),
                database: parsed_db_name(url),
            });
        }
    }
    Ok(())
}

/// The database name in a connection URL, for credential-safe reporting.
fn parsed_db_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next())
                .map(str::to_owned)
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "<unknown>".to_owned())
}

// ─── Reporting ──────────────────────────────────────────────────────────────

/// A paste-ready `scrub.toml` fragment declaring every unclassified column, so
/// adopting the command on an existing schema is one copy away.
fn suggested_config_stanza(unclassified: &[String]) -> String {
    use std::fmt::Write as _;

    let mut by_table: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in unclassified {
        if let Some((table, column)) = entry.split_once('.') {
            by_table.entry(table).or_default().push(column);
        }
    }
    let mut out = String::from(
        "# Paste into scrub.toml, then replace `auto` with an explicit strategy\n\
         # (email/name/phone/redact/null/uuid/bytes/json/zero/epoch), or move the\n\
         # column into that table's `safe = [...]` list if it holds no PII.\n",
    );
    for (table, columns) in by_table {
        let _ = write!(out, "\n[tables.{table}.pii]\n");
        for column in columns {
            let _ = writeln!(out, "{column} = \"auto\"");
        }
    }
    out
}

// ─── Entry point ────────────────────────────────────────────────────────────

/// Entry point for `autumn db scrub`. Prints a credential-safe message and exits
/// non-zero on failure.
pub fn run(args: &ScrubArgs) {
    eprintln!("\u{1F342} autumn db scrub\n");
    if let Err(e) = scrub(args) {
        eprintln!("\u{2717} {e}");
        if let ScrubError::Unclassified { columns } = &e {
            eprintln!("\n{}", suggested_config_stanza(columns));
        }
        std::process::exit(1);
    }
}

fn scrub(args: &ScrubArgs) -> Result<(), ScrubError> {
    let profile = migrate::effective_profile(args.profile.as_deref());
    let writes = !args.check && !args.dry_run;
    if writes {
        guard_scrub_target(&profile, args.force)?;
    }

    let targets = super::backup::resolve_all_target_urls(args.profile.as_deref())?;

    let restored = if let Some(artifact) = &args.artifact {
        if let Some(source_profile) = super::backup::artifact_source_profile(artifact) {
            eprintln!("  \u{2139} Artifact was taken under the {source_profile:?} profile.");
            guard_configured_source(&source_profile, &targets, args.force)?;
        }
        eprintln!(
            "\u{2500}\u{2500} restoring {} \u{2500}\u{2500}",
            artifact.display()
        );
        super::backup::restore(&super::backup::RestoreArgs {
            artifact: artifact.clone(),
            profile: args.profile.clone(),
            force: args.force,
            shard: None,
            offsite: false,
        })?;
        true
    } else {
        false
    };

    // A restore has already written the artifact's REAL data into the target, so
    // any refusal from here on leaves unscrubbed data behind. The classification
    // cannot run earlier — it reads the schema the restore just created — so the
    // outcome is stated in the loudest possible terms instead of being implied.
    classify_and_apply(args, &targets).inspect_err(|_| {
        if restored {
            eprintln!(
                "\n\u{26A0} The artifact was ALREADY RESTORED before this failure, so the \
                 target database now holds UNSCRUBBED data.\n  \
                 Do not hand it to anyone: fix the problem below and re-run the same \
                 command, or drop the database. `autumn db scrub --check` catches this \
                 before a restore."
            );
        }
    })
}

/// Classify every target, then — only once every target has classified cleanly —
/// apply the statements.
///
/// The two passes are deliberate and mirror how `autumn db restore` verifies
/// every artifact before touching any database: with a control database plus
/// shards, a single-pass loop would scrub the control database and only then
/// discover that a shard has an undeclared column, leaving the topology half
/// anonymized.
fn classify_and_apply(args: &ScrubArgs, targets: &[(String, String)]) -> Result<(), ScrubError> {
    let config = load_config(args.config.as_deref())?;
    check_purge_list(&config)?;
    let project_root = Path::new(".");
    let encrypted = encrypted_columns(project_root)?;
    let anonymize = scan_anonymize_tables(&project_root.join("src"))?;

    // ── Pass 1: classify everything ─────────────────────────────────────────
    let mut plans = Vec::with_capacity(targets.len());
    for (label, url) in targets {
        let tables = introspect::introspect_postgres(url).map_err(|e| ScrubError::Introspect {
            label: label.clone(),
            detail: e.to_string(),
        })?;
        let plan = build_plan(&ClassificationInputs {
            tables: &tables,
            config: &config,
            encrypted: &encrypted,
            anonymize_tables: &anonymize,
        })?;
        report_plan(label, &plan);
        let framework = framework_payload_tables_present(url, label, &config)?;
        report_framework_tables(&framework, &config);
        plans.push((label, url, plan, framework));
    }

    if args.check {
        eprintln!("\n\u{2713} Every column is classified \u{2014} no unclassified data can leak.");
        return Ok(());
    }
    if args.dry_run {
        for (_, _, plan, framework) in &plans {
            for table in &plan.tables {
                eprintln!("  {};", table.sql);
            }
            for (_, statement) in purge_statements(framework, &config) {
                eprintln!("  {statement};");
            }
        }
        eprintln!("\n\u{2713} Dry run only \u{2014} nothing was written.");
        return Ok(());
    }

    // ── Pass 2: apply ───────────────────────────────────────────────────────
    for (label, url, plan, framework) in &plans {
        for (table, rows) in execute(url, plan, &purge_statements(framework, &config), label)? {
            eprintln!("  \u{2713} {table}: {rows} row(s) scrubbed.");
        }
    }

    if let Some(dir) = &args.output {
        eprintln!("\u{2500}\u{2500} writing a scrubbed artifact \u{2500}\u{2500}");
        super::backup::backup(&super::backup::BackupArgs {
            profile: args.profile.clone(),
            dir: Some(dir.clone()),
            format: super::backup::BackupFormat::Custom,
            keep: None,
            target: super::backup::TargetSelector::All,
            upload: false,
        })?;
    }

    eprintln!("\n\u{2713} Scrub complete.");
    Ok(())
}

/// Print what one database's scrub will do: every column, its strategy, and what
/// classified it — so the operator can audit the decision, not just its effect.
fn report_plan(label: &str, plan: &ScrubPlan) {
    eprintln!(
        "\u{2500}\u{2500} {label} \u{2500}\u{2500}\n  {} column(s) across {} table(s) \
         classified as PII.",
        plan.column_count(),
        plan.tables.len()
    );
    for table in &plan.tables {
        for column in &table.columns {
            eprintln!(
                "    {}.{} \u{2192} {} ({})",
                table.table,
                column.column,
                column.strategy.as_str(),
                column.source.as_str()
            );
        }
    }
}

/// The framework-owned payload tables that actually exist in one database.
///
/// Introspection filters `autumn_*` / `_autumn*` out of the classified universe,
/// so this is a separate probe: the scrub still has to say something about them
/// rather than pretend they are not there.
fn framework_payload_tables_present(
    url: &str,
    label: &str,
    config: &ScrubConfig,
) -> Result<Vec<String>, ScrubError> {
    let mut conn = PgConnection::establish(url).map_err(|_| ScrubError::Introspect {
        label: label.to_owned(),
        detail: format!(
            "could not connect to database {:?} to inspect framework-owned tables",
            parsed_db_name(url)
        ),
    })?;
    // The known payload carriers PLUS anything the app explicitly opted into
    // purging — otherwise a `purge` entry outside the built-in list would be
    // accepted by the config check and then silently do nothing.
    let wanted = probe_table_names(config)
        .iter()
        .map(|t| quote_literal(t))
        .collect::<Vec<_>>()
        .join(", ");
    let rows: Vec<NameRow> = sql_query(format!(
        "SELECT table_name AS name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         AND table_name IN ({wanted}) ORDER BY table_name"
    ))
    .load(&mut conn)
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    Ok(rows.into_iter().map(|r| r.name).collect())
}

/// The framework-owned table names worth probing for: the built-in payload
/// carriers plus every `[framework] purge` entry.
fn probe_table_names(config: &ScrubConfig) -> BTreeSet<String> {
    FRAMEWORK_PAYLOAD_TABLES
        .iter()
        .map(|t| (*t).to_owned())
        .chain(config.framework.purge.iter().cloned())
        .collect()
}

/// A single `name` column produced by the framework-table probe.
#[derive(diesel::QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// Tell the operator about framework-owned payload tables the classification
/// never sees: which ones will be emptied, and which ones are being left alone.
fn report_framework_tables(present: &[String], config: &ScrubConfig) {
    if present.is_empty() {
        return;
    }
    let (purged, kept): (Vec<&String>, Vec<&String>) = present
        .iter()
        .partition(|t| config.framework.purge.contains(t));
    for table in purged {
        eprintln!("    {table} \u{2192} emptied (framework, [framework] purge)");
    }
    // Only the built-in payload carriers are warned about: an app that named
    // some other framework table in `purge` has already decided about it.
    let kept: Vec<&&String> = kept
        .iter()
        .filter(|t| FRAMEWORK_PAYLOAD_TABLES.contains(&t.as_str()))
        .collect();
    if kept.is_empty() {
        return;
    }
    eprintln!(
        "  \u{26A0} {} framework-owned table(s) are NOT scrubbed and may carry app-supplied \
         payloads (queued jobs, offline-sync rows, experiment assignments):\n{}\n    \
         Add them to `[framework] purge = [...]` in {SCRUB_CONFIG_FILE} to empty them, or \
         empty them yourself.",
        kept.len(),
        kept.iter()
            .map(|t| format!("      - {t}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// The `DELETE FROM` statements for the opted-in framework tables that exist.
fn purge_statements(present: &[String], config: &ScrubConfig) -> Vec<(String, String)> {
    present
        .iter()
        .filter(|t| config.framework.purge.contains(t))
        .map(|t| (t.clone(), format!("DELETE FROM {}", quote_ident(t))))
        .collect()
}

/// Run every statement for one database inside a single transaction, so a
/// failure can never leave a half-scrubbed database behind.
fn execute(
    url: &str,
    plan: &ScrubPlan,
    purges: &[(String, String)],
    label: &str,
) -> Result<Vec<(String, usize)>, ScrubError> {
    if plan.tables.is_empty() && purges.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = PgConnection::establish(url).map_err(|_| ScrubError::Introspect {
        label: label.to_owned(),
        detail: format!(
            "could not connect to database {:?} to apply the scrub",
            parsed_db_name(url)
        ),
    })?;
    let mut counts = Vec::with_capacity(plan.tables.len());
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        for table in &plan.tables {
            let rows = sql_query(&table.sql).execute(conn)?;
            counts.push((table.table.clone(), rows));
        }
        for (table, statement) in purges {
            let rows = sql_query(statement).execute(conn)?;
            counts.push((format!("{table} (emptied)"), rows));
        }
        Ok(())
    })
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use autumn_schema_core::{Backend, Column, ColumnType, ForeignKey, Index, Table};

    use super::*;

    // ── Fixtures ────────────────────────────────────────────────────────────

    fn text_col(name: &str) -> Column {
        Column::new(name, ColumnType::Text)
    }

    fn pk_col(name: &str) -> Column {
        let mut c = Column::new(name, ColumnType::Int64);
        c.primary_key = true;
        c
    }

    /// `users(id PK, email TEXT UNIQUE, full_name TEXT, bio TEXT NULL,
    /// created_at TIMESTAMP)`.
    fn users_table() -> Table {
        let mut t = Table::new("users", Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        let mut email = text_col("email");
        email.unique = true;
        let mut bio = text_col("bio");
        bio.nullable = true;
        t.columns = vec![
            pk_col("id"),
            email,
            text_col("full_name"),
            bio,
            Column::new("created_at", ColumnType::Timestamp),
        ];
        t
    }

    fn empty_encrypted() -> BTreeMap<String, BTreeSet<String>> {
        BTreeMap::new()
    }

    fn no_anonymize() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn plan_for(
        tables: &[Table],
        config: &ScrubConfig,
        encrypted: &BTreeMap<String, BTreeSet<String>>,
        anonymize: &BTreeSet<String>,
    ) -> Result<ScrubPlan, ScrubError> {
        build_plan(&ClassificationInputs {
            tables,
            config,
            encrypted,
            anonymize_tables: anonymize,
        })
    }

    // ── Config parsing ──────────────────────────────────────────────────────

    #[test]
    fn config_parses_defaults_safe_and_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]

            [tables.users]
            safe = ["role"]

            [tables.users.pii]
            email = "email"
            full_name = "name"
            "#,
        )
        .expect("config must parse");

        assert_eq!(config.defaults.safe_columns, vec!["id", "created_at"]);
        let users = config.tables.get("users").expect("users rule");
        assert_eq!(users.safe, vec!["role"]);
        assert_eq!(users.pii.get("email"), Some(&Strategy::Email));
        assert_eq!(users.pii.get("full_name"), Some(&Strategy::Name));
    }

    #[test]
    fn config_rejects_unknown_strategy() {
        let err = parse_config_str(
            r#"
            [tables.users.pii]
            email = "obfuscate"
            "#,
        )
        .expect_err("unknown strategy must be rejected");
        assert!(
            err.to_string().contains("obfuscate"),
            "error should name the bad strategy: {err}"
        );
    }

    #[test]
    fn config_rejects_unknown_keys() {
        let err = parse_config_str(
            r#"
            [tables.users]
            saf = ["role"]
            "#,
        )
        .expect_err("a typo'd key must not be silently ignored");
        assert!(err.to_string().contains("saf"), "error: {err}");
    }

    #[test]
    fn empty_config_parses_to_default() {
        assert_eq!(parse_config_str("").unwrap(), ScrubConfig::default());
    }

    // ── Fail-closed classification (AC #3) ──────────────────────────────────

    #[test]
    fn unclassified_columns_are_refused_and_listed() {
        let tables = vec![users_table()];
        let err = plan_for(
            &tables,
            &ScrubConfig::default(),
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("an all-unclassified schema must be refused");

        let ScrubError::Unclassified { columns } = &err else {
            panic!("expected Unclassified, got {err:?}");
        };
        assert_eq!(
            columns,
            &vec![
                "users.bio".to_owned(),
                "users.created_at".to_owned(),
                "users.email".to_owned(),
                "users.full_name".to_owned(),
                "users.id".to_owned(),
            ]
        );
        // The message must be actionable: it names the columns and the file.
        let rendered = err.to_string();
        assert!(rendered.contains("users.email"), "{rendered}");
        assert!(rendered.contains(SCRUB_CONFIG_FILE), "{rendered}");
    }

    #[test]
    fn a_newly_added_column_flips_a_previously_clean_config_to_failure() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users]
            safe = []
            [tables.users.pii]
            email = "email"
            full_name = "name"
            bio = "redact"
            "#,
        )
        .unwrap();
        let tables = vec![users_table()];
        plan_for(&tables, &config, &empty_encrypted(), &no_anonymize())
            .expect("the fully-declared schema must pass");

        // Someone adds `users.ssn` and forgets the declaration.
        let mut with_new_column = users_table();
        with_new_column.columns.push(text_col("ssn"));
        let err = plan_for(
            &[with_new_column],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a new undeclared column must fail the scrub");
        assert!(matches!(
            err,
            ScrubError::Unclassified { ref columns } if columns == &vec!["users.ssn".to_owned()]
        ));
    }

    #[test]
    fn stale_config_entries_are_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [tables.users.pii]
            emial = "email"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a config naming a column that no longer exists must fail");
        assert!(
            matches!(err, ScrubError::StaleConfig { ref entries } if entries.contains(&"users.emial".to_owned())),
            "got {err:?}"
        );
    }

    #[test]
    fn stale_config_table_is_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [tables.legacy_users]
            safe = ["x"]
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a config table that no longer exists must fail");
        assert!(
            matches!(err, ScrubError::StaleConfig { ref entries } if entries.contains(&"legacy_users".to_owned())),
            "got {err:?}"
        );
    }

    #[test]
    fn a_column_cannot_be_both_safe_and_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = ["email"]
            [tables.users.pii]
            email = "email"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a contradictory declaration must fail");
        assert!(matches!(err, ScrubError::Contradiction { .. }), "{err:?}");
    }

    // ── Automatic classification (AC #2) ────────────────────────────────────

    #[test]
    fn encrypted_columns_are_pii_without_any_declaration() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = []
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert("users".to_owned(), BTreeSet::from(["email".to_owned()]));

        let plan = plan_for(&[users_table()], &config, &encrypted, &no_anonymize())
            .expect("an #[encrypted] column needs no declaration");
        let column = plan
            .column("users", "email")
            .expect("email must be in the plan");
        assert_eq!(column.source, ClassSource::Encrypted);
    }

    #[test]
    fn safe_cannot_override_an_encrypted_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = ["email"]
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert("users".to_owned(), BTreeSet::from(["email".to_owned()]));

        let err = plan_for(&[users_table()], &config, &encrypted, &no_anonymize())
            .expect_err("marking an #[encrypted] column safe must be refused");
        assert!(
            matches!(err, ScrubError::SafeOverridesEncrypted { ref columns } if columns == &vec!["users.email".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn gdpr_anonymize_table_classifies_its_columns_as_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            "#,
        )
        .unwrap();
        let anonymize = BTreeSet::from(["users".to_owned()]);
        let plan = plan_for(&[users_table()], &config, &empty_encrypted(), &anonymize)
            .expect("a GDPR-anonymize table needs no per-column declaration");

        for column in ["email", "full_name", "bio"] {
            let decision = plan
                .column("users", column)
                .unwrap_or_else(|| panic!("{column} must be scrubbed"));
            assert_eq!(decision.source, ClassSource::GdprAnonymize);
        }
        // `id`/`created_at` were explicitly declared safe, so they are untouched.
        assert!(plan.column("users", "id").is_none());
        assert!(plan.column("users", "created_at").is_none());
    }

    #[test]
    fn safe_may_narrow_a_gdpr_anonymize_table() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users]
            safe = ["full_name"]
            "#,
        )
        .unwrap();
        let anonymize = BTreeSet::from(["users".to_owned()]);
        let plan = plan_for(&[users_table()], &config, &empty_encrypted(), &anonymize).unwrap();
        assert!(plan.column("users", "full_name").is_none());
        assert!(plan.column("users", "email").is_some());
    }

    #[test]
    fn explicit_pii_wins_over_the_auto_strategy() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "redact"
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert("users".to_owned(), BTreeSet::from(["email".to_owned()]));
        let plan = plan_for(&[users_table()], &config, &encrypted, &no_anonymize()).unwrap();
        let column = plan.column("users", "email").unwrap();
        assert_eq!(column.strategy, Strategy::Redact);
        assert_eq!(column.source, ClassSource::Config);
    }

    // ── Constraint safety (AC #4) ───────────────────────────────────────────

    #[test]
    fn pii_on_a_primary_key_is_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["created_at", "email", "full_name", "bio"]
            [tables.users.pii]
            id = "zero"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("scrubbing a primary key would break referencing rows");
        assert!(
            matches!(err, ScrubError::PiiOnKeyColumn { ref columns } if columns == &vec!["users.id".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn pii_on_a_foreign_key_is_refused() {
        let mut posts = Table::new("posts", Backend::Postgres);
        posts.primary_key = vec!["id".to_owned()];
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        posts.columns = vec![pk_col("id"), author];

        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id"]
            [tables.posts.pii]
            author_id = "zero"
            "#,
        )
        .unwrap();
        let err = plan_for(&[posts], &config, &empty_encrypted(), &no_anonymize())
            .expect_err("scrubbing a foreign key would break referential integrity");
        assert!(
            matches!(err, ScrubError::PiiOnKeyColumn { ref columns } if columns == &vec!["posts.author_id".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn a_gdpr_anonymize_table_never_auto_classifies_its_key_columns() {
        let mut posts = Table::new("posts", Backend::Postgres);
        posts.primary_key = vec!["id".to_owned()];
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        posts.columns = vec![pk_col("id"), author, text_col("body")];

        let plan = plan_for(
            &[posts],
            &ScrubConfig::default(),
            &empty_encrypted(),
            &BTreeSet::from(["posts".to_owned()]),
        )
        .expect("key columns are structurally safe under a table-level inference");
        assert!(plan.column("posts", "id").is_none());
        assert!(plan.column("posts", "author_id").is_none());
        assert!(plan.column("posts", "body").is_some());
    }

    #[test]
    fn null_strategy_is_refused_on_a_not_null_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "bio"]
            [tables.users.pii]
            full_name = "null"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("NULL into a NOT NULL column must be refused at plan time");
        assert!(matches!(err, ScrubError::NullOnNotNull { .. }), "{err:?}");
    }

    #[test]
    fn null_strategy_is_allowed_on_a_nullable_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name"]
            [tables.users.pii]
            bio = "null"
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert_eq!(
            plan.column("users", "bio").unwrap().strategy,
            Strategy::Null
        );
    }

    #[test]
    fn a_non_injective_strategy_is_refused_on_a_unique_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "json"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a constant replacement would violate the unique index");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "got {err:?}"
        );
    }

    // ── Replacement expressions ─────────────────────────────────────────────

    #[test]
    fn row_key_uses_the_primary_key_when_present() {
        assert_eq!(row_key_expr(&users_table()), r#"coalesce("id"::text, '')"#);
    }

    #[test]
    fn row_key_falls_back_to_ctid_without_a_primary_key() {
        let mut t = Table::new("legacy", Backend::Postgres);
        t.columns = vec![text_col("note")];
        assert_eq!(row_key_expr(&t), "ctid::text");
    }

    #[test]
    fn row_key_concatenates_a_composite_primary_key() {
        let mut t = Table::new("memberships", Backend::Postgres);
        t.primary_key = vec!["user_id".to_owned(), "team_id".to_owned()];
        let mut user_id = Column::new("user_id", ColumnType::Int64);
        user_id.primary_key = true;
        let mut team_id = Column::new("team_id", ColumnType::Int64);
        team_id.primary_key = true;
        t.columns = vec![user_id, team_id];
        assert_eq!(
            row_key_expr(&t),
            r#"coalesce("user_id"::text, '') || '|' || coalesce("team_id"::text, '')"#
        );
    }

    #[test]
    fn token_is_salted_per_column_so_two_columns_never_match() {
        let table = users_table();
        assert_ne!(
            token_expr(&table, "email"),
            token_expr(&table, "full_name"),
            "two PII columns of one row must not receive the same fake value"
        );
    }

    #[test]
    fn email_expression_is_unique_per_row_and_uses_a_reserved_domain() {
        let expr = replacement_expr(Strategy::Email, &text_col("email"), "TOK").unwrap();
        assert!(expr.contains("TOK"), "must vary per row: {expr}");
        assert!(
            expr.contains("@example.invalid"),
            "must use a reserved, undeliverable domain: {expr}"
        );
    }

    #[test]
    fn varchar_length_bounds_the_generated_value() {
        let column = Column::new(
            "email",
            ColumnType::Opaque {
                pg_type: "varchar(40)".to_owned(),
            },
        );
        let expr = replacement_expr(Strategy::Email, &column, "TOK").unwrap();
        // `scrubbed+` (9) + token + `@example.invalid` (16) must fit in 40.
        assert!(
            expr.contains("substr(TOK, 1, 15)"),
            "token must be narrowed to fit varchar(40): {expr}"
        );
    }

    #[test]
    fn a_too_narrow_column_is_refused_rather_than_silently_truncated() {
        let column = Column::new(
            "email",
            ColumnType::Opaque {
                pg_type: "varchar(28)".to_owned(),
            },
        );
        let err = replacement_expr(Strategy::Email, &column, "TOK")
            .expect_err("a column too narrow for a unique fake must be refused");
        assert!(matches!(err, ScrubError::ColumnTooNarrow { .. }), "{err:?}");
    }

    #[test]
    fn char_length_is_parsed_from_the_opaque_pg_type() {
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "varchar(64)".to_owned()
            }),
            Some(64)
        );
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "char(2)".to_owned()
            }),
            Some(2)
        );
        assert_eq!(char_max_len(&ColumnType::Text), None);
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "citext".to_owned()
            }),
            None
        );
    }

    #[test]
    fn auto_strategy_is_derived_from_the_column_type() {
        assert_eq!(
            auto_strategy(&text_col("email")).unwrap(),
            Strategy::Email,
            "an email-named text column gets a syntactically valid address"
        );
        assert_eq!(auto_strategy(&text_col("bio")).unwrap(), Strategy::Redact);
        assert_eq!(
            auto_strategy(&Column::new("token", ColumnType::Uuid)).unwrap(),
            Strategy::Uuid
        );
        assert_eq!(
            auto_strategy(&Column::new("blob", ColumnType::Bytes)).unwrap(),
            Strategy::Bytes
        );
        assert_eq!(
            auto_strategy(&Column::new("meta", ColumnType::Json)).unwrap(),
            Strategy::Json
        );
        assert_eq!(
            auto_strategy(&Column::new("age", ColumnType::Int32)).unwrap(),
            Strategy::Zero
        );
        assert_eq!(
            auto_strategy(&Column::new("seen_at", ColumnType::TimestampTz)).unwrap(),
            Strategy::Epoch
        );
    }

    #[test]
    fn auto_strategy_refuses_to_guess_for_a_closed_set_or_exotic_type() {
        assert!(matches!(
            auto_strategy(&Column::new(
                "status",
                ColumnType::Enum {
                    variants: vec!["draft".to_owned()]
                }
            )),
            Err(ScrubError::NoAutoStrategy { .. })
        ));
        assert!(matches!(
            auto_strategy(&Column::new(
                "addr",
                ColumnType::Opaque {
                    pg_type: "inet".to_owned()
                }
            )),
            Err(ScrubError::NoAutoStrategy { .. })
        ));
    }

    // ── Statement generation ────────────────────────────────────────────────

    #[test]
    fn update_preserves_nulls_and_batches_a_table_into_one_statement() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users.pii]
            email = "email"
            full_name = "name"
            bio = "redact"
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert_eq!(plan.tables.len(), 1, "one statement per table");
        let sql = &plan.tables[0].sql;
        assert!(sql.starts_with(r#"UPDATE "users" SET "#), "{sql}");
        assert!(
            sql.contains(r#""bio" = CASE WHEN "bio" IS NULL THEN NULL ELSE"#),
            "a nullable column must keep its NULLs: {sql}"
        );
        assert!(
            !sql.contains(r#""full_name" = CASE"#),
            "a NOT NULL column needs no CASE: {sql}"
        );
        assert!(sql.contains(r#""email" = "#) && sql.contains(r#""full_name" = "#));
    }

    #[test]
    fn a_table_with_no_pii_produces_no_statement() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert!(plan.tables.is_empty(), "nothing to scrub, nothing emitted");
    }

    #[test]
    fn identifiers_with_quotes_are_escaped_in_the_statement() {
        let mut t = Table::new(r#"we"ird"#, Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        t.columns = vec![pk_col("id"), text_col(r#"na"me"#)];
        let mut config = ScrubConfig::default();
        config.defaults.safe_columns = vec!["id".to_owned()];
        config.tables.insert(
            r#"we"ird"#.to_owned(),
            TableRule {
                safe: Vec::new(),
                pii: BTreeMap::from([(r#"na"me"#.to_owned(), Strategy::Redact)]),
            },
        );
        let plan = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize()).unwrap();
        assert!(
            plan.tables[0]
                .sql
                .starts_with(r#"UPDATE "we""ird" SET "na""me" = "#),
            "{}",
            plan.tables[0].sql
        );
    }

    // ── GDPR anonymize extraction from app source ───────────────────────────

    #[test]
    fn anonymize_registrations_are_extracted_from_source() {
        let src = r#"
            use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
            fn registry() -> GdprRegistry {
                GdprRegistry::new()
                    .register(ModelRegistration::hard_delete("posts"))
                    .register(ModelRegistration::anonymize("comments"))
                    .register(ModelRegistration::retain("invoices", "legal hold"))
                    .register(autumn_web::gdpr::ModelRegistration::anonymize("profiles"))
            }
        "#;
        let found = extract_anonymize_tables(src).expect("source must parse");
        assert_eq!(
            found,
            BTreeSet::from(["comments".to_owned(), "profiles".to_owned()])
        );
    }

    #[test]
    fn a_commented_out_registration_is_not_extracted() {
        let src = r#"
            fn registry() {
                // ModelRegistration::anonymize("ghosts")
                let _ = ModelRegistration::anonymize("comments");
            }
        "#;
        let found = extract_anonymize_tables(src).unwrap();
        assert_eq!(found, BTreeSet::from(["comments".to_owned()]));
    }

    #[test]
    fn a_non_literal_registration_argument_is_reported_not_ignored() {
        let src = "
            fn registry() {
                let _ = ModelRegistration::anonymize(table_name());
            }
        ";
        let err = extract_anonymize_tables(src)
            .expect_err("a table name the scanner cannot resolve must not pass silently");
        assert!(
            matches!(err, ScrubError::UnresolvableAnonymize { .. }),
            "{err:?}"
        );
    }

    // ── Production guard (AC #5) ────────────────────────────────────────────

    #[test]
    fn scrub_refuses_a_production_profile_without_force() {
        for profile in ["prod", "production", "staging"] {
            assert!(
                matches!(
                    guard_scrub_target(profile, false),
                    Err(ScrubError::ProductionRefused { .. })
                ),
                "{profile} must be refused"
            );
            assert!(guard_scrub_target(profile, true).is_ok());
        }
        for profile in ["dev", "development", "test"] {
            assert!(guard_scrub_target(profile, false).is_ok());
        }
    }

    #[test]
    fn same_database_compares_host_port_and_name_ignoring_credentials() {
        // Same server + database, different credentials: still the same target.
        assert!(same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://readonly:other@db.example.com:5432/myapp"
        ));
        assert!(!same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://app:pw@db.example.com:5432/myapp_staging"
        ));
        assert!(!same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://app:pw@staging.example.com:5432/myapp"
        ));
        // An unparsable URL never claims a match.
        assert!(!same_database("not a url", "not a url"));
    }

    #[test]
    fn errors_never_leak_credentials() {
        let err = ScrubError::ProductionRefused {
            profile: "prod".to_owned(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("prod"));
        assert!(!rendered.contains("postgres://"));
        assert!(!rendered.contains("hunter2"));
    }

    // ── Report ──────────────────────────────────────────────────────────────

    #[test]
    fn check_report_prints_a_paste_ready_stanza_for_unclassified_columns() {
        let stanza = suggested_config_stanza(&[
            "users.email".to_owned(),
            "users.full_name".to_owned(),
            "posts.body".to_owned(),
        ]);
        assert!(stanza.contains("[tables.users.pii]"), "{stanza}");
        assert!(stanza.contains("email = \"auto\""), "{stanza}");
        assert!(stanza.contains("[tables.posts.pii]"), "{stanza}");
        assert!(stanza.contains("body = \"auto\""), "{stanza}");
    }

    #[test]
    fn per_column_errors_are_reported_with_their_table() {
        let mut t = Table::new("orders", Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        t.columns = vec![
            pk_col("id"),
            Column::new(
                "status",
                ColumnType::Enum {
                    variants: vec!["draft".to_owned(), "paid".to_owned()],
                },
            ),
        ];
        let err = plan_for(
            &[t],
            &parse_config_str(
                r#"
                [defaults]
                safe_columns = ["id"]
                [tables.orders.pii]
                status = "auto"
                "#,
            )
            .unwrap(),
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a closed-set column has no generic fake");
        assert!(
            matches!(err, ScrubError::NoAutoStrategy { ref column, .. } if column == "orders.status"),
            "the error must name the table too: {err:?}"
        );
    }

    #[test]
    fn a_strategy_that_cannot_produce_the_column_type_is_refused() {
        let err = replacement_expr(Strategy::Uuid, &text_col("note"), "TOK")
            .expect_err("a uuid cannot be written into a text column");
        assert!(
            matches!(err, ScrubError::StrategyTypeMismatch { .. }),
            "{err:?}"
        );
        assert!(
            matches!(
                replacement_expr(Strategy::Zero, &text_col("note"), "TOK"),
                Err(ScrubError::StrategyTypeMismatch { .. })
            ),
            "zero is meaningless for a text column"
        );
        assert!(
            matches!(
                replacement_expr(Strategy::Epoch, &Column::new("n", ColumnType::Int64), "TOK"),
                Err(ScrubError::StrategyTypeMismatch { .. })
            ),
            "epoch is meaningless for an integer column"
        );
    }

    #[test]
    fn typed_strategies_render_their_casts() {
        assert_eq!(
            replacement_expr(Strategy::Uuid, &Column::new("t", ColumnType::Uuid), "TOK").unwrap(),
            "(TOK)::uuid"
        );
        assert_eq!(
            replacement_expr(Strategy::Bytes, &Column::new("b", ColumnType::Bytes), "TOK").unwrap(),
            "decode(TOK, 'hex')"
        );
        assert_eq!(
            replacement_expr(Strategy::Zero, &Column::new("ok", ColumnType::Bool), "TOK").unwrap(),
            "false"
        );
        assert_eq!(
            replacement_expr(
                Strategy::Epoch,
                &Column::new("at", ColumnType::TimestampTz),
                "TOK"
            )
            .unwrap(),
            "'1970-01-01 00:00:00+00'::timestamptz"
        );
        assert_eq!(
            replacement_expr(Strategy::Null, &text_col("bio"), "TOK").unwrap(),
            "NULL"
        );
    }

    #[test]
    fn phone_produces_digits_and_refuses_a_column_that_cannot_hold_them() {
        let expr = replacement_expr(Strategy::Phone, &text_col("phone"), "TOK").unwrap();
        assert!(
            expr.contains("translate("),
            "must map hex onto digits: {expr}"
        );
        assert!(expr.starts_with("'+1555'"), "{expr}");

        let narrow = Column::new(
            "phone",
            ColumnType::Opaque {
                pg_type: "varchar(8)".to_owned(),
            },
        );
        assert!(matches!(
            replacement_expr(Strategy::Phone, &narrow, "TOK"),
            Err(ScrubError::ColumnTooNarrow { .. })
        ));
    }

    #[test]
    fn a_partial_unique_index_does_not_constrain_the_whole_column() {
        let mut t = users_table();
        t.columns[1].unique = false;
        let mut index = Index::new("idx_users_email", vec!["email".to_owned()], true);
        index.is_partial = true;
        t.indexes = vec![index];
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "json"
            "#,
        )
        .unwrap();
        // A partial unique index only constrains the rows matching its
        // predicate, so it must not be treated as table-wide uniqueness.
        let plan = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize())
            .expect("a partial unique index is not table-wide uniqueness");
        assert!(plan.column("users", "email").is_some());
    }

    #[test]
    fn a_missing_config_file_is_not_an_error_but_an_explicit_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("scrub.toml");
        // The conventional path may simply not exist yet.
        assert_eq!(
            load_config_at(None, &absent).unwrap(),
            ScrubConfig::default()
        );
        // A path the developer named explicitly must not be silently ignored.
        assert!(matches!(
            load_config_at(Some(&absent), &absent),
            Err(ScrubError::Config { .. })
        ));
    }

    // ── Framework-owned tables ──────────────────────────────────────────────

    #[test]
    fn purge_only_accepts_framework_owned_tables() {
        let config = parse_config_str(
            r#"
            [framework]
            purge = ["autumn_jobs", "users"]
            "#,
        )
        .unwrap();
        let err = check_purge_list(&config)
            .expect_err("emptying a user table must never hide behind `purge`");
        assert!(
            matches!(err, ScrubError::PurgeNotFrameworkTable { ref tables } if tables == &vec!["users".to_owned()]),
            "got {err:?}"
        );

        let ok = parse_config_str(
            r#"
            [framework]
            purge = ["autumn_jobs", "_autumn_ledger_revisions"]
            "#,
        )
        .unwrap();
        assert!(check_purge_list(&ok).is_ok());
    }

    #[test]
    fn purge_statements_cover_only_the_opted_in_tables_that_exist() {
        let config = parse_config_str(
            r#"
            [framework]
            purge = ["autumn_jobs", "autumn_sync_rows"]
            "#,
        )
        .unwrap();
        // `autumn_sync_rows` is opted in but absent; `autumn_job_tracking` is
        // present but not opted in.
        let present = vec!["autumn_job_tracking".to_owned(), "autumn_jobs".to_owned()];
        let statements = purge_statements(&present, &config);
        assert_eq!(
            statements,
            vec![(
                "autumn_jobs".to_owned(),
                r#"DELETE FROM "autumn_jobs""#.to_owned()
            )]
        );
    }

    #[test]
    fn no_purge_declaration_empties_nothing() {
        let present = vec!["autumn_jobs".to_owned()];
        assert!(purge_statements(&present, &ScrubConfig::default()).is_empty());
    }

    #[test]
    fn the_probe_covers_the_built_in_list_plus_every_purge_entry() {
        let config = parse_config_str(
            r#"
            [framework]
            purge = ["autumn_custom_outbox"]
            "#,
        )
        .unwrap();
        let probed = probe_table_names(&config);
        assert!(
            probed.contains("autumn_custom_outbox"),
            "a purge entry outside the built-in list must still be probed, or it \
             would be accepted and then silently do nothing"
        );
        for table in FRAMEWORK_PAYLOAD_TABLES {
            assert!(probed.contains(*table));
        }
    }

    #[test]
    fn framework_payload_tables_are_all_framework_owned() {
        for table in FRAMEWORK_PAYLOAD_TABLES {
            assert!(
                is_framework_table(table),
                "{table} must be filtered out of the classified universe"
            );
        }
        // The unprefixed set must stay in lock-step with the introspection
        // filter, or a table nothing classifies would also be un-purgeable.
        for table in UNPREFIXED_FRAMEWORK_TABLES {
            assert!(is_framework_table(table));
        }
        assert!(
            FRAMEWORK_PAYLOAD_TABLES.contains(&"api_tokens"),
            "production API tokens in a staging copy are a live credential leak"
        );
        // Schema bookkeeping must never be offered for purging.
        assert!(!FRAMEWORK_PAYLOAD_TABLES.contains(&"autumn_migration_checksums"));
        assert!(!FRAMEWORK_PAYLOAD_TABLES.contains(&"_autumn_shard_map"));
    }

    #[test]
    fn index_backed_uniqueness_is_recognized() {
        // A single-column unique INDEX (not a column flag) still forbids a
        // constant replacement.
        let mut t = users_table();
        t.columns[1].unique = false;
        t.indexes = vec![Index::new(
            "idx_users_email",
            vec!["email".to_owned()],
            true,
        )];
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "json"
            "#,
        )
        .unwrap();
        let err = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize())
            .expect_err("a unique index must be honored like a unique column");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "{err:?}"
        );
    }
}
