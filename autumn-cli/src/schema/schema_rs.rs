//! Derive `src/schema.rs` from the models (issue #1975, Decision 1).
//!
//! `schema diff --write-migration` calls [`sync_for_plan`]. The sync:
//!
//! - writes a `diesel::table!` block for each managed model;
//! - keeps a block that already has the same columns, types and key;
//! - removes the block of a dropped managed table;
//! - gives the block of a renamed table the new name, and keeps its attributes;
//! - updates `joinable!` and `allow_tables_to_appear_in_same_query!`;
//! - never changes the block of an unmanaged table.
//!
//! The sync does not write a block in these conditions. It reports the table
//! as skipped:
//!
//! - the parser skipped a field of the model (for example an enum field);
//! - a name is not a Rust identifier, or a column type has no diesel type;
//! - the block has attributes (for example `#[sql_name]`) and does not match;
//! - the scanner cannot read the block (for example `posts {` with no key).
//!
//! `schema doctor` uses [`check_tables`] for its `schema-rs-drift` row.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autumn_schema_core::{Backend, Column, ColumnType, Table};

use super::diff::{MigrationPlan, SchemaChange};
use super::parse::ParsedSchema;
use crate::generate::introspect::schema_block_range;

/// The path of the diesel schema file, from the project root.
pub const SCHEMA_RS_PATH: &str = "src/schema.rs";

/// The result of a sync.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchemaRsSync {
    /// The new file text.
    pub text: String,
    /// Tables whose block the sync added or replaced.
    pub written: Vec<String>,
    /// Tables whose block the sync removed.
    pub removed: Vec<String>,
    /// `(old, new)` for each block that the sync gave a new table name.
    pub renamed: Vec<(String, String)>,
    /// Tables the sync did not write, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// The result of [`check_tables`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchemaRsCheck {
    /// Managed tables whose block is missing or stale.
    pub stale: Vec<String>,
    /// Managed tables that the check cannot compare, with the reason.
    pub unchecked: Vec<(String, String)>,
}

/// Render the `diesel::table!` block for `table`.
///
/// # Errors
///
/// Returns the reason when the block cannot be rendered.
pub fn render_block(table: &Table) -> Result<String, String> {
    let name = ident_token(&table.name)
        .ok_or_else(|| format!("the table name `{}` is not a Rust identifier", table.name))?;
    if table.primary_key.is_empty() {
        return Err("the table has no primary key".to_owned());
    }
    let pk = table
        .primary_key
        .iter()
        .map(|c| column_token(c))
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = format!("diesel::table! {{\n    {name} ({}) {{\n", pk.join(", "));
    for column in &table.columns {
        let _ = writeln!(
            out,
            "        {} -> {},",
            column_token(&column.name)?,
            diesel_type(column, table.backend)?
        );
    }
    out.push_str("    }\n}\n");
    Ok(out)
}

/// Update `existing` (the `src/schema.rs` text). First, apply the table drops
/// and renames of `plan`. Then write the managed tables of `desired`.
#[must_use]
pub fn sync_for_plan(existing: &str, desired: &ParsedSchema, plan: &MigrationPlan) -> SchemaRsSync {
    sync(existing, desired, &plan_edits(plan))
}

/// Compare the blocks in `existing` with the managed tables of `desired`.
#[must_use]
pub fn check_tables(existing: &str, desired: &ParsedSchema) -> SchemaRsCheck {
    let sync = sync(existing, desired, &PlanEdits::default());
    SchemaRsCheck {
        stale: sync.written,
        unchecked: sync.skipped,
    }
}

/// The edits of a plan that `src/schema.rs` follows.
#[derive(Default)]
struct PlanEdits {
    /// `(old, new)` table names. `None` is a drop.
    tables: Vec<(String, Option<String>)>,
    /// `(table, old column, new column)`.
    columns: Vec<(String, String, String)>,
}

/// The table drops and renames, and the column renames, of `plan`.
fn plan_edits(plan: &MigrationPlan) -> PlanEdits {
    let mut edits = PlanEdits::default();
    for change in &plan.changes {
        match change {
            SchemaChange::DropTable(table) => edits.tables.push((table.name.clone(), None)),
            SchemaChange::RenameTable { from, to } => {
                edits.tables.push((from.clone(), Some(to.clone())));
            }
            SchemaChange::RenameColumn { table, from, to } => {
                edits
                    .columns
                    .push((table.clone(), from.clone(), to.clone()));
            }
            _ => {}
        }
    }
    edits
}

