//! `SQLite` targets for `autumn db scrub` (issue #2553).
//!
//! The classifier is [`super::build_plan`], unchanged. This module supplies what
//! is `SQLite`-shaped: how to read the facts the classifier needs, how to write
//! the SQL, and how to apply it.
//!
//! - **Facts** come from `sqlite_master` and the `pragma_*` functions.
//!   `CHECK` columns are read from the `CREATE TABLE` text, because no pragma
//!   lists them.
//! - **Hashing.** `SQLite` has no `md5`. Autumn registers `autumn_sha256` on the
//!   connection. A dry run prints SQL that calls it, so plain `sqlite3` cannot
//!   run that script.
//! - **Atomic.** One `BEGIN IMMEDIATE` transaction per file. The write lock
//!   also stops a concurrent writer.
//! - **No old bytes.** `secure_delete` and a post-commit `VACUUM` and WAL
//!   truncate remove the pre-scrub values from free pages and the WAL.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use autumn_schema_core::{Column, ColumnType, Table};
use diesel::connection::SimpleConnection as _;
use diesel::sql_types::{Integer, Nullable, Text};
use diesel::{QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};
use sha2::{Digest as _, Sha256};

use super::{
    ClassificationInputs, DatabaseFacts, Dialect, RowTokenRow, ScrubArgs, ScrubError, ScrubPlan,
    SourceClassification, Strategy, TOKEN_HEX_LEN, UNIQUE_TOKEN_HEX_LEN, build_plan, email_shaped,
    encrypted_plaintext, guard_scrub_target, introspect, is_texty, probe_table_names, quote_ident,
    quote_literal, report_framework_tables, report_plan, report_triggers, require_type,
    resolve_key_ring, unreachable_tables,
};
use crate::db::sqlite_snapshot;

diesel::define_sql_function! {
    /// Lowercase hex SHA-256 of a text value.
    fn autumn_sha256(value: Text) -> Text;
}

/// The file a `SQLite` URL names, with symlinks and `..` resolved. `None` for a
/// URL that is not `SQLite`, or is in-memory.
pub(super) fn database_file(url: &str) -> Option<PathBuf> {
    if !is_sqlite_url(url) {
        return None;
    }
    let path = sqlite_snapshot::database_path(url).ok()?;
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

/// Whether a resolved target URL names a `SQLite` database.
fn is_sqlite_url(url: &str) -> bool {
    matches!(
        autumn_web::config::DatabaseBackend::detect(url),
        Some(autumn_web::config::DatabaseBackend::Sqlite)
    )
}

/// Decide whether this run is a `SQLite` run, and refuse what has no `SQLite`
/// form. Runs before any write, an `--artifact` restore included.
///
/// # Errors
///
/// [`ScrubError::MixedBackends`] for a mixed set, and
/// [`ScrubError::UnsupportedOnSqlite`] for `--sample`.
pub(super) fn check_targets(
    args: &ScrubArgs,
    targets: &[(String, String)],
) -> Result<bool, ScrubError> {
    let sqlite = targets.iter().filter(|(_, url)| is_sqlite_url(url)).count();
    if sqlite == 0 {
        return Ok(false);
    }
    if sqlite != targets.len() {
        return Err(ScrubError::MixedBackends);
    }
    if !args.sample.is_empty() {
        return Err(ScrubError::UnsupportedOnSqlite {
            what: "--sample",
            detail: "row sampling is Postgres-only. Scrub the whole copy instead.".to_owned(),
        });
    }
    Ok(true)
}

// ─── Connection ─────────────────────────────────────────────────────────────

fn database_path(label: &str, url: &str) -> Result<PathBuf, ScrubError> {
    sqlite_snapshot::database_path(url).map_err(|e| ScrubError::Introspect {
        label: label.to_owned(),
        detail: e.to_string(),
    })
}

/// Open an existing database file and register `autumn_sha256`. Never creates a
/// file.
fn connect(label: &str, url: &str) -> Result<SqliteConnection, ScrubError> {
    let path = database_path(label, url)?;
    let mut conn = sqlite_snapshot::open(&path).map_err(|e| ScrubError::Introspect {
        label: label.to_owned(),
        detail: e.to_string(),
    })?;
    autumn_sha256_utils::register_impl(&mut conn, |value: String| {
        hex::encode(Sha256::digest(value.as_bytes()))
    })
    .map_err(|e| ScrubError::Sql(e.to_string()))?;
    Ok(conn)
}

// ─── SQL builders ───────────────────────────────────────────────────────────

/// The SQL expression that identifies a row: its primary key, else `rowid`.
///
/// A rowid table can hold several rows with a `NULL` non-integer key. Those fall
/// back to `rowid`, or they would share one seed and one token. Both branches
/// carry a prefix, so a real key such as `rowid:1` cannot equal a fallback key.
pub(super) fn row_key_expr(table: &Table, facts: &DatabaseFacts) -> String {
    let mut keys: Vec<&str> = table.primary_key.iter().map(String::as_str).collect();
    if keys.is_empty() {
        keys = table
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect();
    }
    let key = match keys.as_slice() {
        [] => return "CAST(rowid AS TEXT)".to_owned(),
        // `quote()` keeps the storage class: the text '6162', the integer 6162 and
        // the blob x'6162' stay distinct, and a blob never has to be valid text.
        [key] => format!("quote({})", quote_ident(key)),
        many => many
            .iter()
            .map(|key| format!("quote({})", quote_ident(key)))
            .collect::<Vec<_>>()
            .join(" || ',' || "),
    };
    if !facts.rowid_tables.contains(&table.name) {
        return key;
    }
    let any_null = keys
        .iter()
        .map(|key| format!("{} IS NULL", quote_ident(key)))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("CASE WHEN {any_null} THEN 'rowid:' || rowid ELSE 'key:' || {key} END")
}

/// The per-row, per-column token every replacement derives from. The column
/// name salts it, so two PII columns of one row differ.
pub(super) fn token_expr(
    table: &Table,
    facts: &DatabaseFacts,
    column: &str,
    unique: bool,
) -> String {
    token_expr_from(&row_key_expr(table, facts), column, unique)
}

fn token_expr_from(row_key: &str, column: &str, unique: bool) -> String {
    let hash = format!(
        "autumn_sha256({} || '|' || {row_key})",
        quote_literal(column)
    );
    if unique {
        debug_assert_eq!(UNIQUE_TOKEN_HEX_LEN, 64);
        hash
    } else {
        format!("substr({hash}, 1, {TOKEN_HEX_LEN})")
    }
}

/// Pick a strategy from the column type alone (`#[encrypted]`, GDPR anonymize).
///
/// # Errors
///
/// [`ScrubError::NoAutoStrategy`] when no generic fake fits the type.
pub(super) fn auto_strategy(column: &Column) -> Result<Strategy, ScrubError> {
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
        // A NUMERIC-affinity declared type. Guess only for a type that is
        // plainly a date, a time or a number. `UUID` and `JSON` land here too,
        // and a made-up `0` in them reads back as garbage.
        ColumnType::Opaque { pg_type } => {
            if opaque_epoch(pg_type).is_some() {
                Strategy::Epoch
            } else if is_plainly_numeric(pg_type) {
                Strategy::Zero
            } else {
                return Err(ScrubError::NoAutoStrategy {
                    column: column.name.clone(),
                    detail: format!("declared type {pg_type:?}"),
                });
            }
        }
        other @ ColumnType::Enum { .. } => {
            return Err(ScrubError::NoAutoStrategy {
                column: column.name.clone(),
                detail: format!("{other:?}"),
            });
        }
    })
}

