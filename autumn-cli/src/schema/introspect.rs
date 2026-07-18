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
//! - **Composite / multi-column foreign keys**: only the first
//!   referencing/referenced column pair is recorded (the IR [`ForeignKey`] is
//!   single-column). The model parser never emits a composite FK.
//! - **Foreign-key / constraint names**: the IR [`ForeignKey`] carries only
//!   `table`/`column`, so the Postgres constraint name is not represented (and so
//!   never diffed).
//! - **Enum `CHECK` constraints**: enum recovery from `CHECK` expressions is a
//!   later slice; a `TEXT`-with-`CHECK` column pulls back as plain
//!   [`Text`](ColumnType::Text).
//! - **Constraint-vs-index reconciliation for constraint-owned indexes**: a
//!   brownfield `UNIQUE`/`EXCLUDE` constraint is retained by its *index*
//!   definition (`pg_get_indexdef`, a valid `CREATE UNIQUE INDEX …`), not by the
//!   originating `ALTER TABLE … ADD CONSTRAINT`. So if the authoritative path
//!   (`pull --dry-run`) ever rendered a recreation it would emit the
//!   `CREATE UNIQUE INDEX` form rather than re-adding the constraint; and a model
//!   that *also* declares `#[unique]` on a column already covered by a brownfield
//!   `UNIQUE` constraint additively creates the model's own
//!   `idx_<table>_<col>_unique` index — a redundant-but-valid second unique
//!   index, never a failure. Full constraint↔index reconciliation is deferred;
//!   the load-bearing property this slice guarantees is only that the generated
//!   migration no longer *fails* with a rejected `DROP INDEX`.
//! - **Cascade-dropped retained indexes on a dropped column**: when a model
//!   removes a column that a retained (`definition`-carrying) expression / partial
//!   / constraint-owned index depends on, Postgres cascade-drops that index with
//!   the `ALTER TABLE … DROP COLUMN` (the up path emits no `DROP INDEX`). The
//!   snapshot projection prunes it to match (so `doctor` / `pull --dry-run` do not
//!   false-drift) and the down migration recreates it from its verbatim
//!   `definition`. Two nuances are deferred: **(i)** a cascade-removed brownfield
//!   `UNIQUE`/`EXCLUDE` *constraint* is restored on rollback as a unique *index*
//!   (via its `pg_get_indexdef` definition), not as the original named
//!   `CONSTRAINT` — functional uniqueness is preserved, exact constraint-object
//!   reconstruction is deferred; **(ii)** whether a column is *depended on* by an
//!   expression index is detected best-effort — `columns` membership plus a
//!   word-bounded token match of the column name in the `definition` text (so
//!   `email` matches `lower(email)` but not `emailer`), not a full parse of the
//!   index expression.
//!
//! Errors are **credential-safe** by construction: no variant ever embeds the
//! resolved database URL (only a parsed host/port on the connection-error path,
//! mirroring [`crate::db_pull`]).

use std::collections::BTreeMap;

