//! The `SQLite` lake: the first [`LakeSink`] (ADR 0040 §6).
//!
//! ```text
//! add   ─► ATTACH the restored file. In one transaction: copy each table
//!          with an _autumn_tenant column, add the manifest row. DETACH.
//! query ─► a read-only connection runs one SELECT over the lake
//! ```
//!
//! Each load makes a new lake file. The sink does not use `has`, and it
//! refuses slot fleets.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate".
// To allow one call, put
// #[allow(clippy::disallowed_methods, reason = "…")] on the smallest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diesel::connection::SimpleConnection as _;
use diesel::{OptionalExtension as _, RunQueryDsl as _, SqliteConnection};

use super::{
    LakeError, LakeGeneration, LakeMember, LakeSink, LakeSkip, LakeTenant, MANIFEST_TABLE,
    TENANT_COLUMN, refuse_under,
};
use crate::fleet_layout::{FleetDbKey, TenantDbId};

/// The temporary view a query runs through.
const RESULT_VIEW: &str = "_autumn_lake_result";

/// The identity table of a fleet database.
const IDENTITY_TABLE: &str = "_autumn_fleet_identity";

/// Columns per `json_array` call. `SQLite` caps function arguments.
const COLUMNS_PER_ARRAY: usize = 100;

/// A value in a lake result. It keeps the `SQLite` storage class.
#[derive(Debug, Clone, PartialEq)]
pub enum LakeValue {
    /// `NULL`.
    Null,
    /// An `INTEGER`.
    Integer(i64),
    /// A `REAL`.
    Real(f64),
    /// A `TEXT`.
    Text(String),
    /// A `BLOB`.
    Blob(Vec<u8>),
}

impl LakeValue {
    /// The value as an `i64`, when it is an integer.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// The value as an `f64`, when it is a number.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "an integer read as a real")]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Integer(value) => Some(*value as f64),
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    /// The value as text, when it is text.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

/// The rows of one lake query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LakeRows {
    /// Column names, in result order.
    pub columns: Vec<String>,
    /// Rows, in result order.
    pub rows: Vec<Vec<LakeValue>>,
}

// ── Sink ────────────────────────────────────────────────────────────────────

/// Loads each member into one new `SQLite` lake file.
pub(super) struct SqliteSink {
    conn: SqliteConnection,
    db: PathBuf,
    tables: Option<Vec<String>>,
}

impl SqliteSink {
    /// Make the lake file `db` and its manifest table. `tables`: load only
    /// these tables.
    pub(super) fn create(db: PathBuf, tables: Option<Vec<String>>) -> Result<Self, LakeError> {
        Ok(Self {
            conn: open_lake(&db)?,
            db,
            tables,
        })
    }
}

impl LakeSink for SqliteSink {
    /// The lake file.
    type Lake = PathBuf;

    fn add(&mut self, member: LakeMember<'_>) -> Result<(), LakeError> {
        // Core gives no slot member: `TAKES_SLOTS` is `false`.
        let tenant = member.key.tenant().ok_or(LakeError::SlotFleet)?;
        ingest(
            &mut self.conn,
            tenant,
            member.generation,
            member.file,
            self.tables.as_deref(),
        )
    }

    fn finish(self) -> Result<PathBuf, LakeError> {
        drop(self.conn);
        Ok(self.db)
    }
}

// ── Ingest ──────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct Name {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(diesel::QueryableByName)]
struct ListedTable {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
}

#[derive(diesel::QueryableByName)]
struct Column {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    hidden: i32,
}

/// A `file:` URI for `path` with `query`.
fn uri(path: &Path, query: &str) -> Result<String, String> {
    crate::replication::file_uri(path)
        .map(|uri| format!("{uri}?{query}"))
        .ok_or_else(|| format!("{} is not valid Unicode", path.display()))
}

/// Make the lake file and its manifest table.
fn open_lake(db: &Path) -> Result<SqliteConnection, LakeError> {
    let scratch = |detail: String| LakeError::Scratch(format!("{}: {detail}", db.display()));
    let mut conn =
        crate::db::establish_sqlite_migration_connection(&uri(db, "mode=rwc").map_err(scratch)?)
            .map_err(|error| scratch(error.to_string()))?;
    // A scratch file: a crash loses nothing the next load does not make.
    // The journal stays (in memory): a skipped tenant rolls back.
    conn.batch_execute(&format!(
        "PRAGMA journal_mode = MEMORY; PRAGMA synchronous = OFF; \
         CREATE TABLE {MANIFEST_TABLE} (tenant TEXT PRIMARY KEY, generation TEXT NOT NULL, \
         effective TEXT NOT NULL, effective_ms INTEGER NOT NULL);"
    ))
    .map_err(|error| scratch(error.to_string()))?;
    Ok(conn)
}