/// Whether a declared type is plainly a number or a boolean.
fn is_plainly_numeric(decl: &str) -> bool {
    let decl = decl.to_ascii_lowercase();
    ["numeric", "decimal", "number", "bool", "money"]
        .iter()
        .any(|word| decl.contains(word))
}

/// The epoch literal for a date/time-like declared type.
fn opaque_epoch(decl: &str) -> Option<&'static str> {
    let decl = decl.to_ascii_lowercase();
    if decl.contains("datetime") || decl.contains("timestamp") {
        Some("'1970-01-01 00:00:00'")
    } else if decl.contains("date") {
        Some("'1970-01-01'")
    } else if decl.contains("time") {
        Some("'00:00:00'")
    } else {
        None
    }
}

/// Build the replacement expression for one column.
///
/// # Errors
///
/// [`ScrubError::StrategyTypeMismatch`] when the strategy cannot produce the
/// column's type.
pub(super) fn replacement_expr(
    strategy: Strategy,
    column: &Column,
    token: &str,
) -> Result<String, ScrubError> {
    let mismatch = || ScrubError::StrategyTypeMismatch {
        column: column.name.clone(),
        strategy: strategy.as_str(),
        detail: format!("{:?}", column.ty),
    };
    if matches!(
        strategy,
        Strategy::Email | Strategy::Name | Strategy::Redact | Strategy::Phone
    ) {
        require_type(column, strategy, is_texty(&column.ty))?;
    }
    Ok(match strategy {
        Strategy::Auto => replacement_expr(auto_strategy(column)?, column, token)?,
        Strategy::Encrypted => return Err(mismatch()),
        Strategy::Email => format!("'scrubbed+' || {token} || '{}'", super::SCRUB_EMAIL_DOMAIN),
        Strategy::Name => format!("'Scrubbed ' || {token}"),
        Strategy::Redact => format!("'[scrubbed:' || {token} || ']'"),
        Strategy::Phone => {
            // Ten hex characters onto digits: a->0 .. f->5.
            let mut digits = format!("substr({token}, 1, 10)");
            for (from, to) in ["a", "b", "c", "d", "e", "f"].iter().zip(0..) {
                digits = format!("replace({digits}, '{from}', '{to}')");
            }
            format!("'+1555' || {digits}")
        }
        Strategy::Null => "NULL".to_owned(),
        Strategy::Uuid => {
            // SQLite stores a UUID as `TEXT`, so a declared `TEXT` column is fine.
            require_type(
                column,
                strategy,
                matches!(column.ty, ColumnType::Uuid | ColumnType::Text),
            )?;
            format!(
                "substr({token}, 1, 8) || '-' || substr({token}, 9, 4) || '-' || \
                 substr({token}, 13, 4) || '-' || substr({token}, 17, 4) || '-' || \
                 substr({token}, 21, 12)"
            )
        }
        Strategy::Bytes => {
            require_type(column, strategy, matches!(column.ty, ColumnType::Bytes))?;
            format!("CAST({token} AS BLOB)")
        }
        Strategy::Json => {
            if matches!(
                column.ty,
                ColumnType::Json | ColumnType::Attachment | ColumnType::Text
            ) {
                "'{\"scrubbed\": true}'".to_owned()
            } else {
                return Err(mismatch());
            }
        }
        Strategy::Zero => match &column.ty {
            ColumnType::Bool
            | ColumnType::Int32
            | ColumnType::Int64
            | ColumnType::Float32
            | ColumnType::Float64
            | ColumnType::Decimal { .. }
            // Explicit only: `auto` never picks `zero` for text. SQLite keeps
            // a decimal as `TEXT`, and `0` converts to `'0'` there.
            | ColumnType::Text
            | ColumnType::Opaque { .. } => "0".to_owned(),
            _ => return Err(mismatch()),
        },
        Strategy::Epoch => match &column.ty {
            ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Text => {
                "'1970-01-01 00:00:00'".to_owned()
            }
            ColumnType::Opaque { pg_type } => {
                opaque_epoch(pg_type).ok_or_else(mismatch)?.to_owned()
            }
            _ => return Err(mismatch()),
        },
    })
}

// ─── Reading `CREATE` text ──────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// An unquoted word, lowercased.
    Word(String),
    /// A quoted identifier, lowercased.
    Quoted(String),
    Punct(char),
}

/// Split SQL text into words, quoted identifiers and punctuation. String
/// literals and comments are dropped: a name inside one is not a reference.
fn tokenize(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut name = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == close {
                        if close != ']' && chars.get(i + 1) == Some(&close) {
                            name.push(close);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    name.push(chars[i]);
                    i += 1;
                }
                i += 1;
                tokens.push(Token::Quoted(name.to_lowercase()));
            }
            c if c.is_alphanumeric() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Word(
                    chars[start..i].iter().collect::<String>().to_lowercase(),
                ));
            }
            c if c.is_whitespace() => i += 1,
            c => {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        }
    }
    tokens
}

/// The names (lowercased) inside every `CHECK ( … )` of a `CREATE TABLE`.
fn check_names(table_sql: &str) -> BTreeSet<String> {
    let tokens = tokenize(table_sql);
    let mut names = BTreeSet::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == Token::Word("check".to_owned())
            && tokens.get(i + 1) == Some(&Token::Punct('('))
        {
            let mut depth = 0_usize;
            let mut j = i + 1;
            while j < tokens.len() {
                match &tokens[j] {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Token::Word(w) | Token::Quoted(w) => {
                        names.insert(w.clone());
                    }
                    Token::Punct(_) => {}
                }
                j += 1;
            }
            i = j;
        }
        i += 1;
    }
    names
}

/// Every name (lowercased) that appears in a piece of SQL text.
fn names_in(sql: &str) -> BTreeSet<String> {
    tokenize(sql)
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(w) | Token::Quoted(w) => Some(w),
            Token::Punct(_) => None,
        })
        .collect()
}

/// Whether a `CREATE TABLE` ends in `WITHOUT ROWID`.
fn is_without_rowid(sql: &str) -> bool {
    tokenize(sql).windows(2).any(|pair| {
        pair == [
            Token::Word("without".to_owned()),
            Token::Word("rowid".to_owned()),
        ]
    })
}