use autumn_schema_core::{Backend, Column, ColumnDefault, ColumnType, ForeignKey, Index, Table};
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

    let mut tables = Vec::with_capacity(table_names.len());
    for name in &table_names {
        tables.push(build_table(
            name,
            columns_by_table.get(name).map_or(&[][..], Vec::as_slice),
            pks_by_table.get(name).map_or(&[][..], Vec::as_slice),
            indexes_by_table.get(name).map_or(&[][..], Vec::as_slice),
            fks_by_table.get(name).map_or(&[][..], Vec::as_slice),
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
    /// Whether the index backs a constraint (`pg_constraint.conindid` points at
    /// it) — true for UNIQUE/EXCLUDE (and PK, but PKs are already filtered out).
    /// Such an index cannot be dropped independently of its constraint, and the
    /// model DSL cannot express it, so it is retained verbatim.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    is_constraint: bool,
    /// Comma-joined resolvable ordinary column names, in key order (may be empty).
    #[diesel(sql_type = diesel::sql_types::Text)]
    columns: String,
}

#[derive(QueryableByName)]
struct ForeignKeyRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    column_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    foreign_table: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    foreign_column: String,
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
    // `numeric_precision` / `numeric_scale` are the `information_schema`
    // `cardinal_number` domain, not a plain `int4`. Decoding that domain as diesel
    // `Nullable<Integer>` on the wire is unproven (no sibling query reads them), and
    // if it does not present as `int4` the whole column query would error and every
    // pull would fail. Cast both to a plain `integer` in SQL so the output type is
    // guaranteed `int4` regardless of the domain. The `AS <name>` aliases keep the
    // result column names aligned with the `ColumnRow` field bindings.
    let query = format!(
        "SELECT table_name, column_name, udt_name, is_nullable, column_default, \
         numeric_precision::integer AS numeric_precision, \
         numeric_scale::integer AS numeric_scale FROM information_schema.columns \
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
/// `pg_constraint` (UNIQUE/EXCLUDE), so the builder can classify each index as
/// simple-representable vs. preserved-verbatim.
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
         EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) AS is_constraint, \
         COALESCE(( \
           SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
           FROM unnest(string_to_array(i.indkey::text, ' ')::int[]) \
                WITH ORDINALITY AS k(attnum, ord) \
           JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
           WHERE k.attnum > 0 \
         ), '') AS columns \
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
         ft.relname AS foreign_table, fatt.attname AS foreign_column \
         FROM pg_constraint con \
         JOIN pg_class t ON t.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class ft ON ft.oid = con.confrelid \
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
        let ty = ColumnType::from_pg_introspection(
            &row.udt_name,
            row.numeric_precision,
            row.numeric_scale,
        );
        let is_pk = primary_key.iter().any(|c| c == &row.column_name);
        let mut column = Column::new(row.column_name.clone(), ty.clone());
        column.nullable = row.is_nullable.eq_ignore_ascii_case("YES");
        column.primary_key = is_pk;
        column.unique = unique_columns.contains(row.column_name.as_str());
        column.default = normalize_default(row.column_default.as_deref(), &ty, is_pk, primary_key);
        if let Some(fk) = fk_by_column.get(row.column_name.as_str()) {
            column.references = Some(ForeignKey::new(
                fk.foreign_table.clone(),
                fk.foreign_column.clone(),
            ));
        }
        table.columns.push(column);
    }

    table.indexes = ir_indexes;
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
/// - **expression, partial, or constraint-owned** — preserved verbatim via
///   `definition: Some(pg_get_indexdef(...))`, never dropped. A constraint-owned
///   index (a brownfield column/table-level `UNIQUE`/`EXCLUDE`, which auto-creates
///   a `pg_constraint`-backed index Postgres refuses to drop independently, and
///   which the declarative model DSL cannot express) is retained just like an
///   expression/partial index, so a model-side diff never emits a `DROP INDEX`
///   that Postgres would reject. A single-column constraint-owned unique index
///   still sets the owning column's `unique` flag (accurate, and the flag is not
///   diffed) — that classification is independent of constraint ownership.
fn collapse_indexes(rows: &[IndexRow]) -> (Vec<Index>, std::collections::BTreeSet<String>) {
    let mut indexes = Vec::with_capacity(rows.len());
    let mut unique_columns = std::collections::BTreeSet::new();
    for row in rows {
        let columns: Vec<String> = row
            .columns
            .split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        // "Simple" == a single representable column set (not partial, not an
        // expression). A single-column simple unique index sets the owning
        // column's `unique` flag whether or not it backs a constraint (the flag
        // is accurate either way and is never diffed).
        let simple = !row.is_partial && !row.has_expression;
        if simple && row.is_unique && columns.len() == 1 {
            unique_columns.insert(columns[0].clone());
        }
        // Retain (preserve verbatim, never drop) any index the model DSL cannot
        // express: an expression or partial index, OR a constraint-owned index
        // (UNIQUE/EXCLUDE) whose backing constraint Postgres won't let us drop.
        // A plain, non-constraint index keeps `definition: None` so its JSON is
        // unchanged and the model round-trip stays clean.
        let definition = if simple && !row.is_constraint {
            None
        } else {
            Some(row.definition.clone())
        };
        indexes.push(Index {
            name: row.index_name.clone(),
            columns,
            unique: row.is_unique,
            definition,
        });
    }
    (indexes, unique_columns)
}

/// Normalize a raw Postgres `column_default` string into a [`ColumnDefault`],
/// matching what the model IR records so a round-trip diff is empty.
///
/// - `NULL` (no default) → `None`.
/// - `now()` / `CURRENT_TIMESTAMP` → [`ColumnDefault::Now`].
/// - a `nextval(...)` serial default on a single-column integer primary key →
///   `None` (a `BIGSERIAL`-shaped id has no explicit default in the model IR;
///   see [`normalize_serial_pk_default`]).
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
/// a single-column integer primary key — i.e. the shape the model IR represents
/// as an [`Int64`](ColumnType::Int64) PK with **no** default. Such a default is
/// stripped to `None` so an introspected `BIGSERIAL` id round-trips against the
/// model parser (whose `pk_kind_for` requires `default.is_none()` for a
/// `BigSerial` PK).
fn normalize_serial_pk_default(
    lowered_default: &str,
    ty: &ColumnType,
    is_primary_key: bool,
    primary_key: &[String],
) -> bool {
    is_primary_key
        && primary_key.len() == 1
        && matches!(ty, ColumnType::Int32 | ColumnType::Int64)
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
        // A `nextval(...)` default on a single-column int PK → None (BIGSERIAL id).
        let pk = vec!["id".to_owned()];
        let d = normalize_default(
            Some("nextval('posts_id_seq'::regclass)"),
            &ColumnType::Int64,
            true,
            &pk,
        );
        assert_eq!(d, None, "serial PK default must be stripped to None");
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
    /// resolvable-column string the SQL aggregation produces. `is_constraint` is
    /// left `false` (an ordinary/model-style or expression/partial index); use
    /// [`constraint_index_row`] for a constraint-owned index.
    fn index_row(
        index_name: &str,
        is_unique: bool,
        columns: &str,
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
            is_constraint: false,
            columns: columns.to_owned(),
        }
    }

    /// Build a constraint-owned (non-primary, non-partial, non-expression)
    /// [`IndexRow`] — the auto-created index behind a brownfield column/table-level
    /// `UNIQUE`/`EXCLUDE` constraint (`is_constraint == true`).
    fn constraint_index_row(
        index_name: &str,
        is_unique: bool,
        columns: &str,
        definition: &str,
    ) -> IndexRow {
        IndexRow {
            is_constraint: true,
            ..index_row(index_name, is_unique, columns, false, false, definition)
        }
    }

    #[test]
    fn collapse_indexes_single_column_unique_sets_flag_and_index() {
        let rows = vec![index_row(
            "idx_accounts_email_unique",
            true,
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
            "", // no resolvable ordinary columns
            false,
            true,
            def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1, "expression index preserved, not dropped");
        assert_eq!(indexes[0].name, "users_lower_email_idx");
        assert!(indexes[0].columns.is_empty());
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
            "email",
            true,
            false,
            def,
        )];
        let (indexes, unique_cols) = collapse_indexes(&rows);
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].columns, vec!["email".to_owned()]);
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
            },
            ColumnRow {
                table_name: "comments".to_owned(),
                column_name: "post_id".to_owned(),
                udt_name: "int8".to_owned(),
                is_nullable: "NO".to_owned(),
                column_default: None,
                numeric_precision: None,
                numeric_scale: None,
            },
        ];
        let pk = vec!["id".to_owned()];
        let indexes = vec![index_row(
            "idx_comments_post_id",
            false,
            "post_id",
            false,
            false,
            "CREATE INDEX idx_comments_post_id ON public.comments USING btree (post_id)",
        )];
        let fks = vec![ForeignKeyRow {
            table_name: "comments".to_owned(),
            column_name: "post_id".to_owned(),
            foreign_table: "posts".to_owned(),
            foreign_column: "id".to_owned(),
        }];
        let table = build_table("comments", &columns, &pk, &indexes, &fks);
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
        }];
        let table = build_table("widgets", &columns, &[], &[], &[]);
        let addr = table.columns.iter().find(|c| c.name == "addr").unwrap();
        assert_eq!(
            addr.ty,
            ColumnType::Opaque {
                pg_type: "inet".to_owned()
            }
        );
        assert!(addr.nullable);
    }
}