/// Apply the table and column edits, then write the managed tables of
/// `desired`. Last, remove each `joinable!` that names a column that its
/// managed table no longer has.
fn sync(existing: &str, desired: &ParsedSchema, edits: &PlanEdits) -> SchemaRsSync {
    let mut out = SchemaRsSync {
        text: existing.to_owned(),
        ..SchemaRsSync::default()
    };
    let held = apply_removals(&mut out, &edits.tables);
    for (table, from, to) in &edits.columns {
        let to = ident_token(to).unwrap_or_else(|| to.clone());
        out.text = edit_joinables(&out.text, |left, right, column| {
            if unraw(left) == table && unraw(column) == from {
                JoinEdit::Replace(left.to_owned(), right.to_owned(), to.clone())
            } else {
                JoinEdit::Keep
            }
        });
    }
    // Managed tables whose block now has the model's columns.
    let mut current: Vec<&Table> = Vec::new();

    let blocked: BTreeSet<&str> = desired
        .diagnostics
        .iter()
        .map(|d| d.table.as_str())
        .collect();
    for table in desired
        .tables
        .iter()
        .filter(|t| t.managed && !held.contains(&t.name))
    {
        if blocked.contains(table.name.as_str()) {
            out.skipped.push((
                table.name.clone(),
                "the parser skipped a field of the model".to_owned(),
            ));
            continue;
        }
        let block = match render_block(table) {
            Ok(block) => block,
            Err(reason) => {
                out.skipped.push((table.name.clone(), reason));
                continue;
            }
        };
        let token = ident_token(&table.name).unwrap_or_else(|| table.name.clone());
        match schema_block_range(&out.text, &token) {
            Some((start, end)) => {
                let shape = block_shape(&out.text[start..end]);
                if shape.as_ref().is_some_and(|s| s.matches(table)) {
                    current.push(table);
                    continue;
                }
                if shape.is_none_or(|s| s.has_attrs) {
                    out.skipped.push((
                        table.name.clone(),
                        "the block has attributes or an unknown shape; edit it by hand".to_owned(),
                    ));
                    continue;
                }
                out.text = format!(
                    "{}{}{}",
                    &out.text[..start],
                    block.trim_end(),
                    &out.text[end..]
                );
            }
            // A block that the scanner cannot find, such as `posts {` with no
            // key: do not add a second block for the same table.
            None if has_header(&out.text, &token) => {
                out.skipped.push((
                    table.name.clone(),
                    "the block has an unknown shape; edit it by hand".to_owned(),
                ));
                continue;
            }
            None if out.text.trim().is_empty() => out.text = block,
            None => out.text = format!("{}\n\n{block}", out.text.trim_end()),
        }
        out.written.push(table.name.clone());
        current.push(table);
    }
    out.text = edit_joinables(&out.text, |left, _, column| {
        let stale = current
            .iter()
            .find(|t| t.name == unraw(left))
            .is_some_and(|t| !t.columns.iter().any(|c| c.name == unraw(column)));
        if stale {
            JoinEdit::Remove
        } else {
            JoinEdit::Keep
        }
    });
    out
}

/// Drop or rename the blocks in `removals` (`None` drops, `Some` renames),
/// and update the macros. Returns the new names of the renames it did not
/// do. The table loop does not write those, so the file never gets a second
/// block for one table.
fn apply_removals(
    out: &mut SchemaRsSync,
    removals: &[(String, Option<String>)],
) -> BTreeSet<String> {
    let mut held = BTreeSet::new();
    for (old, new) in removals {
        let new_token = match new.as_deref().map(|n| (n, ident_token(n))) {
            Some((n, None)) => {
                out.skipped.push((
                    old.clone(),
                    format!("the new name `{n}` is not a Rust identifier"),
                ));
                held.insert(n.to_owned());
                continue;
            }
            Some((_, token)) => token,
            None => None,
        };
        // A rename keeps the block and its attributes under the new name. The
        // table loop of `sync` then updates the columns, if it can.
        let text = match &new_token {
            Some(token) => rename_header(&out.text, old, token),
            None => remove_block(&out.text, old),
        };
        match text {
            Some(text) => {
                out.text = text;
                match new {
                    Some(new) => out.renamed.push((old.clone(), new.clone())),
                    None => out.removed.push(old.clone()),
                }
            }
            None if has_header(&out.text, &ident_token(old).unwrap_or_else(|| old.clone())) => {
                out.skipped.push((
                    old.clone(),
                    "the block has an unknown shape; edit it by hand".to_owned(),
                ));
                if let Some(new) = new {
                    held.insert(new.clone());
                }
                continue;
            }
            None => {}
        }
        out.text = retarget_macros(&out.text, old, new_token.as_deref());
    }
    held
}

/// The diesel type of `column`, with `Nullable<...>` for a nullable column.
fn diesel_type(column: &Column, backend: Backend) -> Result<String, String> {
    if let ColumnType::Opaque { pg_type } = &column.ty {
        return Err(format!(
            "the column `{}` has the database type `{pg_type}`, which has no diesel type",
            column.name
        ));
    }
    let ty = column.ty.diesel_type(backend);
    Ok(if column.nullable {
        format!("Nullable<{ty}>")
    } else {
        ty.to_owned()
    })
}

/// [`ident_token`], or an error that names the column.
fn column_token(name: &str) -> Result<String, String> {
    ident_token(name).ok_or_else(|| format!("the column name `{name}` is not a Rust identifier"))
}

/// `name` as a Rust identifier token: as is, or as a raw identifier
/// (`r#type`) for a keyword. `None` when neither form is valid.
fn ident_token(name: &str) -> Option<String> {
    if syn::parse_str::<syn::Ident>(name).is_ok() {
        return Some(name.to_owned());
    }
    let raw = format!("r#{name}");
    syn::parse_str::<syn::Ident>(&raw).is_ok().then_some(raw)
}

/// The name without a raw `r#` prefix.
fn unraw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