/// Whether the text is `CREATE VIRTUAL TABLE …`. A `VIRTUAL` generated column
/// is not one.
fn is_virtual_table(sql: &str) -> bool {
    let words: Vec<Token> = tokenize(sql).into_iter().take(3).collect();
    words
        == [
            Token::Word("create".to_owned()),
            Token::Word("virtual".to_owned()),
            Token::Word("table".to_owned()),
        ]
}

/// Whether a `CREATE TRIGGER` fires on `DELETE`. Unreadable text counts as yes.
fn fires_on_delete(trigger_sql: &str) -> bool {
    let tokens = tokenize(trigger_sql);
    tokens
        .iter()
        .position(|t| *t == Token::Word("on".to_owned()))
        .is_none_or(|on| tokens[..on].contains(&Token::Word("delete".to_owned())))
}

// ─── Probe ──────────────────────────────────────────────────────────────────

#[derive(QueryableByName)]
struct MasterRow {
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    tbl_name: String,
    #[diesel(sql_type = Nullable<Text>)]
    sql: Option<String>,
}

#[derive(QueryableByName)]
struct ColumnRow {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Integer)]
    hidden: i32,
    #[diesel(sql_type = Integer)]
    pk: i32,
}

#[derive(QueryableByName)]
struct ForeignKeyRow {
    #[diesel(sql_type = Text)]
    parent: String,
    #[diesel(sql_type = Text)]
    from_col: String,
    #[diesel(sql_type = Nullable<Text>)]
    to_col: Option<String>,
}

#[derive(QueryableByName)]
struct IndexRow {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Integer)]
    uniq: i32,
    #[diesel(sql_type = Integer)]
    partial: i32,
}

#[derive(QueryableByName)]
struct IndexColumnRow {
    #[diesel(sql_type = Nullable<Text>)]
    col: Option<String>,
    #[diesel(sql_type = Integer)]
    key: i32,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// What the probe found beyond the classifier's facts.
struct Probed {
    facts: DatabaseFacts,
    /// Virtual tables. Their shadow tables hold copies of row text.
    virtual_tables: Vec<String>,
}

fn sql_err(error: &diesel::result::Error) -> ScrubError {
    ScrubError::Sql(error.to_string())
}

/// Gather every fact the classifier needs, in one read.
#[allow(clippy::too_many_lines)]
fn probe(conn: &mut SqliteConnection, config: &super::ScrubConfig) -> Result<Probed, ScrubError> {
    let master: Vec<MasterRow> = sql_query(
        "SELECT type AS kind, name, tbl_name, sql FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\'",
    )
    .load(conn)
    .map_err(|e| sql_err(&e))?;

    let tables: Vec<&MasterRow> = master.iter().filter(|r| r.kind == "table").collect();
    let canon_table: BTreeMap<String, String> = tables
        .iter()
        .map(|t| (t.name.to_lowercase(), t.name.clone()))
        .collect();

    let mut public_columns = BTreeSet::new();
    let mut generated_columns = BTreeSet::new();
    let mut columns_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pk_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut virtual_tables = Vec::new();
    for table in &tables {
        let rows: Vec<ColumnRow> = sql_query(format!(
            "SELECT name, hidden, pk FROM pragma_table_xinfo({})",
            quote_literal(&table.name)
        ))
        .load(conn)
        .map_err(|e| sql_err(&e))?;
        let mut pk: Vec<(i32, String)> = Vec::new();
        for row in rows {
            // 1 = a virtual table's internal column.
            if row.hidden == 1 {
                continue;
            }
            if row.hidden >= 2 {
                generated_columns.insert((table.name.clone(), row.name.clone()));
            }
            if row.pk > 0 {
                pk.push((row.pk, row.name.clone()));
            }
            columns_of
                .entry(table.name.clone())
                .or_default()
                .push(row.name.clone());
            public_columns.insert((table.name.clone(), row.name));
        }
        pk.sort();
        pk_of.insert(table.name.clone(), pk.into_iter().map(|(_, n)| n).collect());
        if table.sql.as_deref().is_some_and(is_virtual_table) {
            virtual_tables.push(table.name.clone());
        }
    }
    let canon_col = |table: &str, column: &str| -> String {
        columns_of
            .get(table)
            .and_then(|cols| cols.iter().find(|c| c.eq_ignore_ascii_case(column)))
            .cloned()
            .unwrap_or_else(|| column.to_owned())
    };

    let mut foreign_key_columns = BTreeSet::new();
    for table in &tables {
        let rows: Vec<ForeignKeyRow> = sql_query(format!(
            "SELECT \"table\" AS parent, \"from\" AS from_col, \"to\" AS to_col \
             FROM pragma_foreign_key_list({})",
            quote_literal(&table.name)
        ))
        .load(conn)
        .map_err(|e| sql_err(&e))?;
        for row in rows {
            foreign_key_columns.insert((table.name.clone(), canon_col(&table.name, &row.from_col)));
            let parent = canon_table
                .get(&row.parent.to_lowercase())
                .cloned()
                .unwrap_or(row.parent);
            match row.to_col {
                Some(to) => {
                    foreign_key_columns.insert((parent.clone(), canon_col(&parent, &to)));
                }
                // No named parent column: SQLite points at the parent's key.
                None => {
                    for key in pk_of.get(&parent).into_iter().flatten() {
                        foreign_key_columns.insert((parent.clone(), key.clone()));
                    }
                }
            }
        }
    }

    let index_sql: BTreeMap<&str, &str> = master
        .iter()
        .filter(|r| r.kind == "index")
        .filter_map(|r| r.sql.as_deref().map(|s| (r.name.as_str(), s)))
        .collect();
    let mut unique_columns = BTreeSet::new();
    let mut unique_expression_columns = BTreeSet::new();
    for table in &tables {
        let indexes: Vec<IndexRow> = sql_query(format!(
            "SELECT name, \"unique\" AS uniq, partial FROM pragma_index_list({})",
            quote_literal(&table.name)
        ))
        .load(conn)
        .map_err(|e| sql_err(&e))?;
        for index in indexes.into_iter().filter(|i| i.uniq != 0) {
            let cols: Vec<IndexColumnRow> = sql_query(format!(
                "SELECT name AS col, key FROM pragma_index_xinfo({})",
                quote_literal(&index.name)
            ))
            .load(conn)
            .map_err(|e| sql_err(&e))?;
            let keyed: Vec<&IndexColumnRow> = cols.iter().filter(|c| c.key != 0).collect();
            let has_expression = keyed.iter().any(|c| c.col.is_none());
            for column in keyed.iter().filter_map(|c| c.col.as_deref()) {
                unique_columns.insert((table.name.clone(), canon_col(&table.name, column)));
            }
            if index.partial != 0 || has_expression {
                // Every column the predicate or expression reads.
                let mentioned = index_sql
                    .get(index.name.as_str())
                    .map(|s| names_in(s))
                    .unwrap_or_default();
                for column in columns_of.get(&table.name).into_iter().flatten() {
                    if mentioned.contains(&column.to_lowercase()) {
                        unique_expression_columns.insert((table.name.clone(), column.clone()));
                    }
                }
            }
        }
    }

    let mut checked_columns = BTreeSet::new();
    for table in &tables {
        let Some(sql) = table.sql.as_deref() else {
            continue;
        };
        let names = check_names(sql);
        for column in columns_of.get(&table.name).into_iter().flatten() {
            if names.contains(&column.to_lowercase()) {
                checked_columns.insert((table.name.clone(), column.clone()));
            }
        }
    }

    let mut triggered_tables = BTreeSet::new();
    let mut delete_triggered_tables = BTreeSet::new();
    let mut trigger_identities = BTreeSet::new();
    for row in master.iter().filter(|r| r.kind == "trigger") {
        let table = canon_table
            .get(&row.tbl_name.to_lowercase())
            .cloned()
            .unwrap_or_else(|| row.tbl_name.clone());
        let sql = row.sql.as_deref().unwrap_or_default();
        trigger_identities.insert(format!(
            "{table}:trigger:{}:{}",
            row.name,
            hex::encode(Sha256::digest(sql))
        ));
        triggered_tables.insert(table.clone());
        if fires_on_delete(sql) {
            delete_triggered_tables.insert(table);
        }
    }

    let rowid_tables: BTreeSet<String> = tables
        .iter()
        .filter(|t| !t.sql.as_deref().is_some_and(is_without_rowid))
        .map(|t| t.name.clone())
        .collect();
    let present: BTreeSet<String> = canon_table.values().cloned().collect();
    let framework_tables = probe_table_names(config)
        .into_iter()
        .filter(|t| present.contains(t))
        .collect();

    Ok(Probed {
        facts: DatabaseFacts {
            dialect: Dialect::Sqlite,
            rowid_tables,
            foreign_key_columns,
            checked_columns,
            generated_columns,
            unique_columns,
            unique_expression_columns,
            triggered_tables,
            delete_triggered_tables,
            trigger_identities,
            replication_role: "origin".to_owned(),
            framework_tables,
            public_columns,
            public_base_tables: present,
            ..DatabaseFacts::default()
        },
        virtual_tables,
    })
}

// ─── Statements ─────────────────────────────────────────────────────────────

/// The `DELETE` for each opted-in framework table that exists.
fn purges_for(facts: &DatabaseFacts, config: &super::ScrubConfig) -> Vec<(String, String)> {
    facts
        .framework_tables
        .iter()
        .filter(|t| config.framework.purge.contains(*t))
        .map(|t| (t.clone(), format!("DELETE FROM {}", quote_ident(t))))
        .collect()
}

/// One statement of the transaction, with the table it writes.
enum Step<'a> {
    /// A `[framework] purge` delete.
    Purge { table: &'a str, sql: &'a str },
    /// A column rewrite.
    Rewrite { table: &'a str, sql: &'a str },
}

