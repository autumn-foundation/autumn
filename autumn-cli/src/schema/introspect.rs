//! Postgres database introspection into the [`autumn_schema_core`] schema IR —
//! the read-side of `autumn schema pull` (the first DB-introspection slice of
//! tracking issue #1975).
//!
//! Where [`crate::schema::parse`] lifts the **declared** `#[model]` structs into
//! the IR (the desired state), this module lifts the **live database** into the
//! same IR — a [`Table`] per public base table, with columns, primary keys,
//! unique/plain indexes, and single-column foreign keys — so a pulled snapshot
//! and the model-derived snapshot are directly diffable by the same
//! [`crate::schema::diff`] engine.
//!
//! # Design notes / fidelity
//!
//! - **Centralized type mapping**: every column type flows through
//!   [`ColumnType::from_pg_introspection`], which preserves an unmappable
//!   Postgres type as [`ColumnType::Opaque`] rather than dropping the column —
//!   introspection never silently loses a column.
//! - **Serial/UUID PK round-trip**: a single-column integer PK whose default is a
//!   `nextval(...)` sequence **that the column owns** is a `BIGSERIAL`-shaped id,
//!   which the model IR represents as an [`Int64`](ColumnType::Int64) PK with **no**
//!   default — so the `nextval` default is stripped to `None`. Ownership is required:
//!   a brownfield int8 PK backed by a *shared/custom* sequence it does not own
//!   (`id BIGINT PRIMARY KEY DEFAULT nextval('global_ids')`) keeps its raw default
//!   verbatim, so recreation continues allocating from that sequence rather than
//!   minting a fresh owned `BIGSERIAL`. A single-column UUID PK keeps its
//!   `gen_random_uuid()` default (the model parser records the same), so the two
//!   agree. See [`normalize_default`] / [`normalize_serial_pk_default`].
//! - **Uniqueness**: a single-column unique index sets the owning column's
//!   `unique` flag *and* is recorded as an [`Index`] (`unique = true`), mirroring
//!   how the model parser represents a `#[unique]` field, so a round-trip diff is
//!   empty. A *constraint-owned* unique index — the auto-created index behind a
//!   brownfield column/table-level `UNIQUE`/`EXCLUDE` constraint, which the
//!   declarative model DSL cannot express (it models uniqueness as unique
//!   *indexes*, not constraints) and which Postgres refuses to `DROP INDEX`
//!   independently of its constraint — is instead **retained verbatim** via its
//!   [`Index::definition`] (exactly like an expression/partial index), so a
//!   model-side diff never emits a rejected `DROP INDEX`. See [`collapse_indexes`].
//!
//! # Deferred blind spots (documented, not silently dropped)
//!
//! - **Long-identifier unique index names**: Postgres truncates (or hashes) an
//!   index/constraint identifier to `NAMEDATALEN` (63 bytes), but the model parser
//!   emits the un-truncated derived name. So a `#[unique]` field with a long name
//!   pulls back under the truncated DB identifier while the model side keeps the
//!   full one — a pre-existing parser gap that makes a round-trip diff non-empty
//!   for long field names. Introspection records the DB's identifier faithfully;
//!   reconciling the two names is a later slice.
//! - **Composite index key order**: multi-column index columns are ordered by
//!   `attnum`, not the true index key order. The model parser only ever emits
//!   single-column indexes (exact here), so this affects only hand-authored
//!   multi-column indexes; recording them faithfully but with attnum ordering is
//!   preferred over dropping them.
//! - **Operator classes / collations / ordering on *simple* indexes**: a plain
//!   column index with a non-default operator class, collation, or `ASC`/`DESC`
//!   ordering is still recorded by its columns (matching what the model parser
//!   emits), so those attributes are not separately normalized. Expression and
//!   partial indexes do not share this gap — they round-trip verbatim through
//!   [`pg_get_indexdef`](https://www.postgresql.org/docs/current/functions-info.html)
//!   (see [`Index::definition`]), so their operator classes/collations/ordering
//!   are preserved exactly.
//! - **Covering (`INCLUDE`) indexes are retained by definition**: a covering
//!   index (`CREATE UNIQUE INDEX idx ON t (a) INCLUDE (b)`, detected via
//!   `pg_index.indnkeyatts < indnatts`) is *not* a simple droppable index — its
//!   uniqueness scope is only the KEY column(s) `(a)`, but the `indkey`-derived
//!   column list mixes key and `INCLUDE` columns indistinguishably, and the model
//!   DSL cannot express the split. It is retained verbatim via
//!   [`Index::definition`] (like an expression/partial/constraint index) rather
//!   than rebuilt as a scope-widening `(a, b)` unique index, and never sets a
//!   single-column `unique` flag. See [`collapse_indexes`].
//! - **Composite / multi-column foreign keys**: only the first
//!   referencing/referenced column pair is recorded (the IR [`ForeignKey`] is
//!   single-column). The model parser never emits a composite FK.
//! - **Foreign-key / constraint names**: the IR [`ForeignKey`] carries only
//!   `table`/`column`, so the Postgres constraint name is not represented (and so
//!   never diffed).
//! - **Enum `CHECK` constraints**: enum recovery from `CHECK` expressions is a
//!   later slice; a `TEXT`-with-`CHECK` column pulls back as plain
//!   [`Text`](ColumnType::Text).
//! - **Non-PK `SERIAL`/`BIGSERIAL` owned sequences**: a non-primary-key
//!   `SERIAL`/`BIGSERIAL` column is preserved with its raw
//!   `nextval('..._seq'::regclass)` default so the pulled snapshot and `doctor`
//!   drift are faithful, but its owned sequence is not modeled; on rollback of a
//!   dropped table/column the down migration would fail with a missing-relation
//!   error rather than recreate the sequence. Owned-sequence / serial-identity
//!   modeling is deferred to a later slice (alongside views/triggers). The forward
//!   pull/doctor path is unaffected — it already round-trips the raw default
//!   verbatim; only the down-migration recreation of the sequence is out of scope.
//! - **Constraint-vs-index reconciliation for constraint-owned indexes**: an
//!   **EXCLUDE** constraint (`pg_constraint.contype = 'x'`) IS preserved as the real
//!   constraint: its retained `definition` is the
//!   `ALTER TABLE … ADD CONSTRAINT <name> <pg_get_constraintdef>` form
//!   (`EXCLUDE USING <method> (…)`), so a recreation
//!   re-establishes the actual exclusion rather than a plain index that would fail
//!   to enforce it (which would let overlapping rows through — a data-integrity
//!   loss). A brownfield `UNIQUE` (or PK) constraint-owned index, by contrast, is
//!   still retained by its *index* definition (`pg_get_indexdef`, a valid
//!   `CREATE UNIQUE INDEX …`), not by the originating
//!   `ALTER TABLE … ADD CONSTRAINT` — a recreation emits the `CREATE UNIQUE INDEX` form, which enforces
//!   the same uniqueness, so exact constraint-*object* reconstruction for
//!   unique/PK constraints remains the documented deferral. A model that
//!   *also* declares `#[unique]` on a column already covered by a brownfield
//!   `UNIQUE` constraint is **recognized as satisfied** by the existing unique
//!   index: `diff_indexes` (model-diff path) treats a desired unique index whose
//!   exact column set is already enforced by any baseline `unique` index — including
//!   the retained constraint index — as fulfilled, so it emits **no** redundant
//!   `AddIndex` (and no `DropIndex`), and the plan is clean rather than tripping
//!   `guard_plan`'s unique-index dedup refusal. (This resolves the earlier
//!   "additively creates a redundant `idx_<table>_<col>_unique` index" caveat.)
//!   Full unique/PK constraint↔index reconciliation is otherwise deferred; the
//!   load-bearing property this slice guarantees is only that the generated
//!   migration no longer *fails* with a rejected `DROP INDEX`.
//! - **Cascade-dropped retained indexes on a dropped column**: when a model
//!   removes a column that a retained (`definition`-carrying) expression / partial
//!   / constraint-owned index depends on, Postgres cascade-drops that index with
//!   the `ALTER TABLE … DROP COLUMN` (the up path emits no `DROP INDEX`). The
//!   snapshot projection prunes it to match (so `doctor` / `pull --dry-run` do not
//!   false-drift) and the down migration recreates it from its verbatim
//!   `definition`. Whether a column is *depended on* is now detected **exactly**:
//!   a retained index carries in its [`Index::columns`] the precise `pg_depend`
//!   dependent-column set (key, expression-referenced, AND predicate columns) —
//!   the authoritative cascade set Postgres itself uses — so the diff engine tests
//!   dependency by exact set membership, never a `definition`-text scan (which
//!   could false-positive on a column name appearing in a string literal). One
//!   nuance remains deferred: a cascade-removed brownfield `UNIQUE` (or PK)
//!   *constraint* is restored on rollback as a unique *index* (via its
//!   `pg_get_indexdef` definition), not as the original named `CONSTRAINT` —
//!   functional uniqueness is preserved, exact constraint-object reconstruction is
//!   deferred. (An **EXCLUDE** constraint is not subject to this deferral: it is
//!   restored as the real `ADD CONSTRAINT … EXCLUDE USING …`, per the reconciliation
//!   note above.)
//! - **CHECK constraint `NO INHERIT` / `NOT VALID` suffixes**: dropped on
//!   recreation (the predicate is preserved; the suffix is not modeled).
//! - **`GENERATED ... AS IDENTITY` columns**: the identity generation clause
//!   (`information_schema.columns.is_identity` / `identity_generation`) is now
//!   **preserved verbatim** on [`Column::identity`] so the column round-trips a
//!   pull rather than flattening to a plain integer column. (Emitting the identity
//!   clause back out into recreated DDL is a later slice — the IR preserves it, the
//!   DDL emitter does not yet re-render it.)
//! - **PG15+ `NULLS NOT DISTINCT` unique indexes**: now **retained verbatim** via
//!   [`Index::definition`] — detected in the `pg_get_indexdef` text (version-safe on
//!   PG13/14, which never emit the clause) rather than via the PG15-only
//!   `pg_index.indnullsnotdistinct` catalog column — so the clause round-trips.
//! - **Stored/virtual `GENERATED ALWAYS AS (...) STORED` generated columns**: pulled
//!   as ordinary columns; the generation expression
//!   (`information_schema.columns.generation_expression`) is not modeled, so
//!   recreation drops the computed-value behavior. Deferred alongside
//!   identity/owned-sequence modeling.
//!
//! Errors are **credential-safe** by construction: no variant ever embeds the
//! resolved database URL (only a parsed host/port on the connection-error path,
//! mirroring [`crate::db_pull`]).

use std::collections::BTreeMap;

use autumn_schema_core::{
    Backend, CheckConstraint, Column, ColumnDefault, ColumnType, ForeignKey, Index, SerialKind,
    Table,
};
use diesel::{Connection as _, PgConnection, QueryableByName, RunQueryDsl as _, sql_query};

/// Failure modes for Postgres introspection. `Display` is credential-safe: the
/// resolved URL is never embedded — only a parsed host/port on the connection
/// path, and server-sourced messages on the query path.
#[derive(Debug, thiserror::Error)]
pub enum IntrospectError {
    /// The database could not be connected to. Carries only a parsed host/port,
    /// never credentials.
    #[error(
        "could not connect to Postgres at {host}:{port} — is the server running and reachable?"
    )]
    Connection {
        /// The parsed host (defaulting to `localhost` when unparseable).
        host: String,
        /// The parsed port (defaulting to `5432` when unparseable).
        port: u16,
    },
    /// A catalog query failed. The message comes from the server, not the URL.
    #[error("database introspection query failed: {0}")]
    Query(String),
    /// The `SQLite` database file could not be opened. Carries only the parsed
    /// file path (a `SQLite` DSN carries no credentials), never the raw URL.
    #[cfg(feature = "sqlite")]
    #[error("could not open the SQLite database at {path} — is the file present and readable?")]
    ConnectionSqlite {
        /// The resolved `SQLite` file path (`:memory:` for an in-memory target).
        path: String,
    },
}

/// Introspect the `public` schema of the Postgres database at `url` into a set of
/// [`Table`]s tagged [`Backend::Postgres`] and marked `managed`.
///
/// Framework-owned tables (the `autumn_*` / `_autumn*` prefixes plus Diesel's
/// bookkeeping table) are excluded, mirroring [`crate::db_pull`]'s unscoped-pull
/// rule, so a pulled snapshot describes only the app's own tables.
///
/// # Errors
///
/// Returns [`IntrospectError::Connection`] if the database is unreachable, or
/// [`IntrospectError::Query`] if a catalog query fails. Neither ever leaks the
/// database URL.
pub fn introspect_postgres(url: &str) -> Result<Vec<Table>, IntrospectError> {
    // `PgConnection::establish` accepts both URL and libpq key-value DSNs; defer
    // URL parsing to the error path so a valid key-value string still connects
    // (host/port are only needed for a credential-safe message). Mirrors db_pull.
    let mut conn = PgConnection::establish(url).map_err(|_| {
        let (host, port) = parse_host_port(url).unwrap_or_else(|| ("localhost".to_owned(), 5432));
        IntrospectError::Connection { host, port }
    })?;

    let table_names = list_tables(&mut conn)?;
    if table_names.is_empty() {
        return Ok(Vec::new());
    }

    // Batched catalog reads — a constant number of queries regardless of table
    // count (never one query per table).
    let columns_by_table = fetch_columns(&mut conn, &table_names)?;
    let pks_by_table = fetch_primary_keys(&mut conn, &table_names)?;
    let indexes_by_table = fetch_indexes(&mut conn, &table_names)?;
    let fks_by_table = fetch_foreign_keys(&mut conn, &table_names)?;
    let checks_by_table = fetch_checks(&mut conn, &table_names)?;

    let mut tables = Vec::with_capacity(table_names.len());
    for name in &table_names {
        tables.push(build_table(
            name,
            columns_by_table.get(name).map_or(&[][..], Vec::as_slice),
            pks_by_table.get(name).map_or(&[][..], Vec::as_slice),
            indexes_by_table.get(name).map_or(&[][..], Vec::as_slice),
            fks_by_table.get(name).map_or(&[][..], Vec::as_slice),
            checks_by_table.get(name).map_or(&[][..], Vec::as_slice),
        ));
    }
    Ok(tables)
}

/// Parse host/port from a connection URL for credential-safe error messages.
fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str().unwrap_or("localhost").to_owned();
    let port = parsed.port().unwrap_or(5432);
    Some((host, port))
}

// ---------------------------------------------------------------------------
// Catalog row shapes
// ---------------------------------------------------------------------------

#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(QueryableByName)]
struct ColumnRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    column_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    udt_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    is_nullable: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    column_default: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    numeric_precision: Option<i32>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    numeric_scale: Option<i32>,
    /// The declared length limit for a `varchar(n)` / `char(n)` column (`NULL` for
    /// an unbounded `varchar`, `text`, or any non-character type). When set on a
    /// non-domain `varchar`/`bpchar` column it is preserved verbatim as
    /// [`ColumnType::Opaque`] so the DB-enforced length survives recreation rather
    /// than being flattened to an unconstrained `TEXT`.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    character_maximum_length: Option<i32>,
    /// The Postgres DOMAIN name backing this column, or `NULL` when the column's
    /// type is not a domain. When set, the column's type is preserved verbatim as
    /// [`ColumnType::Opaque`] (schema-qualified via `domain_schema`) so the
    /// domain's identity/validation is never flattened to its base `udt_name`.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    domain_name: Option<String>,
    /// The schema owning the column's domain (`NULL` for a non-domain column).
    /// Only qualifies the emitted type when it is not `public`.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    domain_schema: Option<String>,
    /// Whether this column **owns the same sequence its `nextval(...)` default
    /// allocates from** — the conventional `SERIAL`/`BIGSERIAL` shape, where the
    /// sequence is both `OWNED BY` the column (`pg_depend` `deptype = 'a'`) AND the
    /// sequence the column's default expression references (`pg_attrdef`
    /// `deptype = 'n'`). A `nextval(...)` default over a *shared*/custom sequence the
    /// column does not own — OR a column that still owns an OLD sequence but whose
    /// default was repointed to a DIFFERENT one — reports `false`, so its raw default
    /// is preserved verbatim rather than collapsed to a fresh owned `BIGSERIAL` on
    /// recreation (which would silently change ID-allocation behavior — see
    /// [`normalize_serial_pk_default`]). Detected via long-standing catalogs only
    /// (`pg_depend` / `pg_attrdef` / `pg_class` / `pg_attribute`), so it is
    /// version-safe on every supported Postgres (no PG15+ catalog columns).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    owns_sequence: bool,
    /// `information_schema.columns.is_identity` (`'YES'`/`'NO'`) — whether the
    /// column is declared `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`. An
    /// identity column has no `column_default` (its value is minted by an internal
    /// sequence Postgres owns), so without this flag it would pull back as an
    /// ordinary integer column and silently lose its identity behavior. Version-safe:
    /// `is_identity` has existed since PG10 (identity columns were introduced there).
    #[diesel(sql_type = diesel::sql_types::Text)]
    is_identity: String,
    /// `information_schema.columns.identity_generation` (`'ALWAYS'` / `'BY DEFAULT'`,
    /// or `NULL` for a non-identity column) — preserved verbatim on
    /// [`Column::identity`] so the identity clause round-trips through a pull.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    identity_generation: Option<String>,
}

#[derive(QueryableByName)]
struct TablePkRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    column_name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    ord: i32,
}