/// The shape of a `diesel::table!` block: key, column types and attributes.
struct Shape {
    pk: Vec<String>,
    columns: BTreeMap<String, String>,
    has_attrs: bool,
}

impl Shape {
    /// True when the block declares the same key and columns as `table`.
    fn matches(&self, table: &Table) -> bool {
        let pk: Vec<&str> = table.primary_key.iter().map(String::as_str).collect();
        let columns: Option<BTreeMap<String, String>> = table
            .columns
            .iter()
            .map(|c| {
                diesel_type(c, table.backend)
                    .ok()
                    .map(|ty| (c.name.clone(), normalize_type(&ty)))
            })
            .collect();
        self.pk.iter().map(String::as_str).eq(pk) && columns.is_some_and(|c| c == self.columns)
    }
}

/// Read the shape of one `diesel::table!` block. `None` when the block has no
/// `name (key) {` header.
fn block_shape(block: &str) -> Option<Shape> {
    let mut has_attrs = false;
    let mut lines = block.lines().skip(1).map(str::trim);
    let header = lines.find(|line| {
        has_attrs |= line.starts_with("#[");
        line.contains(" (") && line.ends_with('{')
    })?;
    let (_, keys) = header.split_once('(')?;
    let (keys, _) = keys.split_once(')')?;
    let pk = keys
        .split(',')
        .map(|k| unraw(k.trim()).to_owned())
        .filter(|k| !k.is_empty())
        .collect();
    let mut columns = BTreeMap::new();
    for line in lines.take_while(|line| *line != "}") {
        if line.starts_with("#[") {
            has_attrs = true;
        } else if let Some((name, ty)) = line.split_once("->") {
            columns.insert(
                unraw(name.trim()).to_owned(),
                normalize_type(ty.trim().trim_end_matches(',')),
            );
        }
    }
    Some(Shape {
        pk,
        columns,
        has_attrs,
    })
}

/// A diesel type with paths, spaces and aliases removed:
/// `diesel::sql_types::Nullable<BigInt>` becomes `Nullable<Int8>`.
fn normalize_type(ty: &str) -> String {
    let mut out = String::new();
    let mut atom = String::new();
    let flush = |atom: &mut String, out: &mut String| {
        let last = atom.rsplit("::").next().unwrap_or_default();
        out.push_str(match last {
            "BigInt" => "Int8",
            "Integer" => "Int4",
            "SmallInt" => "Int2",
            "Float" => "Float4",
            "Double" => "Float8",
            "VarChar" | "Varchar" => "Text",
            "Bytea" | "Blob" => "Binary",
            "Decimal" => "Numeric",
            other => other,
        });
        atom.clear();
    };
    for c in ty.chars().filter(|c| !c.is_whitespace()) {
        if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
            atom.push(c);
        } else {
            flush(&mut atom, &mut out);
            out.push(c);
        }
    }
    flush(&mut atom, &mut out);
    out
}

/// Remove the block of `table` and one blank line next to it. `None` when the
/// text has no block for `table`.
fn remove_block(text: &str, table: &str) -> Option<String> {
    let token = ident_token(table).unwrap_or_else(|| table.to_owned());
    let (start, end) = schema_block_range(text, &token)?;
    let prefix = &text[..start];
    let rest = &text[end..];
    let mut suffix = rest.strip_prefix('\n').unwrap_or(rest);
    if (prefix.is_empty() || prefix.ends_with("\n\n")) && suffix.starts_with('\n') {
        suffix = &suffix[1..];
    }
    if suffix.is_empty() {
        let prefix = prefix.trim_end();
        return Some(if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}\n")
        });
    }
    Some(format!("{prefix}{suffix}"))
}

/// Give the block of `old` the table name `new` (an identifier token).
/// `None` when the text has no block for `old`.
fn rename_header(text: &str, old: &str, new: &str) -> Option<String> {
    let token = ident_token(old).unwrap_or_else(|| old.to_owned());
    let (start, end) = schema_block_range(text, &token)?;
    let block = &text[start..end];
    // The header line, found with the test `schema_block_range` uses. A doc
    // comment that names the table does not match.
    let needle = format!("{token} (");
    let mut offset = 0;
    let mut at = None;
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&needle) {
            at = Some(offset + line.len() - trimmed.len());
            break;
        }
        offset += line.len();
    }
    let at = at?;
    Some(format!(
        "{}{}{new}{}{}",
        &text[..start],
        &block[..at],
        &block[at + token.len()..],
        &text[end..]
    ))
}

/// True when a line of `text` starts with `token` and then `(` or `{`: a
/// `table!` header in any spacing.
fn has_header(text: &str, token: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(token)
            .is_some_and(|rest| rest.trim_start().starts_with(['(', '{']))
    })
}