impl Step<'_> {
    const fn sql(&self) -> &str {
        match self {
            Self::Purge { sql, .. } | Self::Rewrite { sql, .. } => sql,
        }
    }
}

/// The transaction's statements, in order. The dry run prints this list and
/// `apply` runs it, so the two cannot drift.
fn steps<'a>(plan: &'a ScrubPlan, purges: &'a [(String, String)]) -> Vec<Step<'a>> {
    let purge = |(table, sql): &'a (String, String)| Step::Purge { table, sql };
    let mut out: Vec<Step<'a>> = purges.iter().map(purge).collect();
    out.extend(plan.tables.iter().filter_map(|t| {
        t.sql.as_deref().map(|sql| Step::Rewrite {
            table: &t.table,
            sql,
        })
    }));
    // Again after the rewrites: a trigger on a scrubbed table can refill a
    // purged one.
    out.extend(purges.iter().map(purge));
    out
}

/// The pragmas set before the transaction opens.
const SESSION_PRAGMAS: &str = "PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON;";

/// Failure inside the transaction.
enum Failure {
    Db(diesel::result::Error),
    Refused(ScrubError),
}

impl From<diesel::result::Error> for Failure {
    fn from(error: diesel::result::Error) -> Self {
        Self::Db(error)
    }
}

fn foreign_key_violations(conn: &mut SqliteConnection) -> Result<i64, diesel::result::Error> {
    let row: CountRow =
        sql_query("SELECT count(*) AS n FROM pragma_foreign_key_check").get_result(conn)?;
    Ok(row.n)
}

/// Rewrite one `#[encrypted]` column with a valid envelope of a fake value.
fn rewrite_encrypted_column(
    conn: &mut SqliteConnection,
    table: &str,
    row_key: &str,
    rewrite: &super::EncryptedRewrite,
) -> Result<usize, diesel::result::Error> {
    use autumn_web::encryption::{Mode, encrypt_text};

    let ident = quote_ident(&rewrite.column);
    let token = token_expr_from(&format!("({row_key})"), &rewrite.column, true);
    let rows: Vec<RowTokenRow> = sql_query(format!(
        "SELECT ({row_key}) AS row_key, {token} AS token FROM {} WHERE {ident} IS NOT NULL",
        quote_ident(table),
    ))
    .load(conn)?;
    let mode = if rewrite.deterministic {
        Mode::Deterministic
    } else {
        Mode::Randomized
    };
    let total = rows.len();
    let mut updated = 0;
    for row in rows {
        let plaintext = encrypted_plaintext(rewrite.shape, &row.token);
        let envelope = encrypt_text(mode, &plaintext).map_err(|e| {
            diesel::result::Error::QueryBuilderError(
                format!(
                    "could not encrypt a replacement for {table}.{}: {e}",
                    rewrite.column
                )
                .into(),
            )
        })?;
        updated += sql_query(format!(
            "UPDATE {} SET {ident} = ? WHERE ({row_key}) = ?",
            quote_ident(table)
        ))
        .bind::<Text, _>(envelope)
        .bind::<Text, _>(row.row_key)
        .execute(conn)?;
    }
    // A row the key did not match would keep its old ciphertext.
    if updated != total {
        return Err(diesel::result::Error::QueryBuilderError(
            format!(
                "re-encrypted {updated} of {total} rows in {table}.{}. Nothing was written.",
                rewrite.column
            )
            .into(),
        ));
    }
    Ok(updated)
}