/// Copy one restored tenant into the lake, in one transaction.
fn ingest(
    conn: &mut SqliteConnection,
    tenant: &TenantDbId,
    generation: &LakeGeneration,
    path: &Path,
    tables: Option<&[String]>,
) -> Result<(), LakeError> {
    let failed = |detail: String| LakeError::Ingest {
        tenant: tenant.to_string(),
        detail,
    };
    // A private file `restore::apply` checkpointed: no locks, no sidecars.
    diesel::sql_query("ATTACH DATABASE ? AS src")
        .bind::<diesel::sql_types::Text, _>(uri(path, "mode=ro&immutable=1").map_err(failed)?)
        .execute(conn)
        .map_err(|error| failed(error.to_string()))?;
    let result = conn
        .batch_execute("BEGIN")
        .map_err(|error| failed(error.to_string()))
        .and_then(|()| copy_tenant(conn, tenant, generation, tables));
    let ended = match &result {
        Ok(()) => conn.batch_execute("COMMIT"),
        Err(_) => conn.batch_execute("ROLLBACK"),
    };
    let detached = conn.batch_execute("DETACH DATABASE src");
    result?;
    ended.map_err(|error| failed(error.to_string()))?;
    detached.map_err(|error| failed(error.to_string()))
}

/// The copy inside the transaction of [`ingest`].
fn copy_tenant(
    conn: &mut SqliteConnection,
    tenant: &TenantDbId,
    generation: &LakeGeneration,
    tables: Option<&[String]>,
) -> Result<(), LakeError> {
    let failed = |error: diesel::result::Error| LakeError::Ingest {
        tenant: tenant.to_string(),
        detail: error.to_string(),
    };
    let mut names: Vec<String> =
        diesel::sql_query("SELECT name, type AS kind FROM pragma_table_list WHERE schema = 'src'")
            .load::<ListedTable>(conn)
            .map_err(failed)?
            .into_iter()
            .filter(|table| table.kind == "table" && is_app_table(&table.name))
            .map(|table| table.name)
            .filter(|name| {
                tables.is_none_or(|only| only.iter().any(|t| t.eq_ignore_ascii_case(name)))
            })
            .collect();
    names.sort();
    for table in &names {
        let columns: Vec<String> =
            diesel::sql_query("SELECT name, hidden FROM pragma_table_xinfo(?, 'src')")
                .bind::<diesel::sql_types::Text, _>(table)
                .load::<Column>(conn)
                .map_err(failed)?
                .into_iter()
                // 1: a hidden column of a virtual table. 2, 3: generated.
                .filter(|column| column.hidden != 1)
                .map(|column| column.name)
                .collect();
        if columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(TENANT_COLUMN))
        {
            return Err(LakeError::ReservedColumn {
                tenant: tenant.to_string(),
                table: table.clone(),
            });
        }
        add_lake_table(conn, table, &columns).map_err(failed)?;
        let list = std::iter::once(TENANT_COLUMN.to_owned())
            .chain(columns.iter().cloned())
            .map(|column| quote_ident(&column))
            .collect::<Vec<_>>()
            .join(", ");
        let select = columns
            .iter()
            .map(|column| quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        let table = quote_ident(table);
        diesel::sql_query(format!(
            "INSERT INTO main.{table} ({list}) SELECT ?, {select} FROM src.{table}"
        ))
        .bind::<diesel::sql_types::Text, _>(tenant.as_str())
        .execute(conn)
        .map_err(failed)?;
    }
    diesel::sql_query(format!("INSERT INTO {MANIFEST_TABLE} VALUES (?, ?, ?, ?)"))
        .bind::<diesel::sql_types::Text, _>(tenant.as_str())
        .bind::<diesel::sql_types::Text, _>(&generation.id)
        .bind::<diesel::sql_types::Text, _>(generation.effective.to_rfc3339())
        .bind::<diesel::sql_types::BigInt, _>(generation.effective.timestamp_millis())
        .execute(conn)
        .map_err(failed)?;
    Ok(())
}