/// Update the `joinable!` and `allow_tables_to_appear_in_same_query!` macros
/// for a renamed (`Some(new)`) or dropped (`None`) table `old`. The function
/// removes each `joinable!` of a dropped table. It removes an allow list that
/// has fewer than two tables. A rewritten call loses its comments.
fn retarget_macros(text: &str, old: &str, new: Option<&str>) -> String {
    let mut text = edit_joinables(text, |left, right, column| {
        if unraw(left) != old && unraw(right) != old {
            return JoinEdit::Keep;
        }
        let Some(new) = new else {
            return JoinEdit::Remove;
        };
        let pick = |name: &str| if unraw(name) == old { new } else { name }.to_owned();
        JoinEdit::Replace(pick(left), pick(right), column.to_owned())
    });
    for (start, open, close, end) in macro_calls(&text, ALLOW).into_iter().rev() {
        let inner = strip_comments(&text[open + 1..close]);
        let names: Vec<&str> = inner
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .collect();
        if !names.iter().any(|n| unraw(n) == old) {
            continue;
        }
        let mut kept: Vec<&str> = Vec::new();
        for name in names
            .into_iter()
            .filter_map(|n| if unraw(n) == old { new } else { Some(n) })
        {
            // A rename to a listed name must not list it twice.
            if !kept.iter().any(|k| unraw(k) == unraw(name)) {
                kept.push(name);
            }
        }
        if kept.len() < 2 {
            text.replace_range(start..end, "");
        } else {
            text.replace_range(open + 1..close, &kept.join(", "));
        }
    }
    text
}

/// What [`edit_joinables`] does with one `joinable!` call.
enum JoinEdit {
    Keep,
    Remove,
    /// The new child table, parent table and column.
    Replace(String, String, String),
}

/// Apply `edit` to each `joinable!(child -> parent (column))` call in `text`.
/// A rewritten call loses its comments.
fn edit_joinables(text: &str, edit: impl Fn(&str, &str, &str) -> JoinEdit) -> String {
    let mut text = text.to_owned();
    for (start, open, close, end) in macro_calls(&text, "joinable!").into_iter().rev() {
        let inner = strip_comments(&text[open + 1..close]);
        let Some((left, right)) = inner.split_once("->") else {
            continue;
        };
        let Some((right, column)) = right.split_once('(') else {
            continue;
        };
        let column = column.trim().trim_end_matches(')').trim();
        match edit(left.trim(), right.trim(), column) {
            JoinEdit::Keep => {}
            JoinEdit::Remove => text.replace_range(start..end, ""),
            JoinEdit::Replace(left, right, column) => {
                let call = format!("{}({left} -> {right} ({column}))", &text[start..open]);
                text.replace_range(start..=close, &call);
            }
        }
    }
    text
}

/// The diesel macro that lists the tables one query can join.
const ALLOW: &str = "allow_tables_to_appear_in_same_query!";

/// Each call of the macro `name` (for example `joinable!`) in `text`, not in a
/// `//` comment: `(start, open, close, end)`. `start` includes the path,
/// `open` and `close` are the outer parentheses, and `end` includes a `;` and
/// a line end.
fn macro_calls(text: &str, name: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut calls = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(name) {
        let at = from + rel;
        from = at + name.len();
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..at].contains("//") {
            continue;
        }
        let Some(open) = text[from..]
            .find(|c: char| !c.is_whitespace())
            .map(|i| from + i)
            .filter(|&i| text[i..].starts_with('('))
        else {
            continue;
        };
        let Some(close) = matching_paren(text, open) else {
            break;
        };
        let start = text[..at]
            .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            .len();
        let mut end = close + 1;
        if text[end..].starts_with(';') {
            end += 1;
        }
        if text[end..].starts_with('\n') {
            end += 1;
        }
        calls.push((start, open, close, end));
        from = end;
    }
    calls
}

/// The position of the `)` that closes the `(` at `open`. Skips `//`
/// comments.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_comment = false;
    let mut prev = '\0';
    for (i, c) in text[open..].char_indices() {
        if in_comment {
            in_comment = c != '\n';
        } else if c == '/' && prev == '/' {
            in_comment = true;
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(open + i);
            }
        }
        prev = c;
    }
    None
}