/// Apply one target's plan in a single transaction, then compact.
fn apply(
    label: &str,
    url: &str,
    plan: &ScrubPlan,
    purges: &[(String, String)],
    facts: &DatabaseFacts,
    config: &super::ScrubConfig,
) -> Result<Vec<(String, usize)>, ScrubError> {
    let mut conn = connect(label, url)?;
    conn.batch_execute(SESSION_PRAGMAS)
        .map_err(|e| sql_err(&e))?;
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut drift: Vec<String> = Vec::new();

    conn.immediate_transaction::<_, Failure, _>(|conn| {
        // The plan came from an earlier read. The write lock is held now, so
        // compare against it: a column added in between must not commit
        // unclassified.
        let now = probe(conn, config).map_err(Failure::Refused)?;
        if now.facts != *facts {
            drift = drift_entries(facts, &now.facts);
            return Err(Failure::Db(diesel::result::Error::RollbackTransaction));
        }
        // The pass-1 check ran before this lock. Run it again on the locked file.
        let side_effects = purge_side_effects(conn, facts, purges).map_err(Failure::Refused)?;
        if !side_effects.is_empty() {
            return Err(Failure::Refused(cascade_refusal(&side_effects)));
        }
        let violations_before = foreign_key_violations(conn)?;

        let mut emptied: BTreeMap<&str, usize> = BTreeMap::new();
        for step in steps(plan, purges) {
            let rows = sql_query(step.sql()).execute(conn)?;
            match step {
                Step::Purge { table, .. } => *emptied.entry(table).or_default() += rows,
                Step::Rewrite { table, .. } => counts.push((table.to_owned(), rows)),
            }
        }
        for table in &plan.tables {
            for rewrite in &table.encrypted {
                let rows = rewrite_encrypted_column(conn, &table.table, &table.row_key, rewrite)?;
                counts.push((format!("{}.{}", table.table, rewrite.column), rows));
            }
        }
        for (table, rows) in emptied {
            counts.push((format!("{table} (emptied)"), rows));
        }

        // Prove the promise: a trigger can refill a purged table.
        let mut refilled = Vec::new();
        for (table, _) in purges {
            let row: CountRow =
                sql_query(format!("SELECT count(*) AS n FROM {}", quote_ident(table)))
                    .get_result(conn)?;
            if row.n > 0 {
                refilled.push(format!("{table} ({} row(s))", row.n));
            }
        }
        if !refilled.is_empty() {
            return Err(Failure::Refused(ScrubError::EmptyingTriggerLeak {
                tables: refilled,
            }));
        }
        // A purge can cascade. It must not orphan a row.
        if foreign_key_violations(conn)? > violations_before {
            return Err(Failure::Refused(ScrubError::Sql(
                "the scrub would break a foreign key. Nothing was written.".to_owned(),
            )));
        }
        Ok(())
    })
    .map_err(|failure| match failure {
        _ if !drift.is_empty() => ScrubError::SchemaChanged {
            entries: std::mem::take(&mut drift),
        },
        Failure::Refused(error) => error,
        Failure::Db(error) => ScrubError::Sql(error.to_string()),
    })?;

    // Committed. Free pages, the WAL and the planner statistics still hold the
    // old bytes. A failure here is loud: the target is scrubbed but not yet
    // clean on disk.
    compact(&mut conn).map_err(|e| {
        ScrubError::Sql(format!(
            "the scrub committed, but compacting {label} failed: {e}. Old values may \
             remain in free pages. Run `VACUUM` on it before sharing it."
        ))
    })?;
    Ok(counts)
}

#[derive(QueryableByName)]
struct Checkpoint {
    #[diesel(sql_type = Integer)]
    busy: i32,
    #[diesel(sql_type = Integer)]
    log: i32,
    #[diesel(sql_type = Integer)]
    checkpointed: i32,
}

/// Remove the pre-scrub bytes from the file: drop the planner samples
/// (`sqlite_stat3` and `sqlite_stat4` keep index keys), rewrite the file, and
/// truncate the WAL. A checkpoint that a reader blocks is an error, because the
/// WAL would still hold the old pages.
fn compact(conn: &mut SqliteConnection) -> Result<(), String> {
    let db = |e: diesel::result::Error| e.to_string();
    for table in ["sqlite_stat3", "sqlite_stat4"] {
        let found: CountRow = sql_query(format!(
            "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
        ))
        .get_result(conn)
        .map_err(db)?;
        if found.n > 0 {
            conn.batch_execute(&format!("DELETE FROM {table};"))
                .map_err(db)?;
        }
    }
    conn.batch_execute("VACUUM;").map_err(db)?;
    let rows: Vec<Checkpoint> = sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
        .load(conn)
        .map_err(db)?;
    // A database that is not in WAL mode returns -1 for both counts.
    match rows.first() {
        Some(row) if row.busy != 0 || (row.log >= 0 && row.log != row.checkpointed) => {
            Err("a reader blocked the WAL checkpoint, so the WAL may hold old pages".to_owned())
        }
        _ => Ok(()),
    }
}

#[derive(QueryableByName)]
struct ActionRow {
    #[diesel(sql_type = Text)]
    child: String,
    #[diesel(sql_type = Text)]
    parent: String,
}

/// Tables outside the purge set that a purge would change through a foreign key
/// action (`CASCADE`, `SET NULL`, `SET DEFAULT`), as `child -> parent`.
fn purge_side_effects(
    conn: &mut SqliteConnection,
    facts: &DatabaseFacts,
    purges: &[(String, String)],
) -> Result<Vec<String>, ScrubError> {
    let emptied: BTreeSet<&str> = purges.iter().map(|(table, _)| table.as_str()).collect();
    let mut found = Vec::new();
    for child in &facts.public_base_tables {
        if emptied.contains(child.as_str()) {
            continue;
        }
        let rows: Vec<ActionRow> = sql_query(format!(
            "SELECT {} AS child, \"table\" AS parent FROM pragma_foreign_key_list({}) \
             WHERE upper(on_delete) IN ('CASCADE', 'SET NULL', 'SET DEFAULT')",
            quote_literal(child),
            quote_literal(child)
        ))
        .load(conn)
        .map_err(|e| sql_err(&e))?;
        found.extend(
            rows.into_iter()
                .filter(|row| emptied.iter().any(|p| p.eq_ignore_ascii_case(&row.parent)))
                .map(|row| format!("{} -> {}", row.child, row.parent)),
        );
    }
    found.sort();
    Ok(found)
}

fn cascade_refusal(side_effects: &[String]) -> ScrubError {
    ScrubError::UnsupportedOnSqlite {
        what: "[framework] purge",
        detail: format!(
            "a foreign key action would also change: {}. Drop the action, or \
             empty those tables first.",
            side_effects.join(", ")
        ),
    }
}

/// Name what changed between two probes.
fn drift_entries(before: &DatabaseFacts, after: &DatabaseFacts) -> Vec<String> {
    let mut drift = super::schema_drift(
        (&before.public_base_tables, &before.public_columns),
        (&after.public_base_tables, &after.public_columns),
    );
    if drift.is_empty() {
        drift.push("constraints, indexes or triggers changed".to_owned());
    }
    drift
}

// ─── Run ────────────────────────────────────────────────────────────────────

struct Planned<'a> {
    label: &'a str,
    url: &'a str,
    plan: ScrubPlan,
    facts: DatabaseFacts,
}