/// One row per non-primary index (never one-per-column, so an expression index —
/// whose `indkey` carries a `0` with no matching `pg_attribute` row — is never
/// dropped). `columns` is a comma-joined list of the *resolvable* ordinary
/// columns (the `indkey` entries `> 0`) in true key order; it is empty for an
/// index whose keys are all expressions. `definition` is the verbatim
/// `pg_get_indexdef` statement, preserved for expression/partial indexes.
// A flat `QueryableByName` catalog-row DTO: each bool maps to one `pg_index`
// classification column (`indisunique`, `indpred IS NOT NULL`, expression-key,
// constraint-owned), so they are independent flags, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(QueryableByName)]
struct IndexRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    index_name: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    is_unique: bool,
    /// The full `CREATE [UNIQUE] INDEX …` statement (no trailing `;`).
    #[diesel(sql_type = diesel::sql_types::Text)]
    definition: String,
    /// Whether the index has a `WHERE` predicate (`indpred IS NOT NULL`).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    is_partial: bool,
    /// Whether any key column is an expression (an `indkey` entry equal to `0`).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    has_expression: bool,
    /// Whether the index is a *covering* index — i.e. it carries non-key
    /// `INCLUDE` columns (`pg_index.indnkeyatts < indnatts`). Such an index's
    /// uniqueness scope is only its KEY columns, but its `indkey`-derived
    /// `columns` list mixes key and INCLUDE columns indistinguishably, so it is
    /// not expressible by the model DSL and must be retained verbatim via its
    /// `definition` (never rebuilt as a plain `(key, include)` unique index).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    has_include: bool,
    /// Whether the index backs a constraint (`pg_constraint.conindid` points at
    /// it) — true for UNIQUE/EXCLUDE (and PK, but PKs are already filtered out).
    /// Such an index cannot be dropped independently of its constraint, and the
    /// model DSL cannot express it, so it is retained verbatim.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    is_constraint: bool,
    /// The backing constraint's `pg_constraint.contype` (`'u'` unique, `'x'`
    /// EXCLUDE), or `''` when the index backs no constraint. An `'x'` EXCLUDE index
    /// must be retained as its real `ADD CONSTRAINT` (not just the backing
    /// `CREATE INDEX`), because a plain index does not enforce the exclusion — see
    /// [`collapse_indexes`].
    #[diesel(sql_type = diesel::sql_types::Text)]
    constraint_type: String,
    /// The backing constraint's name (`pg_constraint.conname`), or `''` when the
    /// index backs no constraint. Used to build the `ALTER TABLE … ADD CONSTRAINT
    /// <name> …` recreation for an EXCLUDE constraint.
    #[diesel(sql_type = diesel::sql_types::Text)]
    constraint_name: String,
    /// The backing constraint's canonical definition (`pg_get_constraintdef`), or
    /// `''` when the index backs no constraint. For an EXCLUDE constraint this is
    /// `EXCLUDE USING <method> (…)`, which the `ALTER TABLE … ADD CONSTRAINT` form
    /// wraps into a valid, exclusion-enforcing recreation statement.
    #[diesel(sql_type = diesel::sql_types::Text)]
    constraint_def: String,
    /// Comma-joined resolvable ordinary *key* column names, in key order (may be
    /// empty for an all-expression index). Used to populate a *simple* index's
    /// `Index.columns` for round-trip parity.
    #[diesel(sql_type = diesel::sql_types::Text)]
    columns: String,
    /// Comma-joined **key** column names in key order — the first `indnkeyatts`
    /// index attributes, EXCLUDING any non-key `INCLUDE` columns, and **empty when
    /// any key position is an expression** (an `indkey` `0` inside the key range).
    /// This is the exact set the index's uniqueness is enforced over: distinct from
    /// `columns` (which folds in `INCLUDE` columns for a covering index) and from
    /// `dep_columns` (the full cascade dependency set). Used to populate
    /// `Index.key_columns` so the diff can decide — offline, with no DB — whether a
    /// baseline unique index actually enforces a model `#[unique]`'s exact column set
    /// (a partial or expression index does not).
    #[diesel(sql_type = diesel::sql_types::Text)]
    key_columns: String,
    /// Comma-joined set of every column this index *depends on* — key columns,
    /// expression-referenced columns, AND predicate (`WHERE`) columns per
    /// `pg_depend`, combined via `UNION` with the backing constraint's `conkey`
    /// columns for a constraint-owned index (whose column dependency routes through
    /// `pg_constraint`, not a direct index→column `pg_depend` row). Together this
    /// is exactly what Postgres cascade-drops with an `ALTER TABLE … DROP COLUMN`.
    /// Sorted by name (deterministic). Used to populate a *definition*-carrying
    /// index's `Index.columns` so the diff engine's column-dependency detection is
    /// exact (no string scan).
    #[diesel(sql_type = diesel::sql_types::Text)]
    dep_columns: String,
}

#[derive(QueryableByName)]
struct ForeignKeyRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    column_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    foreign_table: String,
    /// The schema owning the referenced table. A cross-schema FK target (not
    /// `public`) is stored schema-qualified in [`ForeignKey::table`] so it never
    /// silently retargets a same-named table in `public` on recreation.
    #[diesel(sql_type = diesel::sql_types::Text)]
    foreign_schema: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    foreign_column: String,
}

#[derive(QueryableByName)]
struct CheckRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    constraint_name: String,
    /// The full `pg_get_constraintdef` text (`CHECK ((price >= 0))`), stripped to
    /// the inner predicate (`price >= 0`) at build time to match the emitter's
    /// `CHECK ({expression})` wrapping.
    #[diesel(sql_type = diesel::sql_types::Text)]
    definition: String,
}

// ---------------------------------------------------------------------------
// Catalog probes
// ---------------------------------------------------------------------------

/// List the `public` base tables, excluding Diesel's bookkeeping table and
/// Autumn/Diesel framework-owned tables (mirrors [`crate::db_pull`]'s unscoped
/// rule).
fn list_tables(conn: &mut PgConnection) -> Result<Vec<String>, IntrospectError> {
    let query = "SELECT table_name AS name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         AND table_name <> '__diesel_schema_migrations' \
         ORDER BY table_name";
    let rows: Vec<NameRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|r| r.name)
        .filter(|t| !is_framework_table(t))
        .collect())
}

/// Whether `table` is an Autumn/Diesel framework-owned table an introspection
/// should skip. Kept in lock-step with [`crate::db_pull`]'s `is_framework_table`.
fn is_framework_table(table: &str) -> bool {
    table.starts_with("autumn_")
        || table.starts_with("_autumn")
        || matches!(
            table,
            "api_tokens" | "feature_flag_changes" | "__diesel_schema_migrations"
        )
}

/// Build a comma-separated SQL string-literal list (`'a', 'b'`) for an `IN (..)`
/// clause from catalog-sourced names. `tables` is always non-empty here.
fn quoted_in_list(tables: &[String]) -> String {
    tables
        .iter()
        .map(|t| crate::db::quote_literal(t))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fetch every column (ordinal order) for all `tables` in one query, grouped by
/// table.
fn fetch_columns(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<ColumnRow>>, IntrospectError> {
    // `numeric_precision` / `numeric_scale` / `character_maximum_length` are the
    // `information_schema` `cardinal_number` domain, not a plain `int4`. Decoding
    // that domain as diesel `Nullable<Integer>` on the wire is unproven (no sibling
    // query reads them), and if it does not present as `int4` the whole column query
    // would error and every pull would fail. Cast each to a plain `integer` in SQL so
    // the output type is guaranteed `int4` regardless of the domain. The `AS <name>`
    // aliases keep the result column names aligned with the `ColumnRow` field bindings.
    //
    // `owns_sequence` is an `EXISTS` over `pg_depend` that is `true` only when the
    // column OWNS the SAME sequence its `nextval(...)` default allocates from — the
    // conventional `SERIAL`/`BIGSERIAL` shape. It ties TWO dependencies to one
    // sequence:
    //   * OWNED BY (`deptype = 'a'`): a `pg_depend` row linking a sequence
    //     (`pg_class.relkind = 'S'`, as `objid`) to this exact column
    //     (`refobjid = <table oid>`, `refobjsubid = <column attnum>`).
    //   * used-by-default (`deptype = 'n'`): a `pg_depend` row from the column's
    //     default expression (`pg_attrdef`, `classid = 'pg_attrdef'::regclass`) to
    //     that SAME sequence (`refobjid = seq.oid`).
    // Requiring BOTH on the same sequence closes the brownfield hole where a column
    // still owns an old sequence (`t_id_seq`, `deptype = 'a'`) but its default was
    // repointed to a different one (`DEFAULT nextval('global_ids')`): the used-by
    // dependency then targets `global_ids`, not the owned `t_id_seq`, so `EXISTS` is
    // `false`, the raw `nextval('global_ids')` default is preserved verbatim (never
    // stripped to a fresh owned `BIGSERIAL`), and the column classifies as
    // `Some(Plain)`. A plain shared/custom-sequence PK (owns nothing) likewise has no
    // `deptype = 'a'` row → `false` → preserved. `pg_depend`/`pg_attrdef` exist in
    // every supported Postgres, so this never breaks older servers.
    let query = format!(
        "SELECT c.table_name, c.column_name, c.udt_name, c.is_nullable, c.column_default, \
         c.numeric_precision::integer AS numeric_precision, \
         c.numeric_scale::integer AS numeric_scale, \
         c.character_maximum_length::integer AS character_maximum_length, \
         c.domain_name, c.domain_schema, \
         c.is_identity, c.identity_generation, \
         EXISTS ( \
           SELECT 1 \
           FROM pg_class tbl \
           JOIN pg_namespace tn ON tn.oid = tbl.relnamespace \
           JOIN pg_attribute att \
             ON att.attrelid = tbl.oid AND att.attname = c.column_name \
           JOIN pg_attrdef ad ON ad.adrelid = tbl.oid AND ad.adnum = att.attnum \
           JOIN pg_depend owned \
             ON owned.refobjid = tbl.oid AND owned.refobjsubid = att.attnum \
             AND owned.refclassid = 'pg_class'::regclass \
             AND owned.classid = 'pg_class'::regclass \
             AND owned.deptype = 'a' \
           JOIN pg_class seq ON seq.oid = owned.objid AND seq.relkind = 'S' \
           JOIN pg_depend usedby \
             ON usedby.objid = ad.oid AND usedby.classid = 'pg_attrdef'::regclass \
             AND usedby.refobjid = seq.oid AND usedby.refclassid = 'pg_class'::regclass \
             AND usedby.deptype = 'n' \
           WHERE tn.nspname = 'public' \
             AND tbl.relname = c.table_name \
             AND att.attnum > 0 \
         ) AS owns_sequence \
         FROM information_schema.columns c \
         WHERE c.table_schema = 'public' AND c.table_name IN ({}) \
         ORDER BY c.table_name, c.ordinal_position",
        quoted_in_list(tables)
    );
    let rows: Vec<ColumnRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<ColumnRow>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name.clone())
            .or_default()
            .push(row);
    }
    Ok(by_table)
}

/// Fetch the primary-key columns (in key order) for all `tables`, grouped by
/// table.
fn fetch_primary_keys(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<String>>, IntrospectError> {
    // `array_position(i.indkey::int2[]::int[], a.attnum::int)` recovers the true
    // key position. The `int2vector -> text -> int[]` cast chain is the portable
    // way to turn `indkey` into a subscriptable array for `array_position`.
    let query = format!(
        "SELECT c.relname AS table_name, a.attname AS column_name, \
         array_position(string_to_array(i.indkey::text, ' ')::int[], a.attnum::int) AS ord \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
         WHERE n.nspname = 'public' AND i.indisprimary AND c.relname IN ({}) \
         ORDER BY c.relname, ord",
        quoted_in_list(tables)
    );
    let rows: Vec<TablePkRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    // Rows arrive ordered by (table, key position), so push preserves key order.
    let mut by_table: BTreeMap<String, Vec<(i32, String)>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name)
            .or_default()
            .push((row.ord, row.column_name));
    }
    Ok(by_table
        .into_iter()
        .map(|(table, mut cols)| {
            cols.sort_by_key(|(ord, _)| *ord);
            (table, cols.into_iter().map(|(_, name)| name).collect())
        })
        .collect())
}

/// Fetch every non-primary index (unique and plain) for all `tables`, grouped by
/// table. **One row per index** (never one-per-column), so an **expression
/// index** — whose `indkey` contains a `0` for the expression column with no
/// matching `pg_attribute` row — is preserved rather than dropped by an inner
/// join that returns nothing for it. The resolvable ordinary columns are
/// aggregated in true key order via a correlated `unnest(indkey) WITH
/// ORDINALITY` subquery; `has_expression` flags any `0` key, `is_partial`
/// flags an `indpred`, and `is_constraint` flags an index backing a
/// `pg_constraint` (UNIQUE/EXCLUDE) — with `constraint_type` (`pg_constraint.contype`),
/// `constraint_name` (`conname`), and `constraint_def` (`pg_get_constraintdef`) so an
/// EXCLUDE constraint can be recreated as the real constraint rather than a bare
/// backing index — so the builder can classify each index as
/// simple-representable vs. preserved-verbatim. A separate `key_columns` projection
/// aggregates only the first `indnkeyatts` (true KEY) attributes — excluding
/// `INCLUDE` columns and yielding empty for an expression key — which is the exact
/// set an index's uniqueness is enforced over (used to decide, offline, whether a
/// baseline unique index satisfies a model `#[unique]`). A second correlated subquery
/// aggregates `dep_columns` — the exact set of columns the index depends on (key,
/// expression-referenced, AND predicate columns via **`pg_depend`**, combined via
/// `UNION` with the backing constraint's `conkey` columns for a constraint-owned
/// index), the
/// authoritative cascade set Postgres itself uses for `DROP COLUMN` — so a retained
/// index's column-dependency detection is exact, not a string scan.
fn fetch_indexes(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<IndexRow>>, IntrospectError> {
    // The `int2vector -> text -> int[]` cast chain (mirroring `fetch_primary_keys`)
    // turns `indkey` into a subscriptable/unnestable array. The correlated
    // subquery keeps only `> 0` (ordinary-column) keys and preserves key order
    // via `WITH ORDINALITY`; an all-expression index yields an empty string.
    let query = format!(
        "SELECT t.relname AS table_name, ic.relname AS index_name, \
         i.indisunique AS is_unique, pg_get_indexdef(i.indexrelid) AS definition, \
         (i.indpred IS NOT NULL) AS is_partial, \
         (0 = ANY(string_to_array(i.indkey::text, ' ')::int[])) AS has_expression, \
         (i.indnkeyatts < i.indnatts) AS has_include, \
         EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) AS is_constraint, \
         COALESCE((SELECT c.contype::text FROM pg_constraint c WHERE c.conindid = i.indexrelid LIMIT 1), '') AS constraint_type, \
         COALESCE((SELECT c.conname FROM pg_constraint c WHERE c.conindid = i.indexrelid LIMIT 1), '') AS constraint_name, \
         COALESCE((SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c WHERE c.conindid = i.indexrelid LIMIT 1), '') AS constraint_def, \
         COALESCE(( \
           SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
           FROM unnest(string_to_array(i.indkey::text, ' ')::int[]) \
                WITH ORDINALITY AS k(attnum, ord) \
           JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
           WHERE k.attnum > 0 \
         ), '') AS columns, \
         CASE \
           WHEN 0 = ANY((string_to_array(i.indkey::text, ' ')::int[])[1:i.indnkeyatts]) \
             THEN '' \
           ELSE COALESCE(( \
             SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
             FROM unnest((string_to_array(i.indkey::text, ' ')::int[])[1:i.indnkeyatts]) \
                  WITH ORDINALITY AS k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
           ), '') \
         END AS key_columns, \
         COALESCE(( \
           SELECT string_agg(DISTINCT attname, ',' ORDER BY attname) FROM ( \
             SELECT a.attname \
             FROM pg_depend d \
             JOIN pg_attribute a \
               ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
             WHERE d.classid = 'pg_class'::regclass \
               AND d.objid = i.indexrelid \
               AND d.refclassid = 'pg_class'::regclass \
               AND d.refobjid = i.indrelid \
               AND d.refobjsubid > 0 \
             UNION \
             SELECT a.attname \
             FROM pg_constraint c \
             JOIN unnest(c.conkey) AS ck(attnum) ON TRUE \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ck.attnum \
             WHERE c.conindid = i.indexrelid AND ck.attnum > 0 \
           ) deps \
         ), '') AS dep_columns \
         FROM pg_index i \
         JOIN pg_class t ON t.oid = i.indrelid \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE n.nspname = 'public' AND NOT i.indisprimary AND t.relname IN ({}) \
         ORDER BY t.relname, ic.relname",
        quoted_in_list(tables)
    );
    let rows: Vec<IndexRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<IndexRow>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name.clone())
            .or_default()
            .push(row);
    }
    Ok(by_table)
}

/// Fetch single-column foreign keys for all `tables`, grouped by table. Only the
/// first referencing/referenced column pair is read (see the module blind-spot
/// note); the model parser never emits a composite FK.
fn fetch_foreign_keys(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<ForeignKeyRow>>, IntrospectError> {
    let query = format!(
        "SELECT t.relname AS table_name, att.attname AS column_name, \
         ft.relname AS foreign_table, fn.nspname AS foreign_schema, \
         fatt.attname AS foreign_column \
         FROM pg_constraint con \
         JOIN pg_class t ON t.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class ft ON ft.oid = con.confrelid \
         JOIN pg_namespace fn ON fn.oid = ft.relnamespace \
         JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = con.conkey[1] \
         JOIN pg_attribute fatt ON fatt.attrelid = con.confrelid AND fatt.attnum = con.confkey[1] \
         WHERE con.contype = 'f' AND n.nspname = 'public' AND t.relname IN ({})",
        quoted_in_list(tables)
    );
    let rows: Vec<ForeignKeyRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<ForeignKeyRow>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name.clone())
            .or_default()
            .push(row);
    }
    Ok(by_table)
}

