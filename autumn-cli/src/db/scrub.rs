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
//! hard failure (`ScrubError::Unclassified`) — never a silent pass-through.
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
use diesel::connection::SimpleConnection as _;
use diesel::{Connection as _, PgConnection, RunQueryDsl as _, sql_query};
use serde::Deserialize;

use crate::migrate;
use crate::schema::introspect;

/// Referentially-intact row subsetting (issue #1636). A submodule of the scrub
/// rather than a sibling: a sample is a phase of one scrub transaction, never a
/// command of its own, so no flag combination can emit sampled-but-unscrubbed
/// rows.
pub mod sample;

use super::{quote_ident, quote_literal};

/// The per-app PII declaration file, read from the project root unless
/// `--config` points elsewhere.
pub const SCRUB_CONFIG_FILE: &str = "scrub.toml";

/// Width of the per-row `md5` token, in hex characters.
const TOKEN_HEX_LEN: usize = 32;

/// Width of the per-row `sha256` token used for columns that must stay unique.
const UNIQUE_TOKEN_HEX_LEN: usize = 64;

/// Narrowest token a length-bounded, non-unique column may carry. Below this the
/// column is refused rather than silently truncated.
const MIN_TOKEN_WIDTH: usize = 8;

/// Narrowest token a length-bounded **unique** column may carry: 16 hex
/// characters is 64 bits, so the birthday bound puts a collision beyond any
/// plausible row count. Eight (32 bits) is not enough — it collides in practice
/// around 10⁵ rows, which is a routine table size, and the resulting
/// unique-violation aborts the whole scrub.
const MIN_UNIQUE_TOKEN_WIDTH: usize = 16;

/// The reserved, permanently undeliverable domain scrubbed addresses use
/// (RFC 6761 reserves `.invalid`).
const SCRUB_EMAIL_DOMAIN: &str = "@example.invalid";

// ─── Arguments ──────────────────────────────────────────────────────────────