/// Classify every target, then apply. The `SQLite` counterpart of
/// `classify_and_apply`.
///
/// # Errors
///
/// Every refusal the Postgres path has, plus those in [`check_targets`].
#[allow(clippy::too_many_lines)]
pub(super) fn classify_and_apply(
    args: &ScrubArgs,
    profile: &str,
    targets: &[(String, String)],
    sources: &SourceClassification,
) -> Result<(), ScrubError> {
    // ── Pass 1: classify everything ─────────────────────────────────────────
    let mut planned: Vec<Planned<'_>> = Vec::with_capacity(targets.len());
    for (label, url) in targets {
        let mut conn = connect(label, url)?;
        let Probed {
            facts,
            virtual_tables,
        } = probe(&mut conn, &sources.config)?;
        if !virtual_tables.is_empty() {
            return Err(ScrubError::UnsupportedOnSqlite {
                what: "virtual tables",
                detail: format!(
                    "{}. Their shadow tables copy row text, and the scrub cannot classify them.",
                    virtual_tables.join(", ")
                ),
            });
        }
        let tables =
            introspect::introspect_sqlite_conn(&mut conn).map_err(|e| ScrubError::Introspect {
                label: label.clone(),
                detail: e.to_string(),
            })?;
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

        let purges = purges_for(&facts, &sources.config);
        // A purge that cascades would change tables the plan never classified.
        let side_effects = purge_side_effects(&mut conn, &facts, &purges)?;
        if !side_effects.is_empty() {
            return Err(cascade_refusal(&side_effects));
        }
        // The emptying pass runs after every rewrite, so a delete trigger on a
        // purged table can write fresh rows into a table already scrubbed.
        let mut emptying_triggers: Vec<String> = purges
            .iter()
            .map(|(table, _)| table.clone())
            .filter(|t| facts.delete_triggered_tables.contains(t))
            .collect();
        emptying_triggers.sort();
        if !emptying_triggers.is_empty() {
            return Err(ScrubError::EmptyingTriggerLeak {
                tables: emptying_triggers,
            });
        }
        report_plan(label, &plan);
        report_framework_tables(&facts.framework_tables, &sources.config);
        report_triggers(&plan, &purges, None, &facts);
        planned.push(Planned {
            label,
            url,
            plan,
            facts,
        });
    }

    if args.check {
        eprintln!("\n\u{2713} Every column is classified \u{2014} no unclassified data can leak.");
        return Ok(());
    }
    if args.dry_run {
        return dry_run(profile, args.force, &planned, sources);
    }

    // An encrypted rewrite needs the target's key BEFORE anything is written.
    if planned
        .iter()
        .any(|p| p.plan.tables.iter().any(|t| !t.encrypted.is_empty()))
    {
        autumn_web::encryption::install_key_ring(resolve_key_ring(profile, Path::new("."))?);
    }

    // ── Pass 2: apply ───────────────────────────────────────────────────────
    let mut committed: Vec<&str> = Vec::new();
    for target in &planned {
        let purges = purges_for(&target.facts, &sources.config);
        let applied = apply(
            target.label,
            target.url,
            &target.plan,
            &purges,
            &target.facts,
            &sources.config,
        )
        .inspect_err(|_| {
            if !committed.is_empty() {
                eprintln!(
                    "\n\u{26A0}\u{FE0F}  Already committed before this failure: {}. \
                     Those databases ARE scrubbed; every later target is untouched and \
                     still holds real data.",
                    committed.join(", ")
                );
            }
        })?;
        committed.push(target.label);
        for (table, rows) in applied {
            eprintln!("  \u{2713} {table}: {rows} row(s) scrubbed.");
        }
    }

    if let Some(dir) = &args.output {
        eprintln!("\u{2500}\u{2500} writing a scrubbed artifact \u{2500}\u{2500}");
        crate::db::backup::backup(&crate::db::backup::BackupArgs {
            profile: args.profile.clone(),
            dir: Some(dir.clone()),
            format: crate::db::backup::BackupFormat::Custom,
            keep: None,
            target: crate::db::backup::TargetSelector::All,
            upload: false,
        })?;
    }
    eprintln!("\n\u{2713} Scrub complete.");
    Ok(())
}