/// Fetch every table-level `CHECK` constraint for all `tables`, grouped by table.
///
/// `contype = 'c'` selects **real** CHECK constraints only — a column's `NOT NULL`
/// is `pg_attribute.attnotnull` (not a check), and a DOMAIN's check has
/// `conrelid = 0`, so scoping the join to the table's own `conrelid` naturally
/// excludes domain checks. `pg_get_constraintdef` renders the constraint's full
/// `CHECK (...)` definition, which `build_table` strips to the inner predicate to
/// match the emitter's `CHECK ({expression})` wrapping.
fn fetch_checks(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<CheckRow>>, IntrospectError> {
    let query = format!(
        "SELECT t.relname AS table_name, con.conname AS constraint_name, \
         pg_get_constraintdef(con.oid) AS definition \
         FROM pg_constraint con \
         JOIN pg_class t ON t.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE con.contype = 'c' AND n.nspname = 'public' AND t.relname IN ({}) \
         ORDER BY t.relname, con.conname",
        quoted_in_list(tables)
    );
    let rows: Vec<CheckRow> = sql_query(query)
        .load(conn)
        .map_err(|e| IntrospectError::Query(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<CheckRow>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name.clone())
            .or_default()
            .push(row);
    }
    Ok(by_table)
}

/// Strip a `pg_get_constraintdef` CHECK definition (`CHECK ((price >= 0))`) to the
/// inner predicate (`price >= 0`) the emitter re-wraps in `CHECK (...)`. Removes the
/// leading `CHECK ` keyword, then extracts **only** the span inside the FIRST
/// balanced outer paren pair — so a trailing suffix such as `NOT VALID` or
/// `NO INHERIT` (which `pg_get_constraintdef` appends after the predicate) is
/// dropped rather than left inside the re-wrapped `CHECK (...)`, where it would
/// produce syntactically invalid / semantically altered DDL on recreation. The
/// dropped suffix is a documented deferral (see the "Deferred blind spots" note):
/// on a recreated (dropped, empty) table a `NOT VALID` check is trivially satisfied
/// and `NO INHERIT` only affects table inheritance, so the re-wrap stays valid and
/// behavior-equivalent. A definition that does not match the expected `CHECK (...)`
/// shape is returned trimmed verbatim (never silently dropped).
fn strip_check_wrapper(definition: &str) -> String {
    let trimmed = definition.trim();
    let Some(rest) = trimmed
        .strip_prefix("CHECK ")
        .or_else(|| trimmed.strip_prefix("CHECK"))
        .map(str::trim_start)
    else {
        return trimmed.to_owned();
    };
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'(') {
        return trimmed.to_owned();
    }
    // Scan the first balanced `(...)` span, tracking paren depth (ignoring parens
    // inside single-quoted string literals, where Postgres escapes an embedded
    // quote by doubling it). The inner predicate is everything between the outer
    // parens; any trailing suffix after the matching close paren is discarded.
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        // `rest[0]` is `(` and `rest[i]` is `)` — both ASCII, so the
                        // slice boundaries fall on char boundaries.
                        return rest[1..i].trim().to_owned();
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    // Unbalanced parens — the input never matched the expected shape.
    trimmed.to_owned()
}

// ---------------------------------------------------------------------------
// IR assembly (pure over the fetched rows)
// ---------------------------------------------------------------------------

/// Assemble one [`Table`] from its pre-fetched catalog rows. Pure — no I/O.
fn build_table(
    name: &str,
    columns: &[ColumnRow],
    primary_key: &[String],
    indexes: &[IndexRow],
    foreign_keys: &[ForeignKeyRow],
    checks: &[CheckRow],
) -> Table {
    let mut table = Table::new(name, Backend::Postgres);
    table.managed = true;
    table.primary_key = primary_key.to_vec();

    // Collapse index rows into IR indexes, keyed by index name (input already
    // sorted by table, index name, attnum). Also derive the per-column `unique`
    // flag from single-column unique indexes (mirroring the model IR).
    let (ir_indexes, unique_columns) = collapse_indexes(indexes);

    // Foreign keys by referencing column name (first pair only).
    let fk_by_column: BTreeMap<&str, &ForeignKeyRow> = foreign_keys
        .iter()
        .map(|fk| (fk.column_name.as_str(), fk))
        .collect();

    for row in columns {
        // Fail-closed floor for Postgres DOMAIN columns: when the column is typed
        // on a domain (`CREATE DOMAIN email_dom AS text CHECK (…)`),
        // `information_schema` reports the base type in `udt_name` (`text`) while
        // naming the domain in `domain_name`. Flattening to the base type would
        // silently drop the domain's identity and validation on recreation, so
        // preserve the domain verbatim as `Opaque` (schema-qualified when not in
        // `public`). `Opaque`'s `sql_type` emits `pg_type` unchanged, so the down
        // migration re-references the still-existing domain type. A non-domain
        // column (`domain_name` NULL) maps exactly as before.
        let ty = row.domain_name.as_ref().map_or_else(
            || {
                ColumnType::from_pg_introspection(
                    &row.udt_name,
                    row.numeric_precision,
                    row.numeric_scale,
                    row.character_maximum_length,
                )
            },
            |domain| {
                let pg_type = match row.domain_schema.as_deref() {
                    Some(schema) if schema != "public" => format!("{schema}.{domain}"),
                    _ => domain.clone(),
                };
                ColumnType::Opaque { pg_type }
            },
        );
        let is_pk = primary_key.iter().any(|c| c == &row.column_name);
        let mut column = Column::new(row.column_name.clone(), ty.clone());
        column.nullable = row.is_nullable.eq_ignore_ascii_case("YES");
        column.primary_key = is_pk;
        column.unique = unique_columns.contains(row.column_name.as_str());
        column.default = normalize_default(
            row.column_default.as_deref(),
            &ty,
            is_pk,
            primary_key,
            row.owns_sequence,
        );
        // Serial-vs-plain marker: a single-column integer PK whose `nextval(...)`
        // default the column OWNS is a `SERIAL`/`BIGSERIAL` id (distinguished by
        // int4 vs int8). A plain manually-assigned `INTEGER`/`BIGINT` PK (no owned
        // sequence) leaves this `None`, so the two are no longer indistinguishable
        // in the IR. Computed from the RAW default (before `normalize_default`
        // strips an int8 owned-serial default to `None`).
        column.serial = serial_kind_for(
            row.column_default.as_deref(),
            &ty,
            is_pk,
            primary_key,
            row.owns_sequence,
            row.is_identity.eq_ignore_ascii_case("YES"),
        );
        // Identity marker: preserve a `GENERATED { ALWAYS | BY DEFAULT } AS
        // IDENTITY` column's generation clause verbatim so it round-trips a pull
        // instead of flattening to a plain integer column. An identity column has
        // no `column_default`, so it never collides with the serial marker.
        if row.is_identity.eq_ignore_ascii_case("YES") {
            column.identity = Some(
                row.identity_generation
                    .clone()
                    .unwrap_or_else(|| "ALWAYS".to_owned()),
            );
        }
        if let Some(fk) = fk_by_column.get(row.column_name.as_str()) {
            // Fail-closed floor for cross-schema FK targets: a FK to a table
            // outside `public` (`REFERENCES auth.users(id)`) must be stored
            // schema-qualified, else `render_column_def` would emit
            // `REFERENCES users(id)` and point at the wrong (or a nonexistent)
            // `public.users`. A `public` target stays bare (the common case,
            // unchanged).
            let foreign_table = if fk.foreign_schema == "public" {
                fk.foreign_table.clone()
            } else {
                format!("{}.{}", fk.foreign_schema, fk.foreign_table)
            };
            column.references = Some(ForeignKey::new(foreign_table, fk.foreign_column.clone()));
        }
        table.columns.push(column);
    }

    table.indexes = ir_indexes;

    // Preserve real `CHECK` constraints (Fix 4): recording them faithfully so a
    // pull never silently drops a `CHECK (price >= 0)`. The definition is stripped
    // to the inner predicate the emitter re-wraps in `CHECK (...)`. Snapshot
    // canonicalization sorts checks by name, so no explicit ordering is needed
    // here (the query already orders by `conname`).
    table.checks = checks
        .iter()
        .map(|c| CheckConstraint {
            name: Some(c.constraint_name.clone()),
            expression: strip_check_wrapper(&c.definition),
        })
        .collect();

    table
}

/// Map one-per-index rows into IR [`Index`]es plus the set of columns covered by
/// a **single-column simple unique** index (used to set the column `unique` flag,
/// matching how the model parser represents `#[unique]`).
///
/// Each index is classified:
/// - **plain/representable** — NOT partial, NO expression key, AND NOT
///   constraint-owned — is built as `Index { columns, unique, definition: None }`
///   exactly as before, so a plain index's serialized JSON is byte-identical to
///   prior snapshots and a single-column plain unique index (the model-DSL
///   `#[unique]` shape, a bare `CREATE UNIQUE INDEX` with no backing constraint)
///   still sets the owning column's `unique` flag for round-trip parity.
/// - **expression, partial, covering (`INCLUDE`), or constraint-owned** —
///   preserved verbatim via
///   `definition: Some(pg_get_indexdef(...))`, never dropped. A covering
///   `INCLUDE` index (`indnkeyatts < indnatts`) is retained because its
///   uniqueness scope is only its KEY columns while the model DSL cannot
///   distinguish key from `INCLUDE` columns — rebuilding it as a plain
///   `(key, include)` unique index would silently widen the uniqueness scope and
///   drop the covering behavior, so it never sets a single-column `unique` flag.
///   A constraint-owned
///   index (a brownfield column/table-level `UNIQUE`/`EXCLUDE`, which auto-creates
///   a `pg_constraint`-backed index Postgres refuses to drop independently, and
///   which the declarative model DSL cannot express) is retained just like an
///   expression/partial index, so a model-side diff never emits a `DROP INDEX`
///   that Postgres would reject. A single-column constraint-owned unique index
///   still sets the owning column's `unique` flag (accurate, and the flag is not
///   diffed) — that classification is independent of constraint ownership.
///
///   An **EXCLUDE** constraint (`pg_constraint.contype = 'x'`) is a special case:
///   its backing `CREATE INDEX` (`pg_get_indexdef`) does NOT enforce the exclusion,
///   so retaining only the plain index would let overlapping rows through on a
///   rollback/recreation (a data-integrity loss). For an EXCLUDE index the retained
///   `definition` is therefore the REAL constraint recreation —
///   `ALTER TABLE <table> ADD CONSTRAINT <conname> <pg_get_constraintdef>` (whose
///   `pg_get_constraintdef` is `EXCLUDE USING <method> (…)`) — so `index_sql`'s
///   verbatim emission restores the actual exclusion constraint. A UNIQUE/PK
///   constraint-owned index keeps its `pg_get_indexdef` (`CREATE UNIQUE INDEX`)
///   recreation, which enforces the same uniqueness (the documented deferral).
fn collapse_indexes(rows: &[IndexRow]) -> (Vec<Index>, std::collections::BTreeSet<String>) {
    let mut indexes = Vec::with_capacity(rows.len());
    let mut unique_columns = std::collections::BTreeSet::new();
    for row in rows {
        let key_columns: Vec<String> = row
            .columns
            .split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        // "Simple" == a single representable column set (not partial, not an
        // expression, and NOT a covering `INCLUDE` index — whose key/INCLUDE
        // columns are indistinguishable in the `indkey`-derived list). A
        // single-column simple unique index sets the owning column's `unique`
        // flag whether or not it backs a constraint (the flag is accurate either
        // way and is never diffed). A covering `INCLUDE` unique index is NOT
        // simple, so it never sets that flag (its uniqueness scope is only its
        // key columns) and is retained verbatim via its `definition`.
        // A PG15+ `NULLS NOT DISTINCT` unique index enforces STRICTER uniqueness
        // than a plain (nulls-distinct) unique index, and the model DSL cannot
        // express it. `pg_get_indexdef` (version-safe on every supported Postgres —
        // it never emits the clause on PG13/14) renders the `NULLS NOT DISTINCT`
        // clause into the `definition` text; detecting it there (rather than via the
        // PG15-only `pg_index.indnullsnotdistinct` catalog column, which would break
        // introspection on PG13/14) forces the index to be retained VERBATIM via its
        // `definition` so the clause round-trips, instead of collapsing to an
        // ordinary unique index that would silently drop it.
        let nulls_not_distinct = row
            .definition
            .to_ascii_uppercase()
            .contains("NULLS NOT DISTINCT");
        let simple =
            !row.is_partial && !row.has_expression && !row.has_include && !nulls_not_distinct;
        if simple && row.is_unique && key_columns.len() == 1 {
            unique_columns.insert(key_columns[0].clone());
        }
        // Retain (preserve verbatim, never drop) any index the model DSL cannot
        // express: an expression, partial, or covering `INCLUDE` index, OR a
        // constraint-owned index (UNIQUE/EXCLUDE) whose backing constraint
        // Postgres won't let us drop. A plain, non-constraint index keeps
        // `definition: None` so its JSON is unchanged and the model round-trip
        // stays clean.
        //
        // For a SIMPLE index `Index.columns` stays the ordered key columns (exact
        // round-trip parity; the `pg_depend` set equals the key columns anyway).
        // For a DEFINITION-carrying index (expression / partial / constraint-owned),
        // `Index.columns` is the exact `pg_depend` dependent-column set — key,
        // expression-referenced, AND predicate columns — the set the diff engine
        // uses for exact cascade/dependency detection (no string scan).
        let (columns, definition, key_cols) = if simple && !row.is_constraint {
            // A plain simple index's key columns ARE its `columns`, so recording
            // `key_columns` separately would be redundant JSON noise — leave it empty
            // and let the diff derive the key set from `columns` for such indexes.
            (key_columns, None, Vec::new())
        } else {
            let dep_columns: Vec<String> = row
                .dep_columns
                .split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect();
            if row.is_constraint && row.constraint_type == "x" {
                // An EXCLUDE constraint (`pg_constraint.contype = 'x'`) is backed by
                // an index, but the backing `CREATE INDEX` (pg_get_indexdef) alone
                // does NOT enforce the exclusion — recreating only the plain index on
                // rollback would silently allow overlapping rows (a data-integrity
                // loss). Retain the REAL constraint instead: `pg_get_constraintdef`
                // yields `EXCLUDE USING <method> (…)`, which the `ALTER TABLE … ADD
                // CONSTRAINT <name> …` form wraps into a valid, exclusion-enforcing
                // statement (`index_sql` emits `definition` verbatim). The table is
                // referenced by its bare name, matching the rest of the emitter
                // (`index_sql`'s plain branch / `CREATE TABLE`). `key_columns` is left
                // empty: an EXCLUDE index is not `unique`, so it never participates in
                // the model `#[unique]` coverage check.
                let definition = format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} {}",
                    row.table_name, row.constraint_name, row.constraint_def
                );
                (dep_columns, Some(definition), Vec::new())
            } else {
                // For a `definition`-carrying index (expression / partial / covering /
                // unique-constraint-owned) the KEY columns are recorded explicitly:
                // they differ from `columns` (which is the full cascade dependency set
                // here) and are EMPTY for an expression key, which is exactly the
                // signal the diff uses to refuse to treat such an index as satisfying a
                // model `#[unique]`.
                let key_cols: Vec<String> = row
                    .key_columns
                    .split(',')
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
                    .collect();
                (dep_columns, Some(row.definition.clone()), key_cols)
            }
        };
        indexes.push(Index {
            name: row.index_name.clone(),
            columns,
            unique: row.is_unique,
            definition,
            is_partial: row.is_partial,
            key_columns: key_cols,
        });
    }
    (indexes, unique_columns)
}

/// Normalize a raw Postgres `column_default` string into a [`ColumnDefault`],
/// matching what the model IR records so a round-trip diff is empty.
///
/// - `NULL` (no default) → `None`.
/// - `now()` / `CURRENT_TIMESTAMP` → [`ColumnDefault::Now`].
/// - a `nextval(...)` serial default on a single-column **int8** primary key whose
///   sequence is **owned** by the column → `None` (a `BIGSERIAL`-shaped id has no
///   explicit default in the model IR; see [`normalize_serial_pk_default`]). An
///   **int4** `SERIAL` PK keeps its `nextval(...)` default so the emitter can
///   reconstruct it as `SERIAL`.
/// - a `nextval(...)` default over a **shared/custom** sequence NOT owned by the
///   column (`owns_sequence == false`) → preserved verbatim as
///   [`ColumnDefault::Sql`], so recreation continues to allocate from that
///   sequence instead of minting a fresh owned `BIGSERIAL`.
/// - anything else → [`ColumnDefault::Sql`] verbatim (e.g. a UUID PK's
///   `gen_random_uuid()`, which the model parser records identically).
fn normalize_default(
    raw: Option<&str>,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
    owns_sequence: bool,
) -> Option<ColumnDefault> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let lowered = raw.to_ascii_lowercase();
    if lowered == "now()" || lowered == "current_timestamp" {
        return Some(ColumnDefault::Now);
    }
    // A serial (`nextval(...)`) default on a single-column integer PK is the
    // BIGSERIAL id shape ONLY when the column owns its sequence — the model IR
    // carries no explicit default for it. A shared/custom (non-owned) sequence
    // default falls through and is preserved verbatim below.
    if normalize_serial_pk_default(&lowered, ty, is_primary_key, primary_key, owns_sequence) {
        return None;
    }
    Some(ColumnDefault::Sql(raw.to_owned()))
}

