//! `autumn db pull` — scaffold Autumn models from an existing Postgres database.
//!
//! Read-only introspection: connect to the resolved primary/write database the
//! same way `autumn migrate` / `autumn db` do, read the `public` schema's tables
//! and columns from the catalog, map each column back to the generator's
//! [`FieldKind`](crate::generate::dsl::FieldKind) (the inverse of the documented
//! field-type DSL), and emit `#[model]` structs + `schema.rs` blocks (and
//! optionally `#[repository]` traits) through the shared generator plan engine.
//!
//! No migration is written and no data is touched — the tables already exist.
//! Errors are credential-safe: no variant ever embeds the resolved URL.

use std::collections::BTreeMap;

use diesel::{Connection as _, PgConnection, QueryableByName, RunQueryDsl as _, sql_query};

use crate::generate::Flags;
use crate::generate::dsl::{SQL_SUPPORTED_TYPES, sql_type_to_field_kind};
use crate::generate::introspect::{Column, PullOptions, TableSchema, plan_pull};
use crate::migrate;

/// Parsed `autumn db pull` arguments.
#[derive(Debug, Clone)]
pub struct PullArgs {
    /// Resolve the connection through a profile overlay (see `db create`).
    pub profile: Option<String>,
    /// Specific tables to pull. Empty means every non-system table.
    pub tables: Vec<String>,
    /// Also emit a `#[repository(Model)]` trait per table.
    pub with_repository: bool,
    /// Print the planned actions without writing.
    pub dry_run: bool,
    /// Overwrite existing files instead of erroring on collision.
    pub force: bool,
}

/// Failure modes for `db pull`. `Display` is credential-safe by construction.
#[derive(Debug)]
enum PullError {
    /// No database URL could be resolved from config or environment.
    NoUrl,
    /// The resolved URL could not be parsed.
    UnparsableUrl,
    /// Could not connect. Carries only parsed host/port — never credentials.
    Connection { host: String, port: u16 },
    /// A catalog query failed (message comes from the server, not the URL).
    Sql(String),
    /// A requested table does not exist in the `public` schema.
    UnknownTable { table: String },
    /// No tables were found to pull.
    NoTables,
    /// A column has a SQL type outside the documented surface.
    UnsupportedType {
        table: String,
        column: String,
        udt: String,
    },
    /// Generator/file-emission error (collisions, not-in-project, I/O).
    Generate(crate::generate::GenerateError),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoUrl => write!(
                f,
                "No database URL found.\n  Set database.primary_url (or database.url) in autumn.toml, \
                 or set AUTUMN_DATABASE__PRIMARY_URL / AUTUMN_DATABASE__URL / DATABASE_URL."
            ),
            Self::UnparsableUrl => {
                write!(f, "The resolved database URL could not be parsed.")
            }
            Self::Connection { host, port } => write!(
                f,
                "Could not connect to Postgres at {host}:{port}.\n  Is the server running and reachable?"
            ),
            Self::Sql(message) => write!(f, "{message}"),
            Self::UnknownTable { table } => {
                write!(f, "Table {table:?} was not found in the public schema.")
            }
            Self::NoTables => write!(f, "No tables found to pull in the public schema."),
            Self::UnsupportedType { table, column, udt } => write!(
                f,
                "Column {table}.{column} has unsupported SQL type {udt:?}.\n  Supported: {SQL_SUPPORTED_TYPES}"
            ),
            Self::Generate(e) => write!(f, "{e}"),
        }
    }
}

/// Entry point dispatched from `main`. Prints a credential-safe message and
/// exits non-zero on failure.
pub fn run(args: &PullArgs) {
    eprintln!("\u{1F342} autumn db pull\n");
    match pull(args) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("\u{2717} {e}");
            std::process::exit(1);
        }
    }
}

