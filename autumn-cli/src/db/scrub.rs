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
    /// The target uses legacy `INHERITS` table inheritance, which the sample
    /// cannot model: a statement naming the parent silently reaches the child.
    LegacyInheritance {
        /// `child (inherits parent)` descriptions, sorted.
        tables: Vec<String>,
    },
    /// The connection is in a `session_replication_role` other than `origin`,
    /// where `ENABLE REPLICA` triggers and rules fire and ordinary ones do not
    /// — inverting the enablement rule every hazard check here is built on.
    ReplicaSessionRole {
        /// The role the connection reports.
        role: String,
    },
    /// `--dry-run` cannot print a connection boundary for a target, because its
    /// connection string is in keyword form and no password can be removed from
    /// that with certainty. Without the boundary the printed script would run
    /// this target's destructive plan against whatever database the pasting
    /// session happens to be connected to.
    UnprintableTarget {
        /// The target labels, sorted.
        targets: Vec<String>,
    },
    /// `--dry-run` cannot print a runnable script because the plan rewrites an
    /// `#[encrypted]` column, and that rewrite has no SQL form: the replacement
    /// is an AEAD envelope sealed per row under the target's key, which cannot
    /// be printed without printing the key.
    ///
    /// The script is withheld whole rather than printed with the one statement
    /// missing. A printed stream is pasted into an interactive psql, and there
    /// is no marker that stops one: measured on psql 16.13, a `RAISE EXCEPTION`
    /// aborts its own transaction only — the `COMMIT` below it clears the
    /// aborted state, and every later statement, including the next target's
    /// `\connect` and its deletes, runs for real.
    UnprintableEncryptedRewrite {
        /// The columns that cannot be printed, as `target: table.column`, sorted.
        columns: Vec<String>,
    },
    /// `--dry-run` cannot print a runnable script for a profile the command
    /// refuses to scrub without `--force`.
    ///
    /// The script is executable and its `\connect` names the protected target,
    /// so printing it hands over exactly the run the profile guard exists to
    /// stop — and the password is removed on purpose, which `.pgpass` supplies.
    UnprintableProductionTarget {
        /// The refused profile.
        profile: String,
    },
    /// The materialized-view dependency walk did not reach every view, so the
    /// run cannot refresh them all — and an unrefreshed view keeps the rows it
    /// selected before the scrub.
    UnrefreshableViews {
        /// The views the walk never reached, sorted.
        views: Vec<String>,
    },
    /// `--dry-run` cannot print a guard that distinguishes this target from a
    /// physical clone of it: the connection is over a Unix socket (so the
    /// address and port are NULL) and the role cannot read `data_directory`,
    /// which is the only value a clone does not share.
    UnprintableAmbiguousTarget {
        /// The target labels, sorted.
        targets: Vec<String>,
    },
    /// A materialized view's definition calls a function whose body `PostgreSQL`
    /// does not track, so the run cannot know which views it reads and cannot
    /// order the refresh around it.
    UntraceableViewFunction {
        /// `view via function`, sorted.
        views: Vec<String>,
    },
    /// `--dry-run` cannot prove the reconnect for a target whose connection
    /// string leaves the port to libpq: psql resolves it from `PGPORT` (or the
    /// 5432 default) at PASTE time, which need not be what it resolved when the
    /// run planned.
    UnprintablePortlessTarget {
        /// The target labels, sorted.
        targets: Vec<String>,
    },
    /// `--dry-run` cannot print a runnable script for a target whose connection
    /// string states `hostaddr`. It selects the endpoint independently of
    /// `host`, and psql exposes no variable reporting it, so the reconnect proof
    /// cannot tell two such targets apart.
    UnprintableHostaddrTarget {
        /// The target labels, sorted.
        targets: Vec<String>,
    },
    /// `--dry-run` cannot print a runnable script for a target whose connection
    /// string names more than one endpoint. Which member libpq picks is decided
    /// per connection, so the counts baked into the script and the session that
    /// pastes it need not be the same database.
    UnprintableMultiHostTarget {
        /// The target labels, sorted.
        targets: Vec<String>,
    },
    /// A table this run promises to empty carries a user-defined trigger or
    /// rewrite rule that fires on `DELETE`. That emptying pass is the run's LAST
    /// write, so anything it writes lands after every rewrite and is never
    /// verified.
    EmptyingTriggerLeak {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// The target has row-level security on a table this run reads or writes —
    /// a rewrite, an emptying `DELETE`, the sample's own reads, or an endpoint
    /// of the foreign key re-count.
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
            Self::LegacyInheritance { tables } => write!(
                f,
                "This database uses legacy table inheritance on {} table(s):\n{}\n  \
                 Unlike a declarative partition, an inheritance child is an ordinary table \
                 that the sample plans separately — while `DELETE FROM parent` and \
                 `SELECT ... FROM parent` reach its rows too, because they are not written \
                 `ONLY parent`. Parent and child would then select rows independently and \
                 delete each other's, and the run would report a success it cannot stand \
                 behind. Sample a copy without the inheritance, or drop the child tables \
                 from it.",
                tables.len(),
                bullet_list(tables),
            ),
            Self::UnprintableTarget { targets } => write!(
                f,
                "`--dry-run` cannot print a runnable script for {} target(s):\n{}\n  \
                 Each block it prints is destructive, and the `\\connect` line above it \
                 is what points psql at the right database. These targets are configured \
                 with a keyword-form connection string (`host=... password=...`), which \
                 cannot be printed with its password removed for certain — libpq allows \
                 whitespace around the `=` and quoted values with escapes — so the \
                 boundary is withheld. A script without it does not fail: it runs this \
                 target's plan against whichever database the pasting session is already \
                 on, which in a multi-target stream is the previous target. Configure the \
                 target as a URI (`postgres://user@host/db`), or run without `--dry-run`.",
                targets.len(),
                bullet_list(targets),
            ),
            Self::UntraceableViewFunction { views } => write!(
                f,
                "{} materialized view(s) read through a function this run cannot \
                 follow:\n{}\n  \
                 Refresh order is taken from the dependency graph `PostgreSQL` records, \
                 and it records nothing about what a function whose body it cannot \
                 parse reads — a string literal, `plpgsql`, or C. Measured on `a_report -> bridge_fn() -> z_source`: \
                 `pg_depend` holds `a_report -> pg_proc(bridge_fn)` and the function \
                 holds NO relation dependency at all, so the two views sorted by name, \
                 `a_report` refreshed FIRST from a `z_source` still holding pre-scrub \
                 rows, and the run reported success with `users` at 2 rows, 0 original \
                 addresses in the base tables and in `z_source`, and all 200 still in \
                 `a_report`. A `BEGIN ATOMIC` body IS tracked and is followed normally. \
                 Rewrite the function with `BEGIN ATOMIC`, inline its query into the \
                 view, or drop the view and rebuild it after the run.",
                views.len(),
                bullet_list(views),
            ),
            Self::UnprintableMultiHostTarget { targets } => write!(
                f,
                "`--dry-run` cannot print a runnable script for {} target(s):\n{}\n  \
                 Their connection strings name more than one endpoint, and libpq chooses \
                 a member per connection — measured on 16.13, 20 connections with \
                 `load_balance_hosts=random` split 7/13 across two members. The run sizes \
                 the sample on one connection and the pasted script runs on another, so \
                 the root `LIMIT` need not describe the database it lands on: measured \
                 against a two-member URI whose first member held 200 rows and whose \
                 second held none, ten dry runs printed `LIMIT 2` six times and `LIMIT 0` \
                 four times, and `LIMIT 0` selects no root rows at all, so the delete pass \
                 empties the table instead of sampling it. The endpoint proof cannot \
                 separate the members either — it has to accept any of them, which is the \
                 discriminator it exists for. Name one endpoint \
                 (`postgres://user@host:5432/db`), or run without `--dry-run`, where the \
                 command sizes and writes on the one connection it holds.",
                targets.len(),
                bullet_list(targets),
            ),
            Self::UnprintableHostaddrTarget { targets } => write!(
                f,
                "`--dry-run` cannot print a runnable script for {} target(s):\n{}\n  \
                 Their connection strings state `hostaddr`, which selects the endpoint \
                 independently of `host` — measured, `host=not-a-real-host.example \
                 hostaddr=127.0.0.1` connects to 127.0.0.1 although the name does not \
                 resolve at all, and psql still reports `HOST=not-a-real-host.example`. \
                 psql exposes no `:HOSTADDR` to pin instead: `\\echo [:HOSTADDR]` prints \
                 the name back unexpanded. So two targets sharing a host, port and \
                 database name but reaching different servers through different \
                 `hostaddr` values are indistinguishable to the reconnect proof, which \
                 is the one thing it exists to tell apart. Drop `hostaddr` and name the \
                 endpoint in `host` (`postgres://user@10.0.0.7:5432/db`), or run without \
                 `--dry-run`, where the command holds its own connection and never has \
                 to prove which one it is.",
                targets.len(),
                bullet_list(targets),
            ),
            Self::UnprintablePortlessTarget { targets } => write!(
                f,
                "`--dry-run` cannot print a runnable script for {} target(s):\n{}\n  \
                 Their connection strings name a host but no port, so psql resolves one \
                 when the script is pasted — from `PGPORT`, or 5432 — and that need not \
                 be what libpq resolved while this run planned. The block's proof that \
                 `\\connect` reached the intended endpoint would then have to accept ANY \
                 port for that host, which is the discriminator it exists for: measured, \
                 the same URI resolves to 5433 under `PGPORT=5433` and to 5432 without \
                 it, two different servers. Embedding the port this run resolved is not \
                 an option either — libpq's own resolution can come from a service file \
                 this command does not read. State the port in the connection string \
                 (`postgres://user@host:5432/db`), or run without `--dry-run`, where the \
                 command holds its own connection and never has to prove which one it \
                 is.",
                targets.len(),
                bullet_list(targets),
            ),
            Self::UnprintableEncryptedRewrite { columns } => write!(
                f,
                "`--dry-run` cannot print a runnable script: {} column(s) are \
                 #[encrypted]:\n{}\n  \
                 The scrub replaces each of these with an AEAD envelope sealed per row \
                 under the target's key, so there is no SQL text for it that does not \
                 embed the key. Printing the rest and omitting these is worse than \
                 printing nothing: the script is meant to be pasted into psql, and \
                 nothing in a pasted stream can stop it partway — an aborted \
                 transaction ends at the next `COMMIT`, after which the following \
                 target's `\\connect` and its DELETEs run for real. So the whole \
                 script is withheld, not just the statements that cannot be written. \
                 The plan above is complete and accurate — run `autumn db scrub` \
                 without --dry-run to apply it.",
                columns.len(),
                bullet_list(columns),
            ),
            Self::UnprintableProductionTarget { profile } => write!(
                f,
                "`--dry-run` cannot print a runnable script for the {profile:?} profile.\n  \
                 A scrub REWRITES data in place, and this command refuses to run against \
                 {profile:?} without `--force`. The script it prints is executable and its \
                 `\\connect` line names that same database — with the password removed on \
                 purpose, which is what `.pgpass` is for — so printing it would hand over \
                 the run the profile guard exists to stop. The plan above is complete and \
                 accurate; add `--force` to print the script too, or point `--profile` at a \
                 staging target.",
            ),
            Self::UnrefreshableViews { views } => write!(
                f,
                "{} materialized view(s) cannot be refreshed in dependency order:\n{}\n  \
                 The run refreshes views so each is rebuilt from scrubbed data, and it \
                 orders them by walking `pg_depend`. That walk stops at a fixed depth, so \
                 a chain longer than it leaves these views unreached — and a view that is \
                 not refreshed keeps the rows it selected BEFORE the scrub, including the \
                 values the run just removed from the tables it reads. Measured on a \
                 36-deep chain: 33 views refreshed, `users` left with 0 original \
                 addresses, and the deepest view still holding all 200. Refusing is the \
                 only honest answer until the walk covers the whole graph — shorten the \
                 chain, or drop the views this run cannot reach and rebuild them after \
                 it.",
                views.len(),
                bullet_list(views),
            ),
            Self::UnprintableAmbiguousTarget { targets } => write!(
                f,
                "`--dry-run` cannot print a runnable script for {} target(s):\n{}\n  \
                 These are reached over a Unix socket, where the server reports no \
                 address and no port — and nothing else it reports identifies the \
                 instance either. A physical copy carries its origin's \
                 `system_identifier`; the configured port is shared by two clusters on \
                 different socket directories; and `data_directory` is server-LOCAL, so \
                 two containers each answering `/var/lib/postgresql/data` match on it \
                 while being different databases. Every value the guard can ask for is \
                 either cloned or container-local, so a printed block cannot tell this \
                 target from a copy of it — and `\\connect` keeps the PREVIOUS connection \
                 when it fails. Measured on two clusters sharing port 5433: the clone's \
                 script pasted at its origin passed the guard, committed, and took the \
                 ORIGIN from 200 users to 25. Connect over TCP, where the address and \
                 port identify the endpoint and two clones cannot hold the same pair, or \
                 run without --dry-run — the command opens its own connection and cannot \
                 be on the wrong database.",
                targets.len(),
                bullet_list(targets),
            ),
            Self::ReplicaSessionRole { role } => write!(
                f,
                "This connection runs with `session_replication_role = {role}`, not `origin`.\n  \
                 That inverts which hooks fire: a trigger or rule marked `ENABLE REPLICA` runs \
                 and an ordinary one does not, so every check here that asks whether a `DELETE` \
                 can execute code is answering for the wrong session. A replica-only archive \
                 trigger on a table this run empties would copy the rows it removes into a \
                 table nothing verifies, past a refusal that never saw it. Connect without \
                 `options=-c session_replication_role=...`, or reset it before scrubbing.",
            ),
            Self::EmptyingTriggerLeak { tables } => write!(
                f,
                "{} table(s) this run empties carry a user-defined trigger or rewrite rule \
                 that fires on `DELETE`:\n{}\n  \
                 Emptying them is the LAST thing the run writes \u{2014} it has to be, because a \
                 trigger on a scrubbed table can otherwise re-fill them with the very PII \
                 being removed. Anything attached to a `DELETE` on one of them therefore runs \
                 after every column rewrite, and can copy the rows it is removing into an \
                 ordinary table that has already been scrubbed. Nothing downstream can catch \
                 that: the run verifies these tables are empty, not where their triggers and \
                 rules wrote. On the copy, `ALTER TABLE ... DISABLE TRIGGER ...` or \
                 `DROP RULE ... ON ...` before scrubbing.",
                tables.len(),
                bullet_list(tables),
            ),
            Self::RowLevelSecurity { tables } => write!(
                f,
                "{} table(s) this run reads or writes have row-level security enabled:\n{}\n  \
                 A role that does not bypass RLS sees only the rows its policies expose, and \
                 every phase then reports a success it cannot stand behind: an `UPDATE` \
                 rewrites part of the PII, a `DELETE` reports a table emptied that is not, and \
                 the foreign key re-count misses the very orphan it exists to find. \
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
        if inputs.facts.partitions.contains_key(&table.name) {
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

        // Before the sample is planned, not after. An inheritance child has no
        // foreign key of its own — legacy `INHERITS` does not carry constraints
        // down — so the coverage check would otherwise report it as unreachable
        // and advise naming it as a root, which is precisely the arrangement
        // that corrupts. Only the sample is refused: the column rewrites are
        // per-row UPDATEs that reach the child's rows correctly either way.
        if !args.sample.is_empty() && !facts.legacy_inheritance.is_empty() {
            return Err(ScrubError::LegacyInheritance {
                tables: facts.legacy_inheritance,
            });
        }
        // A view whose order cannot be DERIVED at all, because its definition
        // calls a function whose body PostgreSQL does not track. Refused before
        // the reachability check below, which can only compare two lists built
        // from a graph this shape is missing from entirely.
        if !facts.untraceable_view_functions.is_empty() {
            return Err(ScrubError::UntraceableViewFunction {
                views: facts.untraceable_view_functions,
            });
        }
        // A view the dependency walk never reached is one the run cannot
        // refresh, and an unrefreshed view keeps its pre-scrub rows. Detected by
        // comparing the ordered list against the flat enumeration rather than by
        // knowing the walk's depth limit, so it stays true if that limit moves.
        //
        // Measured on a 36-deep chain: the run refreshed 33 views, reported
        // success, left `users` with 0 original addresses — and the deepest view
        // still held all 200. That is a silent leak, so it is refused instead.
        let mut unreachable_views: Vec<String> = facts
            .all_materialized_views
            .iter()
            .filter(|view| !facts.materialized_views.contains(view))
            .cloned()
            .collect();
        if !unreachable_views.is_empty() {
            unreachable_views.sort();
            return Err(ScrubError::UnrefreshableViews {
                views: unreachable_views,
            });
        }
        let plan = build_plan(&ClassificationInputs {
            tables: &tables,
            config: &sources.config,
            encrypted: &sources.encrypted,
            anonymize_tables: &sources.anonymize,
            facts: &facts,
        })?;

        // Resolved in the same pass as the classification, and before ANY
        // target is written, so a graph gap on one shard cannot leave the rest
        // of the topology sampled.
        let sampling = if sources.roots.is_empty() {
            None
        } else {
            Some(build_sample_plan_for(args, sources, &tables, &facts)?)
        };

        // RLS makes an `UPDATE` silently apply to policy-visible rows only,
        // which is a fail-OPEN in a fail-closed tool — refuse rather than
        // report a partial scrub as complete.
        // Purge targets are framework tables, which are excluded from
        // `plan.tables` — so without them an RLS-protected job/token/sync table
        // would have its `DELETE` silently apply to policy-visible rows only,
        // and still be reported emptied.
        // The sample's tables are the third class of write, and the one a
        // column-level plan can never cover: a pure join table has no PII
        // column (PII on a key is refused outright), so it is absent from
        // `plan.tables` — yet the sample deletes from it, and reads it to
        // decide what to keep. Under RLS both would see policy-visible rows
        // only, and the run would report a table emptied that is not.
        let mut rls: Vec<String> = plan
            .tables
            .iter()
            .map(|t| t.table.clone())
            .chain(
                purge_statements(&facts.framework_tables, &sources.config)
                    .into_iter()
                    .map(|(table, _)| table),
            )
            .chain(
                sampling
                    .iter()
                    .flat_map(sample::SamplePlan::locked_tables)
                    .map(str::to_owned),
            )
            .filter(|t| facts.rls_tables.contains(t))
            .collect();
        rls.sort();
        rls.dedup();
        if !rls.is_empty() {
            return Err(ScrubError::RowLevelSecurity { tables: rls });
        }

        // Before any hazard check that reads `tgenabled` or `ev_enabled`: in
        // replica mode those columns mean the opposite of what the checks
        // assume, so their answers cannot be trusted at all.
        if facts.replication_role != "origin" {
            return Err(ScrubError::ReplicaSessionRole {
                role: facts.replication_role,
            });
        }

        // The emptying pass runs LAST — after every column rewrite — because
        // that is the only order in which "this table ends up empty" survives a
        // rewrite trigger re-filling it. The cost is that its own `DELETE`
        // triggers fire after everything else: an archive trigger on a purged
        // or `never_include` table can copy the rows it is removing into an
        // ordinary classified table whose rewrite has already run, and the
        // verification that follows counts these tables rather than tracing
        // where their triggers wrote. There is no order that satisfies both —
        // the trigger graph can be cyclic — and no postcondition to check
        // instead, because a trigger body can write anywhere. So it is refused
        // before anything is written.
        let mut emptying_triggers: Vec<String> =
            purge_statements(&facts.framework_tables, &sources.config)
                .into_iter()
                .map(|(table, _)| table)
                .chain(
                    sampling
                        .iter()
                        .flat_map(|s| s.emptied_tables())
                        .map(|(table, _)| table.to_owned()),
                )
                .filter(|t| facts.delete_triggered_tables.contains(t))
                .collect();
        emptying_triggers.sort();
        emptying_triggers.dedup();
        if !emptying_triggers.is_empty() {
            return Err(ScrubError::EmptyingTriggerLeak {
                tables: emptying_triggers,
            });
        }

        report_plan(label, &plan);
        report_framework_tables(&facts.framework_tables, &sources.config);
        report_triggers(&plan, sampling.as_ref(), &facts);
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
                 emptied unannounced."
            );
        }
        return Ok(());
    }
    if args.dry_run {
        // Refuse before printing anything, if any target's boundary cannot be
        // emitted. The alternative — a comment saying "connect yourself" above
        // a BEGIN and a page of DELETEs — reads as advice and behaves as a
        // loaded gun: pasted, it runs against whatever database the session is
        // already on. Fail closed here, like every other promise in this
        // command that cannot be arranged safely.
        let unprintable: Vec<String> = plans
            .iter()
            .filter(|(_, url, _, _, _)| password_free_conninfo(url).is_none())
            .map(|(label, _, _, _, _)| (*label).clone())
            .collect();
        if !unprintable.is_empty() {
            return Err(ScrubError::UnprintableTarget {
                targets: unprintable,
            });
        }
        // And refuse, the same way, if any target rewrites an #[encrypted]
        // column. That rewrite has no SQL form — the replacement is sealed per
        // row under the target's key — so the script could only be printed with
        // that one statement missing, and a script missing its rewrites empties
        // and samples exactly as advertised while leaving every kept row's
        // production ciphertext in place. Measured: pasting such a script left
        // `users` sampled 200 -> 100 with every address rewritten AND all 100
        // kept rows still holding their original ciphertext, committed without
        // an error.
        //
        // A marker in the stream does not close this. A `RAISE EXCEPTION` aborts
        // its own transaction, and that much works — the deletes below it are
        // refused. But the `COMMIT` this script prints turns the abort into a
        // ROLLBACK and clears the state, and measured on psql 16.13 the pasted
        // lines after it run: the out-of-transaction `VACUUM (FULL)`s, then the
        // next target's `\connect` and its whole block. On two clusters, a
        // stopped first target still took the second from 200 comments to 0,
        // committed. `\quit` is not an answer either: psql exits and the rest
        // of the paste is read by the shell that launched it, which executed a
        // trailing `echo` in the same measurement. Fail closed instead, exactly
        // as above: the plan is still reported in full, only the runnable script
        // is withheld.
        // And refuse for a profile this command will not scrub. `writes` is
        // false for a dry run, so `guard_scrub_target` never ran — harmless
        // when the dry run only described a plan, and not harmless now that it
        // prints a paste-ready script whose `\connect` names the protected
        // target. Measured: `--dry-run --profile production` printed 14
        // runnable lines, including that `\connect`, for a database the same
        // command refuses to touch without `--force`.
        guard_scrub_target(profile, args.force).map_err(|_| {
            ScrubError::UnprintableProductionTarget {
                profile: profile.to_owned(),
            }
        })?;
        // And refuse a socket target outright, because no value the server
        // reports identifies the instance behind a socket. Address and port are
        // NULL there by construction; `system_identifier` is copied by any
        // physical clone; the configured port is shared by two clusters on
        // different socket directories; and `data_directory` is server-LOCAL,
        // so two containers each answering `/var/lib/postgresql/data` match on
        // it while being different databases. Three narrower guards were each
        // defeated by the next topology, which is the shape of a value that
        // does not exist rather than one not yet found. Measured on two
        // clusters sharing port 5433: the clone's script pasted at its origin,
        // guard passed, COMMIT, origin 200 users -> 25.
        let mut ambiguous: Vec<String> = plans
            .iter()
            .filter(|(_, _, _, facts, _)| {
                let e = &facts.endpoint;
                e.address.is_none() && e.port.is_none()
            })
            .map(|(label, _, _, _, _)| (*label).clone())
            .collect();
        ambiguous.sort();
        if !ambiguous.is_empty() {
            return Err(ScrubError::UnprintableAmbiguousTarget { targets: ambiguous });
        }
        // And refuse a TCP target whose conninfo leaves the port to libpq. The
        // reconnect proof pins psql's own `:PORT`, and psql reports the RESOLVED
        // port — measured, the same host-only URI reports 5433 under
        // `PGPORT=5433` and 5432 without it. Accepting any port for the host
        // would drop the discriminator on exactly the pair of servers this proof
        // exists to tell apart; asserting a port this run computed itself would
        // mean replicating libpq's resolution, which can read a service file
        // this command does not. Neither is honest, so it refuses instead.
        let mut portless: Vec<String> = plans
            .iter()
            .filter(|(_, url, _, _, _)| {
                password_free_conninfo(url)
                    .is_some_and(|conninfo| stated_host_and_port(&conninfo).1.is_none())
            })
            .map(|(label, _, _, _, _)| (*label).clone())
            .collect();
        portless.sort();
        if !portless.is_empty() {
            return Err(ScrubError::UnprintablePortlessTarget { targets: portless });
        }
        // Same reasoning, one parameter over: `hostaddr` picks the endpoint on
        // its own. Measured, `host=not-a-real-host.example hostaddr=127.0.0.1`
        // reaches 127.0.0.1 while psql still reports that unresolvable name as
        // `:HOST`, and there is no `:HOSTADDR` to pin in its place — `\echo
        // [:HOSTADDR]` prints the name back unexpanded. Two such targets can
        // therefore agree on every term the proof can state and still be
        // different servers.
        let mut addressed: Vec<String> = plans
            .iter()
            .filter(|(_, url, _, _, _)| {
                password_free_conninfo(url).is_some_and(|c| states_hostaddr(&c))
            })
            .map(|(label, _, _, _, _)| (*label).clone())
            .collect();
        addressed.sort();
        if !addressed.is_empty() {
            return Err(ScrubError::UnprintableHostaddrTarget { targets: addressed });
        }
        // And a target naming more than one endpoint. The sizing connection and
        // the pasting session are separate draws, so the printed `LIMIT` can be
        // computed on a different member than the script runs against —
        // measured, ten dry runs against a two-member URI printed `LIMIT 2` six
        // times and `LIMIT 0` four times, and `LIMIT 0` makes the delete pass
        // empty the root instead of sampling it.
        let mut multi: Vec<String> = plans
            .iter()
            .filter(|(_, url, _, _, _)| {
                password_free_conninfo(url).is_some_and(|c| states_multiple_endpoints(&c))
            })
            .map(|(label, _, _, _, _)| (*label).clone())
            .collect();
        multi.sort();
        if !multi.is_empty() {
            return Err(ScrubError::UnprintableMultiHostTarget { targets: multi });
        }
        let mut encrypted_rewrites: Vec<String> = plans
            .iter()
            .flat_map(|(label, _, plan, _, _)| {
                plan.tables.iter().flat_map(move |table| {
                    table
                        .encrypted
                        .iter()
                        .map(move |rewrite| format!("{label}: {}.{}", table.table, rewrite.column))
                })
            })
            .collect();
        encrypted_rewrites.sort();
        if !encrypted_rewrites.is_empty() {
            return Err(ScrubError::UnprintableEncryptedRewrite {
                columns: encrypted_rewrites,
            });
        }
        // Printed in the order `execute` runs them: purges, then the sample,
        // then the rewrites — and, like `execute`, holding back the purges the
        // plan defers until after the sample. The order is load-bearing (a
        // framework-owned table is emptied before the sample removes the rows
        // it points at, except where the sample must empty its child first), so
        // a reader auditing the dry run has to see the real sequence: printing
        // a deferred purge early would show SQL that fails if it were run.
        //
        // The BEGIN/COMMIT is part of that sequence, not decoration. `execute`
        // runs all of this in one transaction, and the sample's keep-sets are
        // `CREATE TEMPORARY TABLE ... ON COMMIT DROP`: pasted into psql's
        // autocommit, each one would be committed and dropped before the seed
        // INSERT that follows it. Without the envelope the advertised "exact
        // SQL" is not runnable.
        // Non-interactive is the supported way to run this, and there a failed
        // `\connect` already stops processing. This makes every OTHER error stop
        // it too, which a stream of destructive blocks wants. It does nothing for
        // an interactive paste — measured, psql ignores it there — which is what
        // the per-target guard below is for.
        // Size every sampled target BEFORE printing a line. Sizing opens its own
        // connection and reads live counts, so it is the one fallible step left
        // in the emission loop — and a failure on a LATER target used to land
        // after an EARLIER one's complete block, `COMMIT` included. Measured on
        // a control plus one shard where the role could read the catalogs but
        // not `SELECT` the shard's `users`: the run exited 1 with `permission
        // denied for table users`, having already printed the control's whole
        // transaction (one `COMMIT`, seven `DELETE FROM`). Saving that output
        // and running it scrubs the control and leaves the shard untouched —
        // the half-anonymized topology this function's two passes exist to
        // prevent, reached through the one fallible call that had not moved
        // into pass 1.
        let mut sized: Vec<Option<BTreeMap<String, i64>>> = Vec::with_capacity(plans.len());
        for (label, url, _, _, sampling) in &plans {
            sized.push(match sampling {
                Some(plan) => {
                    let mut conn = probe_connection(url, label, "size the sample")?;
                    Some(
                        sample::source_counts(&mut conn, plan)
                            .map_err(|e| ScrubError::Sql(e.to_string()))?,
                    )
                }
                None => None,
            });
        }
        eprintln!("  \\set ON_ERROR_STOP on");
        // One flag for the whole stream: "everything so far succeeded, and the
        // last block was on the target it named". `execute` returns on the first
        // target that fails and never touches the rest, so the script must not
        // either — measured, without this the next target's `\connect` and its
        // whole destructive block ran after an earlier target rolled back,
        // leaving a partially scrubbed topology and a stream that ends without
        // an error. Nested `\if` carries it: a target whose block is skipped
        // never reaches the `\gset` that would set the flag true again, so one
        // failure skips every target after it.
        eprintln!("  \\set autumn_ok true");
        for (index, (label, url, plan, facts, sampling)) in plans.iter().enumerate() {
            let no_deferral = BTreeSet::new();
            let deferred: &BTreeSet<String> =
                sampling.as_ref().map_or(&no_deferral, |s| &s.purge_after);
            let purges = purge_statements(&facts.framework_tables, &sources.config);
            let phases = emptying_phases(&purges, deferred, sampling.as_ref());
            // Each target is a DIFFERENT database, and the printed stream is one
            // file. Without a boundary, pasting it runs every target's
            // transaction against whichever database the session happens to be
            // connected to — sampling one of them repeatedly, with row counts
            // taken from the others, and leaving the rest untouched.
            // Gate the WHOLE block, `\connect` included: an earlier target
            // that rolled back must not be followed by this one connecting and
            // scrubbing anyway.
            eprintln!("  \\if :autumn_ok");
            for line in psql_connect(label, url) {
                eprintln!("  {line}");
            }
            // And prove the reconnect landed where it was aimed, before the
            // BEGIN below. This reads psql's own view of the connection, which
            // the server's answers cannot stand in for — see
            // `psql_connection_assertion`.
            if let Some(conninfo) = password_free_conninfo(url) {
                for line in psql_connection_assertion(&conninfo, &facts.endpoint.database) {
                    eprintln!("  {line}");
                }
            }
            // Reset per target: a previous target's success must not vouch for
            // this one. `\gset` leaves a variable untouched when its query
            // fails, so this explicit `false` is what survives an aborted
            // transaction.
            eprintln!("  \\set autumn_scrubbed false");
            eprintln!("  BEGIN;");
            // The same session pins `execute` sets, before anything reads or
            // writes: without them a role-level `search_path` resolves the
            // generated calls somewhere else entirely. They come before the
            // guard for that reason — the guard is `pg_catalog`-qualified too,
            // but a pin that only lands after the check it protects is not a
            // pin. `SET LOCAL` writes nothing, so nothing destructive precedes
            // the proof below.
            for statement in session_settings() {
                eprintln!("  {statement};");
            }
            // The precondition the run refuses to plan without, asserted here
            // because the pasting session brings its own — see
            // `replication_role_assertion`.
            eprintln!("  {}", replication_role_assertion());
            // Inside the transaction, so a `\connect` that silently failed
            // aborts this block instead of running it against the previous
            // target. Before every destructive statement: nothing may run until
            // the session has proved it is where this block thinks it is.
            eprintln!("  {}", target_guard(&facts.endpoint));
            // The same locks `execute` takes, before any destructive statement:
            // without them a pasted run lets a concurrent insert land after the
            // DELETE that was supposed to remove it.
            for statement in lock_statements(plan, &purges, sampling.as_ref()) {
                eprintln!("  {statement};");
            }
            for (_, statement) in &phases.before {
                eprintln!("  {statement};");
            }
            if let Some(sampling) = sampling {
                report_sample_sql(sampling, sized[index].as_ref());
            }
            for (_, statement) in &phases.after_sample {
                eprintln!("  {statement};");
            }
            // No `table.encrypted` arm here: a plan with any encrypted rewrite
            // is refused above, before a line of this script is printed.
            for table in &plan.tables {
                if let Some(sql) = &table.sql {
                    eprintln!("  {sql};");
                }
            }
            // Last, exactly as `execute` runs it: the pass that makes "emptied"
            // true even if a rewrite trigger just re-filled one of these tables.
            for (_, statement) in &phases.final_pass {
                eprintln!("  {statement};");
            }
            // The emptying statements above fire triggers, and one can refill a
            // table an earlier one emptied. `execute` counts them all and rolls
            // back; the printed sequence has to do the same, or an operator who
            // runs it commits exactly the rows the real command refuses.
            for (table, _) in &phases.final_pass {
                eprintln!("  {};", emptiness_assertion(table));
            }
            // Last inside the transaction, exactly where `execute` runs them: a
            // materialized view keeps its own physical copy of whatever it
            // selected, so a script that skips this leaves the view's heap
            // holding the pre-scrub rows — including the PII just removed from
            // the base tables it reads. In dependency order, so a view over
            // another is rebuilt from the refreshed one rather than the stale
            // one. Inside the envelope like the rest, so a refresh the role may
            // not run rolls the rewrites back instead of committing base tables
            // a stale view contradicts.
            for view in &facts.materialized_views {
                eprintln!("  REFRESH MATERIALIZED VIEW {};", qualified_ident(view));
            }
            // And the same closing pass `execute` runs, in the same position:
            // every view that had no data when the run probed ends with none,
            // whether it was populated for a dependent or skipped entirely.
            for view in &facts.unpopulated_views {
                eprintln!(
                    "  REFRESH MATERIALIZED VIEW {} WITH NO DATA;",
                    qualified_ident(view)
                );
            }
            // The last statement inside the transaction, and the whole reason
            // the compaction below can tell a scrubbed target from an aborted
            // one. In an aborted transaction this SELECT is refused like every
            // other, so `\gset` assigns nothing and the `false` above stands.
            eprintln!("  SELECT true AS autumn_scrubbed \\gset");
            eprintln!("  COMMIT;");
            // Whether this target actually scrubbed, recorded for the
            // compaction pass that follows every target's block. Emitted for
            // EVERY target, not only a sampled one: the flag is also what
            // carries a failure forward, and a target with nothing to compact
            // still has to say whether it succeeded.
            for line in post_commit_fence(&facts.endpoint) {
                eprintln!("  {line}");
            }
            // Closes the `\if :autumn_ok` the connection assertion opened, then
            // the one this target opened.
            eprintln!("  \\endif");
            eprintln!("  \\endif");
        }
        // Compaction comes after EVERY target's transaction, never between two
        // of them, because that is where `classify_and_apply` runs it: it
        // commits each target in one loop and then compacts in a second, where
        // a failure is a warning and the next target is compacted anyway.
        //
        // Printed inside the per-target block it was neither. Measured on two
        // TCP targets with `ON_ERROR_STOP on` — the supported non-interactive
        // way to run this — a `VACUUM (FULL, ANALYZE)` on the first target that
        // timed out against a concurrent reader ended the script at rc=3: the
        // first database was scrubbed and sampled (2 users, 0 original
        // addresses) and the SECOND still held all 200 of its own, from a
        // failure the command itself only warns about.
        //
        // Guarded as a whole on `:autumn_ok`, which matches the executor again:
        // it propagates the first `execute` failure with `?`, so a later
        // target's failure means no earlier target is compacted either.
        let sampled: Vec<_> = plans
            .iter()
            .filter_map(|(label, url, _, facts, sampling)| {
                sampling.as_ref().map(|s| (label, url, facts, s))
            })
            .collect();
        if !sampled.is_empty() {
            eprintln!("  \\if :autumn_ok");
            // Warning-only from here, as in the executor: with `ON_ERROR_STOP`
            // still on, one target's failed VACUUM would skip every later
            // target's. Nothing destructive follows — these statements are
            // outside every transaction and rewrite no rows — and the per-target
            // guard below is what a failed `\connect` runs into, exactly as it
            // does above.
            eprintln!("  \\set ON_ERROR_STOP off");
            for (label, url, facts, sampling) in sampled {
                let purges = purge_statements(&facts.framework_tables, &sources.config);
                for line in psql_connect(label, url) {
                    eprintln!("  {line}");
                }
                // The same fail-closed shape the fence uses: false first, so a
                // `\gset` whose query fails cannot carry the previous target's
                // answer into this one's VACUUM statements.
                //
                // And the SAME psql proof the transaction opens with, not the
                // endpoint alone: this is a second `\connect`, with the same
                // retained-connection failure mode, and server-reported identity
                // cannot tell two clones behind colliding addresses apart. A
                // `VACUUM (FULL)` on the wrong target takes an ACCESS EXCLUSIVE
                // lock and rewrites every table it names.
                eprintln!("  \\set autumn_here false");
                eprintln!(
                    "  SELECT ({} AND NOT ({})) AS autumn_here \\gset",
                    password_free_conninfo(url).map_or_else(
                        || "false".to_owned(),
                        |conninfo| psql_connection_terms(&conninfo, &facts.endpoint.database),
                    ),
                    endpoint_mismatch(&facts.endpoint)
                );
                eprintln!("  \\if :autumn_here");
                eprintln!("  SET lock_timeout = '{COMPACT_LOCK_TIMEOUT}';");
                for table in compacted_tables(sampling, &purged_tables(&purges)) {
                    eprintln!("  VACUUM (FULL, ANALYZE) {};", qualified_ident(table));
                }
                eprintln!("  \\endif");
            }
            eprintln!("  \\endif");
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
    //
    // Every failure from here on says which databases are already scrubbed and
    // which still hold real data — including the post-commit compaction, which
    // runs after this target has committed.
    let mut committed: Vec<&str> = Vec::new();
    let warn_committed = |committed: &[&str]| {
        if !committed.is_empty() {
            eprintln!(
                "\n\u{26A0}\u{FE0F}  Already committed before this failure: {}. \
                 Those databases ARE scrubbed; every later target is untouched and still \
                 holds real data.",
                committed.join(", ")
            );
        }
    };
    // The refreshed materialized views ride along beside the purge targets: the
    // size report has to measure them, though compaction does not touch them
    // (`REFRESH` already rewrote each one's heap).
    let mut pending_compaction: Vec<PendingCompaction<'_>> = Vec::new();
    for (label, url, plan, facts, sampling) in &plans {
        let purges = purge_statements(&facts.framework_tables, &sources.config);
        let (applied, sampled) = execute(
            url,
            plan,
            &purges,
            &ViewRefresh {
                ordered: &facts.materialized_views,
                all: &facts.all_materialized_views,
                unpopulated: &facts.unpopulated_views,
            },
            sampling.as_ref(),
            label,
        )
        .inspect_err(|_| warn_committed(&committed))?;
        // This target is committed from here on, so a later failure must say so.
        committed.push(label);
        for (table, rows) in applied {
            eprintln!("  \u{2713} {table}: {rows} row(s) scrubbed.");
        }
        if let (Some(sampling), Some(sampled)) = (sampling.as_ref(), sampled) {
            report_sample_outcome(label, &sampled);
            pending_compaction.push((
                url.as_str(),
                label.as_str(),
                sampling,
                purged_tables(&purges),
                facts.all_materialized_views.clone(),
                sampled.size_before,
            ));
        }
    }

    // The artifact is captured BEFORE compaction, not after.
    //
    // Every lock is released at commit, so any window between committing and
    // dumping is one in which a concurrent write can land unscrubbed rows in a
    // target and have them captured into an artifact advertised as a scrubbed
    // subset. Compaction is per-table `VACUUM FULL` over the whole schema,
    // which on a large source is long — running it first would stretch that
    // window from moments to minutes. It also contributes nothing to the
    // artifact: `pg_dump` is logical, so a compacted table dumps to exactly the
    // same bytes as an uncompacted one. Compaction is about the live copy's
    // disk, so it can wait until the artifact is safely written.
    //
    // The remaining window — commit to the dump's own snapshot — is inherent to
    // dumping a database the scrub has already released, and is why the guide
    // says to scrub a restored copy rather than something still taking writes.
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

    // Deleting rows leaves the table files exactly as large as they were, so a
    // sample that is not compacted still needs the source's disk. This is the
    // step that makes the live subset actually laptop-sized, and it can only run
    // after the commit: VACUUM FULL rewrites each table and cannot join a
    // transaction.
    for (url, label, sampling, purged, refreshed, size_before) in pending_compaction {
        report_reclaimed_size(url, label, sampling, &purged, &refreshed, size_before);
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
        .filter(|t| !facts.partitions.contains_key(&t.name))
        .map(|t| (t.name.clone(), t.primary_key.clone()))
        .collect();
    let framework: BTreeSet<String> = facts.framework_tables.iter().cloned().collect();
    let purged: BTreeSet<String> = purge_statements(&facts.framework_tables, &sources.config)
        .into_iter()
        .map(|(table, _)| table)
        .collect();
    Ok(sample::build_plan(&sample::SampleInputs {
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
fn report_sample_sql(plan: &sample::SamplePlan, counts: Option<&BTreeMap<String, i64>>) {
    // The counts are read for EVERY target before this prints anything, so a
    // target that cannot be sized takes the whole script down before a line of
    // it exists. They are still live counts read outside the scrub's own
    // transaction, so a concurrent write can move one between this print and a
    // later real run — as it can for any dry run against a live database.
    let empty = BTreeMap::new();
    let counts = counts.unwrap_or(&empty);
    for statement in plan.setup_statements() {
        eprintln!("  {statement};");
    }
    for statement in plan.seed_statements(counts) {
        eprintln!("  {statement};");
    }
    for statement in plan.index_statements() {
        eprintln!("  {statement};");
    }
    // The walk as the loop it really is, not the statements plus a comment
    // saying to repeat them. Within a pass the statements run in list order, so
    // running them once selects only as deep as the catalog's edge order
    // happens to reach; the rows below that survive the walk but not the
    // DELETEs, and nothing catches it, because dropping a descendant leaves
    // every foreign key satisfied and every assertion below still passes.
    eprintln!("  {};", plan.walk_loop_statement().replace('\n', "\n  "));
    for statement in plan.delete_statements() {
        eprintln!("  {statement};");
    }
    for (constraint, statement) in plan.integrity_statements() {
        eprintln!(
            "  {}; -- verifies {constraint}",
            integrity_assertion(&statement)
        );
    }
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
///
/// This runs AFTER the commit, so the subset is already correct and durable —
/// compaction only decides whether the files match it. A failure here is
/// therefore a warning, not a refusal: the alternative would be to fail a run
/// whose data is already right, and to do it on the one step that waits for an
/// `ACCESS EXCLUSIVE` lock. The wait is bounded for the same reason; an idle
/// connection left open against the target would otherwise block it forever.
fn report_reclaimed_size(
    url: &str,
    label: &str,
    plan: &sample::SamplePlan,
    purged: &[String],
    refreshed: &[String],
    before: i64,
) {
    let Ok(mut conn) = probe_connection(url, label, "compact the sampled tables") else {
        warn_not_compacted(before, "could not connect to compact the sampled tables");
        return;
    };
    if let Err(e) = sql_query(format!("SET lock_timeout = '{COMPACT_LOCK_TIMEOUT}'"))
        .execute(&mut conn)
        .map_err(|e| e.to_string())
    {
        warn_not_compacted(before, &e);
        return;
    }
    // A full-copy table is never deleted from, so it has nothing to reclaim.
    // `data_size` still measures it on both sides, which keeps the ratio
    // comparable.
    for table in compacted_tables(plan, purged) {
        // Not in a transaction, and deliberately: VACUUM FULL takes an
        // exclusive lock and rewrites the table, neither of which a transaction
        // block permits.
        if let Err(e) = sql_query(format!("VACUUM (FULL, ANALYZE) {}", qualified_ident(table)))
            .execute(&mut conn)
        {
            warn_not_compacted(before, &format!("{table}: {e}"));
            return;
        }
    }
    // Measured over the same set as `size_before`: the purge targets AND the
    // materialized views this run refreshed. Compaction above deliberately
    // skips the views — REFRESH rewrote each heap already — but leaving them
    // out of the measurement is what let the report announce a laptop-sized
    // result for a database a refreshed view still dominated.
    let Ok(after) = sample::data_size(&mut conn, plan, &also_measured(purged, refreshed)) else {
        warn_not_compacted(before, "could not measure the compacted size");
        return;
    };
    eprintln!(
        "    Table size: {} \u{2192} {} ({} of the source).",
        human_bytes(before),
        human_bytes(after),
        percent_of(after, before),
    );
}

/// How long the compaction waits for the exclusive lock it needs.
const COMPACT_LOCK_TIMEOUT: &str = "30s";

/// Say that the subset is committed but its files were not rewritten, and how
/// to finish the job by hand.
fn warn_not_compacted(before: i64, detail: &str) {
    eprintln!(
        "    \u{26A0}\u{FE0F}  The subset is committed, but the tables were NOT compacted \
         ({detail}).\n    \
         Deleting rows frees no disk on its own, so they still occupy {} \u{2014} close any \
         other connection to this database and run `VACUUM (FULL, ANALYZE)` to reclaim it.",
        human_bytes(before),
    );
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
fn report_triggers(plan: &ScrubPlan, sampling: Option<&sample::SamplePlan>, facts: &DatabaseFacts) {
    // A sample DELETEs from tables the column plan never names — a pure join
    // table has no PII column at all — and an `AFTER DELETE` audit trigger
    // copies the row it removed somewhere else, which is the same hazard from
    // the other side.
    let mut triggered: Vec<&str> = plan
        .tables
        .iter()
        .map(|t| t.table.as_str())
        .chain(
            sampling
                .into_iter()
                .flat_map(sample::SamplePlan::subsetted_tables),
        )
        .filter(|t| facts.triggered_tables.contains(*t))
        .collect();
    triggered.sort_unstable();
    triggered.dedup();
    if triggered.is_empty() {
        return;
    }
    eprintln!(
        "  \u{26A0}\u{FE0F}  {} table(s) this run writes to carry user-defined triggers or \
         rules: {}.\n    \
         An audit or history trigger copies the PRE-scrub row into another table as the \
         rewrite or the sample's removals run, which can re-introduce real values; an \
         `ON DELETE ... DO INSTEAD` rule can stop the sample removing anything at all. \
         Check them, or disable them on the copy before scrubbing.",
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

/// The shared prefix of the materialized-view queries: every view in `public`
/// (`mv`), the source-to-dependent edges among them (`edge`), the closure the
/// run has to refresh (`needed`), and those edges restricted to it (`nedge`).
///
/// `edge` is derived rather than read straight out of the catalog, because a
/// materialized view can read another THROUGH an ordinary view and `pg_depend`
/// records only the hop it actually took. Matching a rewrite rule's dependency
/// directly against the set of materialized views drops both hops of `a_report
/// -> bridge_view -> z_source`, leaving two roots that then sort by name.
/// Measured on `PostgreSQL` 16.13: `a_report` refreshed FIRST, from a `z_source`
/// still holding pre-scrub rows, and refreshing `z_source` afterwards does not
/// touch it — `users` scrubbed to 2 rows with 0 original addresses, `z_source`
/// clean, and `a_report` holding all 200, under a reported success. So
/// `rel_edge` takes every rewrite-rule dependency between relations — and the
/// function hops `PostgreSQL` records, `rewrite -> pg_proc -> pg_class`, which a
/// `BEGIN ATOMIC` body produces. `fn_reach` closes that over function-to-function
/// calls first, so a tracked function calling another tracked function is
/// followed the whole way: measured, `a_report -> outer_fn() -> inner_fn() ->
/// z_source` yielded no edge when only the outer function's own relation
/// dependencies were read, and `a_report` refreshed first from a stale
/// `z_source` while keeping all 200 original addresses. `reach` walks that from each
/// materialized view through anything that is not one, and `edge` keeps the
/// pairs that land on one. A function whose body is a string literal records no
/// such dependency at all and cannot be walked; `untraceable_view_functions`
/// refuses those rather than ordering around a gap. `reach` recurses with `UNION` over a
/// finite set of pairs, so it terminates whatever the view graph looks like.
///
/// `fn_seed` is keyed by the RULE, not by the object the rule references,
/// because a rule can name a function by more than one catalog path and the
/// earlier shape could only follow one of them. Measured on `PostgreSQL`
/// 16.13, over the four indirections a rule can record:
///
/// | how the view reaches the function | the rule's `pg_depend` row |
/// | --- | --- |
/// | calls it directly                | `pg_proc` |
/// | through a cast                   | `pg_proc` (the cast function) |
/// | through an aggregate             | `pg_proc` (then `pg_proc -> pg_proc` to the sfunc) |
/// | through a user-defined operator  | `pg_operator` — and NO `pg_proc` row |
/// | through a domain's CHECK         | `pg_type` — and NO `pg_proc` row |
///
/// The first three were already followed; the last two were not, and the
/// operator case is a silent leak rather than a missed refinement. Measured:
/// `a_report` reading `z_source` only through `1 ==> 200`, whose implementation
/// is a tracked `BEGIN ATOMIC` function, produced no edge at all, so the two
/// views sorted by name, `a_report` refreshed FIRST from a stale `z_source`,
/// and the run reported `Scrub complete` with `users` at 2 rows, `z_source`
/// clean, and all 200 original addresses still in `a_report`. The domain case
/// is the same blind spot and fails closed instead — the check fires against
/// the stale view during the rewrite and aborts the transaction — but it is the
/// same missing edge, so it is seeded the same way rather than left to luck.
///
/// `oprcode` and `pg_constraint.contypid` are both older than every server this
/// command supports, so neither needs a `has_catalog_column` probe.
///
/// `needed` is every POPULATED view, plus every view one of those reads, however
/// deep. Both halves matter:
///
/// - A view created — or last refreshed — `WITH NO DATA` holds no rows at all.
///   Measured on `PostgreSQL` 16.13: `REFRESH ... WITH NO DATA` truncates the
///   heap (24 kB to 8 kB on a 200-row view) and selecting from one afterwards
///   raises `materialized view "…" has not been populated`. So it has no
///   pre-scrub copy to rebuild, and refreshing it would run the view's query and
///   materialize a heap the database deliberately does not have — on the
///   expensive query such a view usually guards, time and disk spent inside the
///   scrub's transaction, where exhausting either rolls the whole run back.
/// - An unpopulated view a populated one reads is a different case: `REFRESH`
///   on the dependent fails outright while its source is unpopulated (measured:
///   `materialized view "mv_a" has not been populated`), and an unrefreshed
///   populated view keeps its pre-scrub rows. So the source is refreshed after
///   all — and `unpopulated_views` puts it back after, which is safe because
///   emptying a source does not un-populate the dependent already rebuilt from
///   it (measured: the dependent kept `relispopulated` and all 200 rows).
///
/// `needed` recurses with `UNION`, not `UNION ALL`, so it terminates on its own
/// and needs no depth cap. The ordered walk built on top of it still has one,
/// and the unreachable-view refusal exists to catch what that cap drops.
const MV_REFRESH_CLOSURE: &str = "WITH RECURSIVE mv AS ( \
     SELECT rel.oid, rel.relispopulated FROM pg_class rel \
     JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
     WHERE rel.relkind = 'm' \
 ), dep_fn AS ( \
     SELECT d.classid, d.objid, d.refobjid AS fn \
     FROM pg_depend d \
     WHERE d.classid IN ('pg_rewrite'::regclass, 'pg_proc'::regclass) \
       AND d.refclassid = 'pg_proc'::regclass \
     UNION \
     SELECT d.classid, d.objid, o.oprcode \
     FROM pg_depend d \
     JOIN pg_operator o ON o.oid = d.refobjid \
     WHERE d.classid IN ('pg_rewrite'::regclass, 'pg_proc'::regclass) \
       AND d.refclassid = 'pg_operator'::regclass AND o.oprcode <> 0 \
     UNION \
     SELECT d.classid, d.objid, cd.refobjid \
     FROM pg_depend d \
     JOIN pg_constraint con ON con.contypid = d.refobjid \
     JOIN pg_depend cd ON cd.classid = 'pg_constraint'::regclass \
       AND cd.objid = con.oid AND cd.refclassid = 'pg_proc'::regclass \
     WHERE d.classid IN ('pg_rewrite'::regclass, 'pg_proc'::regclass) \
       AND d.refclassid = 'pg_type'::regclass \
 ), fn_reach AS ( \
     SELECT df.objid AS rule, df.fn FROM dep_fn df \
     WHERE df.classid = 'pg_rewrite'::regclass \
     UNION \
     SELECT r.rule, df.fn FROM fn_reach r \
     JOIN dep_fn df ON df.classid = 'pg_proc'::regclass AND df.objid = r.fn \
 ), rel_edge AS ( \
     SELECT DISTINCT r.ev_class AS dependent, d.refobjid AS source \
     FROM pg_depend d \
     JOIN pg_rewrite r ON r.oid = d.objid \
     WHERE d.classid = 'pg_rewrite'::regclass \
       AND d.refclassid = 'pg_class'::regclass \
       AND d.refobjid <> r.ev_class \
     UNION \
     SELECT DISTINCT r.ev_class AS dependent, fd.refobjid AS source \
     FROM pg_rewrite r \
     JOIN fn_reach fr ON fr.rule = r.oid \
     JOIN pg_depend fd ON fd.classid = 'pg_proc'::regclass \
       AND fd.objid = fr.fn AND fd.refclassid = 'pg_class'::regclass \
     WHERE fd.refobjid <> r.ev_class \
 ), reach AS ( \
     SELECT m.oid AS dependent, e.source FROM mv m \
     JOIN rel_edge e ON e.dependent = m.oid \
     UNION \
     SELECT h.dependent, e.source FROM reach h \
     JOIN rel_edge e ON e.dependent = h.source \
     WHERE h.source NOT IN (SELECT oid FROM mv) \
 ), edge AS ( \
     SELECT DISTINCT dependent, source FROM reach \
     WHERE source IN (SELECT oid FROM mv) \
 ), needed AS ( \
     SELECT oid FROM mv WHERE relispopulated \
     UNION \
     SELECT e.source FROM edge e JOIN needed n ON n.oid = e.dependent \
 ), nedge AS ( \
     SELECT dependent, source FROM edge WHERE dependent IN (SELECT oid FROM needed) \
 )";

/// Tables a statement naming them can fire a user-defined rewrite rule on.
///
/// Unlike triggers this needs no walk up `pg_inherits`: measured on
/// `PostgreSQL` 16, a rule on a leaf partition or an inheritance child does not
/// fire for a statement naming the parent, because rewriting happens against the
/// relation the query names. `_RETURN` is the `SELECT` rule every view carries,
/// and `ev_enabled` follows the same `O`/`A` rule as `tgenabled`.
///
/// `extra` is an additional `pg_rewrite` predicate, e.g. restricting the event.
fn rules_reaching(extra: &str) -> String {
    format!(
        "SELECT DISTINCT rel.relname AS name FROM pg_rewrite r \
         JOIN pg_class rel ON rel.oid = r.ev_class \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE r.rulename <> '_RETURN' AND r.ev_enabled IN ('O', 'A') {extra}"
    )
}

/// Tables a statement naming them can fire a user-defined trigger on.
///
/// Two catalog rules decide this, and both have bitten this command:
///
/// - **`tgenabled`.** A trigger disabled with `ALTER TABLE ... DISABLE TRIGGER`
///   stays in the catalog and cannot fire — and disabling it is exactly what the
///   trigger warning and the emptying refusal tell operators to do, so counting
///   it would make the documented remedy do nothing. `O` fires for an ordinary
///   session and `A` fires always; `D` never fires, and `R` only under
///   `session_replication_role = replica`, which a scrub does not set.
///
/// - **Row versus statement level, walking up `pg_inherits`.** A statement
///   naming a partitioned parent (or a legacy `INHERITS` parent) fires
///   **row-level** triggers declared on the children, so a row trigger anywhere
///   below a table has to mark that table. It does **not** fire their
///   statement-level triggers — measured on `PostgreSQL` 16, for both inheritance
///   flavours — so those mark only the table they are declared on. Propagating
///   them would refuse a run over a trigger that cannot execute.
///
/// Descendants need no walk: a trigger on a partitioned parent is cloned onto
/// its partitions, and the parent is already named.
///
/// `extra` is an additional `pg_trigger` predicate, e.g. restricting the event.
fn triggers_reaching(extra: &str) -> String {
    format!(
        "WITH RECURSIVE fires AS ( \
           SELECT t.tgrelid AS oid, (t.tgtype & 1) <> 0 AS by_row FROM pg_trigger t \
           WHERE NOT t.tgisinternal AND t.tgenabled IN ('O', 'A') {extra}\
         ), ancestry AS ( \
           SELECT oid, by_row FROM fires \
           UNION \
           SELECT i.inhparent, true FROM pg_inherits i \
           JOIN ancestry a ON a.oid = i.inhrelid AND a.by_row \
         ) \
         SELECT DISTINCT rel.relname AS name FROM ancestry a \
         JOIN pg_class rel ON rel.oid = a.oid \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public'"
    )
}

/// Where a connection actually landed, as the server itself reports it.
///
/// Both `None` over a Unix socket, where Postgres has no address or port to
/// report. Held as strings because they only ever go back into SQL as literals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerEndpoint {
    /// `current_database()`, asked of the target itself rather than parsed out
    /// of its URL. libpq defaults an omitted database name to the user name,
    /// which defaults to the OS user — rules this command would have to
    /// reimplement to guess, and did not: a URI with no database silently
    /// emitted no guard at all, leaving 19 destructive statements unprotected.
    /// The connection already knows the answer.
    pub database: String,
    /// `inet_server_addr()`, e.g. `10.0.0.2/32`. `None` over a Unix socket.
    pub address: Option<String>,
    /// `inet_server_port()`. `None` over a Unix socket.
    pub port: Option<String>,
    /// The cluster's `system_identifier`, from `pg_control_system()`.
    ///
    /// Address and port are `None` for EVERY Unix-socket connection, so two
    /// socket clusters holding the same database name are indistinguishable by
    /// them — measured, both reported `<null>/<null>` while their identifiers
    /// differed. This one is generated at initdb, survives the socket path, and
    /// is readable by an ordinary `LOGIN` role (verified against a non-superuser
    /// on `PostgreSQL` 16.13).
    ///
    /// It does NOT distinguish a cluster from a physical copy of itself: a
    /// replica, or a promoted staging clone, carries the identifier of the
    /// cluster it was cloned from. Hence the three fields below.
    pub system_identifier: Option<String>,
    /// `current_setting('port')` — the port the SERVER is configured on.
    ///
    /// Not the same question as `inet_server_port()`, which is the port this
    /// client reached and is NULL over a Unix socket. This one answers over a
    /// socket too (verified: 5433 and 5434 read back from two socket-only
    /// clusters), and unlike `data_directory` it is readable by an ordinary
    /// `LOGIN` role, so it discriminates even where the scrub role cannot
    /// examine restricted settings.
    pub server_port: Option<String>,
    /// `data_directory`, or `None` where the role may not read it.
    ///
    /// The value a physical clone cannot share with its origin while both run on
    /// the same machine: two postmasters cannot hold one data directory. It is
    /// restricted to `pg_read_all_settings`, so it is read out of `pg_settings`
    /// rather than with `current_setting` — that view omits the row entirely for
    /// a role without the privilege, and the scalar subquery yields NULL instead
    /// of raising `permission denied to examine ...`, which `current_setting`
    /// does raise, `missing_ok` or not (that flag covers unknown parameters, not
    /// forbidden ones). Verified both ways on `PostgreSQL` 16.13.
    pub data_directory: Option<String>,
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
    /// Tables that are partitions, each mapped to the top-level table it
    /// belongs to. Their rows are rewritten through that parent, so planning
    /// them again double-updates — and the mapping also says whose role decides
    /// whether a key declared on a leaf can be ignored.
    pub partitions: BTreeMap<String, String>,
    /// Tables with row-level security enabled. A non-bypassing role silently
    /// updates only the rows its policies expose — a fail-open a scrub cannot
    /// tolerate.
    pub rls_tables: BTreeSet<String>,
    /// Legacy `INHERITS` children as `child (inherits parent)`, sorted. Empty
    /// for a declaratively partitioned schema, which the sample does model.
    pub legacy_inheritance: Vec<String>,
    /// Tables carrying user-defined triggers, which can copy pre-scrub values
    /// into another table mid-scrub.
    pub triggered_tables: BTreeSet<String>,
    /// The address and port the connection actually reached, as the server
    /// reports them. Both `None` over a Unix socket. The dry run's target guard
    /// compares these so a retained connection to a different host holding the
    /// same database name cannot pass for the intended one.
    pub endpoint: ServerEndpoint,
    /// The connection's `session_replication_role`. Anything but `origin`
    /// inverts which triggers and rules fire, so the hazard checks below would
    /// be answering for a session the run is not in.
    pub replication_role: String,
    /// The subset of those whose triggers fire on `DELETE`. The run's emptying
    /// pass is its last write, so a trigger here fires after every rewrite and
    /// can put pre-scrub values into a table already scrubbed.
    pub delete_triggered_tables: BTreeSet<String>,
    /// The materialized views the run refreshes, in dependency order (sources
    /// before dependents), so refreshing them in sequence never re-derives from
    /// stale data. Drawn from the `MV_REFRESH_CLOSURE` set, not from every view:
    /// one left `WITH NO DATA` that nothing populated reads is not refreshed.
    pub materialized_views: Vec<String>,
    /// That same closure, enumerated flat.
    ///
    /// The ordered list above is built by a recursive walk that stops at a depth
    /// cap, so a view past it is absent from that list — and the size report
    /// measures over this set. Measuring the ordered list instead let the report
    /// announce a laptop-sized result while an unmeasured view still held the
    /// disk. A view that is not refreshed still occupies its heap, so honest
    /// measurement enumerates them all; refresh ORDER is a separate question and
    /// keeps its own list. It has to be drawn from the same closure the ordered
    /// list is, though: the unreachable-view refusal compares the two, so a view
    /// present in only one of them reads as one the walk could not reach.
    pub all_materialized_views: Vec<String>,
    /// EVERY materialized view in `public` that was unpopulated when the run
    /// probed it — not only the closure members among them.
    ///
    /// Each gets a `REFRESH ... WITH NO DATA` as the transaction's last view
    /// statement, which does two jobs. A closure member was populated only so a
    /// dependent could be rebuilt from it, and this puts it back. A view outside
    /// the closure was skipped, and this closes the race that skipping opened:
    /// probing runs on its own connection before the apply transaction, the
    /// locks that transaction takes are `SHARE ROW EXCLUSIVE` on base tables and
    /// none at all on a view, and `REFRESH MATERIALIZED VIEW` needs only
    /// `ACCESS SHARE` on the tables it reads. Measured on `PostgreSQL` 16.13: a
    /// concurrent session refreshed a skipped view mid-scrub without waiting,
    /// and the run committed with `users` scrubbed to 2 rows and the view
    /// holding all 200 original addresses. Emptying it under the `ACCESS
    /// EXCLUSIVE` this statement takes serialises that session behind the
    /// commit, after which it can only re-derive from scrubbed rows.
    ///
    /// It costs nothing that skipping saved: `REFRESH ... WITH NO DATA` does not
    /// run the view's query. Measured on a view defined as `SELECT 1/0`, it
    /// succeeds where a plain `REFRESH` raises `division by zero`.
    pub unpopulated_views: Vec<String>,
    /// `view via function` for every materialized view whose definition calls a
    /// non-system function that records no relation dependency of its own.
    ///
    /// Opacity is read from `prosqlbody`, which holds the parsed body of a
    /// `BEGIN ATOMIC` function and is NULL for everything else — a string
    /// literal, `plpgsql`, or C. That is the property itself, where "records no
    /// relation dependency" was only a proxy for it, and a wrong one: a tracked
    /// wrapper that merely calls another tracked function records no relation of
    /// its own, and was refused although `fn_reach` can follow it all the way to
    /// the table. Measured — `view -> wrap_fn() -> inner_fn() -> z_source`, every
    /// body `BEGIN ATOMIC`, refused as `via wrap_fn`.
    ///
    /// `prosqlbody` is `PostgreSQL` 14, so it is probed for with
    /// `has_catalog_column` like every other version-specific catalog fact
    /// here. Reading it unguarded broke every scrub on an older server with
    /// `column p.prosqlbody does not exist` — the testcontainer default is
    /// `postgres:11-alpine`. On a server without it the answer is "every
    /// function reached from a view", not "none": before 14 no SQL body is
    /// parsed into the catalog, so none of them can be followed.
    ///
    /// Read over every relation REACHABLE from a materialized view, not only the
    /// views themselves: the walk crosses ordinary views, so a function called by
    /// one of those is just as invisible and just as able to reorder the
    /// refresh. And over every function in the `fn_reach` closure, not only the
    /// one a rule names directly — a tracked function can call an opaque one,
    /// and the outer function's own relation dependency would otherwise vouch
    /// for a body nothing can see into.
    pub untraceable_view_functions: Vec<String>,
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
    pub foreign_keys: Vec<sample::ForeignKeyConstraint>,
}

/// A single `n` count column, for the promised-empty verification.
#[derive(diesel::QueryableByName)]
struct RowCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
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
    /// The key's columns in key order, as an array rather than a joined
    /// string: a Postgres identifier may contain any character, so no separator
    /// is safe to split on afterwards.
    #[diesel(sql_type = diesel::sql_types::Array<diesel::sql_types::Text>)]
    child_cols: Vec<String>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    parent: String,
    #[diesel(sql_type = diesel::sql_types::Array<diesel::sql_types::Text>)]
    parent_cols: Vec<String>,
    /// True for `MATCH FULL`, whose composite NULL rule differs from the
    /// default `MATCH SIMPLE`.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    match_full: bool,
    /// True when Postgres cloned this constraint onto a partition from its
    /// partitioned parent. The parent's own constraint covers the same rows, so
    /// a clone must not be walked or verified a second time — while a key
    /// declared directly on a partition is NOT a clone and must not be dropped.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    cloned: bool,
}

/// A `(tbl, col)` probe read as a map rather than a set of pairs.
fn pair_rows(sql: &str, conn: &mut PgConnection) -> Result<BTreeMap<String, String>, ScrubError> {
    let rows: Vec<PairRow> = sql_query(sql)
        .load(conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?;
    Ok(rows.into_iter().map(|r| (r.tbl, r.col)).collect())
}

fn pair_set(rows: Vec<PairRow>) -> BTreeSet<(String, String)> {
    rows.into_iter().map(|r| (r.tbl, r.col)).collect()
}

/// Open a connection for a probe, mapping failure to a credential-safe error.
fn probe_connection(url: &str, label: &str, what: &str) -> Result<PgConnection, ScrubError> {
    let mut conn = PgConnection::establish(url).map_err(|_| ScrubError::Introspect {
        label: label.to_owned(),
        detail: format!(
            "could not connect to database {:?} to {what}",
            parsed_db_name(url)
        ),
    })?;
    // Pinned here, not only inside the scrub's transaction: `pg_catalog` is
    // searched implicitly ONLY when the path does not name it, so a target whose
    // `search_path` is `public, pg_catalog` lets an application object shadow a
    // built-in — and every question this command asks the catalog is asked over
    // THIS connection, before any transaction opens. A `public.current_setting`
    // returning `'origin'` was enough to walk straight past the replica-role
    // refusal.
    conn.batch_execute("SET search_path = pg_catalog, public")
        .map_err(|e| ScrubError::Introspect {
            label: label.to_owned(),
            detail: format!("could not pin the search path to {what}: {e}"),
        })?;
    Ok(conn)
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
    let cloned = if has_catalog_column(&mut conn, "pg_constraint", "conparentid")? {
        "c.conparentid <> 0"
    } else {
        "false"
    };
    let constraint_rows: Vec<ConstraintRow> = sql_query(format!(
        "SELECT c.conname AS name, rel.relname AS child, frel.relname AS parent, \
         (SELECT array_agg(att.attname::text ORDER BY k.ord) \
          FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
          JOIN pg_attribute att ON att.attrelid = c.conrelid AND att.attnum = k.attnum) \
         AS child_cols, \
         (SELECT array_agg(att.attname::text ORDER BY k.ord) \
          FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) \
          JOIN pg_attribute att ON att.attrelid = c.confrelid AND att.attnum = k.attnum) \
         AS parent_cols, \
         {cloned} AS cloned, \
         c.confmatchtype = 'f' AS match_full \
         FROM pg_constraint c \
         JOIN pg_class rel ON rel.oid = c.conrelid \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         JOIN pg_class frel ON frel.oid = c.confrelid \
         JOIN pg_namespace fns ON fns.oid = frel.relnamespace AND fns.nspname = 'public' \
         WHERE c.contype = 'f'"
    ))
    .load(&mut conn)
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    let foreign_keys: Vec<sample::ForeignKeyConstraint> = constraint_rows
        .into_iter()
        .filter(|row| !row.cloned)
        .map(|row| sample::ForeignKeyConstraint {
            name: row.name,
            child_table: row.child,
            child_columns: row.child_cols,
            parent_table: row.parent,
            parent_columns: row.parent_cols,
            match_full: row.match_full,
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

    // Each partition mapped to the top-level table it belongs to, not just
    // named: a key declared on a leaf is unrepresentable in general, but
    // harmless when the run empties that leaf's whole tree, and only the
    // mapping can tell the two apart.
    let partitions: BTreeMap<String, String> =
        if has_catalog_column(&mut conn, "pg_class", "relispartition")? {
            pair_rows(
                "WITH RECURSIVE up AS ( \
               SELECT rel.oid, rel.oid AS leaf FROM pg_class rel \
               JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
               WHERE rel.relispartition AND rel.relkind IN ('r', 'p', 'f') \
               UNION ALL \
               SELECT i.inhparent, up.leaf FROM pg_inherits i JOIN up ON up.oid = i.inhrelid \
             ) \
             SELECT DISTINCT leafrel.relname AS tbl, rootrel.relname AS col \
             FROM up \
             JOIN pg_class leafrel ON leafrel.oid = up.leaf \
             JOIN pg_class rootrel ON rootrel.oid = up.oid \
             WHERE NOT rootrel.relispartition",
                &mut conn,
            )?
        } else {
            BTreeMap::new()
        };

    // A declarative partition is also a `pg_inherits` child, so the partition
    // case is excluded explicitly — those the sample models through the parent
    // on purpose. What is left is legacy `INHERITS`, which it cannot.
    let legacy_inheritance: Vec<String> = {
        let partition_filter = if has_catalog_column(&mut conn, "pg_class", "relispartition")? {
            "AND NOT child.relispartition"
        } else {
            ""
        };
        let rows: Vec<NameRow> = sql_query(format!(
            "SELECT child.relname || ' (inherits ' || parent.relname || ')' AS name \
             FROM pg_inherits i \
             JOIN pg_class child ON child.oid = i.inhrelid \
             JOIN pg_namespace cns ON cns.oid = child.relnamespace AND cns.nspname = 'public' \
             JOIN pg_class parent ON parent.oid = i.inhparent \
             JOIN pg_namespace pns ON pns.oid = parent.relnamespace AND pns.nspname = 'public' \
             WHERE child.relkind IN ('r', 'p', 'f') {partition_filter}"
        ))
        .load(&mut conn)
        .map_err(|e| ScrubError::Sql(e.to_string()))?;
        let mut names: Vec<String> = rows.into_iter().map(|r| r.name).collect();
        names.sort();
        names
    };

    let rls_tables = names(
        "SELECT rel.relname AS name FROM pg_class rel \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE rel.relrowsecurity",
        &mut conn,
    )?
    .into_iter()
    .collect();

    // Two questions, one shape: which tables carry a user-defined trigger at
    // all (the warning), and which carry one that fires on `DELETE` (the
    // refusal). `triggers_reaching` builds both, so they cannot drift on the
    // catalog rules they share.
    // Triggers and rules alike: both run code on a write, and this warning exists
    // to say "something here can copy a row somewhere this run does not look".
    // A `DO INSTEAD` rule can also stop the sample's `DELETE` removing anything.
    let mut triggered_tables: BTreeSet<String> = names(&triggers_reaching(""), &mut conn)?
        .into_iter()
        .collect();
    triggered_tables.extend(names(&rules_reaching(""), &mut conn)?);
    // `tgtype` bit 3 is the DELETE event. These are the ones that matter for
    // the run's last write, so the refusal can name exactly the tables that
    // carry one rather than every table carrying any trigger at all.
    // Read rather than pinned: `session_replication_role` needs privileges an
    // ordinary scrub role does not have — measured, a non-superuser cannot set
    // it even to its own default — so `SET LOCAL` would break every unprivileged
    // run to close a hazard only a privileged one can create.
    let replication_role = names(
        "SELECT pg_catalog.current_setting('session_replication_role') AS name",
        &mut conn,
    )?
    .into_iter()
    .next()
    .unwrap_or_else(|| "origin".to_owned());

    // The endpoint this connection actually reached, for the dry run's target
    // guard. Comparing the database name alone is not enough: a sharded fleet
    // runs the SAME database name on different hosts — the topology in
    // docs/guide/sharding.md names all three `app` — so a retained connection to
    // one shard answers `current_database()` exactly as the intended one would.
    // Measured: two clusters both holding `app`, a failed `\connect`, and the
    // shard1 block scrubbed shard0 (200 -> 100 users) past a name-only guard.
    //
    // Read from the live connection rather than parsed out of the URL, because a
    // hostname is not what the server reports back — `inet_server_addr()` is an
    // address, and resolving one at print time is not this command's job. NULL
    // over a Unix socket, which the guard compares as NULL rather than papering
    // over.
    let (address, port) = pair_rows(
        "SELECT coalesce(pg_catalog.inet_server_addr()::text, '') AS tbl, \
         coalesce(pg_catalog.inet_server_port()::text, '') AS col",
        &mut conn,
    )?
    .into_iter()
    .next()
    .unwrap_or_default();
    // A value the target may not be able to answer for. `coalesce` to the empty
    // string rather than letting a NULL fail to deserialize, so "the role may
    // not read this" and "this connection has no such value" arrive the same
    // way: as `None`, which the guard compares as NULL.
    let optional = |query: &str, conn: &mut PgConnection| -> Option<String> {
        names(query, conn)
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .filter(|value| !value.is_empty())
    };
    let setting_of = |parameter: &str| {
        format!(
            "SELECT coalesce((SELECT setting FROM pg_catalog.pg_settings \
             WHERE name = {}), '') AS name",
            quote_literal(parameter),
        )
    };
    let endpoint = ServerEndpoint {
        database: names("SELECT pg_catalog.current_database() AS name", &mut conn)?
            .into_iter()
            .next()
            .unwrap_or_default(),
        address: (!address.is_empty()).then_some(address),
        port: (!port.is_empty()).then_some(port),
        system_identifier: optional(
            "SELECT system_identifier::text AS name FROM pg_catalog.pg_control_system()",
            &mut conn,
        ),
        server_port: optional(
            "SELECT pg_catalog.current_setting('port') AS name",
            &mut conn,
        ),
        // Through `pg_settings`, not `current_setting`: the parameter is
        // restricted to `pg_read_all_settings`, and `current_setting` raises
        // `permission denied to examine ...` for a role without it — which
        // would abort the introspection an ordinary scrub role has to complete.
        // The view simply omits the row, so the scalar subquery answers NULL and
        // the guard compares NULL to NULL, losing the discriminator rather than
        // the run. Verified both ways on PostgreSQL 16.13: as a non-superuser,
        // `current_setting` errored and this returned empty.
        data_directory: optional(&setting_of("data_directory"), &mut conn),
    };

    // Rules are the other way a `DELETE` runs code the plan never saw. Unlike a
    // trigger they fire only on the relation the statement NAMES — measured on
    // PostgreSQL 16: a rule on a leaf partition or an inheritance child does not
    // fire for `DELETE FROM parent` — so this needs no walk up `pg_inherits`.
    // `ev_type` `4` is DELETE; `_RETURN` is the SELECT rule every view carries.
    let mut delete_triggered_tables: BTreeSet<String> =
        names(&triggers_reaching("AND (t.tgtype & 8) <> 0 "), &mut conn)?
            .into_iter()
            .collect();
    delete_triggered_tables.extend(names(&rules_reaching("AND r.ev_type = '4' "), &mut conn)?);

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
    // refreshed after it, or it re-derives from pre-scrub data. Restricted to
    // the closure `MV_REFRESH_CLOSURE` defines — every populated view, and the
    // views those read, however deep.
    let materialized_views = names(
        &format!(
            "{MV_REFRESH_CLOSURE}, depth AS ( \
                 SELECT oid, 0 AS lvl FROM needed \
                 WHERE oid NOT IN (SELECT dependent FROM nedge) \
                 UNION ALL \
                 SELECT e.dependent, d.lvl + 1 FROM nedge e JOIN depth d ON d.oid = e.source \
                 WHERE d.lvl < 32 \
             ) \
             SELECT rel.relname AS name \
             FROM (SELECT oid, max(lvl) AS lvl FROM depth GROUP BY oid) o \
             JOIN pg_class rel ON rel.oid = o.oid ORDER BY o.lvl, rel.relname"
        ),
        &mut conn,
    )?;

    // The same closure, flat and uncapped: the size report measures over this,
    // and a view the walk's depth cap dropped still occupies its heap. It has to
    // be the same set the ordered list is drawn from, because the
    // unreachable-view refusal compares the two — a view missing from only one
    // of them would be read as a view the walk could not reach.
    let all_materialized_views = names(
        &format!(
            "{MV_REFRESH_CLOSURE} \
             SELECT rel.relname AS name FROM needed n \
             JOIN pg_class rel ON rel.oid = n.oid ORDER BY rel.relname"
        ),
        &mut conn,
    )?;

    // The views whose order cannot be derived at all. Deliberately not part of
    // the closure query: this is a refusal, not an edge.
    // Over EVERY relation reachable from a materialized view, not just the views
    // themselves: `reach` walks through ordinary views, so an untracked function
    // called by one of THOSE is exactly as invisible. Measured on
    // `a_report -> bridge_view -> bridge_fn() -> z_source`, where only
    // `bridge_view`'s rule names the function — the earlier `relkind = 'm'`
    // predicate skipped it, `a_report` refreshed first from a stale `z_source`,
    // and the run reported success with all 200 original addresses still in
    // `a_report`.
    //
    // `pg_proc.prosqlbody` holds the PARSED body of a `BEGIN ATOMIC` function
    // and is NULL for a string literal, `plpgsql` or C — the property itself
    // rather than a shadow of it. It arrived in PostgreSQL 14 along with
    // `BEGIN ATOMIC`, so it is probed for like every other version-specific
    // catalog fact in this file. On an older server the answer is not
    // "unknown", it is "every one of them": before 14 no SQL body is parsed
    // into the catalog at all, so no function reached from a view can be
    // followed, and each one is untraceable by construction.
    let opaque_body = if has_catalog_column(&mut conn, "pg_proc", "prosqlbody")? {
        "p.prosqlbody IS NULL"
    } else {
        "true"
    };
    let untraceable_view_functions = names(
        &format!(
            "{MV_REFRESH_CLOSURE}, node AS ( \
                 SELECT oid AS root, oid AS relation FROM mv \
                 UNION \
                 SELECT dependent, source FROM reach \
             ) \
             SELECT DISTINCT root.relname || ' via ' || p.proname AS name \
             FROM node n \
             JOIN pg_class root ON root.oid = n.root \
             JOIN pg_rewrite rw ON rw.ev_class = n.relation \
             JOIN fn_reach fr ON fr.rule = rw.oid \
             JOIN pg_proc p ON p.oid = fr.fn \
             JOIN pg_namespace pn ON pn.oid = p.pronamespace \
             WHERE pn.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND {opaque_body} \
             ORDER BY name"
        ),
        &mut conn,
    )?;

    // EVERY unpopulated view, not only the closure members among them, because
    // this list closes a race as well as restoring a state. See
    // `DatabaseFacts::unpopulated_views`.
    let unpopulated_views = names(
        "SELECT rel.relname AS name FROM pg_class rel \
         JOIN pg_namespace ns ON ns.oid = rel.relnamespace AND ns.nspname = 'public' \
         WHERE rel.relkind = 'm' AND NOT rel.relispopulated ORDER BY rel.relname",
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
        legacy_inheritance,
        triggered_tables,
        endpoint,
        replication_role,
        delete_triggered_tables,
        materialized_views,
        all_materialized_views,
        unpopulated_views,
        untraceable_view_functions,
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
/// The three emptying passes, in the order `execute` runs them.
///
/// `execute` and `--dry-run` both read this, because they drifted apart twice:
/// the dry run kept printing a purge order the executor no longer used, and then
/// missed the final pass entirely. One definition means the printed SQL is the
/// executed SQL by construction rather than by review.
///
/// - **before** — purges that are safe first, so the sample's deletes can remove
///   the rows they point at;
/// - **`after_sample`** — purges the sample's own emptied rows reference, which
///   have to wait for it;
/// - **`final_pass`** — every purge again, plus the `never_include` tables, run
///   after all writes. This is the pass that makes "emptied" true: a trigger on
///   a scrubbed table can insert the original PII into either kind of table
///   while the rewrites run.
fn emptying_phases<'a>(
    purges: &'a [(String, String)],
    deferred: &BTreeSet<String>,
    sampling: Option<&'a sample::SamplePlan>,
) -> EmptyingPhases<'a> {
    let owned = |t: &'a (String, String)| (t.0.as_str(), t.1.clone());
    let mut final_pass: Vec<(&str, String)> = purges.iter().map(owned).collect();
    final_pass.extend(
        sampling
            .map(sample::SamplePlan::emptied_tables)
            .unwrap_or_default(),
    );
    EmptyingPhases {
        before: purges
            .iter()
            .filter(|(t, _)| !deferred.contains(t))
            .map(owned)
            .collect(),
        after_sample: purges
            .iter()
            .filter(|(t, _)| deferred.contains(t))
            .map(owned)
            .collect(),
        final_pass,
    }
}

/// The tables `[framework] purge` empties, as owned names.
///
/// They sit outside the classified universe the sample plans over, so every
/// place that reasons about "what this run empties" has to add them back.
fn purged_tables(purges: &[(String, String)]) -> Vec<String> {
    purges.iter().map(|(table, _)| table.clone()).collect()
}

/// One target's deferred compaction: its URL and label, the sample plan, the
/// `[framework] purge` targets, the materialized views the run refreshed, and
/// the size measured before any of it ran.
///
/// Compaction can only happen after the commit — `VACUUM FULL` cannot join a
/// transaction — so every target's inputs are carried out of the loop that
/// scrubbed it.
type PendingCompaction<'a> = (
    &'a str,
    &'a str,
    &'a sample::SamplePlan,
    Vec<String>,
    Vec<String>,
    i64,
);

/// Every relation the size report measures beside the sample's own tables.
///
/// The `[framework] purge` targets, because `DELETE` frees no file space so an
/// emptied buffer keeps its whole file until compaction rewrites it — and the
/// materialized views the run refreshes, because a view is rebuilt from
/// whatever survives the sample and one over reference data does not shrink at
/// all. Both sides of the ratio use this same set, or the ratio compares
/// different things.
fn also_measured(purged: &[String], refreshed: &[String]) -> Vec<String> {
    let mut names: Vec<String> = purged.iter().chain(refreshed).cloned().collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Every table the run rewrites with `VACUUM (FULL, ANALYZE)` after the commit.
///
/// Deleting rows frees no file space, so a table this run emptied still costs
/// the source's disk until it is rewritten — and the size report is measured
/// over this same set, so a table missing here is one the run reports as
/// reclaimed while its file is untouched. `[framework] purge` targets belong in
/// it for exactly that reason: an emptied offline-sync buffer is often the
/// largest thing the run removes.
///
/// Shared with the dry run, so the printed compaction is the executed one.
fn compacted_tables<'a>(plan: &'a sample::SamplePlan, purged: &'a [String]) -> Vec<&'a str> {
    let mut tables: Vec<&str> = plan
        .subsetted_tables()
        .into_iter()
        .chain(purged.iter().map(String::as_str))
        .collect();
    tables.sort_unstable();
    tables.dedup();
    tables
}

/// The `psql` meta-command that moves the session to one target.
///
/// A bare `\connect dbname` reuses the host, port and user of the existing
/// connection, so on a fleet whose shards are the same database name on
/// different servers it silently keeps running against the first one — the
/// boundary would look like it switched and would not have.
///
/// The whole connection string is therefore passed as a conninfo, rather than
/// rebuilt from parts. Reconstruction is where this goes wrong: a query
/// parameter overrides the authority it duplicates (`?host=` beats the URL's
/// own host, as `pg::sanitize_db_url` exists to normalise), so an endpoint
/// assembled from `Url::host_str` and friends can name a database the run never
/// touches — and this string is the header on destructive SQL.
///
/// Only the password is removed. If it cannot be removed with certainty the
/// line is not emitted at all: a comment naming the target is a lesser failure
/// than a printed credential.
fn psql_connect(label: &str, url: &str) -> Vec<String> {
    password_free_conninfo(url).map_or_else(
        || {
            // Unreachable: the dry run refuses up front when any target's
            // boundary cannot be printed. Kept, and made to STOP rather than to
            // advise, because the failure mode if that guard is ever bypassed is
            // a destructive block running against the previous target. A comment
            // does not stop a paste; `\quit` does.
            vec![
                format!(
                    "\\echo '{label}: no printable connection string (keyword form) \
                     -- connect to this target yourself, then delete the \\quit below'"
                ),
                "\\quit".to_owned(),
            ]
        },
        |conninfo| {
            vec![format!(
                "\\connect -reuse-previous=off {}",
                quote_psql_arg(&conninfo)
            )]
        },
    )
}

/// The host and port a conninfo STATES, or `None` where it leaves one to libpq.
///
/// A query pair wins over the authority it duplicates — `?host=` beats the URL's
/// own host, which is why `pg::sanitize_db_url` exists — so both are read and
/// the query is preferred, the same precedence libpq applies.
///
/// Only stated components are returned. A default is deliberately not guessed:
/// `PGPORT` can differ between the machine that planned the run and the one that
/// pastes the script, so asserting `5432` for an omitted port would refuse a
/// CORRECT paste. An omitted component also cannot be what distinguishes two
/// targets — libpq resolves it identically for both, so two conninfos that
/// differ only there are the same endpoint written twice.
/// Whether the conninfo states `hostaddr`, in the URI query or as a keyword.
///
/// `hostaddr` selects the endpoint independently of `host`, and psql reports no
/// variable for it, so a printed reconnect proof cannot pin it. See
/// `ScrubError::UnprintableHostaddrTarget`.
fn states_hostaddr(conninfo: &str) -> bool {
    let Ok(parsed) = url::Url::parse(conninfo) else {
        return false;
    };
    crate::pg::parse_raw_query_pairs(parsed.query().unwrap_or(""))
        .into_iter()
        .any(|(key, value)| key == "hostaddr" && !value.is_empty())
}

/// Whether the conninfo names more than one endpoint.
///
/// libpq picks a member per connection, so the run's sizing connection and the
/// session that pastes the script need not reach the same database. See
/// `ScrubError::UnprintableMultiHostTarget`.
fn states_multiple_endpoints(conninfo: &str) -> bool {
    let (host, port) = stated_host_and_port(conninfo);
    let parts =
        |value: Option<String>| value.map_or(0, |v| v.split(',').filter(|p| !p.is_empty()).count());
    parts(host) > 1 || parts(port) > 1
}

fn stated_host_and_port(conninfo: &str) -> (Option<String>, Option<String>) {
    let Ok(parsed) = url::Url::parse(conninfo) else {
        return (None, None);
    };
    let mut host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .map(str::to_owned);
    let mut port = parsed.port().map(|p| p.to_string());
    for (key, value) in crate::pg::parse_raw_query_pairs(parsed.query().unwrap_or("")) {
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "host" => host = Some(value),
            "port" => port = Some(value),
            _ => {}
        }
    }
    (host, port)
}

/// The psql-level proof that `\connect` actually moved the session, read from
/// psql's OWN connection variables rather than from anything the server reports.
///
/// The in-transaction guard below compares what the server says about itself,
/// and that is not the connection's identity. `inet_server_addr()` and
/// `inet_server_port()` are the address and port the SERVER accepted on, not the
/// endpoint the operator configured — measured through a forwarder, a session
/// connected to `127.0.0.1:15433` reports `127.0.0.1:5433`. So two servers
/// behind different forwards, or in separate container networks with the same
/// private address, report the same pair; add a physical clone's inherited
/// `system_identifier` and a container-local `data_directory` both answer
/// `/var/lib/postgresql/data` to, and every value that guard compares can match
/// on two different databases. (Two colliding networks are not something this
/// container can build, and I am not claiming to have run that half.)
///
/// psql's `:HOST`, `:PORT` and `:DBNAME` are connection-specific in the way the
/// server's answers are not: they describe the conninfo psql resolved, and a
/// failed `\connect` leaves them describing the PREVIOUS one. Measured on psql
/// 16.13 in an interactive session, where `ON_ERROR_STOP` is ignored and a
/// failed `\connect` keeps the old connection: before, `HOST=127.0.0.1
/// PORT=15433 DBNAME=postgres`; after a `\connect` to port 25433 failed,
/// unchanged — and `inet_server_port()` still answered for the old server.
/// A failed `\connect` sets no error flag to fence on instead: measured,
/// `:ERROR` is still unset afterwards and `LAST_ERROR_MESSAGE` is empty.
///
/// The comparison is `pg_catalog`-qualified so a `public.=` cannot answer it,
/// and the flag is cleared first so a `\gset` whose query fails leaves it false.
fn psql_connection_terms(conninfo: &str, database: &str) -> String {
    let equals = |variable: &str, value: &str| {
        format!(
            ":'{variable}' OPERATOR(pg_catalog.=) {}",
            quote_literal(value)
        )
    };
    let database_is = equals("DBNAME", database);
    // A conninfo that states no host cannot be proved at all, so the proof is
    // `false` and the block is skipped. Unreachable today — a URI with an empty
    // authority (`postgres:///app`, `postgres://user@/app`) fails
    // `password_free_conninfo`, and the run refuses to print it as an
    // unprintable target before this is ever reached, measured both ways — but
    // stated here rather than left to that distant refusal. Omitting the host
    // term instead would silently drop the one discriminator this proof exists
    // for, leaving the database name to stand alone against a physical clone
    // that shares it.
    let (Some(host), port) = stated_host_and_port(conninfo) else {
        return "false".to_owned();
    };
    let hosts: Vec<&str> = host.split(',').filter(|part| !part.is_empty()).collect();
    let ports: Vec<&str> = port
        .as_deref()
        .map(|p| p.split(',').filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    if hosts.is_empty() {
        return "false".to_owned();
    }
    let any_host = || {
        hosts
            .iter()
            .map(|h| equals("HOST", h))
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    // libpq pairs a multi-host list with a multi-port list POSITIONALLY, and
    // broadcasts a single port to every host. Measured on 16.13:
    // `host=127.0.0.9,127.0.0.1&port=5433,5434` connected to 5434 — the port
    // belonging to the host it reached, not the first in the list — and
    // `host=127.0.0.9,127.0.0.1&port=5433` connected to 5433. So `a:5433` is NOT
    // an endpoint `host=a,b&port=5432,5433` names, and matching host and port
    // independently would accept a retained connection to one.
    let endpoint = match (ports.len(), hosts.len()) {
        // Unreachable: a conninfo stating no port is refused before any line is
        // printed, for the reason above. Fail closed rather than accept any port
        // for the host if that refusal is ever relaxed.
        (0, _) => return "false".to_owned(),
        (1, _) => format!("({}) AND {}", any_host(), equals("PORT", ports[0])),
        (p, h) if p == h => hosts
            .iter()
            .zip(&ports)
            .map(|(host, port)| format!("({} AND {})", equals("HOST", host), equals("PORT", port)))
            .collect::<Vec<_>>()
            .join(" OR "),
        // libpq refuses this outright — measured, `could not match 3 port
        // numbers to 2 hosts` — so the run could not have connected either.
        _ => return "false".to_owned(),
    };
    format!("{database_is} AND ({endpoint})")
}

/// Those terms as the psql conditional each target's transaction opens with.
fn psql_connection_assertion(conninfo: &str, database: &str) -> Vec<String> {
    vec![
        "\\set autumn_ok false".to_owned(),
        format!(
            "SELECT ({}) AS autumn_ok \\gset",
            psql_connection_terms(conninfo, database)
        ),
        "\\if :autumn_ok".to_owned(),
    ]
}

/// Abort the transaction unless the session is on the target this block is for.
///
/// `\connect` does NOT close the old connection when the new one fails. Measured
/// on psql 16.13: a failed `\connect -reuse-previous=off` prints "Previous
/// connection kept" and the session carries on against the PREVIOUS database, so
/// a pasted stream runs this target's `BEGIN` and its deletes against the
/// preceding one. `\set ON_ERROR_STOP on` does not help; measured, an
/// interactive session ignores it and keeps going. It is the likely case rather
/// than a remote one, because the printed conninfo has its password removed on
/// purpose: a password-authenticated target fails to connect exactly this way.
///
/// Identity is the whole endpoint, not the database name. A sharded fleet runs
/// the same name on every shard — the topology in docs/guide/sharding.md names
/// all three `app` — and measured on two clusters both holding `app`, a
/// name-only guard let shard1's block scrub shard0 from 200 users to 100 while
/// the shard it named went untouched.
///
/// Every value is asked of the target connection while planning, never parsed
/// out of its URL. libpq defaults an omitted database name to the user name,
/// which defaults to the OS user: deriving it meant reimplementing those rules,
/// and the version that tried simply emitted NO guard for a URI without one,
/// leaving 19 destructive statements unprotected.
///
/// Every call is `pg_catalog`-qualified, and the caller emits this after the
/// session pins. Unqualified, they are resolved by the pasting session's own
/// `search_path`: measured on a database configured `public, pg_catalog`, a
/// `public.current_database()` returning the intended name answered `app` while
/// `pg_catalog.current_database()` answered the truth, so the guard would have
/// consulted the shadow and passed.
///
/// `IS DISTINCT FROM` rather than `<>`, because address and port are both NULL
/// over a Unix socket and `NULL <> NULL` is NULL, which would let the guard pass
/// by failing to be false.
///
/// Printed and never executed: `execute` opens its own connection to a URL it
/// was given and cannot be on the wrong database.
fn target_guard(endpoint: &ServerEndpoint) -> String {
    let literal =
        |value: Option<&String>| value.map_or_else(|| "NULL".to_owned(), |v| quote_literal(v));
    // `RAISE` substitutes bare `%` in argument order and has no `%1$s` form.
    // Writing one named each database as the other and left `2$s` in the text.
    // Every parameter is read the same way the introspection read it, so a value
    // the pasting role may not examine answers NULL on BOTH sides rather than
    // raising inside the guard: `pg_settings` omits a restricted row, where
    // `current_setting` would raise `permission denied to examine ...`.
    let setting = |parameter: &str| {
        format!(
            "coalesce((SELECT setting FROM pg_catalog.pg_settings WHERE name = {}), '')",
            quote_literal(parameter),
        )
    };
    let datadir = setting("data_directory");
    let sysid_call = "(SELECT system_identifier::text FROM pg_catalog.pg_control_system())";
    let body = format!(
        " BEGIN IF {mismatch} THEN \
         RAISE EXCEPTION {message}, {name}, {addr}, {port}, {sysid}, {server_port}, \
         {datadir_want}, \
         pg_catalog.current_database(), pg_catalog.inet_server_addr()::text, \
         pg_catalog.inet_server_port()::text, {sysid_seen}, \
         pg_catalog.current_setting('port'), {datadir_seen}; END IF; END ",
        mismatch = endpoint_mismatch(endpoint),
        // Report a discriminator the planning role could not read as unknown
        // rather than calling for it: `pg_control_system()` raises for a role
        // without EXECUTE, and a RAISE whose own argument raises replaces the
        // mismatch this block exists to explain with `permission denied for
        // function pg_control_system`.
        sysid_seen = if endpoint.system_identifier.is_some() {
            sysid_call.to_owned()
        } else {
            "NULL".to_owned()
        },
        datadir_seen = if endpoint.data_directory.is_some() {
            format!("nullif({datadir}, '')")
        } else {
            "NULL".to_owned()
        },
        name = quote_literal(&endpoint.database),
        addr = literal(endpoint.address.as_ref()),
        port = literal(endpoint.port.as_ref()),
        sysid = literal(endpoint.system_identifier.as_ref()),
        server_port = literal(endpoint.server_port.as_ref()),
        datadir_want = literal(endpoint.data_directory.as_ref()),
        message = quote_literal(
            "this block is for % at %:% (cluster %, port %, data directory %), but the \
             session is on % at %:% (cluster %, port %, data directory %) — the \
             \\connect above did not take effect (psql keeps the previous connection \
             when one fails)"
        ),
    );
    let tag = sample::dollar_tag(&body);
    format!("DO {tag}{body}{tag};")
}

/// Abort the transaction unless the pasting session fires the same triggers the
/// plan was built against.
///
/// `session_replication_role` decides which triggers fire: `origin` fires `O`
/// and `A`, `replica` fires `R` and `A`. The run REFUSES to plan from a
/// connection that is not `origin`, because `triggers_reaching` only ever
/// inspected the `O`/`A` set — but the printed script inherits whatever the
/// pasting session is in, and nothing in it said so. An operator already
/// connected to the right endpoint in `replica` can paste a block whose
/// `\connect` fails (the printed conninfo has its password removed on purpose),
/// keep that session, pass both the psql proof and the endpoint guard — every
/// value matches, it IS the right database — and then fire a replica-only
/// trigger on a table the final pass empties, copying `OLD` rows into one the
/// rewrites already scrubbed.
///
/// Asserted rather than pinned, exactly as the command refuses rather than
/// resets: `session_replication_role` is `SUSET`, so a `SET LOCAL` would fail
/// for the ordinary role this script is written for, and silently switching a
/// superuser out of a mode they chose is not this command's call.
fn replication_role_assertion() -> String {
    let body = " BEGIN IF pg_catalog.current_setting('session_replication_role') <> 'origin' \
                 THEN RAISE EXCEPTION 'this block was planned against \
                 session_replication_role = origin, but this session is in % — a \
                 replica-only trigger fires here that the plan never inspected'\
                 , pg_catalog.current_setting('session_replication_role'); END IF; END "
        .to_owned();
    let tag = sample::dollar_tag(&body);
    format!("DO {tag}{body}{tag};")
}

/// The boolean that is TRUE when the session is NOT on the endpoint this block
/// was planned for.
///
/// Shared by the in-transaction guard and by the psql conditional that fences
/// the post-`COMMIT` compaction, so the two cannot come to disagree about what
/// counts as the right server.
fn endpoint_mismatch(endpoint: &ServerEndpoint) -> String {
    let literal =
        |value: Option<&String>| value.map_or_else(|| "NULL".to_owned(), |v| quote_literal(v));
    // Address and port are compared even when `None`, because there `None` is
    // the target's REAL answer: both are NULL for every Unix-socket connection,
    // and pinning that is what stops a socket block running against a TCP one.
    let mut terms = vec![
        format!(
            "pg_catalog.current_database() <> {}",
            quote_literal(&endpoint.database)
        ),
        format!(
            "pg_catalog.inet_server_addr()::text IS DISTINCT FROM {}",
            literal(endpoint.address.as_ref())
        ),
        format!(
            "pg_catalog.inet_server_port()::text IS DISTINCT FROM {}",
            literal(endpoint.port.as_ref())
        ),
        format!(
            "pg_catalog.current_setting('port') IS DISTINCT FROM {}",
            literal(endpoint.server_port.as_ref())
        ),
    ];
    // These two are different: `None` means the PLANNING role could not read the
    // value, not that the target has none — every live server has both. A term
    // comparing against a value we never learned can only be wrong. It refuses a
    // CORRECT paste whenever the pasting role can read what the planning role
    // could not, and `pg_control_system()` is worse still: measured, with its
    // EXECUTE revoked an ordinary role gets `permission denied for function
    // pg_control_system`, which inside the guard aborts the transaction and
    // takes the whole paste down. So an unreadable discriminator is dropped
    // rather than guessed at; the four above still identify the endpoint.
    if let Some(sysid) = endpoint.system_identifier.as_ref() {
        terms.push(format!(
            "(SELECT system_identifier::text FROM pg_catalog.pg_control_system()) \
             IS DISTINCT FROM {}",
            quote_literal(sysid)
        ));
    }
    if let Some(datadir) = endpoint.data_directory.as_ref() {
        terms.push(format!(
            "nullif(coalesce((SELECT setting FROM pg_catalog.pg_settings \
             WHERE name = 'data_directory'), ''), '') IS DISTINCT FROM {}",
            quote_literal(datadir)
        ));
    }
    terms.join(" OR ")
}

/// The psql predicate that decides whether this target really scrubbed, read by
/// the compaction pass that runs after every target's `COMMIT`.
///
/// `VACUUM (FULL)` cannot run inside a transaction block, so the compaction is
/// the one part of a target's plan the in-transaction guard cannot cover — and
/// measured, that gap was reachable. When a `\connect` fails psql keeps the
/// previous connection; the guard aborts the transaction, every `DELETE` below
/// it is refused, and the printed `COMMIT` then turns that abort into a
/// `ROLLBACK` and clears the aborted state. On a `pg_basebackup` clone pasted
/// at its origin, all 64 destructive statements were refused and six
/// `VACUUM (FULL, ANALYZE)` statements then ran on the ORIGIN — each taking an
/// `ACCESS EXCLUSIVE` lock and rewriting the table.
///
/// So psql decides instead of the server. `\gset` reads the same predicate the
/// guard uses into `autumn_ok`, and the compaction pass `\if`s on it. Both
/// failure modes are closed, measured on psql 16.13: a false value prints
/// `query ignored`, and a `\gset` whose query ERRORED leaves the variable
/// unset, which `\if` reports as `Boolean expected` and still skips.
///
/// The `\if` itself is NOT emitted here. Compaction is printed after every
/// target's transaction rather than inside one, because that is where the
/// executor runs it; this leaves the flag for that pass to read.
///
/// The `search_path` pin is re-applied first because the run's own pins are
/// `SET LOCAL` — they belong to the transaction that just rolled back, so the
/// operators in this predicate would otherwise resolve through whatever the
/// pasting session's path happens to be.
fn post_commit_fence(endpoint: &ServerEndpoint) -> Vec<String> {
    vec![
        // FIRST, before any statement runs: a query would reset psql's own
        // `:ERROR`, and this is the only moment it still describes the `COMMIT`.
        // `\set` is a meta-command and executes nothing, so it is safe here.
        "\\set autumn_commit_error :ERROR".to_owned(),
        "SET search_path = pg_catalog, public;".to_owned(),
        // False FIRST, so a `\\gset` whose query fails leaves it false instead of
        // carrying the previous target's success forward.
        "\\set autumn_ok false".to_owned(),
        format!(
            "SELECT (:'autumn_scrubbed'::bool AND NOT :'autumn_commit_error'::bool \
             AND NOT ({})) AS autumn_ok \\gset",
            endpoint_mismatch(endpoint),
        ),
    ]
}

/// Connection-string keywords whose value is a credential.
///
/// A URI carries these two ways — `postgres://user:secret@host/db` and
/// `postgres://host/db?password=secret` — and the query form wins where both
/// appear, which `pg::sanitize_prefers_query_user_password_dbname_over_url_structure`
/// pins. Clearing only the userinfo therefore prints the effective password.
const SECRET_KEYWORDS: [&str; 2] = ["password", "sslpassword"];

/// `url` with every password removed, or `None` if that cannot be guaranteed.
///
/// Only the URI form is handled. Keyword form (`host=db password = secret`)
/// looks tokenizable and is not: `libpq` allows whitespace around the `=`, and
/// values may be single-quoted with backslash escapes, so "split on whitespace
/// and drop the secret tokens" has now been wrong twice in a row — once for a
/// quoted value, once for a spaced `=`. Rather than reach for a third
/// tokenizer, that form is declined outright, which makes the promise above
/// true by construction instead of by enumerating the ways it can be written.
fn password_free_conninfo(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    // `set_password` returns Err only for a URL that cannot have one
    // (`mailto:` and friends), which a connection string is not.
    parsed.set_password(None).ok()?;
    // Read and rewrite the query the way libpq does, NOT the way
    // `url::Url::query_pairs()` does. That method applies
    // `application/x-www-form-urlencoded` rules, and its `query_pairs_mut()`
    // counterpart serializes a space back as `+` — while libpq's URI parser
    // only ever percent-decodes, so it reads that `+` literally. Measured
    // against psql 16.13: an operator's `?options=-c%20search_path%3Dpg_catalog`
    // connects and sets the path, and the `+` form this used to print does not
    // connect at all —
    //
    //   FATAL:  unrecognized configuration parameter "+search_path"
    //
    // which is the failure mode the target guard exists for. `pg.rs` already
    // holds both halves of the libpq grammar, with the reasoning; this uses
    // them rather than keeping a second opinion about encoding here.
    let kept: Vec<(String, String)> =
        crate::pg::parse_raw_query_pairs(parsed.query().unwrap_or(""))
            .into_iter()
            .filter(|(key, _)| !is_secret_keyword(key))
            .collect();
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        let query = kept
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    crate::pg::query_value_token(key),
                    crate::pg::query_value_token(value),
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        parsed.set_query(Some(&query));
    }
    Some(parsed.into())
}

/// Whether a connection-string keyword carries a credential. Compared without
/// case, because a URI query key is not normalised for us.
fn is_secret_keyword(key: &str) -> bool {
    SECRET_KEYWORDS
        .iter()
        .any(|secret| key.eq_ignore_ascii_case(secret))
}

/// One `psql` meta-command argument, double-quoted with backslash escapes —
/// `psql` reads them that way inside double quotes, unlike SQL identifiers.
fn quote_psql_arg(value: &str) -> String {
    let escaped: String = value
        .chars()
        .flat_map(|c| {
            let escape = matches!(c, '"' | '\\');
            escape.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect();
    format!("\"{escaped}\"")
}

/// The session settings the scrub pins for the whole transaction.
///
/// A role- or database-level `search_path` (tenant schemas) would otherwise
/// redirect an unqualified name — every `md5`, `quote_nullable` and `count` the
/// generated SQL calls — to something the classification never saw, and
/// `quote_literal`'s doubled quotes would mean something else under
/// `standard_conforming_strings = off`.
///
/// Shared with the dry run: printing the statements without them advertises SQL
/// that resolves differently from the command it claims to be, on exactly the
/// targets whose `search_path` made the pinning necessary.
fn session_settings() -> Vec<String> {
    [
        "SET LOCAL search_path = pg_catalog, public",
        "SET LOCAL standard_conforming_strings = on",
        // The rest pin how a value RENDERS, because the sample's row key is
        // hashed from `key::text` and `--seed` promises the same seed against
        // the same source selects the same rows. Measured: the same seed over a
        // `date` primary key selects a different subset under `DateStyle = ISO,
        // YMD` than under `Postgres, DMY`. `bytea_output` and
        // `extra_float_digits` do the same for their types, and `IntervalStyle`
        // for intervals. (`TimeZone` is pinned by the driver on connect, but
        // relying on that leaves the guarantee resting on a dependency's
        // default.)
        "SET LOCAL DateStyle = 'ISO, YMD'",
        "SET LOCAL IntervalStyle = 'iso_8601'",
        "SET LOCAL TimeZone = 'UTC'",
        "SET LOCAL bytea_output = 'hex'",
        "SET LOCAL lc_monetary = 'C'",
        "SET LOCAL extra_float_digits = 3",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Every table the run locks, deduplicated and ordered, as `LOCK TABLE` SQL.
///
/// Shared with the dry run: an operator pasting the printed sequence into a
/// target that still takes writes must exclude the same writers the command
/// does, or a row inserted after that table's `DELETE` survives unsampled and
/// unscrubbed — a difference between the advertised SQL and the real one that
/// shows up as data, not as an error.
fn lock_statements(
    plan: &ScrubPlan,
    purges: &[(String, String)],
    sampling: Option<&sample::SamplePlan>,
) -> Vec<String> {
    let mut locked: Vec<&str> = plan
        .tables
        .iter()
        .map(|t| t.table.as_str())
        .chain(purges.iter().map(|(table, _)| table.as_str()))
        // Every table the sample reads or empties, too: a row inserted into one
        // after the walk selected from it would survive a run that reports the
        // table subsetted.
        .chain(
            sampling
                .into_iter()
                .flat_map(sample::SamplePlan::locked_tables),
        )
        .collect();
    locked.sort_unstable();
    locked.dedup();
    locked
        .into_iter()
        .map(|table| {
            format!(
                "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
                qualified_ident(table)
            )
        })
        .collect()
}

/// The SQL that asserts a foreign key re-count found no orphans.
///
/// `execute` reads the count so it can report how many; the dry run prints this,
/// because a bare `SELECT count(*)` in a pasted sequence returns a number and
/// then commits anyway — exactly the run the command refuses.
fn integrity_assertion(check: &str) -> String {
    // The tag is chosen from the finished body, never fixed: `check` carries
    // quoted identifiers, a Postgres identifier may legally contain `$`, and
    // dollar quoting is lexical — a `$$` inside a column name would close this
    // block mid-statement and turn the rest into a syntax error in the one
    // sequence the operator was told to paste.
    let body = format!(
        "BEGIN IF ({check}) > 0 THEN \
         RAISE EXCEPTION 'a foreign key this run checked does not resolve'; \
         END IF; END"
    );
    let tag = sample::dollar_tag(&body);
    format!("DO {tag} {body} {tag}")
}

/// The SQL that asserts a promised-empty table really is empty.
///
/// `execute` runs the equivalent as a counted query so it can name the row
/// count; the dry run prints this, because a printed sequence that commits where
/// the real command aborts is worse than no sequence at all. Emitted as a `DO`
/// block so pasting it actually fails the transaction rather than quietly
/// returning a row. The table name appears only as an identifier — already
/// quoted — so nothing has to be escaped into a string literal.
fn emptiness_assertion(table: &str) -> String {
    // Same reason as `integrity_assertion`: the table name is an identifier and
    // may legally contain `$`, so the delimiter comes from the body.
    let body = format!(
        "BEGIN IF EXISTS (SELECT 1 FROM {}) THEN \
         RAISE EXCEPTION 'a table this run promised would be empty still holds rows'; \
         END IF; END",
        qualified_ident(table)
    );
    let tag = sample::dollar_tag(&body);
    format!("DO {tag} {body} {tag}")
}

/// The emptying passes `emptying_phases` returns, as `(table, statement)`.
struct EmptyingPhases<'a> {
    before: Vec<(&'a str, String)>,
    after_sample: Vec<(&'a str, String)>,
    final_pass: Vec<(&'a str, String)>,
}

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

/// What one database's scrub wrote: `(table, rows)` per rewrite, plus the
/// sample's own outcome when `--sample` was given.
type Applied = (Vec<(String, usize)>, Option<sample::SampleOutcome>);

/// The materialized-view work one target's transaction owes: which views to
/// refresh and in what order, the flat closure the size report measures over,
/// and every view to leave `WITH NO DATA` once those refreshes are done.
struct ViewRefresh<'a> {
    ordered: &'a [String],
    all: &'a [String],
    unpopulated: &'a [String],
}

/// Run every statement for one database inside a single transaction, so a
/// failure can never leave a half-scrubbed database behind.
fn execute(
    url: &str,
    plan: &ScrubPlan,
    purges: &[(String, String)],
    views: &ViewRefresh<'_>,
    sampling: Option<&sample::SamplePlan>,
    label: &str,
) -> Result<Applied, ScrubError> {
    if plan.tables.is_empty() && purges.is_empty() && views.ordered.is_empty() && sampling.is_none()
    {
        return Ok((Vec::new(), None));
    }
    let mut conn = probe_connection(url, label, "apply the scrub")?;
    let mut counts = Vec::with_capacity(plan.tables.len());
    let mut outcome = None;
    // The transaction's error type carries BOTH channels, so a sample refusal
    // rolls back as itself rather than as an opaque database error.
    conn.transaction::<_, sample::SampleFailure, _>(|conn| {
        // Pin the resolution of every unqualified name and the meaning of every
        // string literal for the whole transaction, so a role- or
        // database-level `search_path` (tenant schemas) cannot redirect a write
        // to a table nothing classified, and `quote_literal`'s doubled quotes
        // cannot be re-interpreted under `standard_conforming_strings = off`.
        conn.batch_execute(&session_settings().join("; "))?;
        // Hold the tables for the duration: the plan was built from a snapshot
        // taken on another connection, and a row inserted between the two would
        // otherwise survive the scrub unnoticed. SHARE ROW EXCLUSIVE blocks
        // writers while still allowing plain reads.
        // Every table the transaction writes, including the ones it empties: a
        // producer inserting into a purged job/sync/token table after that
        // `DELETE` took its snapshot would otherwise survive a run that reports
        // the table emptied.
        for statement in lock_statements(plan, purges, sampling) {
            sql_query(statement).execute(conn)?;
        }
        // Purges run FIRST so a framework-owned table that references a
        // sampled one is already empty when the sample removes its parents —
        // except the ones the plan defers, which are the mirror image: a table
        // the sample empties references them, so they have to wait for it.
        //
        // These early passes exist to make the DELETEs possible, not to make the
        // guarantee true: the authoritative pass is the one after the rewrites,
        // because a trigger on a scrubbed table can write fresh rows — carrying
        // the very PII being removed — into a purged table after these run.
        // Rows removed are accumulated across all passes and reported once.
        let no_deferral = BTreeSet::new();
        let deferred: &BTreeSet<String> = sampling.map_or(&no_deferral, |s| &s.purge_after);
        let phases = emptying_phases(purges, deferred, sampling);
        let mut purged_rows: BTreeMap<&str, usize> = BTreeMap::new();
        for (table, statement) in &phases.before {
            let rows = sql_query(statement).execute(conn)?;
            *purged_rows.entry(*table).or_default() += rows;
        }
        // Then the subset, so the rewrites below touch only the rows that
        // survive it — and so no combination of flags can commit a row that was
        // sampled but not scrubbed: both happen in this one transaction.
        if let Some(sampling) = sampling {
            outcome = Some(sample::apply(
                conn,
                sampling,
                // The uncapped list: refresh ORDER comes from the walk, but a
                // view the walk's depth cap dropped still holds its heap, and
                // measuring only the ordered subset is what let the report
                // announce a size a large unmeasured view contradicted.
                &also_measured(&purged_tables(purges), views.all),
            )?);
        }
        // The deferred purges, now that the sample has emptied what referenced
        // them. Empty unless a plan deferred one, so an unsampled scrub still
        // runs every purge in the single pass above.
        for (table, statement) in &phases.after_sample {
            let rows = sql_query(statement).execute(conn)?;
            *purged_rows.entry(*table).or_default() += rows;
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
        // The authoritative pass, after every rewrite: every purge again AND the
        // `never_include` tables. An `UPDATE` trigger on a scrubbed table — or a
        // `DELETE` trigger fired by the sample — can copy `OLD` values into
        // either kind, so emptying only beforehand would report a table emptied
        // while it holds rows carrying the original PII. Both promises are
        // "this table ends up empty", so both are enforced here, last; the
        // passes above exist only to order the sample's own deletes.
        for (table, statement) in &phases.final_pass {
            let rows = sql_query(statement).execute(conn)?;
            *purged_rows.entry(*table).or_default() += rows;
        }
        for (table, rows) in purged_rows {
            counts.push((format!("{table} (emptied)"), rows));
        }

        // Prove the promise instead of ordering for it.
        //
        // Each statement in the pass above can fire triggers, and one of those
        // can insert into a table an EARLIER statement already emptied — an
        // `ON DELETE` archive trigger between two promised-empty tables does
        // exactly that. No ordering of the deletes rules that out in general:
        // the trigger graph decides, and it can be cyclic. So the guarantee is
        // checked rather than arranged, in the same transaction, the same way
        // the sample re-counts its foreign keys rather than trusting the walk.
        let mut refilled = Vec::new();
        for (table, _) in &phases.final_pass {
            let row: RowCount = sql_query(format!(
                "SELECT count(*) AS n FROM {}",
                qualified_ident(table)
            ))
            .get_result(conn)?;
            if row.n > 0 {
                refilled.push(format!("{table} ({} row(s))", row.n));
            }
        }
        if !refilled.is_empty() {
            refilled.sort();
            refilled.dedup();
            return Err(sample::SampleFailure::Refused(
                sample::SampleError::NotEmptied { tables: refilled },
            ));
        }
        // Inside the transaction, so a refresh the role is not allowed to run
        // rolls the rewrites back rather than committing base tables that a
        // stale materialized view still contradicts.
        for view in views.ordered {
            sql_query(format!(
                "REFRESH MATERIALIZED VIEW {}",
                qualified_ident(view)
            ))
            .execute(conn)?;
            counts.push((format!("{view} (materialized view refreshed)"), 0));
        }
        // And `WITH NO DATA` for every view that had none when the run probed:
        // the ones populated only so a dependent could be rebuilt from them, and
        // the ones skipped entirely — which a concurrent session could have
        // populated from pre-scrub rows in the meantime, since nothing here
        // locks a view the run does not refresh. Last, after every refresh,
        // because emptying a source before its dependent is rebuilt makes that
        // refresh fail — and safe here, because emptying it afterwards does not
        // un-populate the dependent already rebuilt from it.
        for view in views.unpopulated {
            sql_query(format!(
                "REFRESH MATERIALIZED VIEW {} WITH NO DATA",
                qualified_ident(view)
            ))
            .execute(conn)?;
            counts.push((format!("{view} (materialized view left unpopulated)"), 0));
        }
        Ok(())
    })
    .map_err(|e| match e {
        sample::SampleFailure::Refused(refusal) => ScrubError::from(refusal),
        sample::SampleFailure::Db(error) => ScrubError::Sql(error.to_string()),
    })?;
    Ok((counts, outcome))
}

#[cfg(test)]
mod tests {
    use super::{emptiness_assertion, integrity_assertion, rules_reaching, triggers_reaching};

    // ── The dry run's session and connection preamble ──────────────────────

    /// The statement the dry run cannot print truthfully withholds the whole
    /// script, because nothing in a pasted stream can stop it partway.
    ///
    /// Measured: printing the script with the rewrite replaced by a comment left
    /// `users` sampled 200 -> 100 with every address rewritten AND all 100 kept
    /// rows still holding their original production ciphertext, committed
    /// without a single error. Replacing the comment with a `RAISE EXCEPTION`
    /// fixed only that target: on psql 16.13 the abort ends at the printed
    /// `COMMIT`, and the following `VACUUM (FULL)`s and the NEXT target's
    /// `\connect` and DELETEs ran for real — a second cluster went from 200
    /// comments to 0. `\quit` fares no better: psql exits and the shell that
    /// launched it reads the rest of the paste, executing a trailing `echo` in
    /// the same measurement. So the script is refused before it is printed.
    #[test]
    fn an_encrypted_rewrite_withholds_the_whole_printed_script() {
        let refusal = ScrubError::UnprintableEncryptedRewrite {
            columns: vec!["control: users.api_token".to_owned()],
        }
        .to_string();
        assert!(
            refusal.contains("cannot print a runnable script"),
            "it must refuse the script, not annotate it: {refusal}"
        );
        assert!(
            refusal.contains("users.api_token"),
            "and name the column that cannot be printed: {refusal}"
        );
        assert!(
            refusal.contains("without --dry-run"),
            "and say what to run instead: {refusal}"
        );
        // Withholding the SQL is the point; the plan report above it still
        // stands, so the refusal must not read as "nothing was analysed".
        assert!(
            refusal.contains("plan above is complete"),
            "and keep the reported plan standing: {refusal}"
        );
        // The key is the reason this cannot be printed as SQL, so it must not
        // appear in the reason either.
        assert!(
            !refusal.to_lowercase().contains("primary_key")
                && !refusal.contains("deterministic_key")
                && !refusal.contains("key_derivation_salt"),
            "no key material may reach the refusal: {refusal}"
        );
    }

    /// `VACUUM (FULL)` cannot run inside a transaction, so the compaction is the
    /// one part of a target's plan the in-transaction guard cannot cover — and
    /// that gap was reachable, not theoretical.
    ///
    /// Measured against a `pg_basebackup` clone pasted at its origin: the guard
    /// refused all 64 destructive statements, and then six
    /// `VACUUM (FULL, ANALYZE)` statements ran on the ORIGIN, each taking an
    /// `ACCESS EXCLUSIVE` lock and rewriting the table. The printed `COMMIT`
    /// turns the guard's abort into a `ROLLBACK` and clears the aborted state,
    /// so the server has nothing left to refuse with.
    ///
    /// psql decides instead. With the fence, the same paste ran zero VACUUMs on
    /// the origin (seven `query ignored` lines) and the legitimate paste still
    /// ran all six against the clone, 200 -> 100 users, zero errors.
    #[test]
    fn the_compaction_after_commit_is_fenced_by_psql() {
        let endpoint = super::ServerEndpoint {
            database: "app".to_owned(),
            address: None,
            port: None,
            system_identifier: Some("7682669380557907941".to_owned()),
            server_port: Some("5435".to_owned()),
            data_directory: Some("/tmp/pgd3".to_owned()),
        };
        let fence = super::post_commit_fence(&endpoint);
        // The run's own pins are SET LOCAL, so they belong to the transaction
        // that just rolled back. Without re-pinning, the operators below resolve
        // through the pasting session's own search_path.
        assert!(
            fence
                .iter()
                .any(|line| line == "SET search_path = pg_catalog, public;"),
            "the fence must re-pin the path it needs: {fence:?}"
        );
        assert!(
            fence
                .iter()
                .any(|line| line.contains("\\gset") && line.contains("AS autumn_ok")),
            "psql, not the server, has to decide this one: {fence:?}"
        );
        // The `\if` belongs to the compaction pass, which runs after EVERY
        // target's transaction rather than inside one. Emitting it here put the
        // VACUUM statements between two targets: measured on two TCP targets
        // under `ON_ERROR_STOP on`, a first-target VACUUM that timed out ended
        // the script at rc=3 with that database scrubbed and the SECOND still
        // holding all 200 of its original addresses — from a failure the
        // executor only warns about.
        assert!(
            !fence.iter().any(|line| line == "\\if :autumn_ok"),
            "the fence sets the flag; the compaction pass reads it: {fence:?}"
        );
        // One flag for the whole stream, not one per target: `execute` returns
        // on the first target that fails and never touches the rest, and the
        // script has to match. Measured with two real targets and a non-guard
        // failure in the first: without it the second target's `\connect` and
        // its whole destructive block ran anyway, leaving a partially scrubbed
        // topology and a stream that ends with no error. With it, 90 statements
        // were skipped and the second target was untouched at 100 rows.
        assert_eq!(
            fence
                .iter()
                .filter(|line| *line == "\\set autumn_ok false")
                .count(),
            1,
            "the flag must be cleared before the gset that may not run: {fence:?}"
        );
        // Being on the right server is not the same as having scrubbed it.
        // Measured: with a statement inside the transaction failing for a
        // reason the guard knows nothing about, `COMMIT` returned ROLLBACK and
        // an endpoint-only probe still ran all six VACUUM (FULL, ANALYZE) —
        // ACCESS EXCLUSIVE locks and full rewrites on a target whose scrub had
        // just rolled back. `:ERROR` does not catch it either: measured, psql
        // reports that ROLLBACK as SUCCESS, so it is false there. Hence the
        // explicit flag, set as the transaction's last statement.
        assert!(
            fence
                .iter()
                .any(|line| line.contains("autumn_scrubbed") && line.contains("::bool")),
            "the fence must require the transaction to have succeeded: {fence:?}"
        );
        // And `:ERROR` must be captured before anything else runs, because a
        // statement of any kind resets it — `\set` is a meta-command and
        // executes nothing, which is why it can come first.
        assert_eq!(
            fence.first().map(String::as_str),
            Some("\\set autumn_commit_error :ERROR"),
            "the commit's own error state has to be read before it is lost: {fence:?}"
        );
        // The same predicate as the guard, so the two cannot come to disagree
        // about what counts as the right server.
        let mismatch = super::endpoint_mismatch(&endpoint);
        assert!(
            fence.iter().any(|line| line.contains(&mismatch)),
            "the fence must ask exactly what the guard asks: {fence:?}"
        );
        assert!(
            super::target_guard(&endpoint).contains(&mismatch),
            "and the guard must ask it too: {mismatch}"
        );
    }

    /// A failed `\connect` leaves psql on the PREVIOUS database, so the block
    /// proves where it is before it writes — and proves the whole endpoint.
    ///
    /// Measured on psql 16.13: a failed `\connect -reuse-previous=off` prints
    /// "Previous connection kept" and the session carries on; `ON_ERROR_STOP`
    /// does not stop an interactive paste. With the database name alone, two
    /// clusters both holding `app` let shard1's block scrub shard0 from 200
    /// users to 100. With the endpoint pinned, the same paste aborted and left
    /// shard0 at 200/400/500.
    #[test]
    fn the_target_guard_pins_the_whole_endpoint_and_qualifies_every_call() {
        let here = super::ServerEndpoint {
            database: "app_copy".to_owned(),
            address: Some("10.0.0.2/32".to_owned()),
            port: Some("5432".to_owned()),
            system_identifier: Some("7682669380557907941".to_owned()),
            server_port: Some("5432".to_owned()),
            data_directory: Some("/var/lib/postgresql/16/main".to_owned()),
        };
        let guard = super::target_guard(&here);
        assert!(
            guard.contains("pg_catalog.current_database() <> 'app_copy'")
                && guard
                    .contains("pg_catalog.inet_server_addr()::text IS DISTINCT FROM '10.0.0.2/32'")
                && guard.contains("pg_catalog.inet_server_port()::text IS DISTINCT FROM '5432'"),
            "the guard must pin the whole endpoint: {guard}"
        );
        // Address and port are None for EVERY Unix-socket connection, so two
        // socket clusters holding the same database name are identical by them.
        // Measured: both reported `<null>/<null>` while their system identifiers
        // differed (7682669380557907941 vs 7683257673996527196).
        assert!(
            guard.contains("IS DISTINCT FROM '7682669380557907941'"),
            "the cluster identity must be pinned too, or two socket clusters are \
             indistinguishable: {guard}"
        );
        // And the identifier alone is not identity either: a physical copy —
        // a replica, or a promoted staging clone — carries the identifier of
        // the cluster it was cloned from. The configured port answers over a
        // socket where `inet_server_port()` is NULL, and the data directory is
        // the value two postmasters on one machine cannot share.
        assert!(
            guard.contains("pg_catalog.current_setting('port') IS DISTINCT FROM '5432'"),
            "the server's configured port must be pinned: {guard}"
        );
        assert!(
            guard.contains("IS DISTINCT FROM '/var/lib/postgresql/16/main'"),
            "the data directory must be pinned, or a clone passes: {guard}"
        );
        // Read through `pg_settings`, never `current_setting`: the parameter is
        // restricted to `pg_read_all_settings`, and `current_setting` raises
        // `permission denied to examine ...` for a role without it — inside the
        // guard, that failure would abort a CORRECT paste.
        assert!(
            !guard.contains("current_setting('data_directory')")
                && guard.contains("FROM pg_catalog.pg_settings WHERE name = 'data_directory'"),
            "a restricted setting must degrade to NULL, not raise: {guard}"
        );
        // Unqualified, these resolve through the PASTING session's search_path.
        // Measured on a database configured `public, pg_catalog`, a shadowing
        // `public.current_database()` answered `app` while
        // `pg_catalog.current_database()` answered the truth — so an unqualified
        // guard consults the shadow and passes.
        for call in [
            "current_database()",
            "inet_server_addr()",
            "inet_server_port()",
            "pg_control_system()",
        ] {
            for (at, _) in guard.match_indices(call) {
                assert!(
                    guard[..at].ends_with("pg_catalog."),
                    "every catalog call must be qualified: {call} at {at} in {guard}"
                );
            }
        }
        assert!(
            !guard.contains("$s"),
            "RAISE has no positional format specifiers: {guard}"
        );
        assert_eq!(
            guard.matches('%').count(),
            12,
            "six placeholders for the target endpoint, six for the session's: {guard}"
        );

        // Over a Unix socket the server reports neither, and the guard compares
        // that as NULL rather than papering over it.
        let socket = super::target_guard(&super::ServerEndpoint {
            database: "app_copy".to_owned(),
            ..super::ServerEndpoint::default()
        });
        assert!(
            socket.contains("IS DISTINCT FROM NULL"),
            "a socket target pins NULL explicitly: {socket}"
        );
        assert_eq!(
            socket.matches("IS DISTINCT FROM NULL").count(),
            3,
            "address, port and the configured port are compared even when NULL — \
             for a socket target that IS the answer: {socket}"
        );
        // The cluster identifier and the data directory are NOT compared when
        // the planning role could not read them: there, `None` means "unknown",
        // not "the target has none". A term comparing against a value never
        // learned refuses a CORRECT paste whenever the pasting role can read
        // what the planning role could not — and for `pg_control_system()` it
        // is worse: measured, with EXECUTE revoked an ordinary role gets
        // `permission denied for function pg_control_system`, which inside the
        // guard aborts the transaction and takes the whole paste with it.
        assert!(
            !socket.contains("pg_control_system()) IS DISTINCT FROM")
                && !socket.contains("'data_directory'), ''), '') IS DISTINCT FROM"),
            "an unreadable discriminator must be dropped, not guessed at: {socket}"
        );
        // Nor may the RAISE call for one: an argument that raises would replace
        // the mismatch this block exists to explain with a permission error.
        assert!(
            !socket.contains("pg_catalog.pg_control_system()"),
            "and the message must not call for it either: {socket}"
        );

        // The database name comes from the target connection, so a quote in it
        // reaches SQL as a literal like every other value this module prints.
        let hostile = super::target_guard(&super::ServerEndpoint {
            database: "it's".to_owned(),
            ..super::ServerEndpoint::default()
        });
        assert!(
            hostile.contains("'it''s'"),
            "a quote in the database name must not break out of the literal: {hostile}"
        );
    }

    /// Both sides of the size ratio measure the same set of relations.
    ///
    /// The purge targets keep their whole file until compaction rewrites them,
    /// The reconnect proves itself from psql's view, not the server's.
    ///
    /// `inet_server_addr()`/`inet_server_port()` are the endpoint the SERVER
    /// accepted on, not the one the operator configured — measured through a
    /// forwarder, a session connected to `127.0.0.1:15433` reports
    /// `127.0.0.1:5433`. Two servers behind different forwards, or in separate
    /// container networks sharing a private address, therefore report the same
    /// pair, and with a physical clone's inherited `system_identifier` and a
    /// container-local `data_directory` every server-side value can match on two
    /// different databases.
    #[test]
    fn the_reconnect_is_proved_from_psql_own_variables() {
        let lines = super::psql_connection_assertion(
            "postgres://postgres@127.0.0.1:25433/dry_t2",
            "dry_t2",
        );
        // False FIRST, so a `\gset` whose query fails cannot carry the previous
        // target's answer into this target's destructive block.
        assert_eq!(
            lines.first().map(String::as_str),
            Some("\\set autumn_ok false"),
            "the flag must be cleared before the gset that may not run: {lines:?}"
        );
        let probe = lines
            .iter()
            .find(|l| l.contains("\\gset"))
            .unwrap_or_else(|| panic!("psql, not the server, has to decide this: {lines:?}"));
        // The operator's endpoint, which is what `\connect` acted on — NOT the
        // 5434 the server behind that forward reports for itself.
        assert!(
            probe.contains(":'PORT' OPERATOR(pg_catalog.=) '25433'")
                && probe.contains(":'HOST' OPERATOR(pg_catalog.=) '127.0.0.1'")
                && probe.contains(":'DBNAME' OPERATOR(pg_catalog.=) 'dry_t2'"),
            "every stated component must be pinned to psql's own value: {probe}"
        );
        // `pg_catalog`-qualified, so a `public.=` in the pasting session's path
        // cannot answer the one comparison the whole block depends on.
        assert!(
            !probe.contains(" = '"),
            "the comparison must not resolve through search_path: {probe}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some("\\if :autumn_ok"),
            "and the block has to sit inside it: {lines:?}"
        );
    }

    /// A query pair overrides the authority it duplicates.
    ///
    /// That is the precedence libpq applies and `pg::sanitize_db_url`
    /// normalises, so the proof has to read the conninfo the same way — pinning
    /// the authority's port here would assert a port psql never connects to.
    #[test]
    fn a_query_pair_overrides_the_authority_it_duplicates() {
        let overridden = super::psql_connection_assertion(
            "postgres://postgres@a.example:5432/app?port=6000",
            "app",
        )
        .into_iter()
        .find(|l| l.contains("\\gset"))
        .expect("the probe must be emitted");
        assert!(
            overridden.contains(":'PORT' OPERATOR(pg_catalog.=) '6000'")
                && !overridden.contains("'5432'"),
            "the query pair wins over the authority: {overridden}"
        );
    }

    /// A failover list is matched the way libpq resolves it: positionally.
    ///
    /// Measured on `PostgreSQL` 16.13, `host=127.0.0.9,127.0.0.1&port=5433,5434`
    /// connected to 5434 — the port belonging to the host it reached, not the
    /// first in the list — and `host=127.0.0.9,127.0.0.1&port=5433` connected to
    /// 5433. So `a:5433` is not an endpoint `host=a,b&port=5432,5433` names, and
    /// matching host and port independently would accept a retained connection
    /// to one that was never configured.
    #[test]
    fn a_failover_list_keeps_its_host_port_pairing() {
        let paired = super::psql_connection_terms(
            "postgres://postgres@a.example/app?host=a.example,b.example&port=5432,5433",
            "app",
        );
        assert!(
            paired.contains(
                "(:'HOST' OPERATOR(pg_catalog.=) 'a.example' AND :'PORT' OPERATOR(pg_catalog.=) '5432')"
            ) && paired.contains(
                "(:'HOST' OPERATOR(pg_catalog.=) 'b.example' AND :'PORT' OPERATOR(pg_catalog.=) '5433')"
            ),
            "each host must carry its OWN port: {paired}"
        );
        // A single port applies to every host, which libpq does and this must
        // not turn into a refusal.
        let broadcast = super::psql_connection_terms(
            "postgres://postgres@a.example/app?host=a.example,b.example&port=5432",
            "app",
        );
        assert!(
            broadcast.contains(":'HOST' OPERATOR(pg_catalog.=) 'a.example'")
                && broadcast.contains(":'HOST' OPERATOR(pg_catalog.=) 'b.example'")
                && broadcast.matches(":'PORT'").count() == 1,
            "one port must cover every host: {broadcast}"
        );
        // libpq refuses a length mismatch outright — measured, `could not match
        // 3 port numbers to 2 hosts` — so the run could not have connected
        // either. Fail closed rather than invent a pairing.
        let mismatched = super::psql_connection_terms(
            "postgres://postgres@a.example/app?host=a.example,b.example&port=1,2,3",
            "app",
        );
        assert_eq!(
            mismatched, "false",
            "a pairing libpq itself rejects must not be guessed at: {mismatched}"
        );
    }

    /// A conninfo stating no host cannot be proved, so it proves nothing.
    ///
    /// Unreachable today: a URI with an empty authority fails
    /// `password_free_conninfo`, and the run refuses to print such a target
    /// before this is reached — measured for both `postgres:///app` and
    /// `postgres://user@/app`. Pinned here anyway, because omitting the host
    /// term instead would leave the database name standing alone against a
    /// physical clone that shares it.
    #[test]
    fn a_conninfo_without_a_host_proves_nothing() {
        assert_eq!(
            super::psql_connection_terms("postgres:///app", "app"),
            "false",
            "the one discriminator this proof exists for cannot be optional"
        );
        // Nor the port. psql reports the RESOLVED port, and the same host-only
        // URI resolves to 5433 under `PGPORT=5433` and to 5432 without it —
        // measured, two different servers. Such a target is refused before
        // anything is printed; this is the fail-closed answer if that is ever
        // relaxed.
        assert_eq!(
            super::psql_connection_terms("postgres://db.internal/app", "app"),
            "false",
            "accepting any port for the host drops the discriminator"
        );
    }

    /// The pasting session must fire the triggers the plan was built against.
    ///
    /// The run refuses to PLAN from a connection that is not `origin`, because
    /// the trigger walk only inspects the `O`/`A` set — but the printed script
    /// inherits whatever the pasting session is in. Measured: with the session
    /// in `replica` and the printed `\connect` failing (so that session is
    /// retained, on the right endpoint, past both the psql proof and the
    /// endpoint guard), this assertion raised and the transaction aborted with
    /// the database untouched at 200 rows.
    #[test]
    fn the_pasting_session_must_be_in_the_origin_replication_role() {
        let assertion = super::replication_role_assertion();
        assert!(
            assertion
                .contains("pg_catalog.current_setting('session_replication_role') <> 'origin'")
                && assertion.contains("RAISE EXCEPTION"),
            "the script must assert what the command refuses to plan without: {assertion}"
        );
        // Asserted, not pinned: the setting is SUSET, so a `SET LOCAL` would fail
        // for the ordinary role this script is written for.
        assert!(
            !assertion.contains("SET LOCAL"),
            "it must not try to reset a setting an ordinary role cannot: {assertion}"
        );
    }

    /// and a refreshed materialized view is rebuilt from whatever survives the
    /// sample — a view over reference data does not shrink at all. Measuring
    /// base tables only reported `488.0 kB -> 232.0 kB` on a database still
    /// holding a 44 MB refreshed view.
    #[test]
    fn the_measured_set_covers_purges_and_refreshed_views_once_each() {
        let purged = vec!["autumn_jobs".to_owned(), "shared".to_owned()];
        let refreshed = vec!["shared".to_owned(), "big_report".to_owned()];
        assert_eq!(
            super::also_measured(&purged, &refreshed),
            vec![
                "autumn_jobs".to_owned(),
                "big_report".to_owned(),
                "shared".to_owned()
            ],
            "every relation the run touches outside the plan, and each one once"
        );
        assert!(
            super::also_measured(&[], &[]).is_empty(),
            "and an unsampled target with neither measures nothing extra"
        );
    }

    #[test]
    fn the_connect_boundary_carries_the_whole_endpoint_and_no_password() {
        // A bare `\\connect dbname` inherits host, port and user, so a fleet whose
        // shards share a database name on different servers would keep running
        // against the first one while looking like it had moved.
        let line = super::psql_connect(
            "control",
            "postgres://scrubby:hunter2@db1.internal:6543/app",
        )
        .join("\n");
        assert!(
            line.starts_with(r#"\connect -reuse-previous=off ""#),
            "the boundary must inherit nothing from the previous connection: {line}"
        );
        assert!(
            !line.contains("hunter2"),
            "the password must never reach the printed script: {line}"
        );
        assert!(
            line.contains("db1.internal") && line.contains("6543") && line.contains("app"),
            "and the endpoint must survive: {line}"
        );

        // The whole string is passed through, not rebuilt from parts: a query
        // parameter overrides the authority component it duplicates, so an
        // endpoint reassembled from the URL's own host would name the wrong
        // database on the header of destructive SQL.
        let overridden = super::psql_connect(
            "control",
            "postgres://authority/app?host=queryhost&dbname=copy",
        )
        .join("\n");
        assert!(
            overridden.contains("host=queryhost") && overridden.contains("dbname=copy"),
            "an override must survive into the printed boundary: {overridden}"
        );

        // A query value is re-encoded the way libpq reads one, not the way a
        // web form does. `url::Url::query_pairs()` applies
        // `x-www-form-urlencoded` rules and `query_pairs_mut()` writes a space
        // back as `+`; libpq only ever percent-decodes, so it reads that `+`
        // literally. Measured against psql 16.13, the operator's own value
        // connects and the `+` form does not:
        //
        //   ?options=-c%20search_path%3Dpg_catalog   -> search_path = pg_catalog
        //   ?options=-c+search_path%3Dpg_catalog     -> FATAL: unrecognized
        //                                               configuration parameter
        //                                               "+search_path"
        //
        // A boundary that cannot connect is the exact case the target guard
        // exists for — psql keeps the PREVIOUS connection — so printing one is
        // not a cosmetic defect.
        let spaced = super::psql_connect(
            "control",
            "postgres://u@h/app?options=-c%20search_path%3Dpg_catalog&password=hunter2",
        )
        .join("\n");
        assert!(
            !spaced.contains('+'),
            "a space must be percent-encoded, never written as `+`: {spaced}"
        );
        assert!(
            spaced.contains("%20"),
            "and it must still be there, not dropped: {spaced}"
        );
        assert!(
            !spaced.contains("hunter2") && !spaced.contains("password"),
            "while the credential is still removed: {spaced}"
        );

        // Keyword form is declined outright rather than tokenized. `libpq`
        // allows whitespace around the `=` and single-quoted values with
        // backslash escapes, so a whitespace split reads `password = secret` as
        // three unrelated tokens and prints the credential — which is exactly
        // what two successive tokenizers here got wrong.
        for keyword in [
            "host=db2.internal dbname=app password=hunter2",
            "host=db2.internal dbname=app password = hunter2",
            "host=db2 password='two words' dbname=app",
        ] {
            let line = super::psql_connect("control", keyword).join("\n");
            assert!(
                !line.contains("hunter2") && !line.contains("two words"),
                "no keyword form may print its password: {line}"
            );
            assert!(
                !line.contains("\\connect"),
                "and none may claim to move the session: {line}"
            );
            assert!(
                line.contains("control"),
                "the operator must still be told which target it was: {line}"
            );
            // And it must STOP the paste, not merely advise. The dry run refuses
            // before printing anything for such a target, so this branch is
            // unreachable in practice; if that guard is ever bypassed, a comment
            // would let a destructive block run against the previous target and
            // `\quit` will not.
            assert!(
                line.contains("\\quit"),
                "a boundary that cannot be printed must halt psql: {line}"
            );
        }

        // A URI carries the password two ways, and the query form is the
        // EFFECTIVE one where both appear — so clearing the userinfo alone
        // prints the credential that actually authenticates.
        let in_query =
            super::psql_connect("control", "postgres://bob@db/app?password=secret2").join("\n");
        assert!(
            !in_query.contains("secret2"),
            "a query-string password must be stripped too: {in_query}"
        );
        let both = super::psql_connect(
            "control",
            "postgres://alice:secret1@db/app?password=secret2&sslpassword=k3y&application_name=x",
        )
        .join("\n");
        assert!(
            !both.contains("secret1") && !both.contains("secret2") && !both.contains("k3y"),
            "every credential form must go: {both}"
        );
        assert!(
            both.contains("application_name=x"),
            "and every non-secret parameter must stay: {both}"
        );
    }

    #[test]
    fn the_session_pins_cover_every_rendering_the_row_key_depends_on() {
        // `--seed` promises the same seed over the same source selects the same
        // rows, and the key is hashed from `key::text`. Measured: a `date`
        // primary key selects a different subset under `DateStyle = ISO, YMD`
        // than under `Postgres, DMY`.
        let pinned = super::session_settings().join("; ");
        for setting in [
            "search_path",
            "standard_conforming_strings",
            "DateStyle",
            "IntervalStyle",
            "TimeZone",
            "bytea_output",
            "extra_float_digits",
            "lc_monetary",
        ] {
            assert!(
                pinned.contains(setting),
                "{setting} changes how a value renders, so it must be pinned: {pinned}"
            );
        }
        assert!(
            super::session_settings()
                .iter()
                .all(|s| s.starts_with("SET LOCAL ")),
            "and every pin must be transaction-scoped: {pinned}"
        );
    }

    // ── Which triggers a statement can actually fire ────────────────────────

    #[test]
    fn rules_are_asked_for_by_the_same_rules_but_without_the_walk() {
        let sql = rules_reaching("AND r.ev_type = '4' ");
        // Measured on PostgreSQL 16: a rule on a leaf partition or an
        // inheritance child does NOT fire for a statement naming the parent,
        // because rewriting happens against the relation the query names. So
        // this must not grow the ancestry walk its trigger counterpart needs.
        assert!(
            !sql.contains("pg_inherits"),
            "a rule fires only on the relation named, so no walk: {sql}"
        );
        assert!(
            sql.contains("r.ev_enabled IN ('O', 'A')"),
            "a disabled rule cannot fire either: {sql}"
        );
        assert!(
            sql.contains("r.rulename <> '_RETURN'"),
            "every view's own SELECT rule must be excluded: {sql}"
        );
        assert!(
            sql.contains("AND r.ev_type = '4'"),
            "the caller's event filter must reach the query: {sql}"
        );
        assert!(
            !rules_reaching("").contains("ev_type"),
            "and an empty filter must not smuggle one in"
        );
    }

    #[test]
    fn only_row_triggers_propagate_up_the_inheritance_tree() {
        let sql = triggers_reaching("AND (t.tgtype & 8) <> 0 ");
        // Measured on PostgreSQL 16, both inheritance flavours: `DELETE FROM
        // parent` fires a child's ROW trigger and not its STATEMENT trigger. A
        // statement trigger must therefore mark only the table it is on, or the
        // run refuses over a trigger that cannot execute.
        assert!(
            sql.contains("(t.tgtype & 1) <> 0 AS by_row"),
            "the seed must record whether each trigger is row-level: {sql}"
        );
        assert!(
            sql.contains("JOIN ancestry a ON a.oid = i.inhrelid AND a.by_row"),
            "and only row-level ones may propagate to an ancestor: {sql}"
        );
        // A disabled trigger cannot fire, and disabling one is the remedy both
        // the warning and the refusal recommend.
        assert!(
            sql.contains("t.tgenabled IN ('O', 'A')"),
            "a disabled trigger must not count: {sql}"
        );
        assert!(
            sql.contains("AND (t.tgtype & 8) <> 0"),
            "the caller's event filter must reach the seed: {sql}"
        );
        // The warning asks the same question without an event filter.
        assert!(
            !triggers_reaching("").contains("tgtype & 8"),
            "and an empty filter must not smuggle one in"
        );
    }

    // ── Printed assertions survive hostile identifiers ──────────────────────

    #[test]
    fn an_assertion_delimiter_cannot_be_closed_by_an_identifier() {
        // Postgres permits `$` in a quoted identifier, and dollar quoting is
        // lexical — a fixed `DO $$ ... $$` around a query naming `"us$$ers"`
        // closes mid-statement, so the sequence the operator was told to paste
        // is a syntax error rather than the check it advertises.
        let check = r#"SELECT count(*) FROM "public"."us$$ers""#;
        let sql = integrity_assertion(check);
        assert!(
            !sql.starts_with("DO $$ "),
            "the delimiter must come from the body, not a constant: {sql}"
        );
        assert!(sql.contains(check), "the check itself must survive: {sql}");
        let tag = sql
            .split_whitespace()
            .nth(1)
            .expect("the block must open with `DO <tag>`");
        assert_eq!(
            sql.matches(tag).count(),
            2,
            "the tag must appear exactly twice — opening and closing: {sql}"
        );
        assert!(sql.ends_with(tag), "and must close the block: {sql}");
    }

    #[test]
    fn an_emptiness_assertion_delimiter_widens_the_same_way() {
        let sql = emptiness_assertion("jobs$autumn_walk$queue");
        let tag = sql
            .split_whitespace()
            .nth(1)
            .expect("the block must open with `DO <tag>`");
        assert_ne!(
            tag, "$autumn_walk$",
            "a table name carrying the default tag must push it wider: {sql}"
        );
        assert_eq!(sql.matches(tag).count(), 2, "opened and closed once: {sql}");
    }

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
    fn config_parses_the_sample_rules() {
        let config = parse_config_str(
            r#"
            [sample]
            always_include = ["countries"]
            never_include = ["audit_logs"]
            "#,
        )
        .unwrap();
        assert_eq!(config.sample.always_include, vec!["countries".to_owned()]);
        assert_eq!(config.sample.never_include, vec!["audit_logs".to_owned()]);
        assert!(sources_declare_sampling(&config));
        assert!(!sources_declare_sampling(&ScrubConfig::default()));
    }

    #[test]
    fn an_unknown_sample_key_is_refused_rather_than_ignored() {
        // A typo that silently did nothing would leave a table subsetted the
        // operator believed was excluded.
        let err = parse_config_str(
            r#"
            [sample]
            allways_include = ["countries"]
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ScrubError::Config { .. }));
    }

    #[test]
    fn percentages_report_n_a_rather_than_dividing_by_zero() {
        assert_eq!(percent_of(0, 0), "n/a");
        assert_eq!(percent_of(2, 200), "1.0%");
        assert_eq!(percent_of(200, 200), "100.0%");
    }

    #[test]
    fn byte_sizes_read_at_human_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 kB");
        assert_eq!(human_bytes(1024 * 1024 * 3), "3.0 MB");
    }

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
            partitions: BTreeMap::from([("users_2026_01".to_owned(), "users".to_owned())]),
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