/// Whether a raw (lower-cased) default is the auto-increment sequence default of
/// a single-column **`BIGSERIAL`** (int8) primary key **that owns its sequence** —
/// i.e. the shape the model IR represents as an [`Int64`](ColumnType::Int64) PK with
/// **no** default. Such a default is stripped to `None` so an introspected
/// `BIGSERIAL` id round-trips against the model parser (whose `pk_kind_for` requires
/// `default.is_none()` for a `BigSerial` PK).
///
/// **Ownership is load-bearing**: only a *conventional* serial — where the column
/// owns the SAME sequence its default allocates from (`owns_sequence == true`,
/// requiring both the `OWNED BY` `deptype = 'a'` link AND the default's `pg_attrdef`
/// `deptype = 'n'` reference to point at one sequence) — is the default-less
/// `BIGSERIAL` shape. A brownfield int8 PK backed by a **shared/custom** sequence it
/// does NOT own (`id BIGINT PRIMARY KEY DEFAULT nextval('global_ids')`), OR one that
/// still owns an old sequence but whose default was repointed to a different one, is
/// deliberately **not** stripped: stripping it would make recreation/rollback emit a
/// fresh owned `BIGSERIAL` (a NEW sequence) instead of continuing to allocate from
/// the sequence the default names — a silent, wrong ID-allocation change. Its raw
/// `nextval(...)` default is preserved verbatim instead.
///
/// A single-column **`SERIAL`** (int4) PK is deliberately **not** stripped (this
/// function only ever matches [`Int64`](ColumnType::Int64)): the model IR has no
/// default-less int4-PK shape, so keeping the `nextval(...)` default is what lets the
/// DDL emitter recognize it (via `pk_kind_for`) and reconstruct it as `SERIAL PRIMARY
/// KEY` — recreating it as a plain `INTEGER PRIMARY KEY` would silently drop the
/// sequence. A plain int PK with no default stays default-less and so recreates as a
/// plain integer PK (no auto-increment).
fn normalize_serial_pk_default(
    lowered_default: &str,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
    owns_sequence: bool,
) -> bool {
    is_primary_key
        && primary_key.len() == 1
        && matches!(ty, ColumnType::Int64)
        && lowered_default.starts_with("nextval(")
        && owns_sequence
}

/// The [`SerialKind`] marker for a column: `Some` only for a single-column integer
/// primary key whose `nextval(...)` default the column **owns** — a conventional
/// `SERIAL` (int4) / `BIGSERIAL` (int8). A plain manually-assigned `INTEGER` /
/// `BIGINT` PK (no owned sequence), a non-PK serial, a composite PK, or a shared
/// (non-owned) sequence default all return `None`, so the marker cleanly separates
/// an owned-sequence auto-increment id from a plain integer PK of the same width.
///
/// This mirrors [`normalize_serial_pk_default`]'s ownership rule but distinguishes
/// int4 vs int8 (so a brownfield `SERIAL` PK is marked `Serial` and a `BIGSERIAL`
/// PK `BigSerial`), and is independent of the default-stripping: an int8 owned
/// serial's `nextval(...)` default is stripped to `None` while an int4 serial keeps
/// its default, but BOTH carry the marker.
fn serial_kind_for(
    raw_default: Option<&str>,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
    owns_sequence: bool,
    is_identity: bool,
) -> Option<SerialKind> {
    // The serial-vs-plain distinction only applies to a single-column INTEGER
    // primary key. A composite/non-PK column, a non-integer PK (e.g. a UUID id), or
    // a `GENERATED … AS IDENTITY` column (whose id-generation is carried by the
    // separate `identity` marker) leaves this `None` — "not applicable", which the
    // diff treats as compatible.
    if !is_primary_key
        || primary_key.len() != 1
        || is_identity
        || !matches!(ty, ColumnType::Int32 | ColumnType::Int64)
    {
        return None;
    }
    // An owned `nextval(...)` default is a `SERIAL`/`BIGSERIAL` id (distinguished by
    // int4 vs int8). Computed from the RAW default (before `normalize_default`
    // strips an int8 owned-serial default to `None`).
    if owns_sequence
        && raw_default
            .map(|d| d.trim().to_ascii_lowercase())
            .is_some_and(|d| d.starts_with("nextval("))
    {
        return Some(match ty {
            ColumnType::Int32 => SerialKind::Serial,
            _ => SerialKind::BigSerial,
        });
    }
    // A single-column integer PK that owns no sequence is a genuine plain,
    // manually-assigned id. Emit an EXPLICIT `Plain` marker (never `None`, which is
    // reserved for legacy/unknown) so a plain-int-PK ↔ serial mismatch surfaces as
    // drift while a legacy snapshot (marker absent) stays compatible.
    Some(SerialKind::Plain)
}

// ===========================================================================
// SQLite introspection (`autumn schema pull` on the sqlite backend-flip)
// ===========================================================================
//
// Compiled only under the `sqlite` cargo feature (the backend-flip that must never
// be co-built with the default Postgres backend — see `autumn-cli/Cargo.toml`), so
// the default (Postgres) build references no `SqliteConnection` here. It lifts a
// live SQLite database into the SAME `autumn_schema_core` snapshot IR as
// [`introspect_postgres`], reading the SQLite catalog through `sqlite_master` and
// the table-valued `pragma_*` functions in a constant number of batched queries
// (one per catalog, joined against `sqlite_master` — never one query per table
// beyond what a PRAGMA inherently requires).
//
// # Fidelity / fail-closed posture (mirrors the Postgres arm)
//
// - **Type mapping is conservative**: SQLite type affinity maps to the IR
//   [`ColumnType`] only where unambiguous — INTEGER affinity → [`Int64`], TEXT →
//   [`Text`], REAL → [`Float64`], BLOB → [`Bytes`]. A NUMERIC-affinity (or otherwise
//   unmapped) declared type is preserved verbatim as [`Opaque`](ColumnType::Opaque),
//   never dropped, exactly like the Postgres arm's `Opaque` floor.
// - **Convention recovery** keeps a model↔database round-trip clean: a single-column
//   `INTEGER PRIMARY KEY AUTOINCREMENT` id maps to an [`Int64`] PK carrying
//   `serial: Some(BigSerial)` (Autumn's `#[id]` convention; AUTOINCREMENT is detected
//   from the table's `CREATE TABLE` SQL because `sqlite_sequence` is empty until the
//   first insert), and a column defaulting to `CURRENT_TIMESTAMP` — the exact DDL
//   Autumn emits for a `Timestamp` `#[default]` column — is recovered as a
//   [`Timestamp`](ColumnType::Timestamp) with a [`ColumnDefault::Now`] default.
// - **Errors are credential-safe**: a connection failure carries only the parsed
//   file path; a query/PRAGMA failure is an [`IntrospectError::Query`] carrying only
//   the SQLite message — never the URL, and never a partial result.
//
// # Deferred (recorded, not silently dropped)
//
// - **Table-level `CHECK` constraints**: parsing a `CHECK (...)` predicate out of the
//   raw `CREATE TABLE` SQL text is deferred (SQLite exposes no catalog view for it,
//   unlike Postgres `pg_get_constraintdef`); `checks` is left empty for SQLite.
// - **`SERIAL` vs `BIGSERIAL` distinction**: SQLite has a single 64-bit rowid, so an
//   AUTOINCREMENT id is always recorded as `BigSerial` (there is no int4 serial).
// - **FK on-update/on-delete actions, composite FKs, expression/collation index
//   attributes**: same deferrals as the Postgres arm.
// - **Composite inline `UNIQUE(a, b)` constraints**: a table-level UNIQUE-constraint
//   auto-index (`sqlite_autoindex_*`) cannot be recorded — its name is un-creatable
//   (`SQLite` refuses `CREATE … "sqlite_…"`) and the IR has no table-level
//   unique-constraint node — so a composite inline `UNIQUE` is skipped (a
//   single-column inline `UNIQUE` IS captured, as the column's `unique` flag). Rare
//   brownfield shape; not tracked in the snapshot.
// - **Generated columns' expressions**: generated columns are PRESERVED (read via
//   `pragma_table_xinfo`, never dropped), but their `GENERATED ALWAYS AS (...)`
//   expression is not captured — a generated column round-trips as an ordinary column
//   of its declared type. Full verbatim re-emission is future work.

/// Resolve a `sqlite:`/`sqlite://`/`file:` URL (or a bare path) to the concrete
/// target [`diesel::SqliteConnection::establish`] expects, mirroring autumn-web's
/// own `normalize_sqlite_target` (kept in lock-step so the pull connects to the same
/// file `schema migrate` does). An empty or `:memory:` target stays `:memory:`.
#[cfg(feature = "sqlite")]
fn sqlite_target(url: &str) -> String {
    if url.starts_with("file:") {
        return url.to_owned();
    }
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    if rest.is_empty() || rest == ":memory:" {
        return ":memory:".to_owned();
    }
    rest.to_owned()
}