fn pull(args: &PullArgs) -> Result<(), PullError> {
    let url = migrate::resolve_primary_url(args.profile.as_deref()).ok_or(PullError::NoUrl)?;
    // `PgConnection::establish` accepts both URL and libpq key-value DSNs
    // (`host=... dbname=...`), but `url::Url::parse` only understands the former.
    // Defer parsing to the error path so a valid key-value connection string
    // still connects; host/port are only needed for a credential-safe message.
    let mut conn = PgConnection::establish(&url).map_err(|_| {
        let (host, port) = parse_host_port(&url).unwrap_or_else(|_| ("localhost".to_owned(), 5432));
        PullError::Connection { host, port }
    })?;

    let table_names = list_tables(&mut conn, &args.tables)?;
    if table_names.is_empty() {
        return Err(PullError::NoTables);
    }

    // Fetch every target table's columns and primary keys in two batched
    // queries (not two per table), then assemble each `TableSchema` with no
    // further I/O.
    let columns_by_table = fetch_columns(&mut conn, &table_names)?;
    let pks_by_table = fetch_primary_keys(&mut conn, &table_names)?;

    let explicit = !args.tables.is_empty();
    let mut tables = Vec::with_capacity(table_names.len());
    for table in &table_names {
        match build_table_schema(table, &columns_by_table, &pks_by_table) {
            Ok(schema) => tables.push(schema),
            // An unscoped pull skips a table it can't map (e.g. a `jsonb` column)
            // with a notice so the supported tables still come through; an
            // explicitly-requested table is still a hard error. Connection/SQL
            // failures always propagate.
            Err(e) if !explicit && matches!(e, PullError::UnsupportedType { .. }) => {
                eprintln!("  \u{2139} Skipping table '{table}': {e}");
            }
            Err(e) => return Err(e),
        }
    }

    let plan = plan_pull(
        &std::env::current_dir().unwrap_or_default(),
        &tables,
        PullOptions {
            with_repository: args.with_repository,
            force: args.force,
            explicit,
        },
    )
    .map_err(PullError::Generate)?;

    plan.execute(Flags {
        dry_run: args.dry_run,
        force: args.force,
    })
    .map_err(PullError::Generate)
}

/// Parse host/port from a connection URL for credential-safe error messages.
fn parse_host_port(url: &str) -> Result<(String, u16), PullError> {
    let parsed = url::Url::parse(url).map_err(|_| PullError::UnparsableUrl)?;
    let host = parsed.host_str().unwrap_or("localhost").to_owned();
    let port = parsed.port().unwrap_or(5432);
    Ok((host, port))
}

/// A single text column produced by the table/column/pk probes.
#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// One column's catalog metadata (batched across tables, so it carries its
/// owning `table_name`).
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
    /// `'ALWAYS'` for stored generated columns, `'NEVER'` otherwise.
    #[diesel(sql_type = diesel::sql_types::Text)]
    is_generated: String,
    /// `'YES'` for identity columns (`GENERATED ... AS IDENTITY`), `'NO'` otherwise.
    #[diesel(sql_type = diesel::sql_types::Text)]
    is_identity: String,
}

/// A primary-key column paired with its owning table (batched PK probe).
#[derive(QueryableByName)]
struct TablePkRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    table_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// List the base tables in `public`, excluding Diesel's bookkeeping table.
/// When `requested` is non-empty, restrict to those (erroring on any absent).
fn list_tables(conn: &mut PgConnection, requested: &[String]) -> Result<Vec<String>, PullError> {
    let query = "SELECT table_name AS name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         AND table_name <> '__diesel_schema_migrations' \
         ORDER BY table_name";
    let rows: Vec<NameRow> = sql_query(query)
        .load(conn)
        .map_err(|e| PullError::Sql(e.to_string()))?;
    let all: Vec<String> = rows.into_iter().map(|r| r.name).collect();

    if requested.is_empty() {
        // An unscoped pull skips Autumn's own framework-owned tables (created by
        // `autumn migrate`), which carry JSONB/enum columns outside the supported
        // mapping. They can still be pulled by naming them explicitly.
        return Ok(all.into_iter().filter(|t| !is_framework_table(t)).collect());
    }
    for want in requested {
        if !all.iter().any(|t| t == want) {
            return Err(PullError::UnknownTable {
                table: want.clone(),
            });
        }
    }
    Ok(requested.to_vec())
}

/// Whether `table` is an Autumn/Diesel framework-owned table that an unscoped
/// pull should skip by default. Framework tables use the `autumn_` / `_autumn`
/// prefixes, plus a few historically unprefixed names.
fn is_framework_table(table: &str) -> bool {
    table.starts_with("autumn_")
        || table.starts_with("_autumn")
        || matches!(
            table,
            "api_tokens" | "feature_flag_changes" | "__diesel_schema_migrations"
        )
}