/// `text` without its `//` comments.
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::schema::parse::{SchemaDiagnostic, parse_model_source};

    fn posts(backend: Backend) -> Table {
        let mut t = Table::new("posts", backend);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        t.columns.push(id);
        t.columns.push(Column::new("title", ColumnType::Text));
        let mut body = Column::new("body", ColumnType::Text);
        body.nullable = true;
        t.columns.push(body);
        t.columns
            .push(Column::new("created_at", ColumnType::Timestamp));
        t.primary_key = vec!["id".to_owned()];
        t
    }

    fn parsed(tables: Vec<Table>) -> ParsedSchema {
        ParsedSchema::from_tables(tables)
    }

    fn plan(changes: Vec<SchemaChange>) -> MigrationPlan {
        MigrationPlan {
            backend: Backend::Postgres,
            changes,
        }
    }

    const POSTS_BLOCK: &str = "diesel::table! {
    posts (id) {
        id -> Int8,
        title -> Text,
        body -> Nullable<Text>,
        created_at -> Timestamp,
    }
}
";

    // -- render_block --------------------------------------------------------

    #[test]
    fn render_block_postgres() {
        assert_eq!(
            render_block(&posts(Backend::Postgres)).unwrap(),
            POSTS_BLOCK
        );
    }

    #[test]
    fn render_block_sqlite_uses_sqlite_diesel_types() {
        let mut t = Table::new("files", Backend::Sqlite);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        t.columns.push(id);
        t.columns
            .push(Column::new("seen_at", ColumnType::TimestampTz));
        t.columns.push(Column::new("data", ColumnType::Bytes));
        t.columns.push(Column::new("meta", ColumnType::Json));
        t.columns.push(Column::new("token", ColumnType::Uuid));
        t.primary_key = vec!["id".to_owned()];
        let block = render_block(&t).unwrap();
        assert!(block.contains("seen_at -> TimestamptzSqlite,"), "{block}");
        assert!(block.contains("data -> Binary,"), "{block}");
        assert!(block.contains("meta -> Json,"), "{block}");
        assert!(block.contains("token -> Text,"), "{block}");
    }

    #[test]
    fn render_block_writes_a_keyword_column_as_a_raw_ident() {
        let mut t = posts(Backend::Postgres);
        t.columns.push(Column::new("type", ColumnType::Text));
        let block = render_block(&t).unwrap();
        assert!(block.contains("        r#type -> Text,\n"), "{block}");
    }

    #[test]
    fn render_block_refuses_what_diesel_cannot_declare() {
        let mut opaque = posts(Backend::Postgres);
        opaque.columns.push(Column::new(
            "addr",
            ColumnType::Opaque {
                pg_type: "inet".to_owned(),
            },
        ));
        assert!(render_block(&opaque).unwrap_err().contains("addr"));

        let mut no_pk = posts(Backend::Postgres);
        no_pk.primary_key.clear();
        assert!(render_block(&no_pk).unwrap_err().contains("primary key"));

        let mut bad_name = posts(Backend::Postgres);
        bad_name.columns.push(Column::new("self", ColumnType::Text));
        assert!(render_block(&bad_name).unwrap_err().contains("self"));

        let qualified = Table::new("auth.users", Backend::Postgres);
        assert!(render_block(&qualified).is_err());
    }

    // -- sync ----------------------------------------------------------------

    #[test]
    fn sync_creates_the_first_block_in_an_empty_file() {
        let out = sync_for_plan("", &parsed(vec![posts(Backend::Postgres)]), &plan(vec![]));
        assert_eq!(out.text, POSTS_BLOCK);
        assert_eq!(out.written, vec!["posts".to_owned()]);
    }

    #[test]
    fn sync_appends_a_missing_block_and_keeps_the_rest() {
        let existing =
            "// header\n\ndiesel::table! {\n    users (id) {\n        id -> Int8,\n    }\n}\n";
        let out = sync_for_plan(
            existing,
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![]),
        );
        assert!(out.text.starts_with(existing.trim_end()), "{}", out.text);
        assert!(out.text.ends_with(POSTS_BLOCK), "{}", out.text);
    }

    #[test]
    fn sync_keeps_an_equivalent_block_byte_for_byte() {
        // Aliases, paths, other column order and a doc comment: same shape.
        let existing = "diesel::table! {
    /// Blog posts.
    posts (id) {
        title -> diesel::sql_types::Text,
        id -> BigInt,
        created_at -> Timestamp,
        body -> Nullable<VarChar>,
    }
}
";
        let out = sync_for_plan(
            existing,
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![]),
        );
        assert_eq!(out.text, existing);
        assert!(out.written.is_empty(), "{out:?}");
    }

    #[test]
    fn sync_replaces_a_stale_block_in_place() {
        let stale = "diesel::table! {\n    posts (id) {\n        id -> Int8,\n        title -> Text,\n    }\n}\n";
        let existing =
            format!("{stale}\ndiesel::allow_tables_to_appear_in_same_query!(posts, users);\n");
        let out = sync_for_plan(
            &existing,
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![]),
        );
        assert_eq!(
            out.text,
            format!(
                "{POSTS_BLOCK}\ndiesel::allow_tables_to_appear_in_same_query!(posts, users);\n"
            )
        );
        assert_eq!(out.written, vec!["posts".to_owned()]);
    }

    #[test]
    fn sync_never_touches_an_unmanaged_table() {
        let stale = "diesel::table! {\n    posts (id) {\n        id -> Int8,\n    }\n}\n";
        let mut t = posts(Backend::Postgres);
        t.managed = false;
        let out = sync_for_plan(stale, &parsed(vec![t]), &plan(vec![]));
        assert_eq!(out.text, stale);
        assert!(out.written.is_empty());
    }

    #[test]
    fn sync_skips_a_table_with_a_skipped_field() {
        let mut desired = parsed(vec![posts(Backend::Postgres)]);
        desired.diagnostics.push(SchemaDiagnostic {
            model: "Post".to_owned(),
            table: "posts".to_owned(),
            field: "status".to_owned(),
            rust_type: "PostStatus".to_owned(),
            message: "unsupported".to_owned(),
        });
        let out = sync_for_plan("", &desired, &plan(vec![]));
        assert_eq!(out.text, "");
        assert_eq!(out.skipped.len(), 1, "{out:?}");
        assert_eq!(out.skipped[0].0, "posts");
    }

    #[test]
    fn sync_skips_a_stale_block_with_attributes() {
        let tuned = "diesel::table! {\n    posts (id) {\n        id -> Int8,\n        #[sql_name = \"Title\"]\n        title -> Text,\n    }\n}\n";
        let out = sync_for_plan(
            tuned,
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![]),
        );
        assert_eq!(out.text, tuned);
        assert!(out.skipped[0].1.contains("attribute"), "{out:?}");
    }

    #[test]
    fn sync_removes_a_dropped_table_and_its_macro_mentions() {
        let existing = "diesel::table! {
    comments (id) {
        id -> Int8,
        post_id -> Int8,
    }
}

