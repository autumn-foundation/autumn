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
//!   `nextval(...)` sequence is a `BIGSERIAL`-shaped id, which the model IR
//!   represents as an [`Int64`](ColumnType::Int64) PK with **no** default — so the
//!   `nextval` default is stripped to `None`. A single-column UUID PK keeps its
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
//! - **`GENERATED ... AS IDENTITY` columns**: pulled as ordinary integer columns
//!   (their identity metadata from `information_schema.columns.is_identity` is not
//!   modeled); recreation emits a plain column / loses the identity — identity
//!   modeling is deferred alongside owned sequences.
//!
//! Errors are **credential-safe** by construction: no variant ever embeds the
//! resolved database URL (only a parsed host/port on the connection-error path,
//! mirroring [`crate::db_pull`]).

use std::collections::BTreeMap;

use autumn_schema_core::{
    Backend, CheckConstraint, Column, ColumnDefault, ColumnType, ForeignKey, Index, Table,
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
    let query = format!(
        "SELECT table_name, column_name, udt_name, is_nullable, column_default, \
         numeric_precision::integer AS numeric_precision, \
         numeric_scale::integer AS numeric_scale, \
         character_maximum_length::integer AS character_maximum_length, \
         domain_name, domain_schema FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name IN ({}) \
         ORDER BY table_name, ordinal_position",
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
        column.default = normalize_default(row.column_default.as_deref(), &ty, is_pk, primary_key);
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
        let simple = !row.is_partial && !row.has_expression && !row.has_include;
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
/// - a `nextval(...)` serial default on a single-column **int8** primary key →
///   `None` (a `BIGSERIAL`-shaped id has no explicit default in the model IR;
///   see [`normalize_serial_pk_default`]). An **int4** `SERIAL` PK keeps its
///   `nextval(...)` default so the emitter can reconstruct it as `SERIAL`.
/// - anything else → [`ColumnDefault::Sql`] verbatim (e.g. a UUID PK's
///   `gen_random_uuid()`, which the model parser records identically).
fn normalize_default(
    raw: Option<&str>,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
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
    // BIGSERIAL id shape — the model IR carries no explicit default for it.
    if normalize_serial_pk_default(&lowered, ty, is_primary_key, primary_key) {
        return None;
    }
    Some(ColumnDefault::Sql(raw.to_owned()))
}

/// Whether a raw (lower-cased) default is the auto-increment sequence default of
/// a single-column **`BIGSERIAL`** (int8) primary key — i.e. the shape the model IR
/// represents as an [`Int64`](ColumnType::Int64) PK with **no** default. Such a
/// default is stripped to `None` so an introspected `BIGSERIAL` id round-trips
/// against the model parser (whose `pk_kind_for` requires `default.is_none()` for a
/// `BigSerial` PK).
///
/// A single-column **`SERIAL`** (int4) PK is deliberately **not** stripped: the
/// model IR has no default-less int4-PK shape, so keeping the `nextval(...)` default
/// is what lets the DDL emitter recognize it (via `pk_kind_for`) and reconstruct it
/// as `SERIAL PRIMARY KEY` — recreating it as a plain `INTEGER PRIMARY KEY` would
/// silently drop the sequence. A plain int PK with no default stays default-less and
/// so recreates as a plain integer PK (no auto-increment).
fn normalize_serial_pk_default(
    lowered_default: &str,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
) -> bool {
    is_primary_key
        && primary_key.len() == 1
        && matches!(ty, ColumnType::Int64)
        && lowered_default.starts_with("nextval(")
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
        let now = normalize_default(Some("now()"), &ColumnType::Timestamp, false, &[]);
        assert_eq!(now, Some(ColumnDefault::Now));
        let ct = normalize_default(
            Some("CURRENT_TIMESTAMP"),
            &ColumnType::TimestampTz,
            false,
            &[],
        );
        assert_eq!(ct, Some(ColumnDefault::Now));
    }

    #[test]
    fn normalize_default_null_is_none() {
        assert_eq!(normalize_default(None, &ColumnType::Text, false, &[]), None);
        assert_eq!(
            normalize_default(Some("  "), &ColumnType::Text, false, &[]),
            None
        );
    }

    #[test]
    fn normalize_serial_pk_default_stripped_to_none() {
        // A `nextval(...)` default on a single-column int8 PK → None (BIGSERIAL id).
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('posts_id_seq'::regclass)"),
            &ColumnType::Int64,
            true,
            &pk,
        );
        assert_eq!(d, None, "int8 serial PK default must be stripped to None");
    }

    #[test]
    fn normalize_int4_serial_pk_default_is_kept() {
        // A `nextval(...)` default on a single-column int4 PK is a brownfield
        // `SERIAL PRIMARY KEY` — its default is PRESERVED (not stripped) so the DDL
        // emitter can reconstruct it as `SERIAL` rather than a plain `INTEGER`.
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('counters_id_seq'::regclass)"),
            &ColumnType::Int32,
            true,
            &pk,
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
        let d = normalize_default(None, &ColumnType::Int32, true, &pk);
        assert_eq!(d, None, "a plain int4 PK carries no default");
    }

    #[test]
    fn normalize_serial_default_on_non_pk_is_kept_as_sql() {
        // A sequence default on a NON-pk column is not the id shape — keep it.
        let d = normalize_default(
            Some("nextval('counter_seq'::regclass)"),
            &ColumnType::Int64,
            false,
            &[],
        );
        assert!(matches!(d, Some(ColumnDefault::Sql(_))));
    }

    #[test]
    fn normalize_uuid_pk_default_is_kept_as_sql() {
        // A UUID PK keeps its gen_random_uuid() default (the model parser records
        // the same), so the two agree on a round-trip.
        let pk = vec!["id".to_owned()];
        let d = normalize_default(Some("gen_random_uuid()"), &ColumnType::Uuid, true, &pk);
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