/// A table the app owns: not `SQLite`'s, the framework's or diesel's.
fn is_app_table(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !(lower.starts_with("sqlite_")
        || lower.starts_with("_autumn_")
        || lower.starts_with("__diesel"))
}

/// Check that the identity row of the restored file names `key`. Core
/// runs this before `add`, for each sink. `name`: the member name for the
/// error.
pub(super) fn check_identity(key: &FleetDbKey, name: &str, file: &Path) -> Result<(), LakeError> {
    let misplaced = |detail: String| LakeError::Misplaced {
        tenant: name.to_owned(),
        detail,
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(
        &uri(file, "mode=ro&immutable=1").map_err(misplaced)?,
    )
    .map_err(|error| misplaced(error.to_string()))?;
    let expected = key.name();
    let exists =
        diesel::sql_query("SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?")
            .bind::<diesel::sql_types::Text, _>(IDENTITY_TABLE)
            .get_result::<Name>(&mut conn)
            .optional()
            .map_err(|error| misplaced(error.to_string()))?
            .is_some();
    if !exists {
        return Err(misplaced("the identity row is missing".to_owned()));
    }
    let found = diesel::sql_query(format!("SELECT name FROM {IDENTITY_TABLE} WHERE id = 1"))
        .get_result::<Name>(&mut conn)
        .optional()
        .map_err(|error| misplaced(error.to_string()))?;
    match found {
        Some(found) if found.name == expected => Ok(()),
        Some(found) => Err(misplaced(format!(
            "the identity row names {}, not {expected}",
            found.name
        ))),
        None => Err(misplaced("the identity row is missing".to_owned())),
    }
}

/// Make the lake table, or add the columns it does not have yet. The
/// columns have no type, so each value keeps its storage class.
fn add_lake_table(
    conn: &mut SqliteConnection,
    table: &str,
    columns: &[String],
) -> Result<(), diesel::result::Error> {
    let existing: Vec<String> = diesel::sql_query("SELECT name FROM pragma_table_info(?, 'main')")
        .bind::<diesel::sql_types::Text, _>(table)
        .load::<Name>(conn)?
        .into_iter()
        .map(|column| column.name.to_ascii_lowercase())
        .collect();
    let quoted = quote_ident(table);
    if existing.is_empty() {
        let list = std::iter::once(TENANT_COLUMN)
            .chain(columns.iter().map(String::as_str))
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(", ");
        diesel::sql_query(format!("CREATE TABLE main.{quoted} ({list})")).execute(conn)?;
        return Ok(());
    }
    for column in columns {
        if !existing.contains(&column.to_ascii_lowercase()) {
            diesel::sql_query(format!(
                "ALTER TABLE main.{quoted} ADD COLUMN {}",
                quote_ident(column)
            ))
            .execute(conn)?;
        }
    }
    Ok(())
}

/// Quote an identifier for `SQLite`.
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ── Snapshot and query ──────────────────────────────────────────────────────

/// The files of a lake. The last owner removes them, so a query still
/// running keeps them.
#[derive(Debug)]
pub(super) struct LakeFiles {
    pub(super) dir: PathBuf,
    pub(super) db: PathBuf,
}

impl Drop for LakeFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A loaded lake. Dropping it removes its scratch directory, after the
/// last running query.
#[derive(Debug)]
pub struct LakeSnapshot {
    pub(super) files: Arc<LakeFiles>,
    pub(super) tenants: Vec<LakeTenant>,
    pub(super) skipped: Vec<LakeSkip>,
    /// The canonical fleet root, when known.
    pub(super) root: Option<PathBuf>,
}

impl LakeSnapshot {
    /// The tenants in the lake, sorted by id.
    #[must_use]
    pub fn tenants(&self) -> &[LakeTenant] {
        &self.tenants
    }

    /// The databases the lake did not load, sorted by name.
    #[must_use]
    pub fn skipped(&self) -> &[LakeSkip] {
        &self.skipped
    }

    /// The lake database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.files.db
    }

    /// Run one `SELECT`, `WITH` or `VALUES` statement.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub async fn query(&self, sql: &str) -> Result<LakeRows, LakeError> {
        let (files, sql) = (Arc::clone(&self.files), sql.to_owned());
        crate::time::spawn_blocking(move || collect_rows(&files.db, &sql))
            .await
            .map_err(|error| LakeError::Task(error.to_string()))?
    }

    /// The column names of `sql`, with no rows read. Blocking.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub fn columns(&self, sql: &str) -> Result<Vec<String>, LakeError> {
        columns_of(&mut read_only(&self.files.db)?, sql)
    }

    /// Run one statement and give each row to `each`. Blocking. Returns
    /// the number of rows given.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub fn for_each_row(
        &self,
        sql: &str,
        each: impl FnMut(&[String], Vec<LakeValue>) -> ControlFlow<()>,
    ) -> Result<usize, LakeError> {
        for_each_row(&self.files.db, sql, each)
    }

    /// Write a standalone copy of the lake to `path` (owner-only on unix).
    /// `path` must not exist. The copy holds the rows of every tenant.
    /// Blocking.
    ///
    /// # Errors
    ///
    /// [`LakeError::Persist`] when `path` exists or cannot be written.
    /// [`LakeError::UnderRoot`] when `path` is under the fleet root.
    pub fn persist(&self, path: &Path) -> Result<(), LakeError> {
        let failed = |detail: String| LakeError::Persist(format!("{}: {detail}", path.display()));
        let name = path
            .file_name()
            .ok_or_else(|| failed("no file name".to_owned()))?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent = parent
            .canonicalize()
            .map_err(|error| failed(error.to_string()))?;
        refuse_under(&parent, self.root.as_deref())?;
        let target = parent.join(name);
        // A URI, so SQLite writes this file: a name like `file:x` is a name.
        let target_uri = crate::replication::file_uri(&target)
            .ok_or_else(|| failed("the path is not valid Unicode".to_owned()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        drop(
            options
                .open(&target)
                .map_err(|error| failed(error.to_string()))?,
        );
        let written = read_only(&self.files.db).and_then(|mut conn| {
            diesel::sql_query("VACUUM INTO ?")
                .bind::<diesel::sql_types::Text, _>(target_uri)
                .execute(&mut conn)
                .map(|_| ())
                .map_err(|error| LakeError::Query(error.to_string()))
        });
        if written.is_err() {
            let _ = std::fs::remove_file(&target);
        }
        written.map_err(|error| failed(error.to_string()))
    }
}

/// A read-only connection to the lake.
fn read_only(db: &Path) -> Result<SqliteConnection, LakeError> {
    crate::db::establish_sqlite_migration_connection(&uri(db, "mode=ro").map_err(LakeError::Query)?)
        .map_err(|error| LakeError::Query(error.to_string()))
}

pub(super) fn collect_rows(db: &Path, sql: &str) -> Result<LakeRows, LakeError> {
    let mut out = LakeRows::default();
    for_each_row(db, sql, |columns, row| {
        if out.columns.is_empty() {
            out.columns = columns.to_vec();
        }
        out.rows.push(row);
        ControlFlow::Continue(())
    })?;
    if out.columns.is_empty() {
        out.columns = columns_of(&mut read_only(db)?, sql)?;
    }
    Ok(out)
}

/// Make the result view for `sql` and return its columns. The view holds
/// one statement: a trailing statement does not run, and it is refused.
fn columns_of(conn: &mut SqliteConnection, sql: &str) -> Result<Vec<String>, LakeError> {
    let query = |error: diesel::result::Error| LakeError::Query(error.to_string());
    let body = format!("SELECT * FROM (\n{}\n)", statement(sql));
    diesel::sql_query(format!("CREATE TEMP VIEW {RESULT_VIEW} AS {body}"))
        .execute(conn)
        .map_err(query)?;
    let stored: Name =
        diesel::sql_query("SELECT sql AS name FROM temp.sqlite_schema WHERE name = ?")
            .bind::<diesel::sql_types::Text, _>(RESULT_VIEW)
            .get_result(conn)
            .map_err(query)?;
    if !stored.name.ends_with(&body) {
        return Err(LakeError::Query(
            "the lake runs one SELECT, WITH or VALUES statement".to_owned(),
        ));
    }
    Ok(diesel::sql_query(format!(
        "SELECT name FROM pragma_table_info('{RESULT_VIEW}', 'temp')"
    ))
    .load::<Name>(conn)
    .map_err(query)?
    .into_iter()
    .map(|column| column.name)
    .collect())
}

/// `sql` with no trailing `;`, white space or comments. It reads quotes
/// and comments, so a `;` or a `--` in a literal stays.
pub(super) fn statement(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    // The end of the last byte that is code, not space, `;` or a comment.
    let mut end = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`' | b'[') => {
                let close = if quote == b'[' { b']' } else { quote };
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == close {
                        // A doubled quote is one quote in the literal.
                        if close != b']' && bytes.get(at + 1) == Some(&close) {
                            at += 2;
                            continue;
                        }
                        break;
                    }
                    at += 1;
                }
                at = (at + 1).min(bytes.len());
                end = at;
            }
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            byte if byte == b';' || byte.is_ascii_whitespace() => at += 1,
            _ => {
                at += 1;
                end = at;
            }
        }
    }
    // `end` follows an ASCII byte or the end of a literal, so it is a char
    // boundary.
    sql.get(..end).unwrap_or(sql)
}

#[derive(diesel::QueryableByName)]
struct Encoded {
    #[diesel(sql_type = diesel::sql_types::Text)]
    row: String,
}

fn for_each_row(
    db: &Path,
    sql: &str,
    mut each: impl FnMut(&[String], Vec<LakeValue>) -> ControlFlow<()>,
) -> Result<usize, LakeError> {
    let query = |detail: String| LakeError::Query(detail);
    let mut conn = read_only(db)?;
    let columns = columns_of(&mut conn, sql)?;
    conn.batch_execute("PRAGMA query_only = ON")
        .map_err(|error| query(error.to_string()))?;
    // `quote()` keeps the storage class and round-trips a real. Text goes
    // as `T` and hex: `quote()` stops at a NUL. A tab joins the arrays:
    // JSON escapes a tab inside a string.
    let arrays = columns
        .chunks(COLUMNS_PER_ARRAY)
        .map(|chunk| {
            let quoted = chunk
                .iter()
                .map(|column| {
                    let column = quote_ident(column);
                    format!(
                        "CASE typeof({column}) WHEN 'text' THEN 'T' || hex({column}) \
                         ELSE quote({column}) END"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("json_array({quoted})")
        })
        .collect::<Vec<_>>()
        .join(" || char(9) || ");
    // An OFFSET stops SQLite from flattening the subquery into this one. It
    // runs as a co-routine: each value is computed once per row (its type
    // and its bytes agree, even for `random()`), and rows still stream.
    let statement =
        format!("SELECT {arrays} AS row FROM (SELECT * FROM temp.{RESULT_VIEW} LIMIT -1 OFFSET 0)");
    let rows = diesel::sql_query(statement)
        .load_iter::<Encoded, diesel::connection::DefaultLoadingMode>(&mut conn)
        .map_err(|error| query(error.to_string()))?;
    let mut given = 0;
    for row in rows {
        let row = row.map_err(|error| query(error.to_string()))?;
        let mut values = Vec::with_capacity(columns.len());
        for part in row.row.split('\t') {
            let literals: Vec<String> =
                serde_json::from_str(part).map_err(|error| query(error.to_string()))?;
            for literal in literals {
                values.push(parse_literal(&literal).map_err(query)?);
            }
        }
        given += 1;
        if each(&columns, values).is_break() {
            break;
        }
    }
    Ok(given)
}

/// Parse one `quote()` literal, or `T` and the hex of a text, into a value.
pub(super) fn parse_literal(literal: &str) -> Result<LakeValue, String> {
    let bad = || format!("unexpected value {literal:?}");
    if literal == "NULL" {
        return Ok(LakeValue::Null);
    }
    if let Some(hex) = literal.strip_prefix('T') {
        let bytes = decode_hex(hex).ok_or_else(bad)?;
        return Ok(LakeValue::Text(String::from_utf8(bytes).unwrap_or_else(
            |error| String::from_utf8_lossy(error.as_bytes()).into_owned(),
        )));
    }
    if let Some(text) = literal.strip_prefix('\'') {
        let text = text.strip_suffix('\'').ok_or_else(bad)?;
        return Ok(LakeValue::Text(text.replace("''", "'")));
    }
    if let Some(hex) = literal.strip_prefix("X'") {
        let hex = hex.strip_suffix('\'').ok_or_else(bad)?;
        return decode_hex(hex).map(LakeValue::Blob).ok_or_else(bad);
    }
    if literal.contains(['.', 'e', 'E']) {
        return literal
            .parse::<f64>()
            .map(LakeValue::Real)
            .map_err(|_| bad());
    }
    literal
        .parse::<i64>()
        .map(LakeValue::Integer)
        .map_err(|_| bad())
}

/// Decode upper- or lowercase hex. `None` for an odd length or a bad digit.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.is_ascii() {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}