/// Print the exact SQL a run would execute, and write nothing.
fn dry_run(
    profile: &str,
    force: bool,
    planned: &[Planned<'_>],
    sources: &SourceClassification,
) -> Result<(), ScrubError> {
    // Same rule as Postgres: a printed script for a protected profile is the
    // run the guard exists to stop.
    guard_scrub_target(profile, force).map_err(|_| ScrubError::UnprintableProductionTarget {
        profile: profile.to_owned(),
    })?;
    // An encrypted rewrite has no SQL form. Printing the rest would leave the
    // production ciphertext in every kept row.
    let mut encrypted: Vec<String> = planned
        .iter()
        .flat_map(|p| {
            p.plan.tables.iter().flat_map(move |t| {
                t.encrypted
                    .iter()
                    .map(move |r| format!("{}: {}.{}", p.label, t.table, r.column))
            })
        })
        .collect();
    encrypted.sort();
    if !encrypted.is_empty() {
        return Err(ScrubError::UnprintableEncryptedRewrite { columns: encrypted });
    }
    eprintln!(
        "\n-- The statements below are exactly what a real run executes.\n\
         -- `autumn_sha256()` is registered by autumn. Plain `sqlite3` does not have it."
    );
    for target in planned {
        let purges = purges_for(&target.facts, &sources.config);
        let path = database_path(target.label, target.url)?;
        eprintln!("\n  -- {} ({})", target.label, path.display());
        for line in SESSION_PRAGMAS.split("; ") {
            eprintln!("  {};", line.trim_end_matches(';'));
        }
        eprintln!("  BEGIN IMMEDIATE;");
        for step in steps(&target.plan, &purges) {
            eprintln!("  {};", step.sql());
        }
        eprintln!("  COMMIT;");
        eprintln!("  VACUUM;");
        eprintln!("  PRAGMA wal_checkpoint(TRUNCATE);");
    }
    eprintln!("\n\u{2713} Dry run only \u{2014} nothing was written.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use autumn_schema_core::Backend;
    use diesel::Connection as _;

    use super::*;

    #[derive(QueryableByName)]
    struct One {
        #[diesel(sql_type = Text)]
        v: String,
    }

    fn column(name: &str, ty: ColumnType) -> Column {
        let mut column = Column::new(name, ty);
        column.nullable = true;
        column
    }

    #[test]
    fn check_names_reads_column_and_table_checks() {
        let sql = "CREATE TABLE t (a TEXT CHECK (length(a) > 1), b TEXT, \
                   c INT, CONSTRAINT k CHECK (b <> 'c' AND \"c\" > 0))";
        // `'c'` is a string literal, not a reference. `"c"` is.
        let found = check_names(sql);
        for column in ["a", "b", "c"] {
            assert!(found.contains(column), "{column} in {found:?}");
        }
    }

    #[test]
    fn a_virtual_generated_column_is_not_a_virtual_table() {
        assert!(is_virtual_table("CREATE VIRTUAL TABLE d USING fts5(b)"));
        assert!(!is_virtual_table(
            "CREATE TABLE t (a TEXT, s TEXT GENERATED ALWAYS AS (a) VIRTUAL)"
        ));
    }

    #[test]
    fn check_names_ignores_a_table_without_checks() {
        assert!(check_names("CREATE TABLE t (check_me TEXT, b TEXT)").is_empty());
    }

    #[test]
    fn check_names_handles_nested_parens_and_comments() {
        let sql = "CREATE TABLE t (a INT, -- check (a)\n b INT CHECK ((b > 0) AND (b < 9)))";
        let found = check_names(sql);
        assert!(found.contains("b"), "{found:?}");
        assert!(
            !found.contains("a"),
            "a comment is not a reference: {found:?}"
        );
    }

    #[test]
    fn tokenize_skips_string_literals_with_doubled_quotes() {
        let tokens = tokenize("x = 'it''s a' AND y");
        assert_eq!(
            tokens,
            [
                Token::Word("x".into()),
                Token::Punct('='),
                Token::Word("and".into()),
                Token::Word("y".into()),
            ]
        );
    }

    #[test]
    fn delete_triggers_are_recognised() {
        assert!(fires_on_delete(
            "CREATE TRIGGER k AFTER DELETE ON t BEGIN SELECT 1; END"
        ));
        assert!(!fires_on_delete(
            "CREATE TRIGGER k AFTER INSERT ON t BEGIN DELETE FROM u; END"
        ));
        assert!(!fires_on_delete(
            "CREATE TRIGGER k AFTER UPDATE OF a ON t BEGIN SELECT 1; END"
        ));
    }

    #[test]
    fn unreadable_trigger_text_counts_as_a_delete_trigger() {
        assert!(fires_on_delete(""));
    }

    #[test]
    fn email_replacement_is_a_sha256_token_without_postgres_syntax() {
        let col = column("email", ColumnType::Text);
        let token = "autumn_sha256('email' || '|' || x)";
        let sql = replacement_expr(Strategy::Email, &col, token).expect("email");
        assert_eq!(sql, format!("'scrubbed+' || {token} || '@example.invalid'"));
        for pg in ["md5", "::", "ctid", "translate", "public"] {
            assert!(!sql.contains(pg), "{pg} is Postgres-only: {sql}");
        }
    }

    #[test]
    fn phone_replacement_maps_hex_to_digits_with_nested_replace() {
        let col = column("phone", ColumnType::Text);
        let sql = replacement_expr(Strategy::Phone, &col, "tok").expect("phone");
        assert!(sql.starts_with("'+1555' || replace("), "{sql}");
        assert_eq!(sql.matches("replace(").count(), 6);
        assert!(!sql.contains("translate"));
    }

    #[test]
    fn text_strategies_refuse_a_numeric_column() {
        let col = column("age", ColumnType::Int64);
        for strategy in [
            Strategy::Email,
            Strategy::Name,
            Strategy::Redact,
            Strategy::Phone,
        ] {
            assert!(
                matches!(
                    replacement_expr(strategy, &col, "tok"),
                    Err(ScrubError::StrategyTypeMismatch { .. })
                ),
                "{strategy:?}"
            );
        }
    }

    #[test]
    fn zero_and_epoch_follow_the_declared_type() {
        let numeric = column(
            "balance",
            ColumnType::Opaque {
                pg_type: "DECIMAL(10,2)".to_owned(),
            },
        );
        assert_eq!(
            replacement_expr(Strategy::Zero, &numeric, "t").expect("zero"),
            "0"
        );
        let date = column(
            "born",
            ColumnType::Opaque {
                pg_type: "DATE".to_owned(),
            },
        );
        assert_eq!(
            replacement_expr(Strategy::Epoch, &date, "t").expect("epoch"),
            "'1970-01-01'"
        );
        assert_eq!(auto_strategy(&date).expect("auto"), Strategy::Epoch);
        assert_eq!(auto_strategy(&numeric).expect("auto"), Strategy::Zero);
        // `auto` never guesses for a type it cannot read as a number or a date.
        let uuid = column(
            "ref",
            ColumnType::Opaque {
                pg_type: "UUID".to_owned(),
            },
        );
        assert!(matches!(
            auto_strategy(&uuid),
            Err(ScrubError::NoAutoStrategy { .. })
        ));
        let blob = column("raw", ColumnType::Bytes);
        assert!(replacement_expr(Strategy::Zero, &blob, "t").is_err());
    }

    #[test]
    fn a_text_column_can_take_an_explicit_uuid_zero_or_epoch() {
        let text = column("external_ref", ColumnType::Text);
        let uuid = replacement_expr(Strategy::Uuid, &text, "tok").expect("uuid");
        assert!(uuid.contains("substr(tok, 1, 8)"), "{uuid}");
        assert_eq!(
            replacement_expr(Strategy::Zero, &text, "t").expect("zero"),
            "0"
        );
        assert_eq!(
            replacement_expr(Strategy::Epoch, &text, "t").expect("epoch"),
            "'1970-01-01 00:00:00'"
        );
        assert_eq!(auto_strategy(&text).expect("auto"), Strategy::Redact);
    }

    #[test]
    fn row_key_uses_the_key_columns_or_rowid() {
        let mut table = Table::new("t", Backend::Sqlite);
        let mut facts = DatabaseFacts::default();
        assert_eq!(row_key_expr(&table, &facts), "CAST(rowid AS TEXT)");
        table.primary_key = vec!["id".to_owned()];
        assert_eq!(row_key_expr(&table, &facts), "quote(\"id\")");
        table.primary_key = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            row_key_expr(&table, &facts),
            "quote(\"a\") || ',' || quote(\"b\")"
        );
        // A rowid table can hold several NULL keys. They fall back to rowid.
        facts.rowid_tables.insert("t".to_owned());
        let key = row_key_expr(&table, &facts);
        assert!(
            key.starts_with("CASE WHEN \"a\" IS NULL OR \"b\" IS NULL THEN 'rowid:' || rowid"),
            "{key}"
        );
        assert!(
            key.contains("ELSE 'key:' || "),
            "both branches are prefixed: {key}"
        );
    }

    #[test]
    fn without_rowid_tables_are_not_rowid_tables() {
        assert!(is_without_rowid(
            "CREATE TABLE t (a TEXT PRIMARY KEY) WITHOUT ROWID"
        ));
        assert!(!is_without_rowid("CREATE TABLE t (a TEXT PRIMARY KEY)"));
        assert!(!is_without_rowid("CREATE TABLE t (without_rowid TEXT)"));
    }

    #[test]
    fn unique_tokens_are_full_width_and_plain_tokens_are_trimmed() {
        let unique = token_expr_from("k", "email", true);
        assert!(!unique.starts_with("substr("), "{unique}");
        let plain = token_expr_from("k", "email", false);
        assert!(
            plain.starts_with("substr(") && plain.ends_with("1, 32)"),
            "{plain}"
        );
    }

    #[test]
    fn the_registered_function_matches_a_known_sha256() {
        let mut conn = SqliteConnection::establish(":memory:").expect("memory db");
        autumn_sha256_utils::register_impl(&mut conn, |v: String| {
            hex::encode(Sha256::digest(v.as_bytes()))
        })
        .expect("register");
        let row: One = sql_query("SELECT autumn_sha256('abc') AS v")
            .get_result(&mut conn)
            .expect("call");
        assert_eq!(
            row.v,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_probe_reads_keys_uniques_checks_triggers_and_generated_columns() {
        let mut conn = SqliteConnection::establish(":memory:").expect("memory db");
        conn.batch_execute(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, code TEXT UNIQUE); \
             CREATE TABLE child (id INTEGER PRIMARY KEY, \
                 parent_id INTEGER REFERENCES parent, \
                 tier TEXT CHECK (tier IN ('a','b')), \
                 note TEXT, \
                 shout TEXT GENERATED ALWAYS AS (upper(note)) VIRTUAL); \
             CREATE UNIQUE INDEX one_open ON child (note) WHERE tier = 'a'; \
             CREATE TABLE audit (v TEXT); \
             CREATE TRIGGER t AFTER DELETE ON child BEGIN INSERT INTO audit VALUES (OLD.note); END;",
        )
        .expect("schema");
        let probed = probe(&mut conn, &super::super::ScrubConfig::default()).expect("probe");
        let f = probed.facts;
        let pair = |t: &str, c: &str| (t.to_owned(), c.to_owned());
        assert_eq!(f.dialect, Dialect::Sqlite);
        assert!(f.foreign_key_columns.contains(&pair("child", "parent_id")));
        assert!(
            f.foreign_key_columns.contains(&pair("parent", "id")),
            "implicit parent key"
        );
        assert!(f.unique_columns.contains(&pair("parent", "code")));
        assert!(f.unique_columns.contains(&pair("child", "note")));
        assert!(
            f.unique_expression_columns.contains(&pair("child", "tier")),
            "a partial index predicate reads tier"
        );
        assert!(f.checked_columns.contains(&pair("child", "tier")));
        assert!(!f.checked_columns.contains(&pair("child", "note")));
        assert!(f.generated_columns.contains(&pair("child", "shout")));
        assert!(f.triggered_tables.contains("child"));
        assert!(f.delete_triggered_tables.contains("child"));
        assert!(!f.triggered_tables.contains("audit"));
        assert!(probed.virtual_tables.is_empty());
    }

    #[test]
    fn the_probe_reports_a_virtual_table() {
        let mut conn = SqliteConnection::establish(":memory:").expect("memory db");
        conn.batch_execute("CREATE VIRTUAL TABLE docs USING fts5(body);")
            .expect("fts5");
        let probed = probe(&mut conn, &super::super::ScrubConfig::default()).expect("probe");
        assert_eq!(probed.virtual_tables, ["docs"]);
    }

    #[test]
    fn mixed_backends_and_sample_are_refused_before_any_write() {
        let sqlite = ("control".to_owned(), "sqlite://app.db".to_owned());
        let pg = ("shard:a".to_owned(), "postgres://u@h/db".to_owned());
        let args = ScrubArgs::default();
        assert!(!check_targets(&args, std::slice::from_ref(&pg)).expect("pg only"));
        assert!(check_targets(&args, std::slice::from_ref(&sqlite)).expect("sqlite only"));
        assert!(matches!(
            check_targets(&args, &[sqlite.clone(), pg]),
            Err(ScrubError::MixedBackends)
        ));
        let sampled = ScrubArgs {
            sample: vec!["users=1".to_owned()],
            ..ScrubArgs::default()
        };
        assert!(matches!(
            check_targets(&sampled, &[sqlite]),
            Err(ScrubError::UnsupportedOnSqlite { .. })
        ));
    }

    #[test]
    fn a_schema_change_after_the_plan_rolls_the_run_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let url = format!("sqlite://{}", path.display());
        let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open");
        conn.batch_execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT); \
             INSERT INTO users (email) VALUES ('keep@me.example');",
        )
        .expect("seed");
        let config = super::super::ScrubConfig::default();
        let planned = probe(&mut conn, &config).expect("probe").facts;
        // A column appears after the plan was made.
        conn.batch_execute("ALTER TABLE users ADD COLUMN ssn TEXT;")
            .expect("alter");
        let mut plan = ScrubPlan::default();
        plan.tables.push(super::super::TablePlan {
            table: "users".to_owned(),
            columns: Vec::new(),
            sql: Some("UPDATE \"users\" SET \"email\" = 'gone'".to_owned()),
            row_key: "CAST(rowid AS TEXT)".to_owned(),
            encrypted: Vec::new(),
        });
        let result = apply("control", &url, &plan, &[], &planned, &config);
        assert!(
            matches!(&result, Err(ScrubError::SchemaChanged { entries }) if entries.iter().any(|e| e.contains("ssn"))),
            "{result:?}"
        );
        let kept: One = sql_query("SELECT email AS v FROM users")
            .get_result(&mut conn)
            .expect("read");
        assert_eq!(kept.v, "keep@me.example", "nothing was written");
    }

    #[test]
    fn a_cascade_added_after_the_plan_is_refused_under_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let url = format!("sqlite://{}", path.display());
        let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open");
        conn.batch_execute(
            "CREATE TABLE autumn_sync_rows (id INTEGER PRIMARY KEY, payload TEXT); \
             INSERT INTO autumn_sync_rows (payload) VALUES ('x'); \
             CREATE TABLE refs (id INTEGER PRIMARY KEY, \
                 sync_id INTEGER REFERENCES autumn_sync_rows(id) ON DELETE CASCADE); \
             INSERT INTO refs (sync_id) VALUES (1);",
        )
        .expect("seed");
        let config = super::super::ScrubConfig::default();
        let facts = probe(&mut conn, &config).expect("probe").facts;
        let purges = vec![(
            "autumn_sync_rows".to_owned(),
            "DELETE FROM \"autumn_sync_rows\"".to_owned(),
        )];
        // `apply` is the locked path. It must refuse without a pass-1 check.
        let result = apply(
            "control",
            &url,
            &ScrubPlan::default(),
            &purges,
            &facts,
            &config,
        );
        assert!(
            matches!(&result, Err(ScrubError::UnsupportedOnSqlite { detail, .. }) if detail.contains("refs -> autumn_sync_rows")),
            "{result:?}"
        );
        let left: CountRow = sql_query("SELECT count(*) AS n FROM refs")
            .get_result(&mut conn)
            .expect("count");
        assert_eq!(left.n, 1, "nothing was deleted");
    }
}