/// Classify a resolved `SQLite` target for the create-on-open existence guard:
/// `Some(path)` is a concrete filesystem path that must be checked for existence
/// (bare path or a `file:` URI with a real path); `None` is an in-memory database
/// (nothing to check).
///
/// `SQLite`'s `sqlite3_open` CREATES a missing file, so a typo'd path (bare OR inside
/// a `file:` URI) would otherwise pull zero tables and overwrite the snapshot. Only
/// genuine in-memory targets are exempt: `:memory:`, `file::memory:` (and its
/// shared-cache `?cache=shared` form), and any `file:` URI carrying `mode=memory`.
/// For a `file:` URI with a real path the path is parsed out — the `file:` scheme
/// stripped (handling `file:p`, `file:/p`, `file://host/p`, `file:///p`), the `?query`
/// separator honored, and the path percent-decoded — then existence-checked like a
/// bare path.
#[cfg(feature = "sqlite")]
fn sqlite_existence_check_path(target: &str) -> Option<String> {
    if target.is_empty() || target == ":memory:" {
        return None;
    }
    let Some(rest) = target.strip_prefix("file:") else {
        // A bare filesystem path.
        return Some(target.to_owned());
    };
    // Split off the `?query` (URI parameters like `mode=`, `cache=`).
    let (path_part, query) = rest.split_once('?').unwrap_or((rest, ""));
    // Strip an optional authority: `file://host/path` / `file:///path`.
    let path_part = path_part.strip_prefix("//").map_or(path_part, |after| {
        after.find('/').map_or(after, |idx| &after[idx..])
    });
    // In-memory forms are exempt (no file to check).
    let is_memory = path_part == ":memory:"
        || query
            .split('&')
            .any(|kv| kv.trim().eq_ignore_ascii_case("mode=memory"));
    if is_memory {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(path_part)
        .decode_utf8_lossy()
        .into_owned();
    Some(decoded)
}

#[cfg(all(test, feature = "sqlite"))]
mod target_tests {
    use super::sqlite_existence_check_path;

    #[test]
    fn existence_check_path_parses_files_and_exempts_memory() {
        // In-memory targets: no file to check.
        for mem in [
            ":memory:",
            "",
            "file::memory:",
            "file::memory:?cache=shared",
            "file:app.db?mode=memory",
            "file:app.db?cache=shared&mode=memory",
        ] {
            assert_eq!(
                sqlite_existence_check_path(mem),
                None,
                "{mem} is in-memory (exempt)"
            );
        }
        // Bare filesystem path.
        assert_eq!(
            sqlite_existence_check_path("/srv/app.db"),
            Some("/srv/app.db".to_owned())
        );
        // `file:` URI forms → the real path is parsed out (scheme/authority/query
        // stripped, percent-decoded).
        assert_eq!(
            sqlite_existence_check_path("file:/srv/app.db"),
            Some("/srv/app.db".to_owned())
        );
        assert_eq!(
            sqlite_existence_check_path("file:///srv/app.db"),
            Some("/srv/app.db".to_owned())
        );
        assert_eq!(
            sqlite_existence_check_path("file://localhost/srv/app.db"),
            Some("/srv/app.db".to_owned())
        );
        assert_eq!(
            sqlite_existence_check_path("file:app.db?mode=ro"),
            Some("app.db".to_owned())
        );
        assert_eq!(
            sqlite_existence_check_path("file:/srv/my%20app.db"),
            Some("/srv/my app.db".to_owned()),
            "percent-decoded"
        );
    }
}

/// Introspect a live `SQLite` database at `url` into a set of [`Table`]s tagged
/// [`Backend::Sqlite`] and marked `managed` — the `SQLite` counterpart of
/// [`introspect_postgres`], producing the identical snapshot IR so a pulled snapshot
/// and the model-derived snapshot are diffable by the same engine.
///
/// Framework-owned tables (`is_framework_table`), `SQLite` internal tables
/// (`sqlite_*`), and the Diesel migrations table are excluded, mirroring the
/// Postgres arm's unscoped-pull rule.
///
/// # Errors
///
/// Returns [`IntrospectError::ConnectionSqlite`] if the database file cannot be
/// opened, or [`IntrospectError::Query`] if a catalog query/PRAGMA fails. Neither
/// leaks the database URL.
#[cfg(feature = "sqlite")]
pub fn introspect_sqlite(url: &str) -> Result<Vec<Table>, IntrospectError> {
    use diesel::{Connection as _, SqliteConnection};

    let target = sqlite_target(url);
    // Guard against SQLite's create-if-missing behavior: `establish` on a nonexistent
    // file SUCCEEDS against a brand-new EMPTY database, so a typo'd or missing DB path
    // would otherwise pull ZERO tables and (on a non-dry-run pull) silently overwrite
    // the checked-in snapshot with an empty one. Require a concrete file-backed target
    // (bare path OR a `file:` URI with a real path) to already exist; genuine
    // in-memory targets have no file to check and are exempt.
    if let Some(path) = sqlite_existence_check_path(&target)
        && !std::path::Path::new(&path).exists()
    {
        return Err(IntrospectError::ConnectionSqlite { path });
    }
    let mut conn = SqliteConnection::establish(&target)
        .map_err(|_| IntrospectError::ConnectionSqlite { path: target })?;
    sqlite::introspect(&mut conn)
}

/// `SQLite` catalog probes + IR assembly. All row DTOs and helpers live here so the
/// default (Postgres) build never compiles a `SqliteConnection` reference.
#[cfg(feature = "sqlite")]
mod sqlite {
    use std::collections::BTreeMap;

    use autumn_schema_core::{
        Backend, Column, ColumnDefault, ColumnType, ForeignKey, Index, SerialKind, Table,
    };
    use diesel::sql_types::{Integer, Nullable, Text};
    use diesel::{QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

    use super::IntrospectError;

    /// `(table_name, index_name)` identity for grouping index columns.
    type IndexKey = (String, String);

    #[derive(QueryableByName)]
    struct TableRow {
        #[diesel(sql_type = Text)]
        name: String,
        #[diesel(sql_type = Nullable<Text>)]
        sql: Option<String>,
    }

    #[derive(QueryableByName)]
    struct ColumnRow {
        #[diesel(sql_type = Text)]
        table_name: String,
        #[diesel(sql_type = Text)]
        column_name: String,
        /// The declared type verbatim (`INTEGER`, `TEXT`, `REAL`, `BLOB`, …); may be
        /// the empty string for a typeless `SQLite` column.
        #[diesel(sql_type = Text)]
        decl_type: String,
        /// `PRAGMA table_info.notnull` (`0`/`1`).
        #[diesel(sql_type = Integer)]
        not_null: i32,
        /// `PRAGMA table_info.dflt_value` (raw default text, `NULL` for none).
        #[diesel(sql_type = Nullable<Text>)]
        dflt_value: Option<String>,
        /// `PRAGMA table_info.pk` — `0` for a non-key column, else the 1-based key
        /// position.
        #[diesel(sql_type = Integer)]
        pk: i32,
        /// `PRAGMA table_xinfo.hidden` — `0` = ordinary column, `1` = hidden column
        /// (virtual-table / `WITHOUT ROWID` internal — not a user column), `2` =
        /// `VIRTUAL` generated column, `3` = `STORED` generated column. Read via
        /// `table_xinfo` (NOT `table_info`, which OMITS generated columns entirely and
        /// would silently drop them from the pull).
        #[diesel(sql_type = Integer)]
        hidden: i32,
    }

    #[derive(QueryableByName)]
    struct IndexRow {
        #[diesel(sql_type = Text)]
        table_name: String,
        #[diesel(sql_type = Text)]
        index_name: String,
        /// `PRAGMA index_list.unique` (`0`/`1`).
        #[diesel(sql_type = Integer)]
        is_unique: i32,
        /// `PRAGMA index_list.origin` — `'c'` explicit CREATE INDEX, `'u'` UNIQUE
        /// constraint, `'pk'` primary key.
        #[diesel(sql_type = Text)]
        origin: String,
        /// `PRAGMA index_list.partial` (`0`/`1`).
        #[diesel(sql_type = Integer)]
        partial: i32,
        /// The verbatim `CREATE [UNIQUE] INDEX …` statement from `sqlite_master`
        /// (`NULL` for a constraint/PK auto-index, which has no standalone SQL).
        #[diesel(sql_type = Nullable<Text>)]
        index_sql: Option<String>,
    }

    #[derive(QueryableByName)]
    struct IndexColumnRow {
        #[diesel(sql_type = Text)]
        table_name: String,
        #[diesel(sql_type = Text)]
        index_name: String,
        #[diesel(sql_type = Integer)]
        seqno: i32,
        /// The indexed column name, or `NULL` for an expression key.
        #[diesel(sql_type = Nullable<Text>)]
        column_name: Option<String>,
    }

    #[derive(QueryableByName)]
    struct ForeignKeyRow {
        #[diesel(sql_type = Text)]
        table_name: String,
        #[diesel(sql_type = Text)]
        foreign_table: String,
        #[diesel(sql_type = Text)]
        column_name: String,
        /// The referenced column; `NULL` when the FK targets the referenced table's
        /// PK implicitly (recovered as `id`, the Autumn convention).
        #[diesel(sql_type = Nullable<Text>)]
        foreign_column: Option<String>,
    }

    fn query_err(e: &diesel::result::Error) -> IntrospectError {
        IntrospectError::Query(e.to_string())
    }

    /// Introspect every app table of an open `SQLite` connection into the IR. Keeps
    /// the private row DTOs entirely inside this module (the outer `introspect_sqlite`
    /// only opens the connection), so the default Postgres build compiles no `SQLite`
    /// type into a public signature.
    pub(super) fn introspect(conn: &mut SqliteConnection) -> Result<Vec<Table>, IntrospectError> {
        let tables = list_tables(conn)?;
        if tables.is_empty() {
            return Ok(Vec::new());
        }
        let columns_by_table = fetch_columns(conn)?;
        let indexes_by_table = fetch_indexes(conn)?;
        let index_columns = fetch_index_columns(conn)?;
        let fks_by_table = fetch_foreign_keys(conn)?;

        let mut out = Vec::with_capacity(tables.len());
        for (name, table_sql) in &tables {
            out.push(build_table(
                name,
                table_sql.as_deref().unwrap_or_default(),
                columns_by_table.get(name).map_or(&[][..], Vec::as_slice),
                indexes_by_table.get(name).map_or(&[][..], Vec::as_slice),
                &index_columns,
                fks_by_table.get(name).map_or(&[][..], Vec::as_slice),
            ));
        }
        Ok(out)
    }

    /// List app tables (name + `CREATE TABLE` SQL), excluding `SQLite` internal
    /// tables, the Diesel migrations table, framework-owned tables, and **FTS5
    /// search-index tables** — the `CREATE VIRTUAL TABLE "<table>__fts" USING fts5(…)`
    /// index a `--searchable` model generates (issue #1910) plus its internal shadow
    /// tables (`<vtab>_data`/`_idx`/`_content`/`_docsize`/`_config`). Those are stored
    /// in `sqlite_master` as `type = 'table'` with non-`sqlite_` names, but the model
    /// parser never emits them, so pulling them as managed application tables would
    /// make a follow-up `schema diff`/doctor report/drop the search-index state.
    fn list_tables(
        conn: &mut SqliteConnection,
    ) -> Result<Vec<(String, Option<String>)>, IntrospectError> {
        let rows: Vec<TableRow> = sql_query(
            "SELECT name AS name, sql AS sql FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .load(conn)
        .map_err(|e| query_err(&e))?;
        // Virtual tables (their `sql` starts with `CREATE VIRTUAL TABLE`) are never
        // model-expressible; collect them so their FTS5 shadow tables can be
        // excluded alongside the vtab itself.
        let virtual_tables: Vec<String> = rows
            .iter()
            .filter(|r| r.sql.as_deref().is_some_and(is_create_virtual_table))
            .map(|r| r.name.clone())
            .collect();
        Ok(rows
            .into_iter()
            .filter(|r| {
                !super::is_framework_table(&r.name)
                    && !is_virtual_or_fts5_shadow_table(&r.name, &virtual_tables)
            })
            .map(|r| (r.name, r.sql))
            .collect())
    }

    /// Whether `sql` is a `CREATE VIRTUAL TABLE …` statement (an FTS5 search index or
    /// any other module-backed vtab) — none of which the model parser can express.
    fn is_create_virtual_table(sql: &str) -> bool {
        sql.trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL TABLE")
    }

    /// The FTS5 internal shadow-table suffixes appended to the virtual-table name.
    const FTS5_SHADOW_SUFFIXES: [&str; 5] = ["_data", "_idx", "_content", "_docsize", "_config"];

    /// Whether `name` is one of the `virtual_tables` itself or one of its FTS5 shadow
    /// tables (`<vtab>` + a [`FTS5_SHADOW_SUFFIXES`] suffix).
    fn is_virtual_or_fts5_shadow_table(name: &str, virtual_tables: &[String]) -> bool {
        virtual_tables.iter().any(|vtab| {
            name == vtab
                || FTS5_SHADOW_SUFFIXES
                    .iter()
                    .any(|suffix| name == format!("{vtab}{suffix}"))
        })
    }

    /// Fetch every column of every table in one batched `pragma_table_xinfo` join.
    ///
    /// Uses `table_xinfo` (not `table_info`) BECAUSE the latter OMITS generated
    /// columns, so a pull would silently drop them and a recreate would omit the
    /// column (a fail-closed violation). `table_xinfo` includes them with a `hidden`
    /// discriminator so [`build_table`] can preserve generated columns instead of
    /// dropping them.
    fn fetch_columns(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<String, Vec<ColumnRow>>, IntrospectError> {
        let rows: Vec<ColumnRow> = sql_query(
            "SELECT m.name AS table_name, ti.name AS column_name, ti.type AS decl_type, \
             ti.\"notnull\" AS not_null, ti.dflt_value AS dflt_value, ti.pk AS pk, \
             ti.hidden AS hidden \
             FROM sqlite_master m JOIN pragma_table_xinfo(m.name) ti \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
             ORDER BY m.name, ti.cid",
        )
        .load(conn)
        .map_err(|e| query_err(&e))?;
        Ok(group_by(rows, |r| r.table_name.clone()))
    }

    /// Fetch every non-PK index of every table in one batched `pragma_index_list`
    /// join, carrying each index's standalone `CREATE INDEX` SQL when it has one.
    fn fetch_indexes(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<String, Vec<IndexRow>>, IntrospectError> {
        let rows: Vec<IndexRow> = sql_query(
            "SELECT m.name AS table_name, il.name AS index_name, il.\"unique\" AS is_unique, \
             il.origin AS origin, il.partial AS partial, idx.sql AS index_sql \
             FROM sqlite_master m JOIN pragma_index_list(m.name) il \
             LEFT JOIN sqlite_master idx ON idx.type = 'index' AND idx.name = il.name \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
             ORDER BY m.name, il.name",
        )
        .load(conn)
        .map_err(|e| query_err(&e))?;
        Ok(group_by(rows, |r| r.table_name.clone()))
    }

    /// Fetch the columns of every index in one batched `pragma_index_info` join,
    /// grouped by `(table, index)` in key order.
    fn fetch_index_columns(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<IndexKey, Vec<IndexColumnRow>>, IntrospectError> {
        let mut rows: Vec<IndexColumnRow> = sql_query(
            "SELECT m.name AS table_name, il.name AS index_name, ii.seqno AS seqno, \
             ii.name AS column_name \
             FROM sqlite_master m JOIN pragma_index_list(m.name) il \
             JOIN pragma_index_info(il.name) ii \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
             ORDER BY m.name, il.name, ii.seqno",
        )
        .load(conn)
        .map_err(|e| query_err(&e))?;
        rows.sort_by_key(|r| r.seqno);
        let mut by_index: BTreeMap<IndexKey, Vec<IndexColumnRow>> = BTreeMap::new();
        for row in rows {
            by_index
                .entry((row.table_name.clone(), row.index_name.clone()))
                .or_default()
                .push(row);
        }
        Ok(by_index)
    }

    /// Fetch **single-column** foreign keys of every table in one batched
    /// `pragma_foreign_key_list` join.
    ///
    /// `pragma_foreign_key_list` returns one row PER referencing column, grouped by
    /// the `id` column and ordered by `seq`; a composite (multi-column) FK spans
    /// several rows sharing the same `id`. The IR [`ForeignKey`] is single-column
    /// only, and the model parser never emits a composite FK, so — matching the
    /// fail-closed posture of the Postgres arm (which defers composite FKs) — a
    /// composite FK is **omitted entirely** here rather than flattened to a wrong
    /// single-column FK (which would cause spurious drift / an incorrect recreate).
    /// The `fkl.id NOT IN (… seq > 0 …)` predicate drops every `id` group that has
    /// any `seq > 0` row, keeping only genuinely single-column FKs (the surviving
    /// `seq = 0` rows).
    fn fetch_foreign_keys(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<String, Vec<ForeignKeyRow>>, IntrospectError> {
        let rows: Vec<ForeignKeyRow> = sql_query(
            "SELECT m.name AS table_name, fkl.\"table\" AS foreign_table, \
             fkl.\"from\" AS column_name, fkl.\"to\" AS foreign_column \
             FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) fkl \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND fkl.seq = 0 \
             AND fkl.id NOT IN ( \
               SELECT fkl2.id FROM pragma_foreign_key_list(m.name) fkl2 WHERE fkl2.seq > 0)",
        )
        .load(conn)
        .map_err(|e| query_err(&e))?;
        Ok(group_by(rows, |r| r.table_name.clone()))
    }

    /// Group a flat row list into a per-table map, preserving row order within each
    /// table (input already ordered by table then position).
    fn group_by<T>(rows: Vec<T>, key: impl Fn(&T) -> String) -> BTreeMap<String, Vec<T>> {
        let mut out: BTreeMap<String, Vec<T>> = BTreeMap::new();
        for row in rows {
            out.entry(key(&row)).or_default().push(row);
        }
        out
    }

    /// The `SQLite` type affinity of a declared type, per the `SQLite` type-affinity
    /// determination rules (`SQLite` docs §3.1).
    #[derive(PartialEq, Eq)]
    enum Affinity {
        Integer,
        Text,
        Blob,
        Real,
        Numeric,
    }

    fn affinity(decl_type: &str) -> Affinity {
        let t = decl_type.to_ascii_uppercase();
        if t.contains("INT") {
            Affinity::Integer
        } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
            Affinity::Text
        } else if t.is_empty() || t.contains("BLOB") {
            Affinity::Blob
        } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }

    /// Normalize a raw `SQLite` `dflt_value` into a [`ColumnDefault`]. Recovers the
    /// Autumn `Timestamp` convention default (`CURRENT_TIMESTAMP` → [`Now`]) and
    /// preserves anything else verbatim.
    ///
    /// `pragma_table_info.dflt_value` reports the literal keyword `NULL` (bareword,
    /// any case) for a `DEFAULT NULL` clause — which is semantically "no default",
    /// the same as a column with no `DEFAULT` at all. Capturing it as a real
    /// `Sql("NULL")` default would create spurious drift against a model column that
    /// simply has no default, so a bareword `NULL` maps to `None`. A **quoted**
    /// `'NULL'` (the 4-character text value) is a genuine string default and is
    /// preserved verbatim — only the unquoted keyword means "no default".
    fn sqlite_default(raw: Option<&str>) -> Option<ColumnDefault> {
        let raw = raw?.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("NULL") {
            return None;
        }
        if raw.eq_ignore_ascii_case("CURRENT_TIMESTAMP") {
            return Some(ColumnDefault::Now);
        }
        Some(ColumnDefault::Sql(raw.to_owned()))
    }

    /// Map a `SQLite` declared type + resolved default to an IR [`ColumnType`],
    /// conservatively (fail-closed to [`Opaque`](ColumnType::Opaque) for a
    /// non-cleanly-mappable NUMERIC-affinity type). A `CURRENT_TIMESTAMP` default
    /// recovers the Autumn `Timestamp` convention (a `Timestamp` column is stored as
    /// `TEXT DEFAULT CURRENT_TIMESTAMP`), keeping a model round-trip clean.
    fn sqlite_column_type(decl_type: &str, default: Option<&ColumnDefault>) -> ColumnType {
        if matches!(default, Some(ColumnDefault::Now)) {
            return ColumnType::Timestamp;
        }
        match affinity(decl_type) {
            Affinity::Integer => ColumnType::Int64,
            Affinity::Text => ColumnType::Text,
            Affinity::Real => ColumnType::Float64,
            Affinity::Blob => ColumnType::Bytes,
            // Fail-closed: a NUMERIC-affinity (or otherwise unmapped) declared type is
            // preserved verbatim rather than guessed, matching the Postgres arm's
            // `Opaque` floor. An empty declared type is BLOB affinity above, so this
            // never produces an empty `pg_type`.
            Affinity::Numeric => ColumnType::Opaque {
                pg_type: decl_type.to_owned(),
            },
        }
    }

    /// Whether the table's `CREATE TABLE` SQL declares an `AUTOINCREMENT` primary key.
    ///
    /// Detects the `AUTOINCREMENT` **keyword** as a word-bounded token, AFTER
    /// neutralizing string literals (`'…'`) and quoted identifiers (`"…"`, `` `…` ``,
    /// `[…]`), so neither a NAME containing the substring (`autoincrement_events`) nor
    /// a string literal (`DEFAULT 'AUTOINCREMENT'`) nor a quoted identifier
    /// (`"AUTOINCREMENT"`) is ever mistaken for the keyword. `SQLite`'s grammar only
    /// permits the `AUTOINCREMENT` keyword inside an `INTEGER PRIMARY KEY
    /// AUTOINCREMENT` column definition, so a genuine word-bounded keyword occurrence
    /// unambiguously marks the table as auto-incrementing.
    fn table_sql_has_autoincrement_keyword(table_sql: &str) -> bool {
        const KW: &str = "AUTOINCREMENT";
        let stripped = neutralize_sqlite_literals_and_quoted_idents(table_sql).to_ascii_uppercase();
        let bytes = stripped.as_bytes();
        let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut from = 0;
        while let Some(rel) = stripped[from..].find(KW) {
            let start = from + rel;
            let end = start + KW.len();
            let before_ok = start == 0 || !is_word(bytes[start - 1]);
            let after_ok = end == bytes.len() || !is_word(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = start + 1;
        }
        false
    }

    /// Replace the CONTENTS (and delimiters) of `SQLite` string literals (`'…'`) and
    /// quoted identifiers (`"…"`, `` `…` ``, `[…]`) with spaces, preserving all other
    /// text positionally, so a keyword scan never matches inside a name or literal.
    /// Doubled-delimiter escapes (`''`, `""`, ``` `` ```) keep the quote open. Pure.
    fn neutralize_sqlite_literals_and_quoted_idents(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            let close = match c {
                '\'' => Some('\''),
                '"' => Some('"'),
                '`' => Some('`'),
                '[' => Some(']'),
                _ => None,
            };
            let Some(close) = close else {
                out.push(c);
                continue;
            };
            out.push(' '); // opening delimiter
            while let Some(n) = chars.next() {
                if n == close {
                    // A doubled delimiter is an escape (not for the `[…]` form) — stay
                    // inside the quote.
                    if close != ']' && chars.peek() == Some(&close) {
                        chars.next();
                        out.push(' ');
                        out.push(' ');
                        continue;
                    }
                    break;
                }
                out.push(' ');
            }
            out.push(' '); // closing delimiter
        }
        out
    }

    /// Assemble one [`Table`] from its pre-fetched `SQLite` catalog rows. Pure.
    fn build_table(
        name: &str,
        table_sql: &str,
        columns: &[ColumnRow],
        indexes: &[IndexRow],
        index_columns: &BTreeMap<IndexKey, Vec<IndexColumnRow>>,
        foreign_keys: &[ForeignKeyRow],
    ) -> Table {
        let mut table = Table::new(name, Backend::Sqlite);
        table.managed = true;

        // Primary key, in key order (`pk` is the 1-based key position).
        let mut pk_cols: Vec<(i32, String)> = columns
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| (c.pk, c.column_name.clone()))
            .collect();
        pk_cols.sort_by_key(|(ord, _)| *ord);
        table.primary_key = pk_cols.into_iter().map(|(_, n)| n).collect();
        let single_pk = table.primary_key.len() == 1;
        // AUTOINCREMENT lives in the table's CREATE SQL (sqlite_sequence is empty
        // until the first insert, so it cannot be relied on). Matched as a KEYWORD
        // token, not a substring, so a table/column name or literal containing the
        // substring never misclassifies a plain rowid PK as a serial.
        let autoincrement = table_sql_has_autoincrement_keyword(table_sql);

        let (ir_indexes, unique_columns) = collapse_indexes(name, indexes, index_columns);
        let fk_by_column: BTreeMap<&str, &ForeignKeyRow> = foreign_keys
            .iter()
            .map(|fk| (fk.column_name.as_str(), fk))
            .collect();

        for row in columns {
            // `hidden = 1` is an internal hidden column (virtual-table / rowid
            // machinery), never a user column — skip it. `hidden = 2` (VIRTUAL) / `3`
            // (STORED) are GENERATED columns: they are included (via `table_xinfo`) so
            // they are NEVER silently dropped from the pull. NOTE (DEFERRED): the
            // generation EXPRESSION is not captured here, so a generated column
            // round-trips as an ordinary column of its declared type — enough to keep
            // it in the snapshot (doctor/dry-run see it, a recreate no longer omits
            // it), but full generated-column fidelity (verbatim `GENERATED ALWAYS AS
            // (...)` re-emission) is future work.
            if row.hidden == 1 {
                continue;
            }
            let is_pk = row.pk > 0;
            let default = sqlite_default(row.dflt_value.as_deref());
            let ty = sqlite_column_type(&row.decl_type, default.as_ref());
            let mut column = Column::new(row.column_name.clone(), ty);
            // A SQLite PRIMARY KEY column reports notnull=0 for an INTEGER rowid PK,
            // but is effectively NOT NULL; the model always has non-null PKs, so force
            // a PK column non-null for a clean round-trip.
            column.nullable = row.not_null == 0 && !is_pk;
            column.primary_key = is_pk;
            column.unique = unique_columns.contains(row.column_name.as_str());
            column.default = default;
            // Serial marker (three-state): a single-column INTEGER PK of an
            // AUTOINCREMENT table is the Autumn `BigSerial` id (SQLite has one 64-bit
            // rowid — always BigSerial), symmetric with what the model parser
            // records. A single-column INTEGER PK WITHOUT `AUTOINCREMENT` is a
            // genuine plain, manually-assigned id → an EXPLICIT `Plain` marker (never
            // `None`, which is reserved for legacy/unknown snapshots) so a
            // plain-int-PK ↔ `BigSerial`-model mismatch surfaces as drift while a
            // legacy snapshot stays compatible.
            if is_pk && single_pk && matches!(affinity(&row.decl_type), Affinity::Integer) {
                column.serial = Some(if autoincrement {
                    SerialKind::BigSerial
                } else {
                    SerialKind::Plain
                });
            }
            if let Some(fk) = fk_by_column.get(row.column_name.as_str()) {
                let foreign_column = fk.foreign_column.clone().unwrap_or_else(|| "id".to_owned());
                column.references = Some(ForeignKey::new(fk.foreign_table.clone(), foreign_column));
            }
            table.columns.push(column);
        }

        table.indexes = ir_indexes;
        // Table-level CHECK extraction from the raw CREATE TABLE SQL is deferred (see
        // the module note), so `checks` is left empty for SQLite.
        table
    }

    /// Map `SQLite` index rows into IR [`Index`]es plus the set of columns covered by a
    /// single-column simple unique index (mirroring the Postgres arm's
    /// `collapse_indexes`). A partial or expression index is retained verbatim via its
    /// `CREATE INDEX` SQL; a plain index is representable by its columns. The primary
    /// key auto-index (`origin = 'pk'`) is skipped (the PK is modeled separately).
    ///
    /// A `UNIQUE`-constraint auto-index (`origin = 'u'`, named
    /// `sqlite_autoindex_<table>_<n>`) has **no** standalone `CREATE INDEX` statement
    /// and is owned by the constraint — it cannot be `DROP INDEX`ed, and `SQLite`
    /// refuses to `CREATE` any object named `sqlite_*`, so it must **never** appear as
    /// an [`Index`] in the IR (the emitter would try to re-`CREATE` it by name on a
    /// table rebuild / `DropTable` rollback and produce illegal `CREATE … "sqlite_…"`
    /// DDL). Instead:
    /// * a **single-column** inline `col TEXT UNIQUE` is represented purely as the
    ///   column's `unique = true` flag (no [`Index`] object) — a matching `#[unique]`
    ///   model is recognized as already satisfied (clean diff, no `DropIndex`, no
    ///   `sqlite_`-named `CREATE`);
    /// * a **composite** table-level `UNIQUE(a, b)` cannot be represented (the IR has
    ///   no table-level unique-constraint node, and the model DSL cannot express it),
    ///   so it is **skipped entirely** rather than synthesized as a `sqlite_`-named
    ///   index (DEFERRED: composite inline-`UNIQUE` fidelity — a rare brownfield shape
    ///   — is not tracked in the snapshot; see the module note).
    fn collapse_indexes(
        table_name: &str,
        rows: &[IndexRow],
        index_columns: &BTreeMap<IndexKey, Vec<IndexColumnRow>>,
    ) -> (Vec<Index>, std::collections::BTreeSet<String>) {
        let mut indexes = Vec::new();
        let mut unique_columns = std::collections::BTreeSet::new();
        for row in rows {
            if row.origin == "pk" {
                continue;
            }
            let cols = index_columns
                .get(&(table_name.to_owned(), row.index_name.clone()))
                .map_or(&[][..], Vec::as_slice);
            let has_expression = cols.iter().any(|c| c.column_name.is_none());
            let key_columns: Vec<String> =
                cols.iter().filter_map(|c| c.column_name.clone()).collect();
            let is_unique = row.is_unique != 0;
            let is_partial = row.partial != 0;

            // A UNIQUE-constraint auto-index (`origin = 'u'`) is NEVER emitted as an
            // Index (its `sqlite_*` name is un-creatable and un-droppable). A single
            // column becomes the column's `unique` flag; a composite one is skipped.
            if row.origin == "u" {
                if is_unique && !has_expression && key_columns.len() == 1 {
                    unique_columns.insert(key_columns[0].clone());
                }
                continue;
            }

            let simple = !is_partial && !has_expression;
            if simple && is_unique && key_columns.len() == 1 {
                unique_columns.insert(key_columns[0].clone());
            }
            // A partial or expression index is retained verbatim via its CREATE INDEX
            // SQL (when present); a plain index is representable by its columns.
            let definition = if simple { None } else { row.index_sql.clone() };
            indexes.push(Index {
                name: row.index_name.clone(),
                columns: key_columns,
                unique: is_unique,
                definition,
                is_partial,
                key_columns: Vec::new(),
            });
        }
        (indexes, unique_columns)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn affinity_maps_the_common_declared_types() {
            assert!(matches!(affinity("INTEGER"), Affinity::Integer));
            assert!(matches!(affinity("BIGINT"), Affinity::Integer));
            assert!(matches!(affinity("TEXT"), Affinity::Text));
            assert!(matches!(affinity("VARCHAR(255)"), Affinity::Text));
            assert!(matches!(affinity("REAL"), Affinity::Real));
            assert!(matches!(affinity("DOUBLE"), Affinity::Real));
            assert!(matches!(affinity("BLOB"), Affinity::Blob));
            assert!(matches!(affinity(""), Affinity::Blob));
            assert!(matches!(affinity("NUMERIC"), Affinity::Numeric));
            assert!(matches!(affinity("DECIMAL(10,2)"), Affinity::Numeric));
        }

        #[test]
        fn column_type_is_conservative_and_recovers_conventions() {
            assert_eq!(sqlite_column_type("INTEGER", None), ColumnType::Int64);
            assert_eq!(sqlite_column_type("TEXT", None), ColumnType::Text);
            assert_eq!(sqlite_column_type("REAL", None), ColumnType::Float64);
            assert_eq!(sqlite_column_type("BLOB", None), ColumnType::Bytes);
            // Convention recovery: a CURRENT_TIMESTAMP default → Timestamp.
            assert_eq!(
                sqlite_column_type("TEXT", Some(&ColumnDefault::Now)),
                ColumnType::Timestamp
            );
            // Fail-closed: a NUMERIC-affinity declared type is preserved verbatim.
            assert_eq!(
                sqlite_column_type("NUMERIC", None),
                ColumnType::Opaque {
                    pg_type: "NUMERIC".to_owned()
                }
            );
        }

        #[test]
        fn default_recovers_now_and_preserves_others() {
            assert_eq!(sqlite_default(None), None);
            assert_eq!(sqlite_default(Some("  ")), None);
            assert_eq!(
                sqlite_default(Some("CURRENT_TIMESTAMP")),
                Some(ColumnDefault::Now)
            );
            assert_eq!(
                sqlite_default(Some("'draft'")),
                Some(ColumnDefault::Sql("'draft'".to_owned()))
            );
        }

        #[test]
        fn default_bareword_null_is_no_default_but_quoted_null_is_preserved() {
            // A `DEFAULT NULL` clause surfaces as the bareword keyword `NULL`
            // (any case, possibly padded), which is semantically "no default" — it
            // must map to `None` so it never drifts against a model column that has
            // no default.
            assert_eq!(sqlite_default(Some("NULL")), None);
            assert_eq!(sqlite_default(Some("null")), None);
            assert_eq!(sqlite_default(Some("  Null  ")), None);
            // A QUOTED `'NULL'` is the real 4-character text value, a genuine string
            // default — it must survive verbatim.
            assert_eq!(
                sqlite_default(Some("'NULL'")),
                Some(ColumnDefault::Sql("'NULL'".to_owned()))
            );
        }

        fn col_row(
            name: &str,
            decl: &str,
            not_null: i32,
            dflt: Option<&str>,
            pk: i32,
        ) -> ColumnRow {
            ColumnRow {
                table_name: "posts".to_owned(),
                column_name: name.to_owned(),
                decl_type: decl.to_owned(),
                not_null,
                dflt_value: dflt.map(str::to_owned),
                pk,
                hidden: 0,
            }
        }

        /// A `ColumnRow` with an explicit `table_xinfo.hidden` discriminator (`2` =
        /// VIRTUAL generated, `3` = STORED generated, `1` = internal hidden).
        fn col_row_hidden(name: &str, decl: &str, hidden: i32) -> ColumnRow {
            ColumnRow {
                hidden,
                ..col_row(name, decl, 0, None, 0)
            }
        }

        #[test]
        fn build_table_marks_autoincrement_id_and_recovers_created_at() {
            let columns = vec![
                col_row("id", "INTEGER", 0, None, 1),
                col_row("title", "TEXT", 1, None, 0),
                col_row("body", "TEXT", 0, None, 0),
                col_row("created_at", "TEXT", 1, Some("CURRENT_TIMESTAMP"), 0),
            ];
            let table_sql = "CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                             title TEXT NOT NULL, body TEXT NULL, \
                             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)";
            let table = build_table("posts", table_sql, &columns, &[], &BTreeMap::new(), &[]);
            assert_eq!(table.primary_key, vec!["id".to_owned()]);

            let id = table.columns.iter().find(|c| c.name == "id").unwrap();
            assert_eq!(id.ty, ColumnType::Int64);
            assert!(id.primary_key);
            assert!(!id.nullable, "an INTEGER PK is forced non-null");
            assert_eq!(id.serial, Some(SerialKind::BigSerial));

            let body = table.columns.iter().find(|c| c.name == "body").unwrap();
            assert!(body.nullable);

            let created = table
                .columns
                .iter()
                .find(|c| c.name == "created_at")
                .unwrap();
            assert_eq!(created.ty, ColumnType::Timestamp);
            assert_eq!(created.default, Some(ColumnDefault::Now));
            assert!(!created.nullable);
        }

        #[test]
        fn build_table_plain_integer_pk_is_marked_plain() {
            // Without AUTOINCREMENT in the table SQL, an INTEGER PK is a plain rowid
            // alias, not an Autumn BigSerial id → an EXPLICIT `Plain` marker (three-
            // state: never `None`, which is reserved for legacy/unknown snapshots),
            // so it drifts against a `BigSerial` model while a legacy snapshot stays
            // compatible.
            let columns = vec![col_row("id", "INTEGER", 0, None, 1)];
            let table_sql = "CREATE TABLE posts (id INTEGER PRIMARY KEY)";
            let table = build_table("posts", table_sql, &columns, &[], &BTreeMap::new(), &[]);
            let id = table.columns.iter().find(|c| c.name == "id").unwrap();
            assert_eq!(id.serial, Some(SerialKind::Plain));
        }

        #[test]
        fn autoincrement_keyword_is_tokenized_not_substring_matched() {
            assert!(table_sql_has_autoincrement_keyword(
                "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)"
            ));
            assert!(table_sql_has_autoincrement_keyword(
                "create table t (id integer primary key   autoincrement )"
            ));
            // A table NAME containing the substring is NOT a match.
            assert!(!table_sql_has_autoincrement_keyword(
                "CREATE TABLE autoincrement_events (id INTEGER PRIMARY KEY)"
            ));
            // A column NAME containing the substring is NOT a match.
            assert!(!table_sql_has_autoincrement_keyword(
                "CREATE TABLE t (autoincrement_flag INTEGER, id INTEGER PRIMARY KEY)"
            ));
            // A string-literal default equal to the keyword is NOT a match.
            assert!(!table_sql_has_autoincrement_keyword(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, kind TEXT DEFAULT 'AUTOINCREMENT')"
            ));
            // A quoted identifier equal to the keyword is NOT a match.
            assert!(!table_sql_has_autoincrement_keyword(
                "CREATE TABLE t (\"AUTOINCREMENT\" INTEGER, id INTEGER PRIMARY KEY)"
            ));
            assert!(!table_sql_has_autoincrement_keyword(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, [AUTOINCREMENT] TEXT)"
            ));
        }

        #[test]
        fn build_table_autoincrement_substring_name_is_plain_not_serial() {
            // A plain INTEGER PK in a table whose NAME contains the `autoincrement`
            // substring must be `Some(Plain)`, never mis-flagged BigSerial.
            let columns = vec![col_row("id", "INTEGER", 0, None, 1)];
            let table_sql = "CREATE TABLE autoincrement_events (id INTEGER PRIMARY KEY)";
            let table = build_table(
                "autoincrement_events",
                table_sql,
                &columns,
                &[],
                &BTreeMap::new(),
                &[],
            );
            let id = table.columns.iter().find(|c| c.name == "id").unwrap();
            assert_eq!(id.serial, Some(SerialKind::Plain));
        }

        #[test]
        fn build_table_preserves_generated_columns_and_skips_internal_hidden() {
            // `table_xinfo` surfaces generated columns (hidden 2 = VIRTUAL, 3 =
            // STORED); they must be PRESERVED (never silently dropped), while a truly
            // internal hidden column (hidden = 1) is skipped.
            let columns = vec![
                col_row("id", "INTEGER", 0, None, 1),
                col_row("first", "TEXT", 1, None, 0),
                col_row_hidden("full_stored", "TEXT", 3),
                col_row_hidden("full_virtual", "TEXT", 2),
                col_row_hidden("internal", "TEXT", 1),
            ];
            let table_sql = "CREATE TABLE posts (id INTEGER PRIMARY KEY, first TEXT NOT NULL, \
                             full_stored TEXT GENERATED ALWAYS AS (first) STORED, \
                             full_virtual TEXT GENERATED ALWAYS AS (first) VIRTUAL)";
            let table = build_table("posts", table_sql, &columns, &[], &BTreeMap::new(), &[]);
            let names: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
            assert!(
                names.contains(&"full_stored") && names.contains(&"full_virtual"),
                "generated columns must be preserved, not dropped: {names:?}"
            );
            assert!(
                !names.contains(&"internal"),
                "an internal hidden column (hidden = 1) is skipped: {names:?}"
            );
        }

        #[test]
        fn collapse_indexes_simple_unique_sets_flag() {
            let rows = vec![IndexRow {
                table_name: "authors".to_owned(),
                index_name: "idx_authors_email_unique".to_owned(),
                is_unique: 1,
                origin: "c".to_owned(),
                partial: 0,
                index_sql: Some(
                    "CREATE UNIQUE INDEX idx_authors_email_unique ON authors (email)".to_owned(),
                ),
            }];
            let mut index_columns = BTreeMap::new();
            index_columns.insert(
                ("authors".to_owned(), "idx_authors_email_unique".to_owned()),
                vec![IndexColumnRow {
                    table_name: "authors".to_owned(),
                    index_name: "idx_authors_email_unique".to_owned(),
                    seqno: 0,
                    column_name: Some("email".to_owned()),
                }],
            );
            let (indexes, unique_cols) = collapse_indexes("authors", &rows, &index_columns);
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].columns, vec!["email".to_owned()]);
            assert!(indexes[0].unique);
            assert_eq!(
                indexes[0].definition, None,
                "a simple index carries no definition"
            );
            assert!(unique_cols.contains("email"));
        }

        #[test]
        fn collapse_indexes_skips_pk_and_retains_partial_verbatim() {
            let rows = vec![
                IndexRow {
                    table_name: "t".to_owned(),
                    index_name: "sqlite_autoindex_t_1".to_owned(),
                    is_unique: 1,
                    origin: "pk".to_owned(),
                    partial: 0,
                    index_sql: None,
                },
                IndexRow {
                    table_name: "t".to_owned(),
                    index_name: "t_active_idx".to_owned(),
                    is_unique: 0,
                    origin: "c".to_owned(),
                    partial: 1,
                    index_sql: Some(
                        "CREATE INDEX t_active_idx ON t (email) WHERE deleted_at IS NULL"
                            .to_owned(),
                    ),
                },
            ];
            let mut index_columns = BTreeMap::new();
            index_columns.insert(
                ("t".to_owned(), "t_active_idx".to_owned()),
                vec![IndexColumnRow {
                    table_name: "t".to_owned(),
                    index_name: "t_active_idx".to_owned(),
                    seqno: 0,
                    column_name: Some("email".to_owned()),
                }],
            );
            let (indexes, _unique) = collapse_indexes("t", &rows, &index_columns);
            assert_eq!(indexes.len(), 1, "the PK auto-index is skipped");
            assert_eq!(indexes[0].name, "t_active_idx");
            assert!(indexes[0].is_partial);
            assert!(
                indexes[0].definition.as_deref().unwrap().contains("WHERE"),
                "a partial index is retained verbatim via its CREATE INDEX SQL"
            );
        }

        #[test]
        fn collapse_indexes_single_col_unique_autoindex_becomes_column_flag_not_index() {
            // A single-column `col TEXT UNIQUE` inline constraint produces a
            // `sqlite_autoindex_*` (origin = 'u') with NO standalone CREATE INDEX SQL.
            // Its `sqlite_*` name is un-creatable AND un-droppable, so it must produce
            // NO Index object (only the column's `unique` flag) — otherwise the
            // emitter would try to re-CREATE / DROP `sqlite_autoindex_*` on a rebuild.
            let rows = vec![IndexRow {
                table_name: "accounts".to_owned(),
                index_name: "sqlite_autoindex_accounts_1".to_owned(),
                is_unique: 1,
                origin: "u".to_owned(),
                partial: 0,
                index_sql: None,
            }];
            let mut index_columns = BTreeMap::new();
            index_columns.insert(
                (
                    "accounts".to_owned(),
                    "sqlite_autoindex_accounts_1".to_owned(),
                ),
                vec![IndexColumnRow {
                    table_name: "accounts".to_owned(),
                    index_name: "sqlite_autoindex_accounts_1".to_owned(),
                    seqno: 0,
                    column_name: Some("email".to_owned()),
                }],
            );
            let (indexes, unique_cols) = collapse_indexes("accounts", &rows, &index_columns);
            assert!(
                indexes.is_empty(),
                "a UNIQUE-constraint autoindex is NEVER an Index (its sqlite_* name is \
                 un-creatable/un-droppable): {indexes:?}"
            );
            assert!(
                unique_cols.contains("email"),
                "it is represented purely as the column's unique flag"
            );
        }

        #[test]
        fn collapse_indexes_composite_unique_autoindex_is_skipped() {
            // A composite table-level `UNIQUE(a, b)` autoindex (origin = 'u') cannot be
            // represented (no table-level unique node in the IR, un-creatable name), so
            // it is skipped entirely — never synthesized as a `sqlite_`-named index.
            let rows = vec![IndexRow {
                table_name: "memberships".to_owned(),
                index_name: "sqlite_autoindex_memberships_1".to_owned(),
                is_unique: 1,
                origin: "u".to_owned(),
                partial: 0,
                index_sql: None,
            }];
            let mut index_columns = BTreeMap::new();
            index_columns.insert(
                (
                    "memberships".to_owned(),
                    "sqlite_autoindex_memberships_1".to_owned(),
                ),
                vec![
                    IndexColumnRow {
                        table_name: "memberships".to_owned(),
                        index_name: "sqlite_autoindex_memberships_1".to_owned(),
                        seqno: 0,
                        column_name: Some("org_id".to_owned()),
                    },
                    IndexColumnRow {
                        table_name: "memberships".to_owned(),
                        index_name: "sqlite_autoindex_memberships_1".to_owned(),
                        seqno: 1,
                        column_name: Some("user_id".to_owned()),
                    },
                ],
            );
            let (indexes, unique_cols) = collapse_indexes("memberships", &rows, &index_columns);
            assert!(
                indexes.is_empty(),
                "composite UNIQUE autoindex is skipped: {indexes:?}"
            );
            assert!(
                unique_cols.is_empty(),
                "a composite UNIQUE marks no single column unique"
            );
        }

        #[test]
        fn create_virtual_table_and_fts5_shadow_detection() {
            assert!(is_create_virtual_table(
                "CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"title\")"
            ));
            assert!(is_create_virtual_table(
                "  create virtual table x using fts5(a)"
            ));
            assert!(!is_create_virtual_table(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY)"
            ));

            let vtabs = vec!["posts__fts".to_owned()];
            // The vtab itself and each FTS5 shadow table are excluded.
            for shadow in [
                "posts__fts",
                "posts__fts_data",
                "posts__fts_idx",
                "posts__fts_content",
                "posts__fts_docsize",
                "posts__fts_config",
            ] {
                assert!(
                    is_virtual_or_fts5_shadow_table(shadow, &vtabs),
                    "{shadow} must be excluded"
                );
            }
            // A real application table (and a lookalike that is not a shadow) is kept.
            assert!(!is_virtual_or_fts5_shadow_table("posts", &vtabs));
            assert!(!is_virtual_or_fts5_shadow_table("posts__fts_other", &vtabs));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_extracts_and_defaults() {
        let (host, port) = parse_host_port("postgres://u:pw@db.example.com:6543/app").unwrap();
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 6543);
        let (host, port) = parse_host_port("postgres://localhost/app").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn connection_error_is_credential_safe() {
        let err = IntrospectError::Connection {
            host: "db.example.com".to_owned(),
            port: 5432,
        };
        let msg = err.to_string();
        assert!(msg.contains("db.example.com"));
        assert!(!msg.contains("hunter2") && !msg.contains("postgres://"));
    }

    #[test]
    fn framework_tables_are_excluded_user_tables_kept() {
        for fw in [
            "autumn_jobs",
            "_autumn_version_history",
            "api_tokens",
            "feature_flag_changes",
            "__diesel_schema_migrations",
        ] {
            assert!(is_framework_table(fw), "{fw} is a framework table");
        }
        for user in ["posts", "comments", "accounts"] {
            assert!(!is_framework_table(user), "{user} must be kept");
        }
    }

    #[test]
    fn normalize_default_now_variants() {
        let now = normalize_default(Some("now()"), &ColumnType::Timestamp, false, &[], false);
        assert_eq!(now, Some(ColumnDefault::Now));
        let ct = normalize_default(
            Some("CURRENT_TIMESTAMP"),
            &ColumnType::TimestampTz,
            false,
            &[],
            false,
        );
        assert_eq!(ct, Some(ColumnDefault::Now));
    }

    #[test]
    fn normalize_default_null_is_none() {
        assert_eq!(
            normalize_default(None, &ColumnType::Text, false, &[], false),
            None
        );
        assert_eq!(
            normalize_default(Some("  "), &ColumnType::Text, false, &[], false),
            None
        );
    }

    #[test]
    fn normalize_serial_pk_default_stripped_to_none() {
        // (a) A `nextval(...)` default on a single-column int8 PK that OWNS its
        // sequence (the conventional `BIGSERIAL` shape) → None (BIGSERIAL id).
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('posts_id_seq'::regclass)"),
            &ColumnType::Int64,
            true,
            &pk,
            true, // owns its sequence — conventional BIGSERIAL
        );
        assert_eq!(
            d, None,
            "int8 serial PK default (owned sequence) must be stripped to None"
        );
    }

    #[test]
    fn normalize_int8_serial_pk_shared_sequence_default_is_preserved() {
        // (b) A `nextval(...)` default on a single-column int8 PK backed by a
        // SHARED/custom sequence the column does NOT own
        // (`id BIGINT PRIMARY KEY DEFAULT nextval('global_ids')`) must be PRESERVED
        // verbatim — stripping it would make recreation mint a fresh owned
        // `BIGSERIAL` (a new sequence) instead of continuing to allocate from
        // `global_ids`, silently changing ID-allocation behavior.
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('global_ids'::regclass)"),
            &ColumnType::Int64,
            true,
            &pk,
            false, // does NOT own the sequence — shared/custom
        );
        assert_eq!(
            d,
            Some(ColumnDefault::Sql(
                "nextval('global_ids'::regclass)".to_owned()
            )),
            "int8 PK default over a non-owned (shared) sequence must be preserved verbatim"
        );
    }

    #[test]
    fn normalize_int4_serial_pk_default_is_kept() {
        // (c) A `nextval(...)` default on a single-column int4 PK is a brownfield
        // `SERIAL PRIMARY KEY` — its default is PRESERVED (not stripped) so the DDL
        // emitter can reconstruct it as `SERIAL` rather than a plain `INTEGER`. This
        // holds regardless of ownership (only int8 owned serials are stripped).
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('counters_id_seq'::regclass)"),
            &ColumnType::Int32,
            true,
            &pk,
            true, // even an owned int4 serial keeps its default (int4 is never stripped)
        );
        assert_eq!(
            d,
            Some(ColumnDefault::Sql(
                "nextval('counters_id_seq'::regclass)".to_owned()
            )),
            "int4 SERIAL PK default must be preserved, not stripped"
        );
    }

    #[test]
    fn normalize_int4_pk_without_default_stays_none() {
        // A plain int4 PK with NO default stays default-less → a plain integer PK
        // (no auto-increment), never mistaken for a SERIAL.
        let pk = vec!["id".to_owned()];
        let d = normalize_default(None, &ColumnType::Int32, true, &pk, false);
        assert_eq!(d, None, "a plain int4 PK carries no default");
    }

    #[test]
    fn normalize_serial_default_on_non_pk_is_kept_as_sql() {
        // A sequence default on a NON-pk column is not the id shape — keep it (even
        // when the column owns the sequence, i.e. a non-PK `SERIAL` column).
        let d = normalize_default(
            Some("nextval('counter_seq'::regclass)"),
            &ColumnType::Int64,
            false,
            &[],
            true,
        );
        assert!(matches!(d, Some(ColumnDefault::Sql(_))));
    }

    #[test]
    fn serial_kind_marker_distinguishes_bigserial_from_plain_bigint_pk() {
        let pk = vec!["id".to_owned()];
        // An int8 owned-sequence serial PK → BigSerial.
        assert_eq!(
            serial_kind_for(
                Some("nextval('posts_id_seq'::regclass)"),
                &ColumnType::Int64,
                true,
                &pk,
                true,
                false
            ),
            Some(SerialKind::BigSerial)
        );
        // An int4 owned-sequence serial PK → Serial.
        assert_eq!(
            serial_kind_for(
                Some("nextval('counters_id_seq'::regclass)"),
                &ColumnType::Int32,
                true,
                &pk,
                true,
                false
            ),
            Some(SerialKind::Serial)
        );
        // A plain BIGINT PRIMARY KEY (no default, no owned sequence) → the EXPLICIT
        // `Plain` marker (three-state): it is NOT a BigSerial, and the IR now tells
        // them apart, while a legacy/unknown snapshot (marker absent) stays `None`.
        assert_eq!(
            serial_kind_for(None, &ColumnType::Int64, true, &pk, false, false),
            Some(SerialKind::Plain)
        );
        // A shared/custom (non-owned) sequence default is still a single-column
        // integer PK with no OWNED sequence → the explicit `Plain` marker (it is not
        // a conventional owned serial).
        assert_eq!(
            serial_kind_for(
                Some("nextval('global_ids'::regclass)"),
                &ColumnType::Int64,
                true,
                &pk,
                false,
                false
            ),
            Some(SerialKind::Plain)
        );
        // A non-PK serial column → None (not an id).
        assert_eq!(
            serial_kind_for(
                Some("nextval('c_seq'::regclass)"),
                &ColumnType::Int64,
                false,
                &[],
                true,
                false
            ),
            None
        );
        // A UUID PK → None (the serial-vs-plain distinction is integer-only).
        assert_eq!(
            serial_kind_for(None, &ColumnType::Uuid, true, &pk, false, false),
            None
        );
        // A GENERATED … AS IDENTITY integer PK → None (its id-generation is carried
        // by the separate `identity` marker, never the serial marker).
        assert_eq!(
            serial_kind_for(None, &ColumnType::Int64, true, &pk, false, true),
            None
        );
    }

    #[test]
    fn build_table_marks_bigserial_id_and_preserves_identity() {
        let columns = vec![
            // A conventional BIGSERIAL id (owns its sequence).
            ColumnRow {
                table_name: "t".to_owned(),
                column_name: "id".to_owned(),
                udt_name: "int8".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: Some("nextval('t_id_seq'::regclass)".to_owned()),
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                owns_sequence: true,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
            // A GENERATED ALWAYS AS IDENTITY column (no default, is_identity=YES).
            ColumnRow {
                table_name: "t".to_owned(),
                column_name: "seqno".to_owned(),
                udt_name: "int8".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "YES".to_owned(),
                identity_generation: Some("ALWAYS".to_owned()),
            },
        ];
        let pk = vec!["id".to_owned()];
        let table = build_table("t", &columns, &pk, &[], &[], &[]);
        let id = table.columns.iter().find(|c| c.name == "id").unwrap();
        assert_eq!(id.serial, Some(SerialKind::BigSerial));
        assert_eq!(
            id.default, None,
            "the owned int8 serial default is stripped"
        );
        assert!(id.identity.is_none());
        let seqno = table.columns.iter().find(|c| c.name == "seqno").unwrap();
        assert_eq!(
            seqno.identity.as_deref(),
            Some("ALWAYS"),
            "an identity column preserves its generation clause verbatim"
        );
        assert!(
            seqno.serial.is_none(),
            "an identity column is not a serial (no owned nextval default)"
        );
    }

    #[test]
    fn collapse_indexes_nulls_not_distinct_unique_retained_verbatim() {
        // A PG15+ `NULLS NOT DISTINCT` unique index (a bare CREATE UNIQUE INDEX, no
        // backing constraint) must be RETAINED via `definition` so the clause
        // round-trips — never collapsed to an ordinary unique index that drops it.
        let def = "CREATE UNIQUE INDEX u_idx ON public.t USING btree (email) NULLS NOT DISTINCT";
        let rows = vec![index_row(
            "u_idx", true, "email", "email", false, false, def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(
            indexes[0].definition.as_deref(),
            Some(def),
            "a NULLS NOT DISTINCT unique index must be retained verbatim via its definition"
        );
        assert!(
            unique_cols.is_empty(),
            "a NULLS NOT DISTINCT unique index is not a plain simple unique, so it \
             sets no single-column unique flag"
        );
    }

    #[test]
    fn normalize_uuid_pk_default_is_kept_as_sql() {
        // (d) A UUID PK keeps its gen_random_uuid() default (the model parser records
        // the same), so the two agree on a round-trip — unaffected by the ownership
        // signal (UUID PKs have no sequence).
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("gen_random_uuid()"),
            &ColumnType::Uuid,
            true,
            &pk,
            false,
        );
        assert_eq!(d, Some(ColumnDefault::Sql("gen_random_uuid()".to_owned())));
    }

    /// Build a one-per-index [`IndexRow`] for tests. `columns` is the comma-joined
    /// resolvable *key*-column string the SQL aggregation produces; `dep_columns`
    /// is the comma-joined `pg_depend` dependent-column set (key + expression +
    /// predicate columns). For a simple index the two are equal; for an
    /// expression/partial index they differ (empty/partial key columns, full
    /// dependent set). `is_constraint` is left `false` (an ordinary/model-style or
    /// expression/partial index); use [`constraint_index_row`] for a
    /// constraint-owned index.
    fn index_row(
        index_name: &str,
        is_unique: bool,
        columns: &str,
        dep_columns: &str,
        is_partial: bool,
        has_expression: bool,
        definition: &str,
    ) -> IndexRow {
        IndexRow {
            table_name: "t".to_owned(),
            index_name: index_name.to_owned(),
            is_unique,
            definition: definition.to_owned(),
            is_partial,
            has_expression,
            has_include: false,
            is_constraint: false,
            constraint_type: String::new(),
            constraint_name: String::new(),
            constraint_def: String::new(),
            columns: columns.to_owned(),
            // Mirror the SQL `key_columns`: empty when any key position is an
            // expression, otherwise the (non-INCLUDE) key columns — which, for these
            // `has_include: false` fixtures, are exactly `columns`.
            key_columns: if has_expression {
                String::new()
            } else {
                columns.to_owned()
            },
            dep_columns: dep_columns.to_owned(),
        }
    }

    /// Build a constraint-owned (non-primary, non-partial, non-expression)
    /// [`IndexRow`] — the auto-created index behind a brownfield column/table-level
    /// `UNIQUE`/`EXCLUDE` constraint (`is_constraint == true`). The `pg_depend`
    /// dependent set of such an index equals its key columns.
    fn constraint_index_row(
        index_name: &str,
        is_unique: bool,
        columns: &str,
        definition: &str,
    ) -> IndexRow {
        IndexRow {
            is_constraint: true,
            // A UNIQUE constraint (`contype = 'u'`) — the EXCLUDE (`'x'`) path is
            // exercised by [`exclude_constraint_index_row`].
            constraint_type: "u".to_owned(),
            ..index_row(
                index_name, is_unique, columns, columns, false, false, definition,
            )
        }
    }

    /// Build an EXCLUDE-constraint-owned (`contype = 'x'`) [`IndexRow`]. The
    /// `constraint_def` is the `pg_get_constraintdef` text (`EXCLUDE USING <method>
    /// (…)`); the backing index `definition` is a plain `CREATE INDEX` Postgres
    /// auto-created, which alone would NOT enforce the exclusion.
    fn exclude_constraint_index_row(
        index_name: &str,
        constraint_name: &str,
        dep_columns: &str,
        index_definition: &str,
        constraint_def: &str,
    ) -> IndexRow {
        IndexRow {
            is_constraint: true,
            constraint_type: "x".to_owned(),
            constraint_name: constraint_name.to_owned(),
            constraint_def: constraint_def.to_owned(),
            // An EXCLUDE index is never `unique`.
            ..index_row(
                index_name,
                false,
                dep_columns,
                dep_columns,
                false,
                false,
                index_definition,
            )
        }
    }

    #[test]
    fn collapse_indexes_single_column_unique_sets_flag_and_index() {
        let rows = vec![index_row(
            "idx_accounts_email_unique",
            true,
            "email",
            "email",
            false,
            false,
            "CREATE UNIQUE INDEX idx_accounts_email_unique ON public.accounts USING btree (email)",
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx_accounts_email_unique");
        assert_eq!(indexes[0].columns, vec!["email".to_owned()]);
        assert!(indexes[0].unique);
        // A simple index keeps `definition` None so its JSON is unchanged.
        assert_eq!(indexes[0].definition, None);
        assert!(unique_cols.contains("email"));
    }

    #[test]
    fn collapse_indexes_plain_index_no_unique_flag() {
        let rows = vec![index_row(
            "idx_comments_post_id",
            false,
            "post_id",
            "post_id",
            false,
            false,
            "CREATE INDEX idx_comments_post_id ON public.comments USING btree (post_id)",
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert!(!indexes[0].unique);
        assert_eq!(indexes[0].definition, None);
        assert!(unique_cols.is_empty());
    }

    #[test]
    fn collapse_indexes_multi_column_unique_only_index_not_column_flag() {
        // A composite unique index records an Index but does NOT set a per-column
        // unique flag (the model IR's `unique` flag is single-column only).
        let rows = vec![index_row(
            "idx_memberships_org_user",
            true,
            "org_id,user_id",
            "org_id,user_id",
            false,
            false,
            "CREATE UNIQUE INDEX idx_memberships_org_user ON public.memberships USING btree (org_id, user_id)",
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(
            indexes[0].columns,
            vec!["org_id".to_owned(), "user_id".to_owned()]
        );
        assert!(indexes[0].unique);
        assert_eq!(indexes[0].definition, None);
        assert!(
            unique_cols.is_empty(),
            "composite unique sets no column flag"
        );
    }

    #[test]
    fn collapse_indexes_expression_index_preserved_not_dropped() {
        // An expression index (indkey contains 0 → has_expression) must be
        // preserved verbatim via `definition`, never dropped, and must not set a
        // column `unique` flag.
        let def = "CREATE INDEX users_lower_email_idx ON public.users USING btree (lower(email))";
        let rows = vec![index_row(
            "users_lower_email_idx",
            false,
            "",      // no resolvable ordinary KEY columns (all-expression)
            "email", // but `pg_depend` records the expression-referenced column
            false,
            true,
            def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1, "expression index preserved, not dropped");
        assert_eq!(indexes[0].name, "users_lower_email_idx");
        // A definition-carrying index carries its exact `pg_depend` dependent set,
        // so the expression-referenced `email` column is captured for exact
        // cascade/dependency detection.
        assert_eq!(indexes[0].columns, vec!["email".to_owned()]);
        assert_eq!(indexes[0].definition.as_deref(), Some(def));
        assert!(unique_cols.is_empty());
    }

    #[test]
    fn collapse_indexes_partial_index_preserves_predicate() {
        // A partial index over a real column keeps its `WHERE` predicate via the
        // verbatim definition rather than degrading to a plain column index.
        let def = "CREATE INDEX users_active_idx ON public.users USING btree (email) \
                   WHERE (deleted_at IS NULL)";
        let rows = vec![index_row(
            "users_active_idx",
            false,
            "email",            // key column
            "deleted_at,email", // `pg_depend` set: predicate + key column (sorted)
            true,
            false,
            def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        // A partial index depends on BOTH its key column and its predicate column;
        // the exact `pg_depend` set (which cascades on `DROP COLUMN`) is captured.
        assert_eq!(
            indexes[0].columns,
            vec!["deleted_at".to_owned(), "email".to_owned()]
        );
        assert_eq!(indexes[0].definition.as_deref(), Some(def));
        assert!(
            unique_cols.is_empty(),
            "partial index sets no column unique flag"
        );
    }

    #[test]
    fn collapse_indexes_constraint_owned_index_retained_via_definition() {
        // A brownfield column-level `UNIQUE` (e.g. `email TEXT UNIQUE`) auto-creates
        // a constraint-backed index Postgres names (e.g. `users_email_key`). It must
        // be RETAINED verbatim via `definition` (never an ordinary droppable index),
        // so a later model-side diff can't emit a `DROP INDEX` Postgres rejects. Its
        // single-column `unique` flag is still set (accurate, and not diffed).
        let def = "CREATE UNIQUE INDEX users_email_key ON public.users USING btree (email)";
        let rows = vec![constraint_index_row("users_email_key", true, "email", def)];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "users_email_key");
        assert_eq!(indexes[0].columns, vec!["email".to_owned()]);
        assert!(indexes[0].unique);
        assert_eq!(
            indexes[0].definition.as_deref(),
            Some(def),
            "constraint-owned index must be retained via its definition, not left droppable"
        );
        assert!(
            unique_cols.contains("email"),
            "single-column constraint-owned unique still sets the column unique flag"
        );
    }

    #[test]
    fn collapse_indexes_exclude_constraint_retained_as_real_constraint() {
        // A range EXCLUDE constraint (`EXCLUDE USING gist (during WITH &&)`) is
        // backed by a gist index Postgres auto-creates. Retaining ONLY that backing
        // `CREATE INDEX` would NOT enforce the exclusion on a rollback/recreation
        // (overlapping rows become possible — a data-integrity loss), so the retained
        // `definition` must be the REAL constraint recreation: `ALTER TABLE … ADD
        // CONSTRAINT … EXCLUDE USING …`, not a `CREATE INDEX`.
        let index_def =
            "CREATE INDEX reservations_during_excl ON public.reservations USING gist (during)";
        let constraint_def = "EXCLUDE USING gist (during WITH &&)";
        let rows = vec![exclude_constraint_index_row(
            "reservations_during_excl",
            "reservations_during_excl",
            "during",
            index_def,
            constraint_def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        let idx = &indexes[0];
        let def = idx
            .definition
            .as_deref()
            .expect("EXCLUDE constraint must be retained via a definition");
        assert!(
            def.contains("ADD CONSTRAINT reservations_during_excl")
                && def.contains("EXCLUDE USING gist"),
            "EXCLUDE index must be recreated as the real ADD CONSTRAINT … EXCLUDE form, got: {def}"
        );
        assert!(
            !def.starts_with("CREATE INDEX") && !def.contains("CREATE INDEX"),
            "the retained definition must NOT be the bare backing CREATE INDEX, got: {def}"
        );
        assert_eq!(
            def,
            // The fixture's table_name is "t"; the ALTER uses the bare table name
            // form the emitter uses elsewhere (`index_sql` plain branch / CREATE TABLE).
            "ALTER TABLE t ADD CONSTRAINT reservations_during_excl EXCLUDE USING gist (during WITH &&)",
            "the ALTER TABLE must use the bare table name form the emitter uses elsewhere"
        );
        // Dependency (cascade) set is still populated; not unique; key_columns empty.
        assert_eq!(idx.columns, vec!["during".to_owned()]);
        assert!(!idx.unique, "an EXCLUDE index is not unique");
        assert!(
            idx.key_columns.is_empty(),
            "EXCLUDE index leaves key_columns empty (not a #[unique] candidate)"
        );
        assert!(
            unique_cols.is_empty(),
            "an EXCLUDE constraint sets no column unique flag"
        );
    }

    #[test]
    fn collapse_indexes_model_style_unique_index_stays_ordinary_droppable() {
        // The MODEL style: `CREATE UNIQUE INDEX idx_users_email_unique` with NO
        // backing constraint (`is_constraint == false`) must stay an ORDINARY
        // `Index { definition: None }` so the migrate-from-models round-trip is
        // clean — a `#[unique]` field produces a plain unique index, not a
        // constraint, so it is diffable/droppable as before.
        let def = "CREATE UNIQUE INDEX idx_users_email_unique ON public.users USING btree (email)";
        let rows = vec![index_row(
            "idx_users_email_unique",
            true,
            "email",
            "email",
            false,
            false,
            def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx_users_email_unique");
        assert!(indexes[0].unique);
        assert_eq!(
            indexes[0].definition, None,
            "a non-constraint (model-style) unique index stays ordinary/droppable"
        );
        assert!(unique_cols.contains("email"));
    }

    #[test]
    fn collapse_indexes_covering_include_index_retained_via_definition() {
        // A covering index `CREATE UNIQUE INDEX ... ON t (a) INCLUDE (b)`
        // (`has_include == true`) must be RETAINED verbatim via `definition`,
        // never rebuilt as a plain `(a, b)` unique index (which would widen the
        // uniqueness scope from (a) to (a, b) and drop the covering behavior). It
        // must NOT set a single-column `unique` column flag, and `columns` carries
        // the exact `pg_depend` dependent set (key + INCLUDE columns).
        let def = "CREATE UNIQUE INDEX idx_cover ON public.t USING btree (a) INCLUDE (b)";
        let rows = vec![IndexRow {
            has_include: true,
            ..index_row("idx_cover", true, "a,b", "a,b", false, false, def)
        }];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx_cover");
        assert!(indexes[0].unique);
        assert_eq!(
            indexes[0].definition.as_deref(),
            Some(def),
            "an INCLUDE covering index must be retained via its definition, not left droppable"
        );
        assert_eq!(
            indexes[0].columns,
            vec!["a".to_owned(), "b".to_owned()],
            "columns carries the exact pg_depend dependent set (key + INCLUDE)"
        );
        assert!(
            unique_cols.is_empty(),
            "an INCLUDE unique index must not set a single-column unique flag \
             (its uniqueness scope is only the key column)"
        );
    }

    #[test]
    fn build_table_assembles_columns_pk_fk_and_indexes() {
        let columns = vec![
            ColumnRow {
                table_name: "comments".to_owned(),
                column_name: "id".to_owned(),
                udt_name: "int8".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: Some("nextval('comments_id_seq'::regclass)".to_owned()),
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                // The conventional BIGSERIAL id owns its sequence, so the nextval
                // default is stripped to None below.
                owns_sequence: true,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
            ColumnRow {
                table_name: "comments".to_owned(),
                column_name: "post_id".to_owned(),
                udt_name: "int8".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
        ];
        let pk = vec!["id".to_owned()];
        let indexes = vec![index_row(
            "idx_comments_post_id",
            false,
            "post_id",
            "post_id",
            false,
            false,
            "CREATE INDEX idx_comments_post_id ON public.comments USING btree (post_id)",
        )];
        let fks = vec![ForeignKeyRow {
            table_name: "comments".to_owned(),
            column_name: "post_id".to_owned(),
            foreign_table: "posts".to_owned(),
            foreign_schema: "public".to_owned(),
            foreign_column: "id".to_owned(),
        }];
        let table = build_table("comments", &columns, &pk, &indexes, &fks, &[]);
        assert_eq!(table.name, "comments");
        assert!(table.managed);
        assert_eq!(table.primary_key, vec!["id".to_owned()]);

        let id = table.columns.iter().find(|c| c.name == "id").unwrap();
        assert_eq!(id.ty, ColumnType::Int64);
        assert!(id.primary_key);
        // The serial default is stripped → BigSerial id shape.
        assert_eq!(id.default, None);

        let post_id = table.columns.iter().find(|c| c.name == "post_id").unwrap();
        assert_eq!(post_id.references, Some(ForeignKey::new("posts", "id")));

        assert_eq!(table.indexes.len(), 1);
        assert_eq!(table.indexes[0].name, "idx_comments_post_id");
    }

    #[test]
    fn build_table_preserves_opaque_type() {
        let columns = vec![ColumnRow {
            table_name: "widgets".to_owned(),
            column_name: "addr".to_owned(),
            udt_name: "inet".to_owned(),
            is_nullable: "YES".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: None,
            domain_schema: None,
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let table = build_table("widgets", &columns, &[], &[], &[], &[]);
        let addr = table.columns.iter().find(|c| c.name == "addr").unwrap();
        assert_eq!(
            addr.ty,
            ColumnType::Opaque {
                pg_type: "inet".to_owned()
            }
        );
        assert!(addr.nullable);
    }

    #[test]
    fn build_table_preserves_domain_column_as_opaque() {
        // A column typed on a Postgres DOMAIN reports its base type in `udt_name`
        // (`text`) but names the domain in `domain_name`. The fail-closed floor
        // must preserve the domain verbatim as `Opaque`, NOT flatten to `Text`.
        let columns = vec![
            ColumnRow {
                table_name: "contacts".to_owned(),
                column_name: "email".to_owned(),
                udt_name: "text".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: Some("email_dom".to_owned()),
                domain_schema: Some("public".to_owned()),
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
            // A non-domain column (`domain_name` NULL) maps exactly as before.
            ColumnRow {
                table_name: "contacts".to_owned(),
                column_name: "note".to_owned(),
                udt_name: "text".to_owned(),
                is_nullable: "YES".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
        ];
        let table = build_table("contacts", &columns, &[], &[], &[], &[]);
        let email = table.columns.iter().find(|c| c.name == "email").unwrap();
        assert_eq!(
            email.ty,
            ColumnType::Opaque {
                pg_type: "email_dom".to_owned()
            },
            "a public-schema domain column preserves the bare domain name as Opaque"
        );
        let note = table.columns.iter().find(|c| c.name == "note").unwrap();
        assert_eq!(
            note.ty,
            ColumnType::Text,
            "a non-domain text column maps to Text unchanged"
        );
    }

    #[test]
    fn build_table_preserves_length_limited_char_columns_as_opaque() {
        // `VARCHAR(32)` / `CHAR(2)` report `udt_name` `varchar` / `bpchar` with a
        // `character_maximum_length`. The fail-closed floor must preserve the length
        // verbatim as `Opaque` (`varchar(32)` / `char(2)`), NOT flatten to `Text`
        // and drop the DB-enforced limit. Unbounded `varchar` / `text` stay `Text`.
        let columns = vec![
            ColumnRow {
                table_name: "labels".to_owned(),
                column_name: "code".to_owned(),
                udt_name: "varchar".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: Some(32),
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
            ColumnRow {
                table_name: "labels".to_owned(),
                column_name: "kind".to_owned(),
                udt_name: "bpchar".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: Some(2),
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
            ColumnRow {
                table_name: "labels".to_owned(),
                column_name: "note".to_owned(),
                udt_name: "text".to_owned(),
                is_nullable: "YES".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
                character_maximum_length: None,
                domain_name: None,
                domain_schema: None,
                owns_sequence: false,
                is_identity: "NO".to_owned(),
                identity_generation: None,
            },
        ];
        let table = build_table("labels", &columns, &[], &[], &[], &[]);
        let code = table.columns.iter().find(|c| c.name == "code").unwrap();
        assert_eq!(
            code.ty,
            ColumnType::Opaque {
                pg_type: "varchar(32)".to_owned()
            },
            "VARCHAR(32) is preserved as Opaque, not flattened to Text"
        );
        let kind = table.columns.iter().find(|c| c.name == "kind").unwrap();
        assert_eq!(
            kind.ty,
            ColumnType::Opaque {
                pg_type: "char(2)".to_owned()
            },
            "CHAR(2) is preserved as Opaque, not flattened to Text"
        );
        let note = table.columns.iter().find(|c| c.name == "note").unwrap();
        assert_eq!(
            note.ty,
            ColumnType::Text,
            "an unbounded text column stays Text"
        );
    }

    #[test]
    fn build_table_domain_precedence_over_length_floor() {
        // A domain column reports its base type (e.g. `varchar`) in `udt_name` and
        // can carry a `character_maximum_length`, but the domain floor must win:
        // the column is preserved as the domain type, not `varchar(n)`.
        let columns = vec![ColumnRow {
            table_name: "labels".to_owned(),
            column_name: "code".to_owned(),
            udt_name: "varchar".to_owned(),
            is_nullable: "NO".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: Some(32),
            domain_name: Some("code_dom".to_owned()),
            domain_schema: Some("public".to_owned()),
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let table = build_table("labels", &columns, &[], &[], &[], &[]);
        let code = table.columns.iter().find(|c| c.name == "code").unwrap();
        assert_eq!(
            code.ty,
            ColumnType::Opaque {
                pg_type: "code_dom".to_owned()
            },
            "the domain floor takes precedence over the length floor"
        );
    }

    #[test]
    fn build_table_qualifies_non_public_domain_column() {
        // A domain in a non-`public` schema is schema-qualified so recreation
        // references the correct domain type.
        let columns = vec![ColumnRow {
            table_name: "contacts".to_owned(),
            column_name: "email".to_owned(),
            udt_name: "text".to_owned(),
            is_nullable: "NO".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: Some("email_dom".to_owned()),
            domain_schema: Some("otherschema".to_owned()),
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let table = build_table("contacts", &columns, &[], &[], &[], &[]);
        let email = table.columns.iter().find(|c| c.name == "email").unwrap();
        assert_eq!(
            email.ty,
            ColumnType::Opaque {
                pg_type: "otherschema.email_dom".to_owned()
            }
        );
    }

    #[test]
    fn build_table_qualifies_non_public_fk_target() {
        // A FK referencing a table outside `public` is stored schema-qualified in
        // `ForeignKey.table` so recreation emits `REFERENCES auth.users(id)`, not
        // a wrong bare `REFERENCES users(id)`.
        let columns = vec![ColumnRow {
            table_name: "sessions".to_owned(),
            column_name: "account_id".to_owned(),
            udt_name: "int8".to_owned(),
            is_nullable: "YES".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: None,
            domain_schema: None,
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let fks = vec![ForeignKeyRow {
            table_name: "sessions".to_owned(),
            column_name: "account_id".to_owned(),
            foreign_table: "accounts".to_owned(),
            foreign_schema: "auth".to_owned(),
            foreign_column: "id".to_owned(),
        }];
        let table = build_table("sessions", &columns, &[], &[], &fks, &[]);
        let account_id = table
            .columns
            .iter()
            .find(|c| c.name == "account_id")
            .unwrap();
        assert_eq!(
            account_id.references,
            Some(ForeignKey::new("auth.accounts", "id")),
            "a cross-schema FK target is stored schema-qualified"
        );
    }

    #[test]
    fn build_table_keeps_public_fk_target_bare() {
        // The common case — a FK to a `public` table — stays a bare relname.
        let columns = vec![ColumnRow {
            table_name: "comments".to_owned(),
            column_name: "post_id".to_owned(),
            udt_name: "int8".to_owned(),
            is_nullable: "YES".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: None,
            domain_schema: None,
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let fks = vec![ForeignKeyRow {
            table_name: "comments".to_owned(),
            column_name: "post_id".to_owned(),
            foreign_table: "posts".to_owned(),
            foreign_schema: "public".to_owned(),
            foreign_column: "id".to_owned(),
        }];
        let table = build_table("comments", &columns, &[], &[], &fks, &[]);
        let post_id = table.columns.iter().find(|c| c.name == "post_id").unwrap();
        assert_eq!(
            post_id.references,
            Some(ForeignKey::new("posts", "id")),
            "a public FK target stays bare, unchanged"
        );
    }

    #[test]
    fn build_table_populates_check_constraints() {
        // A fetched CHECK row is preserved as a named CheckConstraint whose
        // expression is stripped to the inner predicate the emitter re-wraps.
        let columns = vec![ColumnRow {
            table_name: "products".to_owned(),
            column_name: "price".to_owned(),
            udt_name: "int4".to_owned(),
            is_nullable: "NO".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: None,
            domain_schema: None,
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let checks = vec![CheckRow {
            table_name: "products".to_owned(),
            constraint_name: "products_price_check".to_owned(),
            definition: "CHECK ((price >= 0))".to_owned(),
        }];
        let table = build_table("products", &columns, &[], &[], &[], &checks);
        assert_eq!(table.checks.len(), 1);
        assert_eq!(
            table.checks[0].name.as_deref(),
            Some("products_price_check")
        );
        assert_eq!(table.checks[0].expression, "(price >= 0)");
    }

    #[test]
    fn build_table_without_checks_is_empty() {
        let columns = vec![ColumnRow {
            table_name: "products".to_owned(),
            column_name: "price".to_owned(),
            udt_name: "int4".to_owned(),
            is_nullable: "NO".to_owned(),
            column_default: None,
            numeric_precision: None,
            numeric_scale: None,
            character_maximum_length: None,
            domain_name: None,
            domain_schema: None,
            owns_sequence: false,
            is_identity: "NO".to_owned(),
            identity_generation: None,
        }];
        let table = build_table("products", &columns, &[], &[], &[], &[]);
        assert!(
            table.checks.is_empty(),
            "a table with no CHECK rows has empty checks (unchanged)"
        );
    }

    #[test]
    fn strip_check_wrapper_unwraps_pg_constraintdef() {
        assert_eq!(strip_check_wrapper("CHECK ((price >= 0))"), "(price >= 0)");
        assert_eq!(
            strip_check_wrapper("CHECK (status IN ('a','b'))"),
            "status IN ('a','b')"
        );
        // A shape that doesn't match is returned trimmed verbatim (never dropped).
        assert_eq!(strip_check_wrapper("price >= 0"), "price >= 0");
    }

    #[test]
    fn strip_check_wrapper_drops_trailing_suffixes() {
        // `pg_get_constraintdef` can append `NOT VALID` / `NO INHERIT` after the
        // predicate; the suffix must be dropped so the emitter's `CHECK ({expr})`
        // re-wrap stays valid — never left inside the parens.
        assert_eq!(
            strip_check_wrapper("CHECK ((price >= 0)) NOT VALID"),
            "(price >= 0)"
        );
        assert_eq!(strip_check_wrapper("CHECK (x > 0) NO INHERIT"), "x > 0");
        // No suffix → unchanged.
        assert_eq!(strip_check_wrapper("CHECK (a = 1)"), "a = 1");
        // A paren inside a string literal never miscounts the balanced span.
        assert_eq!(
            strip_check_wrapper("CHECK (label <> ')') NOT VALID"),
            "label <> ')'"
        );
    }
}