diesel::table! {
    users (id) {
        id -> Int8,
    }
}

diesel::joinable!(comments -> users (post_id));
diesel::allow_tables_to_appear_in_same_query!(comments, users, posts,);
";
        let out = sync_for_plan(
            existing,
            &parsed(vec![]),
            &plan(vec![SchemaChange::DropTable(Table::new(
                "comments",
                Backend::Postgres,
            ))]),
        );
        assert_eq!(
            out.text,
            "diesel::table! {
    users (id) {
        id -> Int8,
    }
}

diesel::allow_tables_to_appear_in_same_query!(users, posts);
"
        );
        assert_eq!(out.removed, vec!["comments".to_owned()]);
    }

    #[test]
    fn sync_drops_an_allow_tables_list_left_with_one_table() {
        let existing = "diesel::table! {\n    a (id) {\n        id -> Int8,\n    }\n}\n\ndiesel::allow_tables_to_appear_in_same_query!(\n    a,\n    b,\n);\n";
        let out = sync_for_plan(
            existing,
            &parsed(vec![]),
            &plan(vec![SchemaChange::DropTable(Table::new(
                "a",
                Backend::Postgres,
            ))]),
        );
        assert!(!out.text.contains("allow_tables"), "{}", out.text);
        assert!(!out.text.contains("a (id)"), "{}", out.text);
    }

    #[test]
    fn sync_renames_a_table_block_and_its_macro_mentions() {
        let existing = "diesel::table! {
    articles (id) {
        id -> Int8,
        title -> Text,
    }
}