/// Build a comma-separated SQL string-literal list (`'a', 'b'`) for an `IN (..)`
/// clause from catalog-sourced names. `tables` is always non-empty here (the
/// caller guards `table_names.is_empty()`), so the list is never `IN ()`.
fn quoted_in_list(tables: &[String]) -> String {
    tables
        .iter()
        .map(|t| crate::db::quote_literal(t))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fetch every column (in ordinal order) for all `tables` in one query, grouped
/// by table name.
fn fetch_columns(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<ColumnRow>>, PullError> {
    let query = format!(
        "SELECT table_name, column_name, udt_name, is_nullable, column_default, is_generated, \
         is_identity FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name IN ({}) \
         ORDER BY table_name, ordinal_position",
        quoted_in_list(tables)
    );
    let rows: Vec<ColumnRow> = sql_query(query)
        .load(conn)
        .map_err(|e| PullError::Sql(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<ColumnRow>> = BTreeMap::new();
    for row in rows {
        by_table
            .entry(row.table_name.clone())
            .or_default()
            .push(row);
    }
    Ok(by_table)
}

/// Fetch the primary-key column set for all `tables` in one query, grouped by
/// table name. Joins through `pg_class`/`pg_namespace` so the index is matched
/// in the `public` schema and tagged with its table.
fn fetch_primary_keys(
    conn: &mut PgConnection,
    tables: &[String],
) -> Result<BTreeMap<String, Vec<String>>, PullError> {
    let query = format!(
        "SELECT c.relname AS table_name, a.attname AS name \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
         WHERE n.nspname = 'public' AND i.indisprimary AND c.relname IN ({})",
        quoted_in_list(tables)
    );
    let rows: Vec<TablePkRow> = sql_query(query)
        .load(conn)
        .map_err(|e| PullError::Sql(e.to_string()))?;
    let mut by_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        by_table.entry(row.table_name).or_default().push(row.name);
    }
    Ok(by_table)
}

/// Assemble a `TableSchema` (columns in ordinal order, PK columns flagged) from
/// the pre-fetched catalog maps. Pure — no database access.
fn build_table_schema(
    table: &str,
    columns_by_table: &BTreeMap<String, Vec<ColumnRow>>,
    pks_by_table: &BTreeMap<String, Vec<String>>,
) -> Result<TableSchema, PullError> {
    let rows = columns_by_table.get(table).map_or(&[][..], Vec::as_slice);
    let pk = pks_by_table.get(table).map_or(&[][..], Vec::as_slice);
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let kind =
            sql_type_to_field_kind(&row.udt_name).ok_or_else(|| PullError::UnsupportedType {
                table: table.to_owned(),
                column: row.column_name.clone(),
                udt: row.udt_name.clone(),
            })?;
        let has_sequence_default = row
            .column_default
            .as_deref()
            .is_some_and(|d| d.trim_start().to_ascii_lowercase().starts_with("nextval("));
        columns.push(Column {
            nullable: row.is_nullable.eq_ignore_ascii_case("YES"),
            is_pk: pk.iter().any(|c| c == &row.column_name),
            has_default: row.column_default.is_some(),
            has_sequence_default,
            is_generated: row.is_generated.eq_ignore_ascii_case("ALWAYS"),
            is_identity: row.is_identity.eq_ignore_ascii_case("YES"),
            name: row.column_name.clone(),
            kind,
        });
    }
    Ok(TableSchema {
        table: table.to_owned(),
        columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_extracts_pieces() {
        let (host, port) = parse_host_port("postgres://user:pw@db.example.com:6543/app").unwrap();
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 6543);
    }

    #[test]
    fn parse_host_port_defaults_port() {
        let (host, port) = parse_host_port("postgres://localhost/app").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn errors_never_leak_credentials() {
        let err = PullError::Connection {
            host: "db.example.com".to_owned(),
            port: 5432,
        };
        let rendered = err.to_string();
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("db.example.com"));

        let unsupported = PullError::UnsupportedType {
            table: "ledger".to_owned(),
            column: "amount".to_owned(),
            udt: "numeric".to_owned(),
        };
        let msg = unsupported.to_string();
        assert!(msg.contains("ledger.amount"));
        assert!(msg.contains("numeric"));
        assert!(msg.contains("Supported:"));
        assert!(!msg.contains("postgres://"));
    }

    #[test]
    fn framework_tables_are_excluded_user_tables_kept() {
        for fw in [
            "autumn_jobs",
            "autumn_feature_flags",
            "_autumn_version_history",
            "_autumn_shard_directory",
            "api_tokens",
            "feature_flag_changes",
            "__diesel_schema_migrations",
        ] {
            assert!(is_framework_table(fw), "{fw} should be a framework table");
        }
        for user in ["posts", "comments", "users", "autumnal_themes"] {
            assert!(
                !is_framework_table(user),
                "{user} is a user table and must be kept"
            );
        }
    }

    #[test]
    fn no_url_message_lists_resolution_sources() {
        let msg = PullError::NoUrl.to_string();
        assert!(msg.contains("AUTUMN_DATABASE__PRIMARY_URL"));
        assert!(msg.contains("autumn.toml"));
    }
}