/// Arguments for `autumn db scrub`.
#[derive(Debug, Clone, Default)]
// Each flag is an independent switch on one run, not a state machine.
#[allow(clippy::struct_excessive_bools)]
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
    /// Bypass the separate guard that refuses to write over the database an
    /// artifact's own non-dev/test profile config declares.
    pub allow_source_overwrite: bool,
    /// Root entities to sample, each `<table>=<count|percent%>` (issue #1636).
    /// Empty means no sampling: the whole scrubbed copy is kept.
    pub sample: Vec<String>,
    /// The seed the sample's row selection is derived from, so the same seed
    /// against the same source data reproduces the identical subset.
    pub seed: u64,
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
        /// Token characters the column must still have room for.
        floor: usize,
    },
    /// A declaration tried to write plaintext into an `#[encrypted]` column.
    PlaintextIntoEncrypted {
        /// Each entry is `table.column` plus the strategy that was declared.
        columns: Vec<String>,
    },
    /// A PII column is covered by a `CHECK` constraint, which no fabricated
    /// value can be proven to satisfy.
    CheckConstrainedColumn {
        /// `table.column`.
        column: String,
    },
    /// The target holds `#[encrypted]` columns but no encryption key could be
    /// resolved, so a valid replacement envelope cannot be produced.
    EncryptionKeyUnavailable {
        /// The profile whose credentials were read.
        profile: String,
        /// A credential-safe reason.
        detail: String,
    },
    /// Neither the model source nor the declaration says which columns are
    /// `#[encrypted]`, so the scrub cannot tell them apart from plain text.
    EncryptedMetadataUnavailable,
    /// `public` holds base tables the connecting role cannot see, so they never
    /// reached the classifier.
    InaccessibleTables {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// The target has base tables outside `public`, which the classification
    /// universe does not cover.
    UnsupportedSchemas {
        /// The schema names, sorted.
        schemas: Vec<String>,
    },
    /// The target has row-level security on a table the scrub would rewrite.
    RowLevelSecurity {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// `[tables.<t>]` declares a framework-owned table, which the column
    /// classification never sees.
    FrameworkTableDeclared {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// `[framework] purge` names a table whose contents the database needs.
    PurgeSchemaBookkeeping {
        /// The table names, sorted.
        tables: Vec<String>,
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
    /// `[framework] purge` names one of the two ledger tables without the other.
    /// A mark outlives the revisions it names by design, so emptying either
    /// alone leaves `ledger_verify` accusing every ledgered record (issue #2323).
    PurgeLedgerTablesUnpaired {
        /// The ledger table that was named.
        listed: String,
        /// The ledger table that must be named alongside it.
        missing: String,
    },
    /// A backup/restore step (artifact restore, `--output` re-dump) failed.
    Backup(Box<super::backup::BackupError>),
    /// The `--sample` subset could not be resolved or verified (issue #1636).
    Sample(Box<sample::SampleError>),
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
                 replace it, or in `safe` to keep it verbatim.",
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
                floor,
            } => write!(
                f,
                "{column:?} holds at most {limit} characters, but the chosen strategy needs \
                 {overhead} for its fixed text plus at least {floor} more for a per-row token.\n  \
                 Strategies by fixed overhead: `phone` (5), `name` (9), `redact` (11), \
                 `email` (25). Pick one that fits, use `null` if the column is nullable, \
                 widen the column, or declare it `safe`."
            ),
            Self::PlaintextIntoEncrypted { columns } => write!(
                f,
                "{} column(s) carry #[encrypted] in the model but {SCRUB_CONFIG_FILE} declares a \
                 plaintext strategy for them:\n{}\n  \
                 Writing a plain string into an at-rest-encrypted column makes every later read \
                 of that row fail as malformed ciphertext. An #[encrypted] column is \
                 re-encrypted automatically — remove the declaration, or use `null` if the \
                 column is nullable.",
                columns.len(),
                bullet_list(columns),
            ),
            Self::CheckConstrainedColumn { column } => write!(
                f,
                "{column:?} is covered by a CHECK constraint, so no fabricated value can be \
                 proven to satisfy it (a closed-set column reaches the database as TEXT plus a \
                 CHECK, which is why the type alone does not reveal this).\n  \
                 Declare the column `safe` if the constraint means it holds no free-form PII, \
                 or drop the constraint on the copy before scrubbing."
            ),
            Self::EncryptionKeyUnavailable { profile, detail } => write!(
                f,
                "This database has #[encrypted] columns, but no encryption key could be resolved \
                 for the {profile:?} profile: {detail}\n  \
                 A scrub must write a VALID ciphertext envelope into an encrypted column — a \
                 plaintext replacement would make every later read of that row fail. Provide the \
                 target's `active_record_encryption` credentials (`autumn credentials edit`), or \
                 declare those columns `null` in {SCRUB_CONFIG_FILE} if they are nullable."
            ),
            Self::EncryptedMetadataUnavailable => write!(
                f,
                "No model source was found, so the scrub cannot tell which columns are \
                 #[encrypted].\n  \
                 That matters more than it sounds: an unrecognised encrypted column declared \
                 `safe` keeps its production ciphertext, and one given a plaintext strategy \
                 becomes permanently unreadable. Run the scrub from the project root (where \
                 `src/models` lives), or name them — with their mode — in {SCRUB_CONFIG_FILE}:\n    \
                 [tables.users.encrypted]\n    api_token = \"randomized\"\n    \
                 email = \"deterministic\"\n  \
                 An app with no encrypted columns at all still needs one empty \
                 `[tables.<any>.encrypted]` section to say so deliberately."
            ),
            Self::InaccessibleTables { tables } => write!(
                f,
                "{} table(s) exist in `public` but could not be read by the connecting \
                 role:\n{}\n  \
                 They never reached the classifier, so the scrub cannot claim they carry no \
                 PII — and it will not rewrite them either. Connect as a role that can see \
                 every table (the owner, or one with the needed privileges).",
                tables.len(),
                bullet_list(tables),
            ),
            Self::UnsupportedSchemas { schemas } => write!(
                f,
                "This database has base tables in {} schema(s) outside `public`:\n{}\n  \
                 The classification universe is `public`-only, so a scrub cannot prove those \
                 tables carry no PII and refuses rather than reporting a completeness it did \
                 not check. Scrub those schemas separately, or drop them from the copy.",
                schemas.len(),
                bullet_list(schemas),
            ),
            Self::RowLevelSecurity { tables } => write!(
                f,
                "{} table(s) the scrub would rewrite have row-level security enabled:\n{}\n  \
                 A role that does not bypass RLS updates only the rows its policies expose and \
                 reports success, leaving the rest of the PII in place — a silent partial scrub. \
                 Connect as the table owner or a BYPASSRLS role.",
                tables.len(),
                bullet_list(tables),
            ),
            Self::FrameworkTableDeclared { tables } => write!(
                f,
                "{SCRUB_CONFIG_FILE} declares {} framework-owned table(s) under [tables.*]:\n{}\n  \
                 Framework-owned tables are deliberately outside the column classification \
                 (exactly as `autumn db pull` and `autumn schema pull` skip them), so a \
                 per-column declaration for them has no effect. Use \
                 `[framework] purge = [...]` to empty one instead.",
                tables.len(),
                bullet_list(tables),
            ),
            Self::PurgeSchemaBookkeeping { tables } => write!(
                f,
                "[framework] purge in {SCRUB_CONFIG_FILE} names {} table(s) that hold schema \
                 bookkeeping, not app payloads:\n{}\n  \
                 Emptying them would make the copy un-migratable or un-routable. Remove them \
                 from `purge`.",
                tables.len(),
                bullet_list(tables),
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
                 The scrub would overwrite the source the artifact was taken from. Point the \
                 target at a separate staging database, or re-run with \
                 --allow-source-overwrite if that really is what you mean."
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
            Self::PurgeLedgerTablesUnpaired { listed, missing } => write!(
                f,
                "[framework] purge in {SCRUB_CONFIG_FILE} names {listed:?} but not \
                 {missing:?}.\n  \
                 The ledger's two tables are emptied together or not at all: a high-water \
                 mark is built to outlive the revisions it names, so a copy holding one \
                 without the other makes `ledger_verify` report every ledgered record as \
                 tampered — and, with the marks kept, makes the write path refuse every \
                 subsequent write. Add {missing:?} to `purge`, or remove {listed:?}."
            ),
            Self::Backup(e) => write!(f, "{e}"),
            Self::Sample(e) => write!(f, "{e}"),
        }
    }
}

impl From<sample::SampleError> for ScrubError {
    fn from(e: sample::SampleError) -> Self {
        Self::Sample(Box::new(e))
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
    /// Re-encrypt: replace an `#[encrypted]` column with a valid AEAD envelope
    /// of a fake plaintext, produced under the target database's own key.
    ///
    /// This is the only strategy that cannot be expressed as SQL — writing a
    /// plain string into an `#[encrypted]` column would make every subsequent
    /// repository read of that row fail as malformed ciphertext, so the
    /// replacement is built in Rust and shipped back per row.
    Encrypted,
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
            Self::Encrypted => "encrypted",
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
            Self::Email
                | Self::Name
                | Self::Redact
                | Self::Null
                | Self::Uuid
                | Self::Bytes
                | Self::Encrypted
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

/// The at-rest encryption mode a column was written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// `#[encrypted]` — a fresh nonce per write.
    Randomized,
    /// `#[encrypted(deterministic)]` — equality-queryable.
    Deterministic,
}

impl EncryptionMode {
    /// Whether this is the deterministic mode.
    const fn is_deterministic(self) -> bool {
        matches!(self, Self::Deterministic)
    }
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
    /// At-rest-encrypted columns and their mode, for a host that has the CLI and
    /// `scrub.toml` but not the model source the `#[encrypted]` markers live in.
    ///
    /// The mode matters: re-encrypting a `deterministic` column in randomized
    /// mode leaves valid ciphertext that the app can no longer equality-query,
    /// so it cannot be guessed.
    #[serde(default)]
    pub encrypted: BTreeMap<String, EncryptionMode>,
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
    /// Per-table subsetting rules for `--sample` (issue #1636).
    #[serde(default)]
    pub sample: sample::SampleRules,
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
    // A full verbatim copy of every mutated row, including `#[private]` and
    // `#[encrypted]` columns — so an unscrubbed ledger hands back exactly the
    // plaintext the column-level scrub just removed.
    "_autumn_ledger_revisions",
    // Listed for lock-step emptying rather than because it carries a payload:
    // the #2323 high-water marks hold only table names, tenant keys, sequence
    // numbers and hashes. But a mark outlives the chain it names, so a copy that
    // purged the revisions and kept the marks would have `ledger_verify` report
    // *every* ledgered record as `MissingRevision` — "the whole chain was
    // erased" — and the write path would then refuse every subsequent write.
    // `check_purge_list` refuses to empty one of the pair without the other.
    "_autumn_ledger_high_water",
    // Before/after values for every tracked column; only those named in
    // `#[version_history(sensitive = [...])]` are redacted.
    "_autumn_version_history",
    // Hashed API tokens minted in production. A staging copy that inherits them
    // is a live credential leak, not merely a PII one.
    "api_tokens",
    "autumn_experiment_assignments",
    // `actor` on both: who was pinned to a variant, and who changed what.
    "autumn_experiment_changes",
    "autumn_experiment_overrides",
    // `actor_allowlist` names the individual users a flag is switched on for.
    "autumn_feature_flags",
    "autumn_job_tracking",
    "autumn_jobs",
    // `context` / `record` JSONB hold the full row a hook was queued for.
    "autumn_repository_commit_hooks",
    // The indexed text of app records — the search index is a second copy of
    // whatever was made searchable.
    "autumn_search_documents",
    "autumn_sync_applied",
    "autumn_sync_pending",
    "autumn_sync_rows",
    // `actor` records who made each flag change.
    "feature_flag_changes",
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

/// Framework-owned tables `[framework] purge` must never accept: emptying them
/// does not remove a payload, it breaks the copy. `__diesel_schema_migrations`
/// and `autumn_migration_checksums` are the migration ledger (an empty one
/// replays every migration against a populated database); `_autumn_shard_map` /
/// `_autumn_shard_directory` are the routing tables a sharded app reads at boot.
const NEVER_PURGEABLE_TABLES: &[&str] = &[
    "__diesel_schema_migrations",
    "_autumn_shard_directory",
    "_autumn_shard_map",
    "autumn_migration_checksums",
];

/// Validate a `[framework] purge` list: it may only name framework-owned
/// tables, and never schema bookkeeping. A user table listed there would be
/// silently emptied, which is never what a scrub should do behind a one-word
/// config key.
fn check_purge_list(config: &ScrubConfig) -> Result<(), ScrubError> {
    let mut bookkeeping: Vec<String> = config
        .framework
        .purge
        .iter()
        .filter(|t| NEVER_PURGEABLE_TABLES.contains(&t.as_str()))
        .cloned()
        .collect();
    if !bookkeeping.is_empty() {
        bookkeeping.sort();
        return Err(ScrubError::PurgeSchemaBookkeeping {
            tables: bookkeeping,
        });
    }
    let mut offenders: Vec<String> = config
        .framework
        .purge
        .iter()
        .filter(|t| !is_framework_table(t))
        .cloned()
        .collect();
    if !offenders.is_empty() {
        offenders.sort();
        return Err(ScrubError::PurgeNotFrameworkTable { tables: offenders });
    }
    check_ledger_purge_pairing(config)
}

/// The two ledger tables are emptied together or not at all (issue #2323).
///
/// A high-water mark is deliberately built to outlive the revisions it names —
/// that is what makes a deleted revision permanent evidence. So a staging copy
/// that purged `_autumn_ledger_revisions` and kept `_autumn_ledger_high_water`
/// would have `ledger_verify` report every ledgered record as a wholly erased
/// chain, and the write path would refuse every subsequent write to them. The
/// reverse — marks purged, revisions kept — is the same shape from the other
/// side: every record reports `HighWaterMissing`.
///
/// Neither is a state an operator asking for a scrub meant to create, and both
/// are silent until something reads the ledger, so the pairing is enforced here
/// rather than left as documentation.
fn check_ledger_purge_pairing(config: &ScrubConfig) -> Result<(), ScrubError> {
    let has = |table: &str| config.framework.purge.iter().any(|t| t == table);
    let (revisions, marks) = (has(LEDGER_REVISIONS_TABLE), has(LEDGER_HIGH_WATER_TABLE));
    if revisions == marks {
        return Ok(());
    }
    let (listed, missing) = if revisions {
        (LEDGER_REVISIONS_TABLE, LEDGER_HIGH_WATER_TABLE)
    } else {
        (LEDGER_HIGH_WATER_TABLE, LEDGER_REVISIONS_TABLE)
    };
    Err(ScrubError::PurgeLedgerTablesUnpaired {
        listed: listed.to_owned(),
        missing: missing.to_owned(),
    })
}

/// The ledger's revision rows.
const LEDGER_REVISIONS_TABLE: &str = "_autumn_ledger_revisions";
/// The ledger's out-of-band high-water marks (issue #2323).
const LEDGER_HIGH_WATER_TABLE: &str = "_autumn_ledger_high_water";

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

/// Parse a `scrub.toml` document, attributing any error to the file it came
/// from.
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
    /// `#[encrypted]` columns keyed by table, each mapped to whether the model
    /// declared `#[encrypted(deterministic)]`.
    pub encrypted: &'a BTreeMap<String, BTreeMap<String, bool>>,
    /// Tables registered with the GDPR anonymize strategy.
    pub anonymize_tables: &'a BTreeSet<String>,
    /// Catalog facts the schema IR does not carry (see [`DatabaseFacts`]).
    pub facts: &'a DatabaseFacts,
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

/// One `#[encrypted]` column's rewrite. Its replacement is an AEAD envelope
/// built in Rust per row, so it cannot join the table's batched `UPDATE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedRewrite {
    /// The column name.
    pub column: String,
    /// Whether the model declared `#[encrypted(deterministic)]`, so equality
    /// lookups against the column keep working after the scrub.
    pub deterministic: bool,
    /// The shape of the fake plaintext to encrypt ([`Strategy::Email`] for an
    /// email-named column, else [`Strategy::Redact`]).
    pub shape: Strategy,
}

/// One table's scrub statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePlan {
    /// The table name.
    pub table: String,
    /// The columns rewritten, in table column order.
    pub columns: Vec<ColumnPlan>,
    /// The single `UPDATE` rewriting every SQL-expressible column, or `None`
    /// when the table's only PII columns are `#[encrypted]` ones.
    pub sql: Option<String>,
    /// The SQL expression identifying a row (see [`row_key_expr`]), reused to
    /// match rows when shipping encrypted replacements back.
    pub row_key: String,
    /// Columns whose replacement must be produced in Rust.
    pub encrypted: Vec<EncryptedRewrite>,
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
#[allow(clippy::too_many_lines)]
pub fn build_plan(inputs: &ClassificationInputs<'_>) -> Result<ScrubPlan, ScrubError> {
    let by_name: BTreeMap<&str, &Table> =
        inputs.tables.iter().map(|t| (t.name.as_str(), t)).collect();

    check_config_freshness(inputs, &by_name)?;
    check_contradictions(inputs)?;
    check_safe_overrides_encrypted(inputs, &by_name)?;

    let mut unclassified = Vec::new();
    let mut key_pii = Vec::new();
    let mut plaintext_into_encrypted: Vec<(String, &'static str)> = Vec::new();
    let mut planned: Vec<(&Table, Vec<(ColumnPlan, &Column)>)> = Vec::new();

    for table in inputs.tables {
        // A partition's rows are rewritten through its parent, so planning it
        // again would double-update them (and, on a table with no primary key,
        // re-randomize the values the parent pass just wrote).
        if inputs.facts.partitions.contains(&table.name) {
            continue;
        }
        let rule = inputs.config.tables.get(&table.name);
        let anonymized = inputs.anonymize_tables.contains(&table.name);
        let encrypted = inputs.encrypted.get(&table.name);
        let mut columns = Vec::new();

        for column in &table.columns {
            let qualified = format!("{}.{}", table.name, column.name);
            // A generated column is derived data Postgres refuses to `UPDATE`
            // at all — scrubbing the columns it reads already covers it — so it
            // is structurally safe rather than something to declare.
            if inputs
                .facts
                .generated_columns
                .contains(&(table.name.clone(), column.name.clone()))
            {
                continue;
            }
            let is_key = is_key_column(table, column, inputs.facts);
            let declared_pii = rule.and_then(|r| r.pii.get(&column.name)).copied();
            let is_encrypted = encrypted.is_some_and(|e| e.contains_key(&column.name));
            let declared_safe_here = rule.is_some_and(|r| r.safe.contains(&column.name));
            // A cross-table convenience list is not a per-column review, so it
            // may not narrow a table-level GDPR anonymize registration: only an
            // explicit `[tables.<t>] safe` entry can.
            let declared_safe = declared_safe_here
                || (!anonymized && inputs.config.defaults.safe_columns.contains(&column.name));

            let (spec, source) = if is_encrypted {
                // An at-rest-encrypted column is never rewritten with a plain
                // string: the resolved strategy is always a re-encryption (see
                // `Strategy::Encrypted`), so a declaration can only choose to
                // NULL it, never to write plaintext into it.
                match declared_pii {
                    None | Some(Strategy::Encrypted) => {
                        (Strategy::Encrypted, ClassSource::Encrypted)
                    }
                    Some(Strategy::Null) => (Strategy::Null, ClassSource::Config),
                    Some(other) => {
                        plaintext_into_encrypted.push((qualified, other.as_str()));
                        continue;
                    }
                }
            } else if let Some(strategy) = declared_pii {
                (strategy, ClassSource::Config)
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

    if !plaintext_into_encrypted.is_empty() {
        plaintext_into_encrypted.sort();
        return Err(ScrubError::PlaintextIntoEncrypted {
            columns: plaintext_into_encrypted
                .into_iter()
                .map(|(column, strategy)| format!("{column} (declared `{strategy}`)"))
                .collect(),
        });
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
        let mut encrypted_rewrites = Vec::new();
        for (mut plan, column) in columns {
            let qualified = format!("{}.{}", table.name, column.name);
            let pair = (table.name.clone(), column.name.clone());
            let unique = is_unique_column(table, column, inputs.facts);

            // A `CHECK` predicate is arbitrary SQL, so no fabricated value can be
            // proven to satisfy it — and a real Autumn closed-set column reaches
            // the database as plain `TEXT` plus a `CHECK`, so this (not the
            // model-IR-only `ColumnType::Enum`) is what actually catches it.
            if inputs.facts.checked_columns.contains(&pair) {
                return Err(ScrubError::CheckConstrainedColumn { column: qualified });
            }
            if plan.strategy == Strategy::Auto {
                plan.strategy = auto_strategy(column).map_err(|e| qualify(e, &qualified))?;
            }
            if plan.strategy == Strategy::Null {
                if !column.nullable {
                    return Err(ScrubError::NullOnNotNull { column: qualified });
                }
                // Postgres normally allows any number of NULLs in a unique
                // index — but not under `NULLS NOT DISTINCT`.
                if inputs.facts.nulls_not_distinct_columns.contains(&pair) {
                    return Err(ScrubError::NonUniqueStrategy {
                        column: qualified,
                        strategy: plan.strategy.as_str(),
                    });
                }
            }
            if unique && !plan.strategy.allowed_on_unique() {
                return Err(ScrubError::NonUniqueStrategy {
                    column: qualified,
                    strategy: plan.strategy.as_str(),
                });
            }

            if plan.strategy == Strategy::Encrypted {
                // Not expressible as SQL: the replacement is an AEAD envelope
                // produced in Rust, row by row, under the target's own key.
                encrypted_rewrites.push(EncryptedRewrite {
                    column: column.name.clone(),
                    deterministic: inputs
                        .encrypted
                        .get(&table.name)
                        .and_then(|c| c.get(&column.name))
                        .copied()
                        .unwrap_or(false),
                    shape: email_shaped(&column.name),
                });
                resolved.push(plan);
                continue;
            }

            let token = token_expr(table, &column.name, unique);
            let value = replacement_expr(plan.strategy, column, &token, unique)
                .map_err(|e| qualify(e, &qualified))?;
            assignments.push(assignment(column, &value, plan.strategy));
            resolved.push(plan);
        }
        out.tables.push(TablePlan {
            table: table.name.clone(),
            columns: resolved,
            sql: (!assignments.is_empty()).then(|| {
                format!(
                    "UPDATE {} SET {}",
                    qualified_ident(&table.name),
                    assignments.join(", ")
                )
            }),
            row_key: row_key_expr(table),
            encrypted: encrypted_rewrites,
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
            limit,
            overhead,
            floor,
            ..
        } => ScrubError::ColumnTooNarrow {
            column,
            limit,
            overhead,
            floor,
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
    let mut framework = Vec::new();
    for (name, rule) in &inputs.config.tables {
        let Some(table) = by_name.get(name.as_str()) else {
            // A framework-owned table is not "missing" — it is deliberately
            // outside the classified universe, and saying "the database does
            // not have it" would send the developer hunting for a typo.
            if is_framework_table(name) {
                framework.push(name.clone());
            } else {
                stale.push(name.clone());
            }
            continue;
        };
        let columns: BTreeSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        for column in rule
            .safe
            .iter()
            .chain(rule.pii.keys())
            .chain(rule.encrypted.keys())
        {
            if !columns.contains(column.as_str()) {
                stale.push(format!("{name}.{column}"));
            }
        }
    }
    if !framework.is_empty() {
        framework.sort();
        return Err(ScrubError::FrameworkTableDeclared { tables: framework });
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
        for column in encrypted.keys() {
            // An explicit PII entry is not an override — it only picks the
            // strategy — so only `safe` declarations conflict.
            if rule.is_some_and(|r| r.pii.contains_key(column)) {
                continue;
            }
            // A key column is exempt: it can never be rewritten anyway, so
            // refusing its `safe` declaration would leave the developer with no
            // configuration that terminates.
            //
            // This asks `is_key_column` rather than re-deriving the test from
            // the IR flags alone: the catalog contributes two more sources (the
            // REFERENCED side of a foreign key, and generated columns) that the
            // IR does not carry, so the two tests disagreed about what counts as
            // structural.
            //
            // Necessary but NOT sufficient to make such a column configurable:
            // classification resolves `#[encrypted]` before it looks at `safe`,
            // so a structural encrypted column still fails as `PiiOnKeyColumn`
            // with no declaration that terminates. Tracked in #2366.
            if by_name
                .get(table.as_str())
                .and_then(|t| {
                    t.columns
                        .iter()
                        .find(|c| &c.name == column)
                        .map(|c| (*t, c))
                })
                .is_some_and(|(t, c)| is_key_column(t, c, inputs.facts))
            {
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

/// Base tables the catalog reports in `public` that introspection did not return
/// and that are not framework-owned — i.e. ones the connecting role cannot see.
fn unreachable_tables(facts: &DatabaseFacts, introspected: &[Table]) -> Vec<String> {
    let seen: BTreeSet<&str> = introspected.iter().map(|t| t.name.as_str()).collect();
    let mut missing: Vec<String> = facts
        .public_base_tables
        .iter()
        .filter(|t| !seen.contains(t.as_str()) && !is_framework_table(t))
        .cloned()
        .collect();

    // Privileges are per COLUMN as well as per table: a table can be visible
    // while some of its columns are not, and those would be scrubbed by nothing
    // while the table itself classified cleanly.
    let seen_columns: BTreeSet<(&str, &str)> = introspected
        .iter()
        .flat_map(|t| {
            t.columns
                .iter()
                .map(move |c| (t.name.as_str(), c.name.as_str()))
        })
        .collect();
    missing.extend(
        facts
            .public_columns
            .iter()
            .filter(|(table, column)| {
                seen.contains(table.as_str())
                    && !is_framework_table(table)
                    && !seen_columns.contains(&(table.as_str(), column.as_str()))
            })
            .map(|(table, column)| format!("{table}.{column}")),
    );
    missing.sort();
    missing.dedup();
    missing
}

/// Whether a column is structural — a primary key, either side of any foreign
/// key, or a generated column Postgres will not let an `UPDATE` touch.
///
/// The foreign-key half comes from [`DatabaseFacts`], not from the schema IR:
/// the IR records only the *referencing* side and only a composite key's first
/// component, so a natural key another table points at (`users.email` ←
/// `orders.user_email`) would otherwise look freely rewritable and fail the
/// constraint at apply time — or, under `ON UPDATE CASCADE`, silently rewrite a
/// child column that was declared safe.
fn is_key_column(table: &Table, column: &Column, facts: &DatabaseFacts) -> bool {
    let pair = (table.name.clone(), column.name.clone());
    column.primary_key
        || table.primary_key.contains(&column.name)
        || column.references.is_some()
        || facts.foreign_key_columns.contains(&pair)
        || facts.generated_columns.contains(&pair)
}

/// Whether a rewrite of this column could violate a uniqueness constraint.
///
/// This is deliberately **broader** than the schema IR's single-column `unique`
/// flag, which answers the migration-diff question "does this satisfy a model
/// `#[unique]`" and therefore excludes composite and partial unique indexes. For
/// a writer both of those still abort the statement: one member of a composite
/// unique key set to a constant collides as soon as its partner repeats, and a
/// partial unique index constrains every row its predicate matches. So the
/// probed `unique_columns` set — every column of every unique index — is what
/// gates strategy choice.
fn is_unique_column(table: &Table, column: &Column, facts: &DatabaseFacts) -> bool {
    column.unique
        || facts
            .unique_columns
            .contains(&(table.name.clone(), column.name.clone()))
        || table.indexes.iter().any(|index| {
            if !index.unique {
                return false;
            }
            let keys = if index.key_columns.is_empty() {
                &index.columns
            } else {
                &index.key_columns
            };
            keys.iter().any(|k| k == &column.name)
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
    if keys.len() == 1 {
        return format!("coalesce({}::text, '')", quote_ident(keys[0]));
    }
    // `ROW(...)::text` renders a composite key with Postgres's own quoting, so
    // ('a|','b') and ('a','|b') cannot collapse to one row key the way a plain
    // separator-joined concatenation does.
    format!(
        "ROW({})::text",
        keys.iter()
            .map(|key| quote_ident(key))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The per-row, per-column token every replacement is derived from. Salting with
/// the column name keeps two PII columns of one row from receiving identical
/// fake values.
fn token_expr(table: &Table, column: &str, unique: bool) -> String {
    token_expr_from(&row_key_expr(table), column, unique)
}

/// [`token_expr`] against an already-computed row key.
///
/// A column that must stay unique gets a 64-hex-character `sha256` token rather
/// than `md5`'s 32, so a length-bounded column still has room for a token wide
/// enough to make collisions impossible in practice (see [`bounded_token`]).
fn token_expr_from(row_key: &str, column: &str, unique: bool) -> String {
    let seed = format!("{} || '|' || {row_key}", quote_literal(column));
    if unique {
        // Two independently-salted `md5`s concatenated, rather than `sha256`:
        // both halves would have to collide at once, and this needs no
        // `text`-to-`bytea` cast (whose escape-format interpretation would
        // mangle a row key containing a backslash) and no Postgres 11 floor.
        format!("md5({seed}) || md5(({seed}) || '#2')")
    } else {
        format!("md5({seed})")
    }
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
        // `from_pg_udt` maps only Autumn's own scalar surface, so several
        // everyday PII column types arrive as `Opaque` — `date_of_birth DATE`
        // most of all. Without these arms `auto` refuses them and the explicit
        // fallbacks (`epoch`, `zero`) reject them too, leaving no usable
        // strategy at all.
        ColumnType::Opaque { pg_type }
            if matches!(pg_type.as_str(), "date" | "time" | "timetz") =>
        {
            Strategy::Epoch
        }
        ColumnType::Opaque { pg_type }
            if matches!(pg_type.as_str(), "int2" | "money" | "oid")
                || pg_type.starts_with("numeric") =>
        {
            Strategy::Zero
        }
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
#[allow(clippy::too_many_lines)]
fn replacement_expr(
    strategy: Strategy,
    column: &Column,
    token: &str,
    unique: bool,
) -> Result<String, ScrubError> {
    let limit = char_max_len(&column.ty);
    let narrow = |overhead: usize| -> Result<String, ScrubError> {
        bounded_token(token, limit, overhead, &column.name, unique)
    };
    // Every text-shaped strategy needs a character column to land in. Without
    // this gate `age = "redact"` on an `integer` (or `last_login_ip = "redact"`
    // on an `inet`) passes classification and only fails once Postgres runs the
    // statement — after an `--artifact` restore has already written real data.
    if matches!(
        strategy,
        Strategy::Email | Strategy::Name | Strategy::Redact | Strategy::Phone
    ) {
        require_type(column, strategy, is_texty(&column.ty))?;
    }
    Ok(match strategy {
        Strategy::Auto => replacement_expr(auto_strategy(column)?, column, token, unique)?,
        Strategy::Encrypted => {
            return Err(ScrubError::StrategyTypeMismatch {
                column: column.name.clone(),
                strategy: strategy.as_str(),
                detail: "an encrypted replacement is built in Rust, not in SQL".to_owned(),
            });
        }
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
                    floor: PHONE_DIGITS,
                });
            }
            format!("'+1555' || translate(substr({token}, 1, {PHONE_DIGITS}), 'abcdef', '0123456')")
        }
        Strategy::Null => "NULL".to_owned(),
        Strategy::Uuid => {
            require_type(column, strategy, matches!(column.ty, ColumnType::Uuid))?;
            // A UUID is exactly 128 bits, and Postgres rejects any other width —
            // so the wider token a unique column gets must be trimmed to its
            // first 32 hex characters. That is the full entropy a UUID can hold,
            // so nothing is lost.
            format!("(substr({token}, 1, 32))::uuid")
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
                ty if is_texty(ty) => {
                    // Unlike every other text-producing strategy this one is a
                    // fixed literal, so a narrow `varchar(n)` has to be checked
                    // explicitly rather than by narrowing a token.
                    const JSON_LITERAL_LEN: usize = 18;
                    if let Some(limit) = limit
                        && limit < JSON_LITERAL_LEN
                    {
                        return Err(ScrubError::ColumnTooNarrow {
                            column: column.name.clone(),
                            limit,
                            overhead: JSON_LITERAL_LEN,
                            floor: 0,
                        });
                    }
                    json.to_owned()
                }
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
            ColumnType::Opaque { pg_type }
                if pg_type.starts_with("numeric")
                    || matches!(pg_type.as_str(), "int2" | "money" | "oid") =>
            {
                "0".to_owned()
            }
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
            ColumnType::Opaque { pg_type } if pg_type == "date" => "'1970-01-01'::date".to_owned(),
            ColumnType::Opaque { pg_type } if pg_type == "time" => "'00:00:00'::time".to_owned(),
            ColumnType::Opaque { pg_type } if pg_type == "timetz" => {
                "'00:00:00+00'::timetz".to_owned()
            }
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
    unique: bool,
) -> Result<String, ScrubError> {
    let (full, floor) = if unique {
        (UNIQUE_TOKEN_HEX_LEN, MIN_UNIQUE_TOKEN_WIDTH)
    } else {
        (TOKEN_HEX_LEN, MIN_TOKEN_WIDTH)
    };
    let Some(limit) = limit else {
        return Ok(token.to_owned());
    };
    let available = limit.saturating_sub(overhead);
    if available >= full {
        Ok(token.to_owned())
    } else if available >= floor {
        Ok(format!("substr({token}, 1, {available})"))
    } else {
        Err(ScrubError::ColumnTooNarrow {
            column: column.to_owned(),
            limit,
            overhead,
            floor,
        })
    }
}

/// One `SET` clause. A nullable column keeps its `NULL`s (a scrub anonymizes
/// values, it does not invent them), so it is wrapped in a `CASE`; a `NOT NULL`
/// column needs no guard.
fn assignment(column: &Column, value: &str, strategy: Strategy) -> String {
    let ident = quote_ident(&column.name);
    // A `CASE` whose arms are both bare `NULL` has no type to infer from, so
    // Postgres resolves it to `text` and the assignment fails on every
    // non-character column. The guard is pointless there anyway: the
    // replacement already IS null.
    if column.nullable && strategy != Strategy::Null {
        format!("{ident} = CASE WHEN {ident} IS NULL THEN NULL ELSE {value} END")
    } else {
        format!("{ident} = {value}")
    }
}

/// A `public`-qualified table identifier.
///
/// Every catalog read that produced the plan is scoped to `public`, so the
/// writes must be too: a database- or role-level `search_path` (which Autumn
/// supports for tenant schemas) would otherwise resolve a bare `UPDATE "users"`
/// to a *different* table than the one that was classified — leaving the
/// classified rows unscrubbed and overwriting rows nothing planned.
fn qualified_ident(table: &str) -> String {
    format!("\"public\".{}", quote_ident(table))
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
) -> Result<BTreeMap<String, BTreeMap<String, bool>>, ScrubError> {
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

/// Refuse to scrub a database that a **config file** declares for a
/// production-ish profile — the "I pointed staging at the production URL"
/// mistake the profile guard alone cannot see.
///
/// Every non-dev/test profile with an `autumn-<profile>.toml` in the project is
/// checked, not just the one an artifact's manifest names. A bare `.dump`/`.sql`
/// artifact carries no manifest at all, so keying the guard off known provenance
/// would silently skip it in exactly the case where the operator knows least
/// about what they are restoring.
///
/// Deliberately reads only `autumn.toml` / `autumn-<profile>.toml`, never the
/// environment: an env-provided `DATABASE_URL` is shared by every profile
/// resolution, so consulting it would make this guard fire on legitimate scrubs.
///
/// It has its own waiver (`--allow-source-overwrite`) rather than riding on
/// `--force`: the documented staging drill ALWAYS passes `--force` (staging is
/// not `dev`/`test`), so a guard that `--force` waived would be inert in exactly
/// the workflow it exists for.
fn guard_configured_source(
    artifact_profile: Option<&str>,
    project_root: &Path,
    targets: &[(String, String)],
    allowed: bool,
) -> Result<(), ScrubError> {
    if allowed {
        return Ok(());
    }
    let mut candidates: Vec<String> = artifact_profile
        .map(str::to_owned)
        .into_iter()
        .chain(profiles_with_config(project_root))
        .filter(|p| !super::is_safe_destructive_profile(p))
        .collect();
    candidates.sort();
    candidates.dedup();

    for profile in candidates {
        let table = migrate::read_autumn_toml_table_with_profile(Some(&profile));
        let Some(declared) = migrate::resolve_primary_database_url_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            table.as_ref(),
        ) else {
            continue;
        };
        for (_, url) in targets {
            if same_database(&declared, url) {
                return Err(ScrubError::OverwritesConfiguredTarget {
                    profile,
                    database: parsed_db_name(url),
                });
            }
        }
    }
    Ok(())
}

/// Profile names that have an `autumn-<profile>.toml` overlay in the project.
fn profiles_with_config(project_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("autumn-")
                .and_then(|rest| rest.strip_suffix(".toml"))
                .map(str::to_owned)
        })
        .filter(|p| !p.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
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
            // stdout, not stderr: the diagnostics above are stderr, so
            // `autumn db scrub --check 2>/dev/null >> scrub.toml` appends a
            // valid stanza instead of a wall of interleaved prose.
            println!("{}", suggested_config_stanza(columns));
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

    // Everything that does NOT need the database is resolved first, so a typo in
    // `scrub.toml`, an unknown strategy, a `purge` entry naming a user table, or
    // an unparsable model file fails BEFORE an `--artifact` restore writes real
    // data into the target. Only schema-dependent refusals can land after it.
    let sources = load_source_classification(args)?;

    let targets = super::backup::resolve_all_target_urls(args.profile.as_deref())?;

    let restored = if let Some(artifact) = &args.artifact {
        let artifact_profile = super::backup::artifact_source_profile(artifact);
        // `--check`/`--dry-run` promise to write nothing, and a restore is the
        // largest write there is. clap already rejects the combination; this
        // keeps the invariant true inside the function that relies on it.
        debug_assert!(
            writes,
            "--check/--dry-run must never reach an artifact restore"
        );
        eprintln!(
            "  \u{2139} Artifact provenance: {}.",
            artifact_profile.as_deref().map_or_else(
                || "unknown (no manifest)".to_owned(),
                |p| format!("{p:?} profile")
            )
        );
        guard_configured_source(
            artifact_profile.as_deref(),
            Path::new("."),
            &targets,
            args.allow_source_overwrite,
        )?;
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

    // The classification that remains reads the schema the restore just created,
    // so it cannot run earlier — and a refusal here leaves the artifact's real
    // data in the target. Say so in the loudest possible terms.
    classify_and_apply(args, &profile, &targets, &sources).inspect_err(|_| {
        if restored {
            eprintln!(
                "\n\u{26A0}\u{FE0F}  The artifact was ALREADY RESTORED before this failure, so \
                 the target database now holds UNSCRUBBED data.\n  \
                 Do not hand it to anyone: fix the problem below and re-run the same \
                 command, or drop the database."
            );
        }
    })
}

/// The classification inputs that come from files rather than from the database.
struct SourceClassification {
    config: ScrubConfig,
    encrypted: BTreeMap<String, BTreeMap<String, bool>>,
    anonymize: BTreeSet<String>,
    /// Parsed `--sample` roots. Empty means the whole copy is kept.
    roots: Vec<sample::SampleSpec>,
}

/// Read and validate every file-based classification source, reporting what each
/// one contributed.
///
/// The counts are printed rather than assumed: both automatic sources degrade to
/// empty when the command runs outside the project root (a deployed staging host
/// often has the binary and `scrub.toml` but no source tree), and a silently
/// empty `#[encrypted]` map would also silently disable the "a `safe`
/// declaration may not override `#[encrypted]`" refusal.
fn load_source_classification(args: &ScrubArgs) -> Result<SourceClassification, ScrubError> {
    let config = load_config(args.config.as_deref())?;
    check_purge_list(&config)?;
    let project_root = Path::new(".");
    let models_path = crate::schema::existing_models_path(project_root);
    let declared_encrypted = config
        .tables
        .iter()
        .any(|(_, rule)| !rule.encrypted.is_empty());

    // Without the model source there is no way to know WHICH columns are
    // `#[encrypted]`, and an unknown one is the worst possible outcome: declared
    // `safe` it keeps production ciphertext, given a plaintext strategy it
    // becomes permanently unreadable. So this is a refusal, not a warning —
    // unless the declaration names them (and their mode) itself.
    if models_path.is_none() && !declared_encrypted {
        return Err(ScrubError::EncryptedMetadataUnavailable);
    }

    let mut encrypted = encrypted_columns(project_root)?;
    // A declaration supplements the model scan (and supplies everything when
    // there is no model source at all); the model stays authoritative where the
    // two overlap, since it is the definition rather than a copy of it.
    for (table, rule) in &config.tables {
        for (column, mode) in &rule.encrypted {
            encrypted
                .entry(table.clone())
                .or_default()
                .entry(column.clone())
                .or_insert_with(|| mode.is_deterministic());
        }
    }
    let anonymize = scan_anonymize_tables(&project_root.join("src"))?;

    let encrypted_count: usize = encrypted.values().map(BTreeMap::len).sum();
    eprintln!(
        "  \u{2139} Automatic classification: {encrypted_count} #[encrypted] column(s), \
         {} GDPR anonymize registration(s).",
        anonymize.len()
    );

    // Parsed here rather than at the call site so a mistyped `--sample` fails
    // alongside every other file-based refusal: BEFORE an `--artifact` restore
    // writes real data into the target.
    let roots = args
        .sample
        .iter()
        .map(|spec| sample::parse_spec(spec))
        .collect::<Result<Vec<_>, _>>()?;
    if roots.is_empty() && sources_declare_sampling(&config) {
        eprintln!(
            "  \u{2139} scrub.toml declares [sample] rules, but no --sample root was \
             given \u{2014} the whole copy is kept."
        );
    }

    Ok(SourceClassification {
        config,
        encrypted,
        anonymize,
        roots,
    })
}

/// Whether `scrub.toml` carries any `[sample]` rule at all.
const fn sources_declare_sampling(config: &ScrubConfig) -> bool {
    !config.sample.always_include.is_empty() || !config.sample.never_include.is_empty()
}

/// Classify every target, then — only once every target has classified cleanly —
/// apply the statements.
///
/// The two passes are deliberate and mirror how `autumn db restore` verifies
/// every artifact before touching any database: with a control database plus
/// shards, a single-pass loop would scrub the control database and only then
/// discover that a shard has an undeclared column, leaving the topology half
/// anonymized.
#[allow(clippy::too_many_lines)]
fn classify_and_apply(
    args: &ScrubArgs,
    profile: &str,
    targets: &[(String, String)],
    sources: &SourceClassification,
) -> Result<(), ScrubError> {
    // ── Pass 1: classify everything ─────────────────────────────────────────
    let mut plans = Vec::with_capacity(targets.len());
    for (label, url) in targets {
        let facts = probe_database_facts(url, label, &sources.config)?;

        // A universe the classifier never looked at cannot be reported clean.
        if !facts.other_schemas.is_empty() {
            return Err(ScrubError::UnsupportedSchemas {
                schemas: facts.other_schemas.iter().cloned().collect(),
            });
        }

        let tables = introspect::introspect_postgres(url).map_err(|e| ScrubError::Introspect {
            label: label.clone(),
            detail: e.to_string(),
        })?;

        // A table the connecting role cannot see is absent from the classified
        // universe, and "not classified" must never read as "clean".
        let unreachable = unreachable_tables(&facts, &tables);
        if !unreachable.is_empty() {
            return Err(ScrubError::InaccessibleTables {
                tables: unreachable,
            });
        }
        let plan = build_plan(&ClassificationInputs {
            tables: &tables,
            config: &sources.config,
            encrypted: &sources.encrypted,
            anonymize_tables: &sources.anonymize,
            facts: &facts,
        })?;

        // RLS makes an `UPDATE` silently apply to policy-visible rows only,
        // which is a fail-OPEN in a fail-closed tool — refuse rather than
        // report a partial scrub as complete.
        // Purge targets are framework tables, which are excluded from
        // `plan.tables` — so without them an RLS-protected job/token/sync table
        // would have its `DELETE` silently apply to policy-visible rows only,
        // and still be reported emptied.
        let mut rls: Vec<String> = plan
            .tables
            .iter()
            .map(|t| t.table.clone())
            .chain(
                purge_statements(&facts.framework_tables, &sources.config)
                    .into_iter()
                    .map(|(table, _)| table),
            )
            .filter(|t| facts.rls_tables.contains(t))
            .collect();
        rls.sort();
        rls.dedup();
        if !rls.is_empty() {
            return Err(ScrubError::RowLevelSecurity { tables: rls });
        }

        report_plan(label, &plan);
        report_framework_tables(&facts.framework_tables, &sources.config);
        report_triggers(&plan, &facts);

        // Resolved in the same pass as the classification, and before ANY
        // target is written, so a graph gap on one shard cannot leave the rest
        // of the topology sampled.
        let sampling = if sources.roots.is_empty() {
            None
        } else {
            Some(build_sample_plan_for(args, sources, &tables, &facts)?)
        };
        if let Some(sampling) = &sampling {
            report_sample_plan(label, sampling);
        }
        plans.push((label, url, plan, facts, sampling));
    }

    if args.check {
        eprintln!(
            "\n\u{2713} Every column in `public` is classified \u{2014} no unclassified data can leak."
        );
        if !sources.roots.is_empty() {
            eprintln!(
                "\u{2713} Every table is covered by the sample \u{2014} no table would be \
                 emptied unannounced, and every foreign key resolves."
            );
        }
        return Ok(());
    }
    if args.dry_run {
        for (label, url, plan, facts, sampling) in &plans {
            if let Some(sampling) = sampling {
                report_sample_sql(url, label, sampling)?;
            }
            for table in &plan.tables {
                if let Some(sql) = &table.sql {
                    eprintln!("  {sql};");
                }
                for rewrite in &table.encrypted {
                    eprintln!(
                        "  -- {}.{}: re-encrypted per row under the target's key ({} mode)",
                        table.table,
                        rewrite.column,
                        if rewrite.deterministic {
                            "deterministic"
                        } else {
                            "randomized"
                        }
                    );
                }
            }
            for (_, statement) in purge_statements(&facts.framework_tables, &sources.config) {
                eprintln!("  {statement};");
            }
            if sampling.is_some() {
                eprintln!(
                    "  -- then VACUUM (FULL, ANALYZE) on every subsetted table, so the files \
                     shrink to the sample"
                );
            }
        }
        eprintln!("\n\u{2713} Dry run only \u{2014} nothing was written.");
        return Ok(());
    }

    // An encrypted rewrite needs the target's key BEFORE anything is written,
    // so a missing key is a refusal rather than a half-scrubbed database.
    if plans
        .iter()
        .any(|(_, _, plan, _, _)| plan.tables.iter().any(|t| !t.encrypted.is_empty()))
    {
        let ring = resolve_key_ring(profile, Path::new("."))?;
        autumn_web::encryption::install_key_ring(ring);
    }

    // ── Pass 2: apply ───────────────────────────────────────────────────────
    let mut committed: Vec<&str> = Vec::new();
    for (label, url, plan, facts, sampling) in &plans {
        let purges = purge_statements(&facts.framework_tables, &sources.config);
        let (applied, sampled) = execute(
            url,
            plan,
            &purges,
            &facts.materialized_views,
            sampling.as_ref(),
            label,
        )
        .inspect_err(|_| {
            if !committed.is_empty() {
                eprintln!(
                    "\n\u{26A0}\u{FE0F}  Already committed before this failure: {}. \
                     Those databases ARE scrubbed; every later target is untouched and still \
                     holds real data.",
                    committed.join(", ")
                );
            }
        })?;
        for (table, rows) in applied {
            eprintln!("  \u{2713} {table}: {rows} row(s) scrubbed.");
        }
        if let (Some(sampling), Some(sampled)) = (sampling.as_ref(), sampled) {
            report_sample_outcome(label, &sampled);
            // Deleting rows leaves the table files exactly as large as they
            // were, so a sample that is not compacted still needs the source's
            // disk and still dumps slowly. This is the step that makes the
            // subset actually laptop-sized, and it can only run after the
            // commit: VACUUM FULL rewrites each table and cannot join a
            // transaction.
            report_reclaimed_size(url, label, sampling, sampled.size_before)?;
        }
        committed.push(label);
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

/// Resolve the `--sample` subset for one target from the same schema snapshot
/// the column classification used.
fn build_sample_plan_for(
    args: &ScrubArgs,
    sources: &SourceClassification,
    tables: &[Table],
    facts: &DatabaseFacts,
) -> Result<sample::SamplePlan, ScrubError> {
    // A partition's rows belong to its parent, which is what the walk and the
    // deletes address — planning it separately would count and remove them
    // twice, exactly as `build_plan` skips it for the rewrites.
    let universe: Vec<(String, Vec<String>)> = tables
        .iter()
        .filter(|t| !facts.partitions.contains(&t.name))
        .map(|t| (t.name.clone(), t.primary_key.clone()))
        .collect();
    let framework: BTreeSet<String> = facts.framework_tables.iter().cloned().collect();
    let purged: BTreeSet<String> = purge_statements(&facts.framework_tables, &sources.config)
        .into_iter()
        .map(|(table, _)| table)
        .collect();
    Ok(sample::build_sample_plan(&sample::SampleInputs {
        roots: &sources.roots,
        seed: args.seed,
        rules: &sources.config.sample,
        tables: &universe,
        foreign_keys: &facts.foreign_keys,
        framework_tables: &framework,
        purged: &purged,
        partitions: &facts.partitions,
    })?)
}

/// Print what the sample will select, before anything is written.
fn report_sample_plan(label: &str, plan: &sample::SamplePlan) {
    let roots: Vec<String> = plan
        .tables
        .iter()
        .filter_map(|t| match t.role {
            sample::SampleRole::Root(amount) => Some(match amount {
                sample::SampleAmount::Percent(pct) => format!("{} {pct}%", t.table),
                sample::SampleAmount::Count(n) => format!("{} {n} row(s)", t.table),
            }),
            _ => None,
        })
        .collect();
    eprintln!(
        "  \u{2139} Sampling {label} from {}, seed {} \u{2014} the same seed against the same \
         source selects the identical rows.",
        roots.join(", "),
        plan.seed,
    );
}

/// Print the statements a sample would run, for `--dry-run`.
///
/// The selection walk repeats until it stops finding related rows, so its
/// statements are shown once with that noted rather than unrolled: how many
/// passes a schema needs is a property of the data, not of the plan.
fn report_sample_sql(url: &str, label: &str, plan: &sample::SamplePlan) -> Result<(), ScrubError> {
    // Read the live row counts rather than printing a placeholder: `--dry-run`
    // promises the EXACT statements, and a root's `LIMIT` is the one number a
    // reader checks.
    let mut conn = probe_connection(url, label, "size the sample")?;
    let counts =
        sample::source_counts(&mut conn, plan).map_err(|e| ScrubError::Sql(e.to_string()))?;
    for statement in plan.setup_statements() {
        eprintln!("  {statement};");
    }
    for statement in plan.seed_statements(&counts) {
        eprintln!("  {statement};");
    }
    eprintln!("  -- then, repeated until no new related rows are found:");
    for statement in plan.walk_statements() {
        eprintln!("  {statement};");
    }
    for statement in plan.delete_statements() {
        eprintln!("  {statement};");
    }
    for (constraint, statement) in plan.integrity_statements() {
        eprintln!("  {statement}; -- verifies {constraint}");
    }
    Ok(())
}

/// Report what the sample kept, per table and in total (AC #6).
fn report_sample_outcome(label: &str, outcome: &sample::SampleOutcome) {
    eprintln!("  \u{2500}\u{2500} {label}: sampled rows \u{2500}\u{2500}");
    for count in &outcome.counts {
        eprintln!(
            "    {}: {} \u{2192} {} row(s) ({}, {})",
            count.table,
            count.before,
            count.after,
            percent_of(count.after, count.before),
            count.role,
        );
    }
    let before: i64 = outcome.counts.iter().map(|c| c.before).sum();
    let after: i64 = outcome.counts.iter().map(|c| c.after).sum();
    eprintln!(
        "    Total: {before} \u{2192} {after} row(s) ({} of the source), settled in {} pass(es).",
        percent_of(after, before),
        outcome.passes,
    );
    eprintln!(
        "  \u{2713} {} foreign key(s) re-verified \u{2014} every reference in the subset resolves.",
        outcome.verified,
    );
}

/// Rewrite every subsetted table so the freed space is really freed, then
/// report the size the sample actually costs.
fn report_reclaimed_size(
    url: &str,
    label: &str,
    plan: &sample::SamplePlan,
    before: i64,
) -> Result<(), ScrubError> {
    let mut conn = probe_connection(url, label, "compact the sampled tables")?;
    // Exactly the tables `data_size` measures, so the reported before/after
    // describes the same set of files this rewrites.
    for table in plan.subsetted_tables() {
        // Not in a transaction, and deliberately: VACUUM FULL takes an
        // exclusive lock and rewrites the table, neither of which a transaction
        // block permits.
        sql_query(format!("VACUUM (FULL, ANALYZE) {}", qualified_ident(table)))
            .execute(&mut conn)
            .map_err(|e| ScrubError::Sql(e.to_string()))?;
    }
    let after = sample::data_size(&mut conn, plan).map_err(|e| ScrubError::Sql(e.to_string()))?;
    eprintln!(
        "    Table size: {} \u{2192} {} ({} of the source).",
        human_bytes(before),
        human_bytes(after),
        percent_of(after, before),
    );
    Ok(())
}

/// `part` as a percentage of `whole`, one decimal place.
#[allow(clippy::cast_precision_loss)]
fn percent_of(part: i64, whole: i64) -> String {
    if whole <= 0 {
        return "n/a".to_owned();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// Bytes at human scale, so "128.0 MB → 3.0 MB" reads at a glance.
#[allow(clippy::cast_precision_loss)]
fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Warn when a table the scrub rewrites carries user-defined triggers.
///
/// An audit/history trigger copies the pre-scrub `OLD` row into another table as
/// the `UPDATE` runs — so a table scrubbed earlier in the same transaction can be
/// re-populated with real values behind the scrub's back.
fn report_triggers(plan: &ScrubPlan, facts: &DatabaseFacts) {
    let triggered: Vec<&str> = plan
        .tables
        .iter()
        .filter(|t| facts.triggered_tables.contains(&t.table))
        .map(|t| t.table.as_str())
        .collect();
    if triggered.is_empty() {
        return;
    }
    eprintln!(
        "  \u{26A0}\u{FE0F}  {} rewritten table(s) carry user-defined triggers: {}.\n    \
         An audit or history trigger copies the PRE-scrub row into another table as the \
         rewrite runs, which can re-introduce real values. Check those triggers, or disable \
         them on the copy before scrubbing.",
        triggered.len(),
        triggered.join(", ")
    );
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

/// Everything about one live database that the pure classifier cannot read from
/// the schema IR, gathered in a single connection.
///
/// The IR [`crate::schema::introspect`] produces is shaped for *migration
/// diffing*, not for "is it safe to rewrite this column": it records only
/// outgoing single-column foreign keys, drops generated/identity semantics, and
/// says nothing about row-level security, triggers, partitions, materialized
/// views, or schemas outside `public`. Every one of those decides whether an
/// `UPDATE` this command emits succeeds, silently under-applies, or leaks — so
/// they are probed here rather than assumed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DatabaseFacts {
    /// Every column on either side of any foreign key, all components of a
    /// composite key included, as `(table, column)`. Rewriting any of them
    /// breaks referential integrity (or is silently cascaded into a child).
    pub foreign_key_columns: BTreeSet<(String, String)>,
    /// Columns named by a `CHECK` constraint. A fabricated value has no way to
    /// satisfy an arbitrary predicate, so these are refused rather than guessed.
    pub checked_columns: BTreeSet<(String, String)>,
    /// Generated / `GENERATED ALWAYS AS IDENTITY` columns. Postgres refuses to
    /// `UPDATE` them at all, and they are derived data that a scrub of their
    /// source columns already covers.
    pub generated_columns: BTreeSet<(String, String)>,
    /// Columns covered by ANY unique index — composite and partial included.
    /// Broader than the IR's single-column `unique` flag, which is the right
    /// question for a migration diff and the wrong one for "can this rewrite
    /// collide".
    pub unique_columns: BTreeSet<(String, String)>,
    /// Columns covered by a `NULLS NOT DISTINCT` unique index, where more than
    /// one `NULL` is itself a uniqueness violation.
    pub nulls_not_distinct_columns: BTreeSet<(String, String)>,
    /// Tables that are partitions of another table. Their rows are rewritten
    /// through the parent, so planning them again double-updates.
    pub partitions: BTreeSet<String>,
    /// Tables with row-level security enabled. A non-bypassing role silently
    /// updates only the rows its policies expose — a fail-open a scrub cannot
    /// tolerate.
    pub rls_tables: BTreeSet<String>,
    /// Tables carrying user-defined triggers, which can copy pre-scrub values
    /// into another table mid-scrub.
    pub triggered_tables: BTreeSet<String>,
    /// Materialized views, in dependency order (sources before dependents), so
    /// refreshing them in sequence never re-derives from stale data.
    pub materialized_views: Vec<String>,
    /// Non-system schemas other than `public` that hold base tables. The whole
    /// classification universe is `public`-only, so these are refused.
    pub other_schemas: BTreeSet<String>,
    /// Framework-owned tables present that the classification never sees.
    pub framework_tables: Vec<String>,
    /// Every column of every `public` table, read from `pg_attribute`.
    ///
    /// A role can hold privileges on some columns of a table but not others, and
    /// `information_schema.columns` (what introspection reads) omits the ones it
    /// cannot see — so the table would classify, the visible columns would be
    /// rewritten, and the hidden PII would survive.
    pub public_columns: BTreeSet<(String, String)>,
    /// Every base table in `public`, read from `pg_class`.
    ///
    /// Introspection enumerates through `information_schema.tables`, which shows
    /// only what the connecting role has some privilege on — so a table the
    /// scrub role cannot see would silently drop out of the classified universe
    /// and be reported clean. `pg_class` shows them all, and the difference is
    /// a refusal.
    pub public_base_tables: BTreeSet<String>,
    /// Every foreign key in `public`, whole constraints rather than the loose
    /// columns above: `--sample` walks this graph to decide which rows a subset
    /// must carry for every reference to resolve.
    pub foreign_keys: Vec<sample::ForeignKeyRef>,
}

/// A single `name` column.
#[derive(diesel::QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// A `(table, column)` pair.
#[derive(diesel::QueryableByName)]
struct PairRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    tbl: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    col: String,
}

/// One whole foreign key constraint, both key lists rendered as unit-separated
/// column names (a separator no identifier can contain).
#[derive(diesel::QueryableByName)]
struct ConstraintRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    child: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    child_cols: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    parent: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    parent_cols: String,
}

/// The column-name separator the constraint probe aggregates on. Postgres
/// identifiers can contain a comma, so a printable separator would be
/// ambiguous; `US` (unit separator) cannot appear in one.
const KEY_SEPARATOR: char = '\u{1f}';

fn pair_set(rows: Vec<PairRow>) -> BTreeSet<(String, String)> {
    rows.into_iter().map(|r| (r.tbl, r.col)).collect()
}

/// Open a connection for a probe, mapping failure to a credential-safe error.
fn probe_connection(url: &str, label: &str, what: &str) -> Result<PgConnection, ScrubError> {
    PgConnection::establish(url).map_err(|_| ScrubError::Introspect {
        label: label.to_owned(),
        detail: format!(
            "could not connect to database {:?} to {what}",
            parsed_db_name(url)
        ),
    })
}

/// Whether a system catalog has a given column on this server.
///
/// The catalog grows with each Postgres release — `attgenerated` arrives in 12,
/// `indnkeyatts` in 11, `indnullsnotdistinct` in 15 — and a scrub that only ran
/// on the newest server would be useless. Each version-specific fact is probed
/// for first and degrades to "no such thing on this server", which is the
/// correct answer: a release without generated columns has none to skip.
fn has_catalog_column(
    conn: &mut PgConnection,
    relation: &str,
    column: &str,
) -> Result<bool, ScrubError> {
    let rows: Vec<NameRow> = sql_query(format!(
        "SELECT 'yes' AS name FROM pg_attribute \
         WHERE attrelid = {}::regclass AND attname = {} AND NOT attisdropped",
        quote_literal(relation),
        quote_literal(column)
    ))
    .load(conn)
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    Ok(!rows.is_empty())
}

/// Gather every catalog fact the plan validation needs.
// One catalog read per fact; splitting it would scatter closely-related SQL
// across helpers that each need the same connection.
#[allow(clippy::too_many_lines)]
fn probe_database_facts(
    url: &str,
    label: &str,
    config: &ScrubConfig,
) -> Result<DatabaseFacts, ScrubError> {
    let mut conn = probe_connection(url, label, "inspect its catalog")?;

    // ── Foreign keys: BOTH sides, every component ───────────────────────────
    // `pg_constraint.conkey`/`confkey` are arrays; `unnest` covers composite
    // keys, which the IR (first component, referencing side only) cannot.
    let foreign_key_columns = pair_set(
        sql_query(
            "SELECT rel.relname AS tbl, att.attname AS col \
             FROM pg_constraint c \
             JOIN pg_class rel ON rel.oid = c.conrelid \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             CROSS JOIN LATERAL unnest(c.conkey) AS k(attnum) \
             JOIN pg_attribute att ON att.attrelid = rel.oid AND att.attnum = k.attnum \
             WHERE c.contype = 'f' \
             UNION \
             SELECT frel.relname AS tbl, fatt.attname AS col \
             FROM pg_constraint c \
             JOIN pg_class frel ON frel.oid = c.confrelid \
             JOIN pg_namespace fns ON fns.oid = frel.relnamespace AND fns.nspname = 'public' \
             CROSS JOIN LATERAL unnest(c.confkey) AS fk(attnum) \
             JOIN pg_attribute fatt ON fatt.attrelid = frel.oid AND fatt.attnum = fk.attnum \
             WHERE c.contype = 'f'",
        )
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?,
    );

    // ── Foreign keys as whole constraints ───────────────────────────────────
    // The set above answers "may this column be rewritten"; `--sample` asks a
    // different question — "which rows must travel together" — and that needs
    // the constraint, in key order, both sides paired.
    let sep = KEY_SEPARATOR;
    let constraint_rows: Vec<ConstraintRow> = sql_query(format!(
        "SELECT c.conname AS name, rel.relname AS child, frel.relname AS parent, \
         (SELECT string_agg(att.attname, '{sep}' ORDER BY k.ord) \
          FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
          JOIN pg_attribute att ON att.attrelid = c.conrelid AND att.attnum = k.attnum) \
         AS child_cols, \
         (SELECT string_agg(att.attname, '{sep}' ORDER BY k.ord) \
          FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) \
          JOIN pg_attribute att ON att.attrelid = c.confrelid AND att.attnum = k.attnum) \
         AS parent_cols \
         FROM pg_constraint c \
         JOIN pg_class rel ON rel.oid = c.conrelid \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         JOIN pg_class frel ON frel.oid = c.confrelid \
         JOIN pg_namespace fns ON fns.oid = frel.relnamespace AND fns.nspname = 'public' \
         WHERE c.contype = 'f'"
    ))
    .load(&mut conn)
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    let foreign_keys: Vec<sample::ForeignKeyRef> = constraint_rows
        .into_iter()
        .map(|row| sample::ForeignKeyRef {
            name: row.name,
            child_table: row.child,
            child_columns: row
                .child_cols
                .split(KEY_SEPARATOR)
                .map(str::to_owned)
                .collect(),
            parent_table: row.parent,
            parent_columns: row
                .parent_cols
                .split(KEY_SEPARATOR)
                .map(str::to_owned)
                .collect(),
        })
        .collect();

    // ── CHECK-constrained columns ───────────────────────────────────────────
    let checked_columns = pair_set(
        sql_query(
            "SELECT rel.relname AS tbl, att.attname AS col \
             FROM pg_constraint c \
             JOIN pg_class rel ON rel.oid = c.conrelid \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             CROSS JOIN LATERAL unnest(c.conkey) AS k(attnum) \
             JOIN pg_attribute att ON att.attrelid = rel.oid AND att.attnum = k.attnum \
             WHERE c.contype = 'c'",
        )
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?,
    );

    // ── Generated / identity-always columns ─────────────────────────────────
    let mut generated_predicates: Vec<&str> = Vec::new();
    if has_catalog_column(&mut conn, "pg_attribute", "attgenerated")? {
        generated_predicates.push("att.attgenerated <> ''");
    }
    if has_catalog_column(&mut conn, "pg_attribute", "attidentity")? {
        generated_predicates.push("att.attidentity = 'a'");
    }
    let generated_columns = if generated_predicates.is_empty() {
        BTreeSet::new()
    } else {
        pair_set(
            sql_query(format!(
                "SELECT rel.relname AS tbl, att.attname AS col \
                 FROM pg_attribute att \
                 JOIN pg_class rel ON rel.oid = att.attrelid \
                 JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
                 WHERE att.attnum > 0 AND NOT att.attisdropped AND ({})",
                generated_predicates.join(" OR ")
            ))
            .load(&mut conn)
            .map_err(|e| ScrubError::Sql(e.to_string()))?,
        )
    };

    // ── Uniqueness, the write-side question ─────────────────────────────────
    // ANY unique index counts (composite and partial included): the IR's
    // single-column `unique` flag answers a migration-diff question, not
    // "can this rewrite collide".
    // `pg_index.indkey` is an `int2vector`, not a real array, so it is matched
    // with `= ANY(...)` rather than `unnest`. The `[0:indnkeyatts-1]` slice is
    // the KEY columns only — a covering index's `INCLUDE` columns carry no
    // uniqueness and must not be treated as constrained.
    // Covering indexes (`INCLUDE`) arrived with `indnkeyatts` in Postgres 11; on
    // an older server every `indkey` entry IS a key column.
    let key_slice = if has_catalog_column(&mut conn, "pg_index", "indnkeyatts")? {
        "i.indkey[0:i.indnkeyatts-1]"
    } else {
        "i.indkey"
    };
    let mut unique_columns = pair_set(
        sql_query(format!(
            "SELECT rel.relname AS tbl, att.attname AS col \
             FROM pg_index i \
             JOIN pg_class rel ON rel.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             JOIN pg_attribute att ON att.attrelid = rel.oid \
             AND att.attnum = ANY({key_slice}) \
             WHERE i.indisunique AND att.attnum > 0"
        ))
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?,
    );

    // Two kinds of unique index whose real inputs `indkey` does not name:
    //
    // - an EXPRESSION index (`ON users (left(email, 1))`) stores `0` for the
    //   expression position, so the join above sees no column at all — and
    //   uniqueness after an arbitrary expression cannot be preserved by a
    //   per-row token, since `left(x, 1)` collapses every scrubbed value onto
    //   one character;
    // - a PARTIAL index (`ON events (group_id) WHERE active = false`) is keyed
    //   on `group_id`, but rewriting `active` changes which rows the index
    //   COVERS — pulling previously-excluded duplicates into it.
    //
    // `pg_depend` records both the expression- and the predicate-referenced
    // columns, so one query covers them.
    //
    // NOTE: adding them here is only a PARTIAL guard, and deliberately recorded
    // as such. `Strategy::allowed_on_unique` permits `email`/`name`/`redact` on
    // a unique column because those are injective on the column's own value —
    // but injectivity does not survive an arbitrary expression. Every scrubbed
    // address starts `scrubbed+`, so `UNIQUE (left(email, 1))` still collides at
    // execution time. A correct guard needs its own refusal for expression
    // operands rather than folding them into the unique set; tracked in #2366.
    unique_columns.extend(pair_set(
        sql_query(
            "SELECT rel.relname AS tbl, att.attname AS col \
             FROM pg_index i \
             JOIN pg_class rel ON rel.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             JOIN pg_depend d ON d.objid = i.indexrelid AND d.refobjid = i.indrelid \
             JOIN pg_attribute att ON att.attrelid = rel.oid AND att.attnum = d.refobjsubid \
             WHERE i.indisunique AND d.refobjsubid > 0 \
             AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL)",
        )
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?,
    ));

    // `NULLS NOT DISTINCT` (PG15+) makes a second NULL a violation, so the
    // `null` strategy stops being unique-safe. `indnullsnotdistinct` does not
    // exist before 15; probe the column's presence first so this stays
    // compatible with older servers.
    let nulls_not_distinct_columns =
        if has_catalog_column(&mut conn, "pg_index", "indnullsnotdistinct")? {
            pair_set(
                sql_query(format!(
                    "SELECT rel.relname AS tbl, att.attname AS col \
                 FROM pg_index i \
                 JOIN pg_class rel ON rel.oid = i.indrelid \
                 JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
                 JOIN pg_attribute att ON att.attrelid = rel.oid \
                 AND att.attnum = ANY({key_slice}) \
                 WHERE i.indisunique AND i.indnullsnotdistinct AND att.attnum > 0"
                ))
                .load(&mut conn)
                .map_err(|e| ScrubError::Sql(e.to_string()))?,
            )
        } else {
            BTreeSet::new()
        };

    // ── Table-level facts ───────────────────────────────────────────────────
    let names = |q: &str, conn: &mut PgConnection| -> Result<Vec<String>, ScrubError> {
        let rows: Vec<NameRow> = sql_query(q)
            .load(conn)
            .map_err(|e| ScrubError::Sql(e.to_string()))?;
        Ok(rows.into_iter().map(|r| r.name).collect())
    };

    let partitions = if has_catalog_column(&mut conn, "pg_class", "relispartition")? {
        names(
            "SELECT rel.relname AS name FROM pg_class rel \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             WHERE rel.relispartition",
            &mut conn,
        )?
        .into_iter()
        .collect()
    } else {
        BTreeSet::new()
    };

    let rls_tables = names(
        "SELECT rel.relname AS name FROM pg_class rel \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE rel.relrowsecurity",
        &mut conn,
    )?
    .into_iter()
    .collect();

    let triggered_tables = names(
        "SELECT DISTINCT rel.relname AS name FROM pg_trigger t \
         JOIN pg_class rel ON rel.oid = t.tgrelid \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE NOT t.tgisinternal",
        &mut conn,
    )?
    .into_iter()
    .collect();

    // `m` (materialized views) belongs here as much as the table relkinds do: a
    // schema holding only `analytics.user_emails AS SELECT … FROM public.users`
    // keeps its own copy of the PII, and the refresh pass only reaches `public`.
    let other_schemas = names(
        "SELECT DISTINCT ns.nspname AS name FROM pg_class rel \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace \
         WHERE rel.relkind IN ('r', 'p', 'f', 'm') \
         AND ns.nspname NOT IN ('public', 'information_schema') \
         AND ns.nspname NOT LIKE 'pg\\_%'",
        &mut conn,
    )?
    .into_iter()
    .collect();

    // Materialized views in dependency order: a view that reads another must be
    // refreshed after it, or it re-derives from pre-scrub data.
    let materialized_views = names(
        "WITH RECURSIVE mv AS ( \
             SELECT rel.oid FROM pg_class rel \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             WHERE rel.relkind = 'm' \
         ), edge AS ( \
             SELECT DISTINCT r.ev_class AS dependent, d.refobjid AS source \
             FROM pg_depend d \
             JOIN pg_rewrite r ON r.oid = d.objid \
             WHERE d.classid = 'pg_rewrite'::regclass \
               AND r.ev_class IN (SELECT oid FROM mv) \
               AND d.refobjid IN (SELECT oid FROM mv) \
               AND d.refobjid <> r.ev_class \
         ), depth AS ( \
             SELECT oid, 0 AS lvl FROM mv \
             WHERE oid NOT IN (SELECT dependent FROM edge) \
             UNION ALL \
             SELECT e.dependent, d.lvl + 1 FROM edge e JOIN depth d ON d.oid = e.source \
             WHERE d.lvl < 32 \
         ) \
         SELECT rel.relname AS name FROM (SELECT oid, max(lvl) AS lvl FROM depth GROUP BY oid) o \
         JOIN pg_class rel ON rel.oid = o.oid ORDER BY o.lvl, rel.relname",
        &mut conn,
    )?;

    // ── Framework-owned tables (read from pg_class, not information_schema,
    //    which hides tables the connecting role has no privilege on) ─────────
    let wanted = probe_table_names(config)
        .iter()
        .map(|t| quote_literal(t))
        .collect::<Vec<_>>()
        .join(", ");
    let framework_tables = names(
        &format!(
            "SELECT rel.relname AS name FROM pg_class rel \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             WHERE rel.relkind IN ('r', 'p') AND rel.relname IN ({wanted}) \
             ORDER BY rel.relname"
        ),
        &mut conn,
    )?;

    // `f` (foreign tables) is deliberately included: introspection reads only
    // `BASE TABLE`, so a foreign table left pointing at production would be
    // classified by nothing at all and still report a clean scrub.
    let public_base_tables = names(
        "SELECT rel.relname AS name FROM pg_class rel \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE rel.relkind IN ('r', 'p', 'f')",
        &mut conn,
    )?
    .into_iter()
    .collect();

    let public_columns = pair_set(
        sql_query(
            "SELECT rel.relname AS tbl, att.attname AS col \
             FROM pg_attribute att \
             JOIN pg_class rel ON rel.oid = att.attrelid \
             JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
             WHERE rel.relkind IN ('r', 'p', 'f') AND att.attnum > 0 AND NOT att.attisdropped",
        )
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?,
    );

    Ok(DatabaseFacts {
        foreign_key_columns,
        checked_columns,
        generated_columns,
        unique_columns,
        nulls_not_distinct_columns,
        partitions,
        rls_tables,
        triggered_tables,
        materialized_views,
        other_schemas,
        framework_tables,
        public_columns,
        public_base_tables,
        foreign_keys,
    })
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
        "  \u{26A0}\u{FE0F}  {} framework-owned table(s) are NOT scrubbed and may carry app-supplied \
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
        .map(|t| (t.clone(), format!("DELETE FROM {}", qualified_ident(t))))
        .collect()
}

/// Resolve the target's attribute-encryption key ring from the project's
/// credentials for `profile`.
///
/// Refused rather than skipped when the plan needs one: writing a plain string
/// into an `#[encrypted]` column would make every later repository read of that
/// row fail as malformed ciphertext, so a missing key is a hard stop, not a
/// silent downgrade.
fn resolve_key_ring(
    profile: &str,
    project_root: &Path,
) -> Result<autumn_web::encryption::KeyRing, ScrubError> {
    let store = autumn_web::credentials::load_credentials(profile, project_root).map_err(|e| {
        ScrubError::EncryptionKeyUnavailable {
            profile: profile.to_owned(),
            detail: e.to_string(),
        }
    })?;
    match autumn_web::encryption::key_ring_from_credentials(&store) {
        Ok(Some(ring)) => Ok(ring),
        Ok(None) => Err(ScrubError::EncryptionKeyUnavailable {
            profile: profile.to_owned(),
            detail: format!(
                "`{}.primary_key` is not configured",
                autumn_web::encryption::CREDENTIALS_NAMESPACE
            ),
        }),
        Err(e) => Err(ScrubError::EncryptionKeyUnavailable {
            profile: profile.to_owned(),
            detail: e.to_string(),
        }),
    }
}

/// How many rows of encrypted replacements are shipped back per statement.
const ENCRYPTED_BATCH_ROWS: usize = 500;

/// One row's encrypted replacement.
#[derive(diesel::QueryableByName)]
struct RowTokenRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    row_key: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    token: String,
}

/// Rewrite one `#[encrypted]` column, row by row, with a valid AEAD envelope of
/// a fake plaintext produced under the target's own key.
///
/// This is the one rewrite that cannot be a SQL expression: the envelope is
/// `base64(header ‖ nonce ‖ ciphertext)` built by [`autumn_web::encryption`], so
/// the rows are read, encrypted in Rust, and shipped back in batched
/// `UPDATE … FROM (VALUES …)` statements. Rows whose value is already `NULL` are
/// left alone, exactly as the SQL path's `CASE` does.
fn rewrite_encrypted_column(
    conn: &mut PgConnection,
    table: &str,
    row_key: &str,
    rewrite: &EncryptedRewrite,
) -> Result<usize, diesel::result::Error> {
    use autumn_web::encryption::{Mode, encrypt_text};

    let ident = quote_ident(&rewrite.column);
    let seed = format!("{} || '|' || ({row_key})", quote_literal(&rewrite.column));
    let rows: Vec<RowTokenRow> = sql_query(format!(
        "SELECT ({row_key}) AS row_key, md5({seed}) || md5(({seed}) || '#2') AS token \
         FROM {} WHERE {ident} IS NOT NULL",
        qualified_ident(table),
    ))
    .load(conn)?;

    let mode = if rewrite.deterministic {
        Mode::Deterministic
    } else {
        Mode::Randomized
    };
    let mut updated = 0;
    for chunk in rows.chunks(ENCRYPTED_BATCH_ROWS) {
        let mut values = Vec::with_capacity(chunk.len());
        for row in chunk {
            let plaintext = match rewrite.shape {
                Strategy::Email => format!("scrubbed+{}{SCRUB_EMAIL_DOMAIN}", row.token),
                _ => format!("[scrubbed:{}]", row.token),
            };
            // A key ring is installed before the apply pass, so this can only
            // fail on a genuinely broken key — surfaced as a SQL-shaped error so
            // the transaction rolls back with everything else.
            let envelope = encrypt_text(mode, &plaintext).map_err(|e| {
                diesel::result::Error::QueryBuilderError(
                    format!(
                        "could not encrypt a replacement for {table}.{}: {e}",
                        rewrite.column
                    )
                    .into(),
                )
            })?;
            values.push(format!(
                "({}, {})",
                quote_literal(&row.row_key),
                quote_literal(&envelope)
            ));
        }
        updated += sql_query(format!(
            "UPDATE {} AS t SET {ident} = v.val FROM (VALUES {}) AS v(k, val) \
             WHERE ({row_key}) = v.k",
            qualified_ident(table),
            values.join(", ")
        ))
        .execute(conn)?;
    }
    Ok(updated)
}

/// Run every statement for one database inside a single transaction, so a
/// failure can never leave a half-scrubbed database behind.
/// What one database's scrub wrote: `(table, rows)` per rewrite, plus the
/// sample's own outcome when `--sample` was given.
type Applied = (Vec<(String, usize)>, Option<sample::SampleOutcome>);

fn execute(
    url: &str,
    plan: &ScrubPlan,
    purges: &[(String, String)],
    materialized_views: &[String],
    sampling: Option<&sample::SamplePlan>,
    label: &str,
) -> Result<Applied, ScrubError> {
    if plan.tables.is_empty()
        && purges.is_empty()
        && materialized_views.is_empty()
        && sampling.is_none()
    {
        return Ok((Vec::new(), None));
    }
    let mut conn = probe_connection(url, label, "apply the scrub")?;
    let mut counts = Vec::with_capacity(plan.tables.len());
    let mut outcome = None;
    // A sample refusal has to abort the transaction, and the only error a
    // transaction closure can carry is diesel's — so the reason travels beside
    // it and is re-raised once the rollback has happened.
    let mut refusal: Option<sample::SampleError> = None;
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        // Pin the resolution of every unqualified name and the meaning of every
        // string literal for the whole transaction, so a role- or
        // database-level `search_path` (tenant schemas) cannot redirect a write
        // to a table nothing classified, and `quote_literal`'s doubled quotes
        // cannot be re-interpreted under `standard_conforming_strings = off`.
        conn.batch_execute(
            "SET LOCAL search_path = pg_catalog, public; \
             SET LOCAL standard_conforming_strings = on",
        )?;
        // Hold the tables for the duration: the plan was built from a snapshot
        // taken on another connection, and a row inserted between the two would
        // otherwise survive the scrub unnoticed. SHARE ROW EXCLUSIVE blocks
        // writers while still allowing plain reads.
        // Every table the transaction writes, including the ones it empties: a
        // producer inserting into a purged job/sync/token table after that
        // `DELETE` took its snapshot would otherwise survive a run that reports
        // the table emptied.
        let locked = plan
            .tables
            .iter()
            .map(|t| t.table.as_str())
            .chain(purges.iter().map(|(table, _)| table.as_str()))
            // Every table the sample reads or empties, too: a row inserted into
            // one after the walk selected from it would survive a run that
            // reports the table subsetted.
            .chain(
                sampling
                    .into_iter()
                    .flat_map(sample::SamplePlan::locked_tables),
            );
        let mut locked: Vec<&str> = locked.collect();
        locked.sort_unstable();
        locked.dedup();
        for table in locked {
            sql_query(format!(
                "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
                qualified_ident(table)
            ))
            .execute(conn)?;
        }
        // Purges run FIRST so a framework-owned table that references a
        // sampled one is already empty when the sample removes its parents.
        for (table, statement) in purges {
            let rows = sql_query(statement).execute(conn)?;
            counts.push((format!("{table} (emptied)"), rows));
        }
        // Then the subset, so the rewrites below touch only the rows that
        // survive it — and so no combination of flags can commit a row that was
        // sampled but not scrubbed: both happen in this one transaction.
        if let Some(sampling) = sampling {
            outcome = Some(sample::apply(conn, sampling, &mut refusal)?);
        }
        for table in &plan.tables {
            if let Some(sql) = &table.sql {
                let rows = sql_query(sql).execute(conn)?;
                counts.push((table.table.clone(), rows));
            }
            for rewrite in &table.encrypted {
                let rows = rewrite_encrypted_column(conn, &table.table, &table.row_key, rewrite)?;
                counts.push((format!("{}.{}", table.table, rewrite.column), rows));
            }
        }
        // Inside the transaction, so a refresh the role is not allowed to run
        // rolls the rewrites back rather than committing base tables that a
        // stale materialized view still contradicts.
        for view in materialized_views {
            sql_query(format!(
                "REFRESH MATERIALIZED VIEW {}",
                qualified_ident(view)
            ))
            .execute(conn)?;
            counts.push((format!("{view} (materialized view refreshed)"), 0));
        }
        Ok(())
    })
    .map_err(|e| {
        refusal
            .take()
            .map_or_else(|| ScrubError::Sql(e.to_string()), ScrubError::from)
    })?;
    Ok((counts, outcome))
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

    fn empty_encrypted() -> BTreeMap<String, BTreeMap<String, bool>> {
        BTreeMap::new()
    }

    /// `#[encrypted]` columns for one table, all randomized-mode.
    fn encrypted_columns_of(
        table: &str,
        columns: &[&str],
    ) -> BTreeMap<String, BTreeMap<String, bool>> {
        BTreeMap::from([(
            table.to_owned(),
            columns.iter().map(|c| ((*c).to_owned(), false)).collect(),
        )])
    }

    fn no_anonymize() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn plan_for(
        tables: &[Table],
        config: &ScrubConfig,
        encrypted: &BTreeMap<String, BTreeMap<String, bool>>,
        anonymize: &BTreeSet<String>,
    ) -> Result<ScrubPlan, ScrubError> {
        plan_with_facts(
            tables,
            config,
            encrypted,
            anonymize,
            &DatabaseFacts::default(),
        )
    }

    fn plan_with_facts(
        tables: &[Table],
        config: &ScrubConfig,
        encrypted: &BTreeMap<String, BTreeMap<String, bool>>,
        anonymize: &BTreeSet<String>,
        facts: &DatabaseFacts,
    ) -> Result<ScrubPlan, ScrubError> {
        build_plan(&ClassificationInputs {
            tables,
            config,
            encrypted,
            anonymize_tables: anonymize,
            facts,
        })
    }

    /// The single `UPDATE` a table plan carries (panics when it has none).
    fn sql_of(plan: &ScrubPlan, table: &str) -> String {
        plan.tables
            .iter()
            .find(|t| t.table == table)
            .unwrap_or_else(|| panic!("no plan for {table}"))
            .sql
            .clone()
            .unwrap_or_else(|| panic!("{table} has no SQL statement"))
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
        let encrypted = encrypted_columns_of("users", &["email"]);

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
        let encrypted = encrypted_columns_of("users", &["email"]);

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
            [tables.users]
            safe = ["id", "created_at"]
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
        // `id`/`created_at` were explicitly declared safe FOR THIS TABLE, so
        // they are untouched.
        assert!(plan.column("users", "id").is_none());
        assert!(plan.column("users", "created_at").is_none());
    }

    #[test]
    fn the_global_safe_list_may_not_narrow_a_gdpr_anonymize_table() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name"]
            "#,
        )
        .unwrap();
        let anonymize = BTreeSet::from(["users".to_owned()]);
        let plan = plan_for(&[users_table()], &config, &empty_encrypted(), &anonymize).unwrap();
        // A cross-table convenience list is not a per-column review, so it must
        // not silently exempt a column from a table the app registered for
        // anonymization.
        assert!(
            plan.column("users", "full_name").is_some(),
            "a global safe_columns entry must not narrow an anonymize registration"
        );
        // Structural columns are still skipped: the registration says nothing
        // about them and rewriting one would break referential integrity.
        assert!(plan.column("users", "id").is_none());
    }

    #[test]
    fn a_table_the_role_cannot_see_is_a_refusal_not_a_clean_report() {
        // `information_schema.tables` shows only what the connecting role has
        // privileges on, so a hidden table drops out of the classified universe
        // entirely — and "not classified" must never read as "clean".
        let facts = DatabaseFacts {
            public_base_tables: BTreeSet::from([
                "users".to_owned(),
                "secrets".to_owned(),
                // Framework-owned tables are excluded on purpose, not hidden.
                "autumn_jobs".to_owned(),
            ]),
            ..DatabaseFacts::default()
        };
        assert_eq!(
            unreachable_tables(&facts, &[users_table()]),
            vec!["secrets".to_owned()]
        );
        // Nothing hidden: no refusal.
        let visible = DatabaseFacts {
            public_base_tables: BTreeSet::from(["users".to_owned()]),
            ..DatabaseFacts::default()
        };
        assert!(unreachable_tables(&visible, &[users_table()]).is_empty());
    }

    #[test]
    fn a_unique_uuid_column_still_gets_a_castable_32_hex_value() {
        // A UUID is exactly 128 bits; Postgres rejects any other width, so the
        // wider token a unique column gets has to be trimmed.
        let column = Column::new("token", ColumnType::Uuid);
        let expr = replacement_expr(Strategy::Uuid, &column, "TOK", true).unwrap();
        assert_eq!(expr, "(substr(TOK, 1, 32))::uuid");
    }

    #[test]
    fn a_purge_target_under_rls_is_refused_like_any_other_write() {
        // Framework tables are excluded from `plan.tables`, so without this the
        // `DELETE` would apply to policy-visible rows only and still report the
        // table emptied.
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [framework]
            purge = ["autumn_jobs"]
            "#,
        )
        .unwrap();
        let facts = DatabaseFacts {
            framework_tables: vec!["autumn_jobs".to_owned()],
            rls_tables: BTreeSet::from(["autumn_jobs".to_owned()]),
            ..DatabaseFacts::default()
        };
        // The plan itself is clean — the hazard is entirely in the purge target.
        let plan = plan_with_facts(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
            &facts,
        )
        .unwrap();
        assert!(plan.tables.is_empty());
        let purges = purge_statements(&facts.framework_tables, &config);
        assert_eq!(purges.len(), 1);
        assert!(
            facts.rls_tables.contains(&purges[0].0),
            "a purge target under RLS must reach the refusal"
        );
    }

    #[test]
    fn column_level_privilege_gaps_are_refused_too() {
        // A role can see a table but not all of its columns: the table
        // classifies, the visible columns are rewritten, and the hidden PII
        // survives a "successful" scrub.
        let facts = DatabaseFacts {
            public_base_tables: BTreeSet::from(["users".to_owned()]),
            public_columns: BTreeSet::from([
                ("users".to_owned(), "id".to_owned()),
                ("users".to_owned(), "email".to_owned()),
                ("users".to_owned(), "ssn".to_owned()),
            ]),
            ..DatabaseFacts::default()
        };
        // `users_table()` has no `ssn`, standing in for a column introspection
        // could not see.
        assert_eq!(
            unreachable_tables(&facts, &[users_table()]),
            vec!["users.ssn".to_owned()]
        );
    }

    #[test]
    fn every_payload_bearing_framework_table_is_listed() {
        // Each of these is excluded from classification by the `autumn_` prefix
        // (or the explicit filter) while holding app-supplied payloads or actor
        // identities.
        for table in [
            "_autumn_ledger_revisions",
            "_autumn_ledger_high_water",
            "_autumn_version_history",
            "api_tokens",
            "autumn_experiment_assignments",
            "autumn_experiment_changes",
            "autumn_experiment_overrides",
            "autumn_feature_flags",
            "autumn_jobs",
            "autumn_repository_commit_hooks",
            "autumn_search_documents",
            "autumn_sync_rows",
            "feature_flag_changes",
        ] {
            assert!(
                FRAMEWORK_PAYLOAD_TABLES.contains(&table),
                "{table} carries app data the classification never sees"
            );
        }
    }

    #[test]
    fn encrypted_columns_may_be_declared_when_there_is_no_model_source() {
        let config = parse_config_str(
            r#"
            [tables.users.encrypted]
            api_token = "randomized"
            email = "deterministic"
            "#,
        )
        .unwrap();
        let users = config.tables.get("users").unwrap();
        assert_eq!(
            users.encrypted.get("email"),
            Some(&EncryptionMode::Deterministic),
            "the mode cannot be guessed: re-encrypting a deterministic column in \
             randomized mode leaves ciphertext the app can no longer equality-query"
        );
        assert!(!users.encrypted["api_token"].is_deterministic());
    }

    #[test]
    fn the_feature_flag_identity_tables_are_payload_carriers() {
        // `feature_flag_changes.actor` and `autumn_feature_flags.actor_allowlist`
        // both name individual users, and both are outside the classified
        // universe.
        for table in ["feature_flag_changes", "autumn_feature_flags"] {
            assert!(
                FRAMEWORK_PAYLOAD_TABLES.contains(&table),
                "{table} carries actor identities the classification never sees"
            );
            assert!(is_framework_table(table));
        }
    }

    #[test]
    fn a_check_constrained_column_is_refused_rather_than_guessed_at() {
        // A real Autumn closed-set column reaches the database as plain TEXT
        // plus a CHECK, so the model-IR-only `ColumnType::Enum` never fires
        // against a live schema — this is what actually catches it.
        let facts = DatabaseFacts {
            checked_columns: BTreeSet::from([("users".to_owned(), "bio".to_owned())]),
            ..DatabaseFacts::default()
        };
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name"]
            [tables.users.pii]
            bio = "redact"
            "#,
        )
        .unwrap();
        let err = plan_with_facts(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
            &facts,
        )
        .expect_err("no fabricated value can be proven to satisfy an arbitrary CHECK");
        assert!(
            matches!(err, ScrubError::CheckConstrainedColumn { ref column } if column == "users.bio"),
            "got {err:?}"
        );
    }

    #[test]
    fn the_referenced_side_of_a_foreign_key_is_refused() {
        // `orders.user_email REFERENCES users(email)` leaves `users.email` with
        // no `references` of its own, so only the probed catalog set can see it.
        let facts = DatabaseFacts {
            foreign_key_columns: BTreeSet::from([("users".to_owned(), "email".to_owned())]),
            ..DatabaseFacts::default()
        };
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "email"
            "#,
        )
        .unwrap();
        let err = plan_with_facts(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
            &facts,
        )
        .expect_err("rewriting a referenced natural key breaks its children");
        assert!(
            matches!(err, ScrubError::PiiOnKeyColumn { ref columns } if columns == &vec!["users.email".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn a_generated_column_is_never_rewritten() {
        // Postgres refuses `UPDATE` on a generated column outright, and it is
        // derived data that a scrub of its source columns already covers.
        let facts = DatabaseFacts {
            generated_columns: BTreeSet::from([("users".to_owned(), "full_name".to_owned())]),
            ..DatabaseFacts::default()
        };
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "bio"]
            "#,
        )
        .unwrap();
        let plan = plan_with_facts(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
            &facts,
        )
        .expect("a generated column needs no declaration");
        assert!(plan.column("users", "full_name").is_none());
    }

    #[test]
    fn a_partition_is_scrubbed_through_its_parent_not_twice() {
        let mut partition = users_table();
        partition.name = "users_2026_01".to_owned();
        let facts = DatabaseFacts {
            partitions: BTreeSet::from(["users_2026_01".to_owned()]),
            ..DatabaseFacts::default()
        };
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
        let plan = plan_with_facts(
            &[users_table(), partition],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
            &facts,
        )
        .expect("a partition needs no declaration of its own");
        assert_eq!(
            plan.tables.len(),
            1,
            "the parent UPDATE already covers the partition's rows"
        );
        assert_eq!(plan.tables[0].table, "users");
    }

    #[test]
    fn null_is_refused_on_a_nulls_not_distinct_unique_column() {
        let mut t = users_table();
        t.columns[3].unique = true; // `bio`, nullable
        let facts = DatabaseFacts {
            nulls_not_distinct_columns: BTreeSet::from([("users".to_owned(), "bio".to_owned())]),
            ..DatabaseFacts::default()
        };
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name"]
            [tables.users.pii]
            bio = "null"
            "#,
        )
        .unwrap();
        // `null` is normally unique-safe (Postgres allows many NULLs in a
        // unique index) — but not under NULLS NOT DISTINCT.
        let err = plan_with_facts(&[t], &config, &empty_encrypted(), &no_anonymize(), &facts)
            .expect_err("a second NULL is a violation under NULLS NOT DISTINCT");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_unique_column_gets_a_wider_token_and_a_higher_floor() {
        let table = users_table();
        let unique_token = token_expr(&table, "email", true);
        assert_eq!(
            unique_token.matches("md5(").count(),
            2,
            "a unique column needs more entropy than one md5's 32 hex characters: \
             {unique_token}"
        );
        assert!(token_expr(&table, "bio", false).contains("md5("));

        // 19 chars with `redact`'s 11 of affixes leaves 8 — fine for a
        // non-unique column, a collision generator for a unique one (32 bits
        // collides in practice around 10^5 rows).
        let narrow = Column::new(
            "code",
            ColumnType::Opaque {
                pg_type: "varchar(19)".to_owned(),
            },
        );
        assert!(replacement_expr(Strategy::Redact, &narrow, "TOK", false).is_ok());
        assert!(matches!(
            replacement_expr(Strategy::Redact, &narrow, "TOK", true),
            Err(ScrubError::ColumnTooNarrow { .. })
        ));
    }

    #[test]
    fn a_text_strategy_is_refused_on_a_non_character_column() {
        for (name, ty) in [
            ("age", ColumnType::Int32),
            ("seen_at", ColumnType::TimestampTz),
            (
                "ip",
                ColumnType::Opaque {
                    pg_type: "inet".to_owned(),
                },
            ),
        ] {
            let column = Column::new(name, ty);
            for strategy in [
                Strategy::Email,
                Strategy::Name,
                Strategy::Redact,
                Strategy::Phone,
            ] {
                assert!(
                    matches!(
                        replacement_expr(strategy, &column, "TOK", false),
                        Err(ScrubError::StrategyTypeMismatch { .. })
                    ),
                    "{strategy:?} on {name} must be refused at plan time, not at apply time"
                );
            }
        }
    }

    #[test]
    fn the_null_assignment_carries_no_untyped_case() {
        // A `CASE` whose arms are both bare NULL has no type to infer from, so
        // Postgres resolves it to `text` and the assignment fails on every
        // non-character column.
        let mut column = Column::new("token", ColumnType::Uuid);
        column.nullable = true;
        assert_eq!(
            assignment(&column, "NULL", Strategy::Null),
            r#""token" = NULL"#
        );
        assert!(
            assignment(&column, "X", Strategy::Uuid).contains("CASE WHEN"),
            "every other strategy still preserves NULLs"
        );
    }

    #[test]
    fn auto_strategy_covers_the_everyday_opaque_pii_types() {
        for (pg_type, expected) in [
            ("date", Strategy::Epoch),
            ("time", Strategy::Epoch),
            ("int2", Strategy::Zero),
            ("numeric(12,2)", Strategy::Zero),
        ] {
            let column = Column::new(
                "value",
                ColumnType::Opaque {
                    pg_type: pg_type.to_owned(),
                },
            );
            assert_eq!(
                auto_strategy(&column).unwrap(),
                expected,
                "`{pg_type}` is an everyday PII column type and needs a usable strategy"
            );
            assert!(replacement_expr(expected, &column, "TOK", false).is_ok());
        }
    }

    #[test]
    fn purge_never_accepts_schema_bookkeeping() {
        for table in NEVER_PURGEABLE_TABLES {
            let config =
                parse_config_str(&format!("[framework]\npurge = [\"{table}\"]\n")).unwrap();
            assert!(
                matches!(
                    check_purge_list(&config),
                    Err(ScrubError::PurgeSchemaBookkeeping { .. })
                ),
                "emptying {table} would make the copy un-migratable or un-routable"
            );
        }
    }

    #[test]
    fn declaring_a_framework_table_points_at_the_right_mechanism() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [tables.api_tokens.pii]
            token = "redact"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a framework table cannot be declared column-by-column");
        // Not "the database does not have it" — that would send the developer
        // hunting for a typo in a name that is spelled correctly.
        assert!(
            matches!(err, ScrubError::FrameworkTableDeclared { ref tables } if tables == &vec!["api_tokens".to_owned()]),
            "got {err:?}"
        );
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
            safe_columns = ["id", "created_at", "email", "bio"]
            [tables.users.pii]
            full_name = "redact"
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
        let column = plan.column("users", "full_name").unwrap();
        assert_eq!(column.strategy, Strategy::Redact);
        assert_eq!(column.source, ClassSource::Config);
    }

    #[test]
    fn a_plaintext_strategy_is_refused_on_an_encrypted_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "redact"
            "#,
        )
        .unwrap();
        // Writing a plain string into an at-rest-encrypted column makes every
        // later read of that row fail as malformed ciphertext, so a declaration
        // may not choose one.
        let err = plan_for(
            &[users_table()],
            &config,
            &encrypted_columns_of("users", &["email"]),
            &no_anonymize(),
        )
        .expect_err("plaintext must never be written into an #[encrypted] column");
        assert!(
            matches!(err, ScrubError::PlaintextIntoEncrypted { ref columns } if columns[0].starts_with("users.email")),
            "got {err:?}"
        );
    }

    #[test]
    fn an_encrypted_column_resolves_to_a_re_encryption_and_carries_its_mode() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            "#,
        )
        .unwrap();
        let encrypted = BTreeMap::from([(
            "users".to_owned(),
            BTreeMap::from([("email".to_owned(), true)]),
        )]);
        let plan = plan_for(&[users_table()], &config, &encrypted, &no_anonymize()).unwrap();
        assert_eq!(
            plan.column("users", "email").unwrap().strategy,
            Strategy::Encrypted
        );
        let table = &plan.tables[0];
        assert!(
            table.sql.is_none(),
            "an encrypted rewrite is not expressible as SQL: {:?}",
            table.sql
        );
        assert_eq!(table.encrypted.len(), 1);
        assert!(
            table.encrypted[0].deterministic,
            "a deterministic column must be re-encrypted deterministically, or equality \
             lookups against it stop matching"
        );
        assert_eq!(table.encrypted[0].shape, Strategy::Email);
    }

    #[test]
    fn null_may_still_be_declared_on_a_nullable_encrypted_column() {
        let mut t = users_table();
        t.columns[1].nullable = true;
        t.columns[1].unique = false;
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "null"
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[t],
            &config,
            &encrypted_columns_of("users", &["email"]),
            &no_anonymize(),
        )
        .expect("NULL is a valid, readable value for an encrypted column");
        assert_eq!(
            plan.column("users", "email").unwrap().strategy,
            Strategy::Null
        );
    }

    // ── Constraint safety (AC #4)     // ── Constraint safety (AC #4) ───────────────────────────────────────────

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
        // `ROW(...)::text` carries Postgres's own quoting, so ('a|','b') and
        // ('a','|b') cannot collapse into one row key the way a plain
        // separator-joined concatenation does.
        assert_eq!(row_key_expr(&t), r#"ROW("user_id", "team_id")::text"#);
    }

    #[test]
    fn token_is_salted_per_column_so_two_columns_never_match() {
        let table = users_table();
        assert_ne!(
            token_expr(&table, "email", false),
            token_expr(&table, "full_name", false),
            "two PII columns of one row must not receive the same fake value"
        );
    }

    #[test]
    fn email_expression_is_unique_per_row_and_uses_a_reserved_domain() {
        let expr = replacement_expr(Strategy::Email, &text_col("email"), "TOK", false).unwrap();
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
        let expr = replacement_expr(Strategy::Email, &column, "TOK", false).unwrap();
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
        let err = replacement_expr(Strategy::Email, &column, "TOK", false)
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
        let sql = sql_of(&plan, "users");
        assert!(sql.starts_with(r#"UPDATE "public"."users" SET "#), "{sql}");
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
                encrypted: BTreeMap::new(),
            },
        );
        let plan = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize()).unwrap();
        let sql = sql_of(&plan, r#"we"ird"#);
        assert!(
            sql.starts_with(r#"UPDATE "public"."we""ird" SET "na""me" = "#),
            "{sql}"
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
    fn a_bare_artifact_with_no_manifest_still_gets_the_source_guard() {
        // `autumn-prod.toml` declares the very database the scrub would write.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[database]\nprimary_url = \"postgres://app:pw@db.example.com:5432/myapp\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("autumn-dev.toml"), "[database]\n").unwrap();

        assert_eq!(
            profiles_with_config(dir.path()),
            vec!["dev".to_owned(), "prod".to_owned()],
            "every profile overlay is a candidate, not just the one an artifact names"
        );
        // A bare `.dump` carries no manifest, so provenance is `None` — which
        // must not read as permission to continue.
        assert!(
            !profiles_with_config(dir.path()).is_empty(),
            "the guard has candidates to check even with unknown provenance"
        );
    }

    #[test]
    fn the_source_guard_waiver_is_not_force() {
        // `--force` is mandatory in the documented staging drill, so a guard it
        // waived would be inert in exactly the workflow it exists for.
        let targets = vec![(
            "control".to_owned(),
            "postgres://app:pw@db.example.com:5432/myapp".to_owned(),
        )];
        let empty = tempfile::tempdir().unwrap();
        assert!(
            guard_configured_source(Some("prod"), empty.path(), &targets, false).is_ok(),
            "no config overlay means nothing to compare against"
        );
        assert!(
            guard_configured_source(Some("prod"), empty.path(), &targets, true).is_ok(),
            "--allow-source-overwrite is the waiver"
        );
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
        let err = replacement_expr(Strategy::Uuid, &text_col("note"), "TOK", false)
            .expect_err("a uuid cannot be written into a text column");
        assert!(
            matches!(err, ScrubError::StrategyTypeMismatch { .. }),
            "{err:?}"
        );
        assert!(
            matches!(
                replacement_expr(Strategy::Zero, &text_col("note"), "TOK", false),
                Err(ScrubError::StrategyTypeMismatch { .. })
            ),
            "zero is meaningless for a text column"
        );
        assert!(
            matches!(
                replacement_expr(
                    Strategy::Epoch,
                    &Column::new("n", ColumnType::Int64),
                    "TOK",
                    false
                ),
                Err(ScrubError::StrategyTypeMismatch { .. })
            ),
            "epoch is meaningless for an integer column"
        );
    }

    #[test]
    fn typed_strategies_render_their_casts() {
        assert_eq!(
            replacement_expr(
                Strategy::Uuid,
                &Column::new("t", ColumnType::Uuid),
                "TOK",
                false
            )
            .unwrap(),
            "(substr(TOK, 1, 32))::uuid"
        );
        assert_eq!(
            replacement_expr(
                Strategy::Bytes,
                &Column::new("b", ColumnType::Bytes),
                "TOK",
                false
            )
            .unwrap(),
            "decode(TOK, 'hex')"
        );
        assert_eq!(
            replacement_expr(
                Strategy::Zero,
                &Column::new("ok", ColumnType::Bool),
                "TOK",
                false
            )
            .unwrap(),
            "false"
        );
        assert_eq!(
            replacement_expr(
                Strategy::Epoch,
                &Column::new("at", ColumnType::TimestampTz),
                "TOK",
                false
            )
            .unwrap(),
            "'1970-01-01 00:00:00+00'::timestamptz"
        );
        assert_eq!(
            replacement_expr(Strategy::Null, &text_col("bio"), "TOK", false).unwrap(),
            "NULL"
        );
    }

    #[test]
    fn phone_produces_digits_and_refuses_a_column_that_cannot_hold_them() {
        let expr = replacement_expr(Strategy::Phone, &text_col("phone"), "TOK", false).unwrap();
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
            replacement_expr(Strategy::Phone, &narrow, "TOK", false),
            Err(ScrubError::ColumnTooNarrow { .. })
        ));
    }

    #[test]
    fn a_partial_unique_index_still_constrains_the_rows_it_covers() {
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
        // A partial unique index does not satisfy a model `#[unique]` — but it
        // absolutely does abort an UPDATE that writes one constant into every
        // row its predicate matches, which is the only question a writer asks.
        let err = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize())
            .expect_err("a partial unique index still constrains the rows it covers");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_composite_unique_index_constrains_each_of_its_members() {
        let mut t = Table::new("cards", Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        t.columns = vec![
            pk_col("id"),
            Column::new("user_id", ColumnType::Int64),
            Column::new("last4", ColumnType::Int32),
        ];
        let mut config = ScrubConfig::default();
        config.defaults.safe_columns = vec!["id".to_owned(), "user_id".to_owned()];
        config.tables.insert(
            "cards".to_owned(),
            TableRule {
                safe: Vec::new(),
                pii: BTreeMap::from([("last4".to_owned(), Strategy::Zero)]),
                encrypted: BTreeMap::new(),
            },
        );
        let facts = DatabaseFacts {
            unique_columns: BTreeSet::from([
                ("cards".to_owned(), "user_id".to_owned()),
                ("cards".to_owned(), "last4".to_owned()),
            ]),
            ..DatabaseFacts::default()
        };
        let err = plan_with_facts(&[t], &config, &empty_encrypted(), &no_anonymize(), &facts)
            .expect_err("a constant in one member of a composite unique key collides");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "{err:?}"
        );
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
            purge = ["autumn_jobs", "_autumn_ledger_revisions", "_autumn_ledger_high_water"]
            "#,
        )
        .unwrap();
        assert!(check_purge_list(&ok).is_ok());
    }

    #[test]
    fn purging_one_ledger_table_without_the_other_is_refused() {
        // #2323: a high-water mark outlives the revisions it names on purpose,
        // so a staging copy holding one without the other has `ledger_verify`
        // accusing every ledgered record on a database nobody tampered with.
        for (listed, missing) in [
            ("_autumn_ledger_revisions", "_autumn_ledger_high_water"),
            ("_autumn_ledger_high_water", "_autumn_ledger_revisions"),
        ] {
            let config = parse_config_str(&format!(
                "[framework]\npurge = [\"autumn_jobs\", \"{listed}\"]\n"
            ))
            .unwrap();
            let err = check_purge_list(&config)
                .expect_err("emptying one ledger table alone must be refused");
            assert!(
                matches!(
                    err,
                    ScrubError::PurgeLedgerTablesUnpaired {
                        listed: ref got_listed,
                        missing: ref got_missing,
                    } if got_listed == listed && got_missing == missing
                ),
                "got {err:?}"
            );
            assert!(err.to_string().contains(missing), "{err}");
        }

        // Both, or neither, is fine.
        for purge in [
            r#"["autumn_jobs"]"#,
            r#"["_autumn_ledger_revisions", "_autumn_ledger_high_water"]"#,
        ] {
            let ok = parse_config_str(&format!("[framework]\npurge = {purge}\n")).unwrap();
            assert!(check_purge_list(&ok).is_ok(), "{purge}");
        }
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
                r#"DELETE FROM "public"."autumn_jobs""#.to_owned()
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