diesel::joinable!(users -> articles (article_id));
diesel::allow_tables_to_appear_in_same_query!(articles, users);
";
        let out = sync_for_plan(
            existing,
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![SchemaChange::RenameTable {
                from: "articles".to_owned(),
                to: "posts".to_owned(),
            }]),
        );
        assert!(!out.text.contains("articles"), "{}", out.text);
        assert!(out.text.contains(POSTS_BLOCK), "{}", out.text);
        assert!(
            out.text
                .contains("diesel::joinable!(users -> posts (article_id));"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("diesel::allow_tables_to_appear_in_same_query!(posts, users);"),
            "{}",
            out.text
        );
        assert_eq!(
            out.renamed,
            vec![("articles".to_owned(), "posts".to_owned())]
        );
    }

    #[test]
    fn a_macro_mention_matches_whole_identifiers_only() {
        let existing = "diesel::allow_tables_to_appear_in_same_query!(posts, posts_tags);\n";
        let out = sync_for_plan(
            existing,
            &parsed(vec![]),
            &plan(vec![SchemaChange::DropTable(Table::new(
                "posts",
                Backend::Postgres,
            ))]),
        );
        assert!(!out.text.contains("allow_tables"), "{}", out.text);
        let existing = "diesel::joinable!(posts_tags -> tags (tag_id));\n";
        let out = sync_for_plan(
            existing,
            &parsed(vec![]),
            &plan(vec![SchemaChange::DropTable(Table::new(
                "posts",
                Backend::Postgres,
            ))]),
        );
        assert_eq!(out.text, existing);
    }

    #[test]
    fn check_tables_lists_missing_and_changed_managed_blocks() {
        let mut users = Table::new("users", Backend::Postgres);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        users.columns.push(id);
        users.primary_key = vec!["id".to_owned()];
        let mut legacy = users.clone();
        legacy.name = "legacy".to_owned();
        legacy.managed = false;
        let existing = format!(
            "{POSTS_BLOCK}\ndiesel::table! {{\n    users (id) {{\n        id -> Int4,\n    }}\n}}\n"
        );
        let desired = parsed(vec![posts(Backend::Postgres), users, legacy]);
        assert_eq!(
            check_tables(&existing, &desired).stale,
            vec!["users".to_owned()]
        );
        assert_eq!(
            check_tables("", &desired).stale,
            vec!["posts".to_owned(), "users".to_owned()]
        );
    }

    // -- parity with the generator --------------------------------------------

    /// A model from `autumn generate model`, parsed back, renders a block with
    /// the same shape as the block the generator wrote.
    #[test]
    fn a_generated_model_renders_the_generated_block() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let fields: Vec<String> = [
            "title:String",
            "body:Text",
            "views:i32",
            "score:i64",
            "ratio:f64",
            "weight:f32",
            "draft:bool",
            "payload:Bytea",
            "meta:json",
            "token:Uuid",
            "posted:DateTime",
            "seen:NaiveDateTime",
            "price:decimal",
            "note:Option<String>",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let gen_plan =
            crate::generate::model::plan_model(root.path(), "Post", &fields, "20260101000000")
                .expect("plan model");
        let file = |suffix: &str| {
            gen_plan
                .actions
                .iter()
                .find_map(|a| match a {
                    crate::generate::emit::Action::Create { path, contents }
                    | crate::generate::emit::Action::Modify { path, contents }
                        if path.ends_with(suffix) =>
                    {
                        Some(contents.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {suffix} in plan"))
        };
        let model = file("models/post.rs");
        let schema = file("schema.rs");
        let mut desired = parse_model_source(&model, Backend::Postgres).expect("parse");
        assert!(desired.diagnostics.is_empty(), "{:?}", desired.diagnostics);
        desired.tables[0].managed = true;
        assert_eq!(check_tables(&schema, &desired).stale, Vec::<String>::new());
    }

    /// Each example app's `schema.rs` agrees with its models.
    #[test]
    fn example_apps_schema_rs_matches_their_models() {
        let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples");
        let Ok(entries) = std::fs::read_dir(&examples) else {
            return;
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let app = entry.path();
            let Some(models) = crate::schema::existing_models_path(&app) else {
                continue;
            };
            let Ok(schema) = std::fs::read_to_string(app.join(SCHEMA_RS_PATH)) else {
                continue;
            };
            let Ok(mut desired) =
                crate::schema::parse::parse_models_path(&models, Backend::Postgres)
            else {
                continue;
            };
            // Only tables that have a block; mark them managed to compare.
            desired
                .tables
                .retain(|t| schema_block_range(&schema, &t.name).is_some());
            for t in &mut desired.tables {
                t.managed = true;
                // The parser adds `created_at` when the model omits it, as the
                // generator does. A hand-made example table can omit it.
                let (start, end) = schema_block_range(&schema, &t.name).unwrap();
                if !schema[start..end].contains("created_at") {
                    t.columns.retain(|c| c.name != "created_at");
                }
            }
            checked += desired.tables.len();
            let stale = check_tables(&schema, &desired).stale;
            assert!(stale.is_empty(), "{}: {stale:?}", app.display());
        }
        assert!(checked > 0, "no example table was checked");
    }

    // -- review findings ------------------------------------------------------

    /// A rename keeps the old block, with its attributes, under the new name
    /// when the new block cannot be rendered.
    #[test]
    fn a_rename_keeps_the_block_when_the_new_block_cannot_render() {
        let existing = "diesel::table! {\n    articles (id) {\n        id -> Int8,\n        #[sql_name = \"Status\"]\n        status -> Text,\n    }\n}\n\ndiesel::allow_tables_to_appear_in_same_query!(articles, users);\n";
        let mut desired = parsed(vec![posts(Backend::Postgres)]);
        desired.diagnostics.push(SchemaDiagnostic {
            model: "Post".to_owned(),
            table: "posts".to_owned(),
            field: "status".to_owned(),
            rust_type: "PostStatus".to_owned(),
            message: "unsupported".to_owned(),
        });
        let out = sync_for_plan(
            existing,
            &desired,
            &plan(vec![SchemaChange::RenameTable {
                from: "articles".to_owned(),
                to: "posts".to_owned(),
            }]),
        );
        assert_eq!(out.text, existing.replace("articles", "posts"));
        assert_eq!(
            out.renamed,
            vec![("articles".to_owned(), "posts".to_owned())]
        );
    }

    #[test]
    fn a_multi_line_joinable_is_removed_or_renamed_on_table_names_only() {
        let existing = "diesel::joinable!(\n    comments -> users (user_id)\n);\ndiesel::joinable!(posts -> category (category));\n";
        let drop = plan(vec![SchemaChange::DropTable(Table::new(
            "comments",
            Backend::Postgres,
        ))]);
        let out = sync_for_plan(existing, &parsed(vec![]), &drop);
        assert_eq!(
            out.text,
            "diesel::joinable!(posts -> category (category));\n"
        );

        let rename = plan(vec![SchemaChange::RenameTable {
            from: "category".to_owned(),
            to: "type".to_owned(),
        }]);
        let out = sync_for_plan(existing, &parsed(vec![]), &rename);
        assert!(
            out.text
                .contains("diesel::joinable!(posts -> r#type (category));"),
            "{}",
            out.text
        );
    }

    #[test]
    fn an_allow_list_with_comments_drops_the_right_name() {
        let existing = "diesel::allow_tables_to_appear_in_same_query!(\n    a, // first (old)\n    // legacy\n    b,\n    c,\n);\n";
        let drop = |name: &str| {
            plan(vec![SchemaChange::DropTable(Table::new(
                name,
                Backend::Postgres,
            ))])
        };
        let out = sync_for_plan(existing, &parsed(vec![]), &drop("b"));
        assert_eq!(
            out.text,
            "diesel::allow_tables_to_appear_in_same_query!(a, c);\n"
        );
        let out = sync_for_plan(existing, &parsed(vec![]), &drop("c"));
        assert_eq!(
            out.text,
            "diesel::allow_tables_to_appear_in_same_query!(a, b);\n"
        );
    }

    /// A block that the scanner cannot read is not appended a second time.
    #[test]
    fn an_unreadable_block_is_skipped_not_duplicated() {
        for existing in [
            "diesel::table! {\n    posts {\n        id -> Int8,\n    }\n}\n",
            "diesel::table! {\n    posts(id) {\n        id -> Int8,\n    }\n}\n",
        ] {
            let out = sync_for_plan(
                existing,
                &parsed(vec![posts(Backend::Postgres)]),
                &plan(vec![]),
            );
            assert_eq!(out.text, existing);
            assert_eq!(out.skipped.len(), 1, "{out:?}");
        }
    }

    #[test]
    fn a_table_macro_in_a_comment_is_not_a_block() {
        let head = "// see the diesel::table! docs\nuse x;\n\n";
        let stale = "diesel::table! {\n    posts (id) {\n        id -> Int8,\n    }\n}\n";
        let out = sync_for_plan(
            &format!("{head}{stale}"),
            &parsed(vec![posts(Backend::Postgres)]),
            &plan(vec![]),
        );
        assert_eq!(out.text, format!("{head}{POSTS_BLOCK}"));
    }

    /// A skipped table is unchecked, not stale.
    #[test]
    fn skipped_tables_are_reported_as_unchecked() {
        let mut desired = parsed(vec![posts(Backend::Postgres)]);
        desired.diagnostics.push(SchemaDiagnostic {
            model: "Post".to_owned(),
            table: "posts".to_owned(),
            field: "status".to_owned(),
            rust_type: "PostStatus".to_owned(),
            message: "unsupported".to_owned(),
        });
        let check = check_tables("", &desired);
        assert!(check.stale.is_empty());
        assert_eq!(check.unchecked.len(), 1);
        assert_eq!(check.unchecked[0].0, "posts");
    }

    fn rename(from: &str, to: &str) -> MigrationPlan {
        plan(vec![SchemaChange::RenameTable {
            from: from.to_owned(),
            to: to.to_owned(),
        }])
    }

    /// The rename changes the header line only, not a doc comment.
    #[test]
    fn a_rename_changes_the_header_not_a_doc_comment() {
        let existing = "diesel::table! {\n    /// Legacy articles (v1).\n    articles (id) {\n        id -> Int8,\n    }\n}\n";
        let mut t = posts(Backend::Postgres);
        t.columns.truncate(1);
        let out = sync_for_plan(existing, &parsed(vec![t]), &rename("articles", "posts"));
        assert_eq!(
            out.text,
            "diesel::table! {\n    /// Legacy articles (v1).\n    posts (id) {\n        id -> Int8,\n    }\n}\n"
        );
    }

    /// A block that the scanner cannot read is not renamed, and no second
    /// block is added.
    #[test]
    fn a_rename_of_an_unreadable_block_is_skipped() {
        let existing = "diesel::table! {\n    articles(id) {\n        id -> Int8,\n    }\n}\n";
        let out = sync_for_plan(
            existing,
            &parsed(vec![posts(Backend::Postgres)]),
            &rename("articles", "posts"),
        );
        assert_eq!(out.text, existing);
        assert!(out.skipped.iter().any(|(t, _)| t == "articles"), "{out:?}");
    }

    /// A new name that is not a Rust identifier is not written.
    #[test]
    fn a_rename_to_a_non_identifier_is_skipped() {
        let existing = "diesel::table! {\n    users (id) {\n        id -> Int8,\n    }\n}\n\ndiesel::allow_tables_to_appear_in_same_query!(users, posts);\n";
        let out = sync_for_plan(existing, &parsed(vec![]), &rename("users", "auth.users"));
        assert_eq!(out.text, existing);
        assert!(out.skipped.iter().any(|(t, _)| t == "users"), "{out:?}");
    }

    /// A rename to a name that the allow list already has does not list it
    /// twice.
    #[test]
    fn an_allow_list_never_lists_a_table_twice() {
        let existing = "diesel::allow_tables_to_appear_in_same_query!(articles, posts, users);\n";
        let out = sync_for_plan(existing, &parsed(vec![]), &rename("articles", "posts"));
        assert_eq!(
            out.text,
            "diesel::allow_tables_to_appear_in_same_query!(posts, users);\n"
        );
    }

    fn comments_block(fk: &str) -> String {
        format!(
            "diesel::table! {{\n    comments (id) {{\n        id -> Int8,\n        {fk} -> Int8,\n    }}\n}}\n"
        )
    }

    fn comments(fk: &str) -> Table {
        let mut t = Table::new("comments", Backend::Postgres);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        t.columns.push(id);
        t.columns.push(Column::new(fk, ColumnType::Int64));
        t.primary_key = vec!["id".to_owned()];
        t
    }

    /// A renamed FK column moves its `joinable!` with it.
    #[test]
    fn a_renamed_fk_column_renames_its_joinable() {
        let existing = format!(
            "{}\ndiesel::joinable!(comments -> posts (post_id));\n",
            comments_block("post_id")
        );
        let out = sync_for_plan(
            &existing,
            &parsed(vec![comments("article_id")]),
            &plan(vec![SchemaChange::RenameColumn {
                table: "comments".to_owned(),
                from: "post_id".to_owned(),
                to: "article_id".to_owned(),
            }]),
        );
        assert!(
            out.text
                .contains("diesel::joinable!(comments -> posts (article_id));"),
            "{}",
            out.text
        );
    }

    /// A `joinable!` whose column the managed table no longer has goes.
    #[test]
    fn a_dropped_fk_column_drops_its_joinable() {
        let existing = format!(
            "{}\ndiesel::joinable!(comments -> posts (post_id));\n",
            comments_block("post_id")
        );
        let out = sync_for_plan(
            &existing,
            &parsed(vec![comments("author_id")]),
            &plan(vec![]),
        );
        assert!(!out.text.contains("joinable"), "{}", out.text);
        assert!(out.text.contains("author_id -> Int8"), "{}", out.text);
    }
}
