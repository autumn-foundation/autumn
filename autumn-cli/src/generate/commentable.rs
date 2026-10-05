//! Scaffold support for threaded, polymorphic comments (issue #1367).
//!
//! `autumn generate scaffold post title:string comments:commentable` adds, on
//! top of the ordinary scaffold:
//!
//! 1. `comment_count BIGINT NOT NULL DEFAULT 0` on the scaffolded model's own
//!    table — handled by the ordinary field pipeline, because the DSL token
//!    *is* that column (see [`super::dsl::FieldKind::Commentable`]);
//! 2. `#[commentable(by = User, counter_cache = comment_count)]` on the
//!    generated `#[model]` — emitted by [`super::model`]; and
//! 3. the **shared** `comments` table migration this module owns.
//!
//! # The comments table is shared, so it is emitted at most once
//!
//! That is the whole point of the polymorphic kind: a second commentable model
//! attaches to the same table under a different `commentable_type`. So this
//! module first looks for an existing `*_create_comments` migration in the
//! project and emits nothing when it finds one — which is what makes "add
//! comments to a second model" the DSL token and nothing else.
//!
//! The `CREATE TABLE` is deliberately **not** `IF NOT EXISTS`. A project that
//! already has an unrelated `comments` table (a `Comment` resource scaffolded
//! the ordinary way, say) is a real conflict the author has to resolve, and
//! `IF NOT EXISTS` would turn it into a silent no-op whose only symptom is a
//! `column "commentable_type" does not exist` at request time. The generator
//! refuses that project instead, and tells the user how to correct it (#2283).

use std::collections::HashMap;
use std::path::Path;

use crate::generate::GenerateError;
use crate::generate::emit::Plan;

/// The shared comments table's name. Not configurable from the DSL token: the
/// `#[commentable(table = …)]` attribute is where a project renames it, and a
/// rename there is a migration the author writes anyway.
pub const COMMENTS_TABLE: &str = "comments";

/// The migration directory suffix, used both to name the directory and to
/// detect an existing one.
const MIGRATION_SUFFIX: &str = "_create_comments";

/// The backend-forked column spellings the shared comments table needs.
///
/// The scaffold's *own* migration is already backend-aware (issue #1614), so
/// this one has to be too, or a `SQLite` project would take `comments:commentable`
/// happily and then fail `diesel migration run` on `BIGSERIAL`. Mirrors
/// [`super::auth`]'s `AuthDdl`, which forks the same three spellings for the same
/// reason.
struct CommentsDdl {
    pk: &'static str,
    big_int: &'static str,
    ts: &'static str,
    ts_not_null_default_now: &'static str,
}

impl CommentsDdl {
    const fn for_backend(backend: autumn_web::config::DatabaseBackend) -> Self {
        match backend {
            autumn_web::config::DatabaseBackend::Postgres => Self {
                pk: "BIGSERIAL PRIMARY KEY",
                big_int: "BIGINT",
                ts: "TIMESTAMP",
                ts_not_null_default_now: "TIMESTAMP NOT NULL DEFAULT NOW()",
            },
            autumn_web::config::DatabaseBackend::Sqlite => Self {
                pk: "INTEGER PRIMARY KEY AUTOINCREMENT",
                big_int: "INTEGER",
                ts: "TEXT",
                ts_not_null_default_now: "TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
            },
        }
    }
}

/// `up.sql` creating the polymorphic, threaded comments table for `backend`.
///
/// `commentable_id` deliberately carries **no** `REFERENCES`: a single column
/// cannot reference two tables, which is the known trade-off of the polymorphic
/// pattern. The framework's write path is the referential check instead — it
/// probes and row-locks the parent before inserting — so an unknown parent is a
/// `404`, not a dangling row.
#[must_use]
pub fn up_sql(backend: autumn_web::config::DatabaseBackend) -> String {
    let CommentsDdl {
        pk,
        big_int,
        ts,
        ts_not_null_default_now,
    } = CommentsDdl::for_backend(backend);
    format!(
        "-- Threaded, polymorphic comments (issue #1367).\n\
         --\n\
         -- ONE table serves every `#[commentable]` model: `commentable_type` holds the\n\
         -- model's name and `commentable_id` its primary key. `commentable_id` carries no\n\
         -- REFERENCES because a single column cannot reference two tables -- the framework\n\
         -- probes and row-locks the parent before every insert instead.\n\
         --\n\
         -- `author_id` has no REFERENCES either, for a different reason: the author model\n\
         -- is named by `#[commentable(by = ...)]` and this migration does not know which\n\
         -- table that is. Add `REFERENCES users(id)` (or your own author table) once it\n\
         -- exists -- it is worth having.\n\
         CREATE TABLE {COMMENTS_TABLE} (\n\
         \x20   id {pk},\n\
         \x20   commentable_type TEXT NOT NULL,\n\
         \x20   commentable_id {big_int} NOT NULL,\n\
         \x20   parent_id {big_int} REFERENCES {COMMENTS_TABLE}(id) ON DELETE CASCADE,\n\
         \x20   author_id {big_int} NOT NULL,\n\
         \x20   body TEXT NOT NULL,\n\
         \x20   created_at {ts_not_null_default_now},\n\
         \x20   deleted_at {ts}\n\
         );\n\
         \n\
         -- Covers the thread read whole: its WHERE is the discriminator pair and its\n\
         -- ORDER BY is (created_at, id), so one index serves both halves.\n\
         CREATE INDEX IF NOT EXISTS idx_{COMMENTS_TABLE}_thread\n\
         \x20   ON {COMMENTS_TABLE} (commentable_type, commentable_id, created_at, id);\n\
         \n\
         -- The delete cascade walks children by parent_id.\n\
         CREATE INDEX IF NOT EXISTS idx_{COMMENTS_TABLE}_parent_id\n\
         \x20   ON {COMMENTS_TABLE} (parent_id);\n"
    )
}

/// `down.sql` dropping it again.
#[must_use]
pub fn down_sql() -> String {
    format!("DROP TABLE IF EXISTS {COMMENTS_TABLE};\n")
}

/// The migration directory name, e.g.
/// `202604270000002_create_comments`.
///
/// Diesel takes the prefix as the migration **version**, and the scaffold has
/// already claimed `{timestamp}` for its own `{timestamp}_create_{table}` and
/// `{timestamp}1` for a `--counter-cache` column (see
/// [`super::counter_cache::migration_dir_name`]). Appending `2` keeps all three
/// distinct while never consuming a wall-clock version a scaffold run one
/// second later would take: `MigrationVersion` is `Ord` over the raw string, a
/// prefix sorts first, and the appended digit sits beyond the index at which
/// `{timestamp}` and `{timestamp + 1}` first differ.
#[must_use]
pub fn migration_dir_name(timestamp: &str) -> String {
    format!("{timestamp}2{MIGRATION_SUFFIX}")
}

/// Whether `project_root` already has a migration creating the **polymorphic**
/// comments table.
///
/// The table is shared, so the second (and third, and tenth) commentable model
/// must not recreate it. Detection is by `up.sql` **content**, not by the
/// directory name: `autumn generate scaffold Comment body:Text` produces a
/// directory called `{timestamp}_create_comments` too, and matching that name
/// would make a later `comments:commentable` skip the shared table while
/// cheerfully reporting it was reused — leaving every `add_comment` to fail at
/// runtime with `42703 column "commentable_type" does not exist`. The
/// discriminator column is the thing that actually distinguishes the two, so
/// that is what is matched.
#[must_use]
pub fn already_migrated(project_root: &Path) -> bool {
    matches!(comments_table(project_root), CommentsTable::Shared)
}

/// The `comments` table the migration history leaves behind.
#[derive(Debug, PartialEq, Eq)]
enum CommentsTable {
    /// No `comments` table.
    Absent,
    /// The shared, polymorphic table: every helper's column is present.
    Shared,
    /// A `comments` table that does not have these [`REQUIRED_COLUMNS`]
    /// (#2283). A `Comment` model scaffolded the ordinary way makes one.
    Conflicting {
        missing: Vec<&'static str>,
        /// A rename from a table no migration creates: `missing` is a guess.
        columns_unknown: bool,
    },
}

/// Replay the history once and classify the `comments` table.
fn comments_table(project_root: &Path) -> CommentsTable {
    classify(&migration_up_sql(project_root))
}

/// Whether `destroy` must keep `migration_dir` (#2283).
///
/// True when another `#[commentable]` model still needs the shared table, and
/// the table is not whole without this migration — an adopted `comments`
/// table starts with the `Comment` model's own `CREATE TABLE`. `excluding`
/// lists the files the same `destroy` removes.
#[must_use]
pub fn comments_migration_still_needed(
    project_root: &Path,
    migration_dir: &Path,
    excluding: &[std::path::PathBuf],
) -> bool {
    let name = migration_dir.file_name();
    if !name
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(MIGRATION_SUFFIX))
    {
        return false;
    }
    let src = project_root.join("src");
    let commentable_elsewhere = commentable_declared_below(&src.join("models"), excluding)
        || std::fs::read_to_string(src.join("models.rs"))
            .is_ok_and(|models| declares_commentable_on_comments(&models));
    if !commentable_elsewhere {
        return false;
    }
    let all = migration_up_sql(project_root);
    let without = migration_up_sql_where(project_root, |dir, _| dir.file_name() != name);
    // A later `ALTER` that needs this migration's table also needs the file:
    // without it, a fresh `migrate` stops on that `ALTER`.
    let orphans_an_alter = |files: &[String]| {
        replay(files)
            .touched_while_absent
            .contains(&TableRef::comments())
    };
    classify(&all) == CommentsTable::Shared
        && (classify(&without) != CommentsTable::Shared
            || (orphans_an_alter(&without) && !orphans_an_alter(&all)))
}

/// Classify the `comments` table that `files`, replayed in order, leave.
fn classify(files: &[String]) -> CommentsTable {
    let tables = replay_migration_history(files);
    match tables.get(&TableRef::comments()) {
        Some(table) if table.exists => {
            let missing = table.missing_columns();
            if missing.is_empty() {
                CommentsTable::Shared
            } else {
                CommentsTable::Conflicting {
                    missing,
                    columns_unknown: table.columns_unknown,
                }
            }
        }
        _ => CommentsTable::Absent,
    }
}

/// The refusal for a `comments` table that is not the shared one (#2283).
///
/// Emitting anyway writes a second `CREATE TABLE comments`, and `migrate`
/// stops on "already exists". Skipping is worse: every helper then queries
/// columns that are not there. Adding the missing columns is not offered: the
/// model that owns the table still inserts rows without them.
fn conflicting_table_error(missing: &[&str], columns_unknown: bool) -> GenerateError {
    let rename_or_drop = format!(
        "  - Rename or drop the existing `{COMMENTS_TABLE}` table in a new migration. \
         Then update or remove the model that uses it."
    );
    let message = if columns_unknown {
        format!(
            "cannot add the shared `{COMMENTS_TABLE}` table: a migration renames another \
             table to `{COMMENTS_TABLE}`, and no migration creates that table. The generator \
             cannot read its columns. The generator wrote no files. Do one of these steps:\n\
             {rename_or_drop} Then run the command again.\n\
             \x20 - If the table is already the shared one, add `#[commentable]` to the model \
             by hand. Do not use `comments:commentable`."
        )
    } else {
        let missing = missing
            .iter()
            .map(|column| format!("`{column}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "cannot add the shared `{COMMENTS_TABLE}` table: the project has a \
             `{COMMENTS_TABLE}` table that is not the shared one. These columns are missing: \
             {missing}. The generator wrote no files. Do this step, then run the command \
             again:\n\
             {rename_or_drop}"
        )
    };
    GenerateError::Config(message)
}

/// The first model file under `src/models` (or `src/models.rs`) with a
/// `#[model]` struct on the `comments` table that cannot insert into the
/// shared table (#2283).
///
/// The shared table needs `commentable_type`, `commentable_id` and
/// `author_id` on each insert. A plain `Comment` model sets none of them.
fn model_using_comments_table(project_root: &Path) -> Option<std::path::PathBuf> {
    let src = project_root.join("src");
    let mut files = vec![src.join("models.rs")];
    let mut dirs = vec![src.join("models")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .find(|file| std::fs::read_to_string(file).is_ok_and(|source| maps_comments_table(&source)))
}

/// Whether `source` declares a live `#[commentable]` on the `comments` table:
/// `table = <name>` when given, else `comments`. A commented-out attribute,
/// or one on another table, does not count. A file that does not parse falls
/// back to a text scan.
fn declares_commentable_on_comments(source: &str) -> bool {
    fn on_comments(items: &[syn::Item]) -> bool {
        items.iter().any(|item| match item {
            syn::Item::Struct(item) => item
                .attrs
                .iter()
                .filter(|attr| {
                    attr.path()
                        .segments
                        .last()
                        .is_some_and(|segment| segment.ident == "commentable")
                })
                .any(|attr| commentable_table(attr) == COMMENTS_TABLE),
            syn::Item::Mod(item) => item
                .content
                .as_ref()
                .is_some_and(|(_, items)| on_comments(items)),
            _ => false,
        })
    }
    syn::parse_file(source).map_or_else(
        |_| source.contains("#[commentable"),
        |file| on_comments(&file.items),
    )
}

/// The table a `#[commentable]` attribute names: `table = <ident>` or
/// `table = "<name>"`, else `comments`.
fn commentable_table(attr: &syn::Attribute) -> String {
    let mut table = None;
    if matches!(attr.meta, syn::Meta::List(_)) {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let value: syn::Expr = meta.value()?.parse()?;
                table = match value {
                    syn::Expr::Path(path) => path.path.get_ident().map(ToString::to_string),
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(name),
                        ..
                    }) => Some(name.value()),
                    _ => None,
                };
            } else if meta.input.peek(syn::Token![=]) {
                // Skip any other `key = value` pair.
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
    }
    table.unwrap_or_else(|| COMMENTS_TABLE.to_owned())
}

/// Whether `source` has a `#[model]` struct on the `comments` table.
///
/// The macro decides the table: `#[model(table = "…")]`, or the name it infers
/// from the struct. Imports only bring diesel's table into scope, so they are
/// not the binding. A file that does not parse falls back to a text scan.
fn maps_comments_table(source: &str) -> bool {
    syn::parse_file(source).map_or_else(
        |_| binds_comments_table(&strip_rust_comments_and_literals(source)),
        |file| items_map_comments_table(&file.items),
    )
}

/// [`maps_comments_table`] over `items`, inline modules included.
fn items_map_comments_table(items: &[syn::Item]) -> bool {
    items.iter().any(|item| match item {
        // A model that declares every shared column writes the shared table
        // correctly. Only one that lacks a column is stale.
        syn::Item::Struct(item) => {
            model_table(item).is_some_and(|table| table == COMMENTS_TABLE)
                && !declares_every_shared_column(item)
        }
        syn::Item::Mod(item) => item
            .content
            .as_ref()
            .is_some_and(|(_, items)| items_map_comments_table(items)),
        _ => false,
    })
}

/// The shared columns each insert must set: `NOT NULL` with no default.
/// `id` is generated, `created_at` has a default, and `parent_id` and
/// `deleted_at` take `NULL`.
const INSERT_COLUMNS: &[&str] = &["commentable_type", "commentable_id", "author_id", "body"];

/// Whether `item` has a field for each of [`INSERT_COLUMNS`], so its inserts
/// work on the shared table. A field's column is its
/// `#[diesel(column_name = …)]` when given, else its name.
fn declares_every_shared_column(item: &syn::ItemStruct) -> bool {
    let fields: Vec<String> = item.fields.iter().filter_map(field_column).collect();
    INSERT_COLUMNS
        .iter()
        .all(|column| fields.iter().any(|field| field == column))
}

/// The column a struct field maps to: `#[diesel(column_name = …)]`, else the
/// field name without a raw `r#` prefix.
fn field_column(field: &syn::Field) -> Option<String> {
    let mut renamed = None;
    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("diesel"))
    {
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("column_name") {
                let value: syn::Ident = meta.value()?.parse()?;
                renamed = Some(value.to_string());
            } else if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
    }
    renamed.or_else(|| {
        field.ident.as_ref().map(|ident| {
            let name = ident.to_string();
            name.strip_prefix("r#")
                .map(ToOwned::to_owned)
                .unwrap_or(name)
        })
    })
}

/// The table of a `#[model]` struct, as the macro decides it. `None` when the
/// struct has no `#[model]` attribute.
fn model_table(item: &syn::ItemStruct) -> Option<String> {
    let attr = item.attrs.iter().find(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "model")
    })?;
    let mut table = None;
    if matches!(attr.meta, syn::Meta::List(_)) {
        // `managed` takes no value; anything else unknown ends the scan.
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                table = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            }
            Ok(())
        });
    }
    Some(table.unwrap_or_else(|| {
        super::naming::pluralize(&super::naming::snake(&item.ident.to_string()))
    }))
}

/// Whether `code` binds the `comments` table: `schema::comments` that ends
/// the path, or a `comments` entry of a grouped `schema::{…}` import. A path
/// that goes on (`schema::comments::table`) only reads the table.
fn binds_comments_table(code: &str) -> bool {
    code.match_indices("schema::").any(|(at, prefix)| {
        let after = &code[at + prefix.len()..];
        if let Some(group) = after.strip_prefix('{') {
            let group = group.split('}').next().unwrap_or_default();
            return group
                .split(',')
                .any(|entry| entry.split_whitespace().next() == Some(COMMENTS_TABLE));
        }
        after.strip_prefix(COMMENTS_TABLE).is_some_and(|rest| {
            !rest.starts_with(is_ident_char) && !rest.trim_start().starts_with("::")
        })
    })
}

/// `source` with Rust comments and string and char literals blanked out, so a
/// mention in prose is not read as code.
fn strip_rust_comments_and_literals(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                // Line comment: skip to the end of the line.
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                // Block comment. Rust nests them.
                chars.next();
                let mut depth = 1;
                while depth > 0 {
                    match chars.next() {
                        Some('/') if chars.peek() == Some(&'*') => {
                            chars.next();
                            depth += 1;
                        }
                        Some('*') if chars.peek() == Some(&'/') => {
                            chars.next();
                            depth -= 1;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                out.push(' ');
            }
            '"' => {
                // String literal, with `\` escapes.
                while let Some(next) = chars.next() {
                    match next {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
                out.push_str("\"\"");
            }
            '\'' => {
                // A char literal (`'"'`, `'\''`) or a lifetime (`'a`).
                let mut ahead = chars.clone();
                let literal = match ahead.next() {
                    Some('\\') => {
                        ahead.next();
                        ahead.any(|next| next == '\'')
                    }
                    Some(_) => ahead.next() == Some('\''),
                    None => false,
                };
                if literal {
                    chars = ahead;
                    out.push_str("' '");
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The refusal for a model that still uses the `comments` table (#2283).
fn stale_model_error(project_root: &Path, model: &Path) -> GenerateError {
    let model = model.strip_prefix(project_root).unwrap_or(model);
    GenerateError::Config(format!(
        "cannot add the shared `{COMMENTS_TABLE}` table: {} still uses the \
         `{COMMENTS_TABLE}` table. Its inserts do not set the shared columns, so each \
         insert would fail. The generator wrote no files. Remove that model, or point it \
         at another table. Then run the command again.",
        model.display().to_string().replace('\\', "/")
    ))
}

/// A table reference parsed from DDL: `[schema.]name`, each half optionally
/// double-quoted.
///
/// Quoting is load-bearing. The pipeline lowercases unquoted text but preserves
/// quoted identifiers, so `comments` (unquoted, folded) and `"comments"`
/// (quoted, exact) both land on `comments`, while `"Comments"` stays distinct —
/// exactly `PostgreSQL`'s case-folding rule. A `public` schema (quoted or not,
/// both spell the same schema) is normalised away: `public.comments` IS
/// `comments` under the default search path, and the old fixed-spelling scan
/// already treated the two as one table. Any other schema stays distinct —
/// guessing at the app's `search_path` would trade a false negative for a
/// false positive.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct TableRef {
    /// `None` for unqualified and `public`-qualified names.
    schema: Option<String>,
    name: String,
}

impl TableRef {
    fn comments() -> Self {
        Self {
            schema: None,
            name: COMMENTS_TABLE.to_owned(),
        }
    }
}

/// One table's running picture in the replay.
#[derive(Default, Debug)]
struct TableState {
    exists: bool,
    /// Which of [`REQUIRED_COLUMNS`] the table currently carries.
    columns: Vec<&'static str>,
    /// The table came from a rename of a table that no migration creates, so
    /// the replay cannot see its columns.
    columns_unknown: bool,
}

impl TableState {
    /// The [`REQUIRED_COLUMNS`] this table does not carry.
    fn missing_columns(&self) -> Vec<&'static str> {
        REQUIRED_COLUMNS
            .iter()
            .copied()
            .filter(|column| !self.columns.contains(column))
            .collect()
    }
}

/// What one statement does to one table in the replayed history.
#[derive(Debug)]
enum TableEvent {
    /// `CREATE TABLE name (…)`, and which of [`REQUIRED_COLUMNS`] its body
    /// declares. A fresh table replaces whatever was known about the old one.
    /// The flag is `IF NOT EXISTS`: then an existing table stays as it is.
    Create(TableRef, Vec<&'static str>, bool),
    /// `ALTER TABLE name … <column>`, adding it.
    Add(TableRef, &'static str),
    /// `ALTER TABLE name DROP COLUMN <column>` (or a rename away).
    Remove(TableRef, &'static str),
    /// `DROP TABLE name`. The flag is `IF EXISTS`: then no table is fine.
    Drop(TableRef, bool),
    /// Any other `ALTER TABLE name …`: it changes no tracked column, but it
    /// still needs the table to exist.
    Touch(TableRef),
    /// `ALTER TABLE old RENAME TO new`: the record moves with the table, so a
    /// rename INTO `comments` carries the source table's columns across.
    Rename { from: TableRef, to: TableRef },
}

impl TableEvent {
    /// The table an `ALTER` event changes; `None` for other events.
    const fn altered_table(&self) -> Option<&TableRef> {
        match self {
            Self::Add(table, _)
            | Self::Remove(table, _)
            | Self::Touch(table)
            | Self::Rename { from: table, .. } => Some(table),
            Self::Create(..) | Self::Drop(..) => None,
        }
    }
}

/// Replay every migration's `up.sql` in version order: for every table, does it
/// exist, and which discriminator columns does it currently carry.
///
/// A migration history is a sequence of edits, not a bag of facts: flags that
/// only ever get SET would report a table a later `DROP TABLE` removed as
/// still present — the generator would skip recreating it, and every helper
/// would fail at runtime on a table that is not there. Every event edits one
/// running picture per table, so a column added by a CREATE body and one added
/// by a later ALTER are the same kind of fact.
///
/// Generalised per #2282: the old scan only understood the `comments`
/// spellings, so a `RENAME TO comments` landed on an empty column list — wrong
/// when the renamed table already carried the discriminator columns
/// (`CREATE TABLE legacy_comments (commentable_type …)` then `RENAME TO
/// comments`). Now every table is tracked and the rename carries its columns
/// across; the final answer is a lookup on the `comments` ref.
fn replay_migration_history(files: &[String]) -> HashMap<TableRef, TableState> {
    replay(files).tables
}

/// The end state of a replay.
struct Replay {
    tables: HashMap<TableRef, TableState>,
    /// Tables that an `ALTER`, a rename, or a `DROP` without `IF EXISTS`
    /// touched while they did not exist. A real `migrate` stops on that
    /// statement (#2283).
    touched_while_absent: std::collections::HashSet<TableRef>,
}

/// The table events one migration file holds, in no order. The flag is
/// `ALTER TABLE IF EXISTS`: on no table the event does nothing.
fn file_events(sql: &str) -> Vec<(usize, TableEvent, bool)> {
    let mut events: Vec<(usize, TableEvent, bool)> = Vec::new();
    for (at, table, body, if_not_exists) in create_tables(sql) {
        let columns = REQUIRED_COLUMNS
            .iter()
            .copied()
            .filter(|column| declares_column(body, column))
            .collect();
        events.push((at, TableEvent::Create(table, columns, if_not_exists), false));
    }
    for (at, dropped, if_exists) in drop_tables(sql) {
        for table in dropped {
            events.push((at, TableEvent::Drop(table, if_exists), false));
        }
    }
    for (at, table, statement, if_exists) in alter_tables(sql) {
        // A table rename moves the whole record; it mentions no column.
        if let Some(to) = table_rename_target(statement) {
            events.push((at, TableEvent::Rename { from: table, to }, if_exists));
            continue;
        }
        // An ALTER naming the column may be adding it, dropping it, or
        // renaming it away. Treating every mention as an add would let
        // `DROP COLUMN commentable_type` read as proof the column is
        // present.
        events.push((at, TableEvent::Touch(table.clone()), if_exists));
        for column in REQUIRED_COLUMNS.iter().copied() {
            if !mentions_column(statement, column) {
                continue;
            }
            if alter_removes_column(statement, column) {
                events.push((at, TableEvent::Remove(table.clone(), column), if_exists));
            } else {
                events.push((at, TableEvent::Add(table.clone(), column), if_exists));
            }
        }
    }
    events
}

/// [`replay_migration_history`], also recording [`Replay::touched_while_absent`].
fn replay(files: &[String]) -> Replay {
    let mut tables: HashMap<TableRef, TableState> = HashMap::new();
    let mut touched_while_absent = std::collections::HashSet::new();
    for sql in files {
        let mut events = file_events(sql);
        events.sort_by_key(|(at, _, _)| *at);
        for (_, event, if_exists) in events {
            if if_exists
                && !event
                    .altered_table()
                    .is_some_and(|table| tables.get(table).is_some_and(|state| state.exists))
            {
                continue;
            }
            match event {
                TableEvent::Create(table, columns, if_not_exists) => {
                    // `IF NOT EXISTS` on an existing table does nothing.
                    if if_not_exists && tables.get(&table).is_some_and(|state| state.exists) {
                        continue;
                    }
                    tables.insert(
                        table,
                        TableState {
                            exists: true,
                            columns,
                            columns_unknown: false,
                        },
                    );
                }
                TableEvent::Add(table, column) => {
                    if !tables.get(&table).is_some_and(|state| state.exists) {
                        touched_while_absent.insert(table.clone());
                    }
                    let state = tables.entry(table).or_default();
                    if !state.columns.contains(&column) {
                        state.columns.push(column);
                    }
                }
                TableEvent::Remove(table, column) => {
                    if !tables.get(&table).is_some_and(|state| state.exists) {
                        touched_while_absent.insert(table.clone());
                    }
                    if let Some(state) = tables.get_mut(&table) {
                        state.columns.retain(|held| *held != column);
                    }
                }
                TableEvent::Touch(table) => {
                    if !tables.get(&table).is_some_and(|state| state.exists) {
                        touched_while_absent.insert(table);
                    }
                }
                TableEvent::Drop(table, if_exists) => {
                    if !if_exists && !tables.get(&table).is_some_and(|state| state.exists) {
                        touched_while_absent.insert(table.clone());
                    }
                    let state = tables.entry(table).or_default();
                    state.exists = false;
                    state.columns.clear();
                    state.columns_unknown = false;
                }
                TableEvent::Rename { from, to } => {
                    // A rename is positive evidence the table exists: the
                    // author just renamed it, and in a valid history the
                    // statement would fail otherwise. The old scan read every
                    // `RENAME TO comments` as the table existing; the
                    // generalisation keeps that and additionally carries the
                    // source table's columns across (#2282). When the history
                    // never saw the source, its columns are unknown — present
                    // but not polymorphic, so generation stays loud instead of
                    // claiming a reuse it cannot verify.
                    //
                    // `RENAME TO` takes a bare relation name: the table stays
                    // in its schema, so `archive.legacy_comments RENAME TO
                    // comments` yields `archive.comments`, not `comments`.
                    let to = TableRef {
                        schema: to.schema.or_else(|| from.schema.clone()),
                        name: to.name,
                    };
                    let source_known = tables.get(&from).is_some_and(|state| state.exists);
                    if !source_known {
                        touched_while_absent.insert(from.clone());
                    }
                    let mut state = tables.remove(&from).unwrap_or_default();
                    state.exists = true;
                    state.columns_unknown |= !source_known;
                    tables.insert(to, state);
                }
            }
        }
    }
    Replay {
        tables,
        touched_while_absent,
    }
}

/// Whether `c` can continue a bare SQL identifier.
const fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// Strip a leading keyword (`only`, …) when it is a whole word.
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> &'a str {
    if let Some(after) = text.strip_prefix(keyword)
        && (after.is_empty() || after.starts_with(|c: char| c.is_whitespace()))
    {
        after.trim_start()
    } else {
        text
    }
}

/// One identifier segment: `"quoted"` (with `""` escapes, case preserved) or a
/// bare word (already lowercased by the pipeline). Returns the segment text and
/// the bytes consumed, including leading whitespace.
fn parse_ident_segment(text: &str) -> Option<(String, usize)> {
    let trimmed = text.trim_start();
    let skip = text.len() - trimmed.len();
    if trimmed.starts_with('"') {
        let mut name = String::new();
        // Past the opening quote; byte indices stay on `"` boundaries.
        let mut i = 1;
        loop {
            let close = trimmed[i..].find('"')?;
            name.push_str(&trimmed[i..i + close]);
            i += close + 1;
            // `""` inside a quoted identifier is an escaped quote.
            if trimmed[i..].starts_with('"') {
                name.push('"');
                i += 1;
            } else {
                break;
            }
        }
        Some((name, skip + i))
    } else {
        let len = trimmed
            .find(|c: char| !is_ident_char(c))
            .unwrap_or(trimmed.len());
        if len == 0 {
            return None;
        }
        Some((trimmed[..len].to_owned(), skip + len))
    }
}

/// Parse `[IF [NOT] EXISTS] [ONLY] [schema.]name` from the start of `text`.
///
/// Returns the canonical ref and the bytes consumed (through the name), so
/// callers can slice what follows. `None` when no table reference starts here.
fn parse_table_ref(text: &str) -> Option<(TableRef, usize)> {
    let mut rest = text.trim_start();
    // `IF EXISTS` / `IF NOT EXISTS`: valid on CREATE, DROP and ALTER alike.
    // The longer keyword first — it starts with the shorter one.
    rest = strip_keyword(rest, "if not exists");
    rest = strip_keyword(rest, "if exists");
    // `ONLY` is PostgreSQL's "do not recurse to inheritance children" marker,
    // and it does NOT sit where `IF EXISTS` does — the grammar is
    // `ALTER TABLE [ IF EXISTS ] [ ONLY ] name [ * ]`, so the two COMBINE.
    // Listing them as alternatives missed `ALTER TABLE IF EXISTS ONLY
    // comments`, a valid spelling whose ALTERs were then ignored: the scan
    // reported an incomplete schema and the generator emitted a duplicate
    // `CREATE TABLE comments` that fails on the next `migrate`.
    rest = strip_keyword(rest, "only");
    let (first, used) = parse_ident_segment(rest)?;
    rest = &rest[used..];
    let (schema, name) = if let Some(dot) = rest.strip_prefix('.') {
        let (second, used) = parse_ident_segment(dot)?;
        rest = &dot[used..];
        (Some(first), second)
    } else {
        (None, first)
    };
    let consumed = text.len() - rest.len();
    // `public` (however spelled — quoted `"public"` names the same schema) is
    // the default schema: `public.comments` and `comments` name the same
    // relation, so they share one record.
    let schema = schema.filter(|schema| schema != "public");
    Some((TableRef { schema, name }, consumed))
}

/// The `CREATE` verbs that produce a PERSISTENT table.
///
/// `UNLOGGED` is a durability setting, not a different kind of object: the
/// relation is still permanent and still occupies the name, so a later
/// `CREATE TABLE comments` fails with "already exists" — confirmed against
/// `PostgreSQL`, not assumed. Missing it made the scan report no table and emit
/// a duplicate migration.
///
/// `TEMPORARY`/`TEMP` is deliberately NOT here, and that is the more
/// interesting half. A temp table lives in a session-local schema and does
/// **not** collide with a permanent one — also confirmed — so a migration that
/// creates a temp table must not register it. Accepting every modifier would
/// have traded one bug for its mirror image.
const CREATE_VERBS: &[&str] = &["table", "unlogged table"];

/// Every persistent `CREATE TABLE` in `sql`: (offset, table, column-list body,
/// `IF NOT EXISTS`).
fn create_tables(sql: &str) -> Vec<(usize, TableRef, &str, bool)> {
    let mut found = Vec::new();
    let mut base = 0usize;
    while let Some(at) = sql[base..].find("create ") {
        let start = base + at;
        base = start + "create ".len();
        // A leading boundary: `recreate table` is not a CREATE.
        if start > 0 && sql[..start].ends_with(is_ident_char) {
            continue;
        }
        let rest = &sql[start + "create ".len()..];
        let verb = CREATE_VERBS.iter().find(|verb| {
            rest.strip_prefix(*verb)
                .is_some_and(|after| after.starts_with(|c: char| c.is_whitespace()))
        });
        let Some(verb) = verb else {
            // `CREATE INDEX`, `CREATE TRIGGER`, `CREATE TEMPORARY TABLE`, …
            continue;
        };
        let after_verb = &rest[verb.len()..];
        // The pipeline lowercases unquoted SQL, so one spelling matches.
        let trimmed = after_verb.trim_start();
        let if_not_exists = strip_keyword(trimmed, "if not exists").len() != trimmed.len();
        let Some((table, used)) = parse_table_ref(after_verb) else {
            continue;
        };
        let body_start = start + "create ".len() + verb.len() + used;
        let Some(body) = create_table_body(sql, body_start) else {
            continue;
        };
        found.push((start, table, body, if_not_exists));
    }
    found
}

/// The column list of a `CREATE TABLE` whose name ends at `from`: the text
/// between the statement's outer parentheses, so callers can ask what *this*
/// table declares rather than what the file mentions anywhere.
///
/// Paren-balanced, because a column can carry its own (`NUMERIC(10, 2)`).
/// Unbalanced SQL yields the rest of the file rather than a silent "no such
/// table" for a migration that does create one. `None` only when there is no
/// opening paren in this statement (e.g. `CREATE TABLE x AS SELECT 1;`): the
/// search stops at the statement's `;`, so a later statement's column list is
/// never borrowed as this table's. Likewise `None` when an `AS` precedes the
/// paren: in `CREATE TABLE x AS SELECT jsonb_build_object('id', id) …` the
/// parens belong to the query, and the table's columns are whatever it
/// projects — unknowable here, so never read as a column list.
fn create_table_body(sql: &str, from: usize) -> Option<&str> {
    let open = sql[from..].find(['(', ';'])? + from;
    if sql[open..].starts_with(';') {
        return None;
    }
    if sql[from..open]
        .split(|c: char| !is_ident_char(c))
        .any(|word| word == "as")
    {
        return None;
    }
    let mut depth = 0usize;
    for (offset, ch) in sql[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&sql[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    Some(&sql[open + 1..])
}

/// Every `DROP TABLE` in `sql`: (offset, tables dropped).
///
/// `DROP TABLE [ IF EXISTS ] name [, ...]` — the LIST is the point. Requiring
/// the name right after the verb missed `DROP TABLE audit_log, comments;`,
/// which really does drop it (confirmed against `PostgreSQL`). The replay then
/// kept a table the database no longer has, the next scaffold skipped creating
/// it, and every generated helper queried a missing relation — silently, until
/// the first request.
fn drop_tables(sql: &str) -> Vec<(usize, Vec<TableRef>, bool)> {
    let mut found = Vec::new();
    let mut base = 0usize;
    while let Some(at) = sql[base..].find("drop table") {
        let start = base + at;
        base = start + "drop table".len();
        if start > 0 && sql[..start].ends_with(is_ident_char) {
            continue;
        }
        let rest = &sql[start + "drop table".len()..];
        // Not `DROP TABLESPACE`: the verb has to end where the words end.
        if rest.starts_with(is_ident_char) {
            continue;
        }
        // Only this statement's own name list.
        let statement = rest.split(';').next().unwrap_or(rest);
        let tables: Vec<TableRef> = statement
            .split(',')
            .filter_map(|entry| {
                // The name is the first reference: `CASCADE` / `RESTRICT`
                // trail the list, `ONLY` may lead an entry, `*` may trail it.
                parse_table_ref(entry).map(|(table, _)| table)
            })
            .collect();
        if !tables.is_empty() {
            // The pipeline lowercases unquoted SQL, so one spelling matches.
            let if_exists = statement.trim_start().starts_with("if exists");
            found.push((start, tables, if_exists));
        }
    }
    found
}

/// Every `ALTER TABLE` in `sql`: (offset, table, statement text after the
/// name, `IF EXISTS`).
fn alter_tables(sql: &str) -> Vec<(usize, TableRef, &str, bool)> {
    let mut found = Vec::new();
    let mut base = 0usize;
    while let Some(at) = sql[base..].find("alter table") {
        let start = base + at;
        base = start + "alter table".len();
        if start > 0 && sql[..start].ends_with(is_ident_char) {
            continue;
        }
        let rest = &sql[start + "alter table".len()..];
        if rest.starts_with(is_ident_char) {
            continue;
        }
        let Some((table, used)) = parse_table_ref(rest) else {
            continue;
        };
        let after = &rest[used..];
        let statement = after.split(';').next().unwrap_or(after);
        // The pipeline lowercases unquoted SQL, so one spelling matches.
        let if_exists = rest.trim_start().starts_with("if exists");
        found.push((start, table, statement, if_exists));
    }
    found
}

/// The destination of an `ALTER TABLE … RENAME TO <name>`, if `statement` (the
/// text after the table name) renames the TABLE rather than a column.
///
/// Scanned by destination because a rename INTO `comments` begins with whatever
/// the table used to be called, which the caller has no other way to know.
fn table_rename_target(statement: &str) -> Option<TableRef> {
    let at = statement.find(" rename to ")?;
    // `RENAME COLUMN … TO …` is a column rename, classified with the columns.
    if statement[..at].contains("rename column") {
        return None;
    }
    parse_table_ref(&statement[at + " rename to ".len()..]).map(|(table, _)| table)
}

/// Whether `haystack` mentions `column` as a complete SQL identifier.
///
/// A bare `contains` would accept `legacy_commentable_type` as
/// `commentable_type`, classify a table that lacks the real columns as
/// polymorphic, and make the generator skip the shared migration while
/// reporting that it reused the table — with every helper then failing at
/// runtime on columns that are not there. The same identifier-boundary rule the
/// table name gets; columns had been left as substrings.
/// Whether a `CREATE TABLE` column-list `body` DECLARES `column` — as the name
/// at the head of one of its top-level elements — rather than merely
/// mentioning it. A mention is not a declaration: `author_id BIGINT
/// REFERENCES users(id)` names `id`, but the table has no `id` column, and
/// reading it as one made a table lacking the key look like the shared
/// comments table.
///
/// Elements split at top-level commas (not inside parens or a `'…'` literal);
/// table constraints (`PRIMARY KEY (…)`, `CONSTRAINT …`, `FOREIGN KEY`, …)
/// declare no column and are skipped.
fn declares_column(body: &str, column: &str) -> bool {
    const TABLE_CONSTRAINTS: &[&str] = &[
        "constraint",
        "primary",
        "foreign",
        "unique",
        "check",
        "exclude",
        "like",
    ];
    let mut elements = Vec::new();
    let (mut depth, mut in_literal, mut start) = (0usize, false, 0usize);
    for (at, ch) in body.char_indices() {
        match ch {
            '\'' => in_literal = !in_literal,
            '(' if !in_literal => depth += 1,
            ')' if !in_literal => depth = depth.saturating_sub(1),
            ',' if !in_literal && depth == 0 => {
                elements.push(&body[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    elements.push(&body[start..]);
    elements.into_iter().any(|element| {
        let element = element.trim_start();
        // A constraint keyword is only one when unquoted: `"check"` is a column.
        let quoted = element.starts_with('"');
        parse_ident_segment(element).is_some_and(|(name, _)| {
            name == column && (quoted || !TABLE_CONSTRAINTS.contains(&name.as_str()))
        })
    })
}

fn mentions_column(haystack: &str, column: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut base = 0usize;
    while let Some(at) = haystack[base..].find(column) {
        let start = base + at;
        let end = start + column.len();
        let before_ok = start == 0 || !haystack[..start].ends_with(is_ident);
        let after_ok = !haystack[end..].starts_with(is_ident);
        if before_ok && after_ok {
            return true;
        }
        base = end;
    }
    false
}

/// Every column the generated helpers read or write on the shared table.
///
/// The discriminator PAIR is not the contract, only its most obvious part. A
/// pre-existing polymorphic `comments` table — a Rails-style one carrying
/// `user_id` where the helpers expect `author_id`, or one with no `parent_id` —
/// satisfies a pair-only check, suppresses the shared migration, and then fails
/// at run time with `42703 undefined_column` on every comment operation. Worse,
/// generation would have SAID it was reusing the table.
///
/// So the whole schema is the question. A table missing any of these is not the
/// shared table: the generator emits its own, or refuses when the name is
/// taken (see `CommentsTable::Conflicting`). A refusal beats a reassuring
/// message and an app that breaks on its first comment.
const REQUIRED_COLUMNS: &[&str] = &[
    "id",
    "commentable_type",
    "commentable_id",
    "parent_id",
    "author_id",
    "body",
    "created_at",
    "deleted_at",
];

/// Whether an `ALTER TABLE comments …` statement takes `column` away.
///
/// `DROP COLUMN <column>` removes it outright; `RENAME COLUMN <column> TO …`
/// removes it under that name, which is all this scan cares about. A
/// `RENAME COLUMN <other> TO <column>` is the opposite and must not count as a
/// removal, so the position of the name relative to `to` decides.
fn alter_removes_column(statement: &str, column: &str) -> bool {
    // COLUMN is optional in a drop as well (`ALTER TABLE comments DROP
    // commentable_type`), so the keyword cannot be what identifies one. Every
    // `DROP` in the statement is examined because one ALTER may carry several
    // (`DROP CONSTRAINT ck, DROP COLUMN commentable_type`).
    for (at, _) in statement.match_indices(" drop ") {
        let rest = statement[at + " drop ".len()..].trim_start();
        // These drop something OTHER than a column: a constraint, or a property
        // of one (`ALTER COLUMN commentable_type DROP NOT NULL` names the column
        // but keeps it). Reading them as removals would report a polymorphic
        // table as broken and emit a duplicate migration.
        if [
            "constraint ",
            "default",
            "not null",
            "expression",
            "identity",
        ]
        .iter()
        .any(|kw| rest.starts_with(kw))
        {
            continue;
        }
        // Only the name actually being dropped: `DROP COLUMN kind, ADD COLUMN
        // commentable_type` must not read as dropping the discriminator.
        let name = rest.strip_prefix("column ").unwrap_or(rest).trim_start();
        let name = name.strip_prefix("if exists ").unwrap_or(name).trim_start();
        let name = name.split([' ', ',', ';']).next().unwrap_or("");
        if mentions_column(name, column) {
            return true;
        }
    }
    // PostgreSQL makes COLUMN optional: `RENAME commentable_type TO kind`
    // renames the column exactly as `RENAME COLUMN commentable_type TO kind`
    // does. Matching only the spelled-out form read the bare one as "the column
    // is still there", so the scan reported a polymorphic table, skipped the
    // shared migration, and left every helper querying a column that had been
    // renamed away — silently, at runtime.
    //
    // One branch serves both spellings: what follows `RENAME` is either
    // `COLUMN <name>` or the bare `<name>`, and the `from` side covers each.
    if let Some(at) = statement.find(" rename ") {
        let rest = statement[at + " rename ".len()..].trim_start();
        // `RENAME TO <table>` renames the TABLE and `RENAME CONSTRAINT …` a
        // constraint. Neither one touches a column.
        if !rest.starts_with("to ") && !rest.starts_with("constraint ") {
            let (from, to) = rest.split_once(" to ").unwrap_or((rest, ""));
            // Renaming the column AWAY removes it; renaming another column INTO
            // the discriminator name adds it, so the `to` side has to be clear.
            return mentions_column(from, column) && !mentions_column(to, column);
        }
    }
    false
}

/// The plpgsql function behind every parent's cleanup trigger.
///
/// Emitted by each PARENT migration rather than by the shared table migration,
/// and `CREATE OR REPLACE` so repeating it is free.
///
/// Ordering is why. Diesel keys on the version prefix, and the shared table's
/// version is `{timestamp}2` while a parent scaffolded in the same second is
/// `{timestamp}` — so the parent migration runs FIRST, and `PostgreSQL` refuses
/// `CREATE TRIGGER` when its function does not yet exist. A parent that carries
/// its own function cannot be ordered wrongly. The body may safely reference
/// `comments` before that table exists: plpgsql resolves it at execution.
fn cleanup_function_sql() -> String {
    format!(
        "-- Deleting a parent must take its comments with it. TG_ARGV[0] is the\n\
         -- parent's `commentable_type`, so ONE function serves every model.\n\
         --\n\
         -- CREATE OR REPLACE, and emitted by every commentable parent's own\n\
         -- migration: the shared table's migration sorts AFTER a parent created\n\
         -- in the same second, so a trigger relying on it there would fail.\n\
         CREATE OR REPLACE FUNCTION {COMMENTS_TABLE}_delete_for_parent()\n\
         RETURNS TRIGGER AS $$\n\
         BEGIN\n\
         \x20   DELETE FROM {COMMENTS_TABLE}\n\
         \x20    WHERE commentable_type = TG_ARGV[0]\n\
         \x20      AND commentable_id = OLD.id;\n\
         \x20   RETURN OLD;\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n"
    )
}

/// The `CREATE TRIGGER` that cleans up `parent_table`'s comments on delete.
///
/// Emitted into the parent's own migration, because the shared table is created
/// once and cannot know which models will later attach to it.
#[must_use]
pub fn parent_cleanup_sql(
    backend: autumn_web::config::DatabaseBackend,
    parent_table: &str,
    commentable_type: &str,
) -> String {
    match backend {
        autumn_web::config::DatabaseBackend::Postgres => format!(
            "\n\
             {function}\n\
             -- `{commentable_type}`'s comments go when the row does. The polymorphic key\n\
             -- cannot be a foreign key, so this trigger is the cascade.\n\
             --\n\
             -- KEEP IN SYNC WITH `#[commentable]` ON THIS MODEL.\n\
             -- The discriminator below is this trigger's ONLY link to the comments;\n\
             -- nothing checks it at run time. Overriding `type_name`, `table` or the\n\
             -- discriminator columns on the model without editing this trigger leaves it\n\
             -- deleting rows that no longer exist, so a deleted parent keeps its thread —\n\
             -- silently, and visibly again if the id is ever reused.\n\
             CREATE TRIGGER {parent_table}_delete_{COMMENTS_TABLE}\n\
             \x20   AFTER DELETE ON {parent_table}\n\
             \x20   FOR EACH ROW\n\
             \x20   EXECUTE FUNCTION {COMMENTS_TABLE}_delete_for_parent('{commentable_type}');\n",
            function = cleanup_function_sql(),
        ),
        autumn_web::config::DatabaseBackend::Sqlite => format!(
            "\n\
             -- `{commentable_type}`'s comments go when the row does. SQLite triggers take\n\
             -- no arguments, so the discriminator is inlined here rather than shared.\n\
             --\n\
             -- KEEP IN SYNC WITH `#[commentable]` ON THIS MODEL.\n\
             -- The discriminator below is this trigger's ONLY link to the comments;\n\
             -- nothing checks it at run time. Overriding `type_name`, `table` or the\n\
             -- discriminator columns on the model without editing this trigger leaves it\n\
             -- deleting rows that no longer exist, so a deleted parent keeps its thread —\n\
             -- silently, and visibly again if the id is ever reused.\n\
             CREATE TRIGGER IF NOT EXISTS {parent_table}_delete_{COMMENTS_TABLE}\n\
             \x20   AFTER DELETE ON {parent_table}\n\
             \x20   FOR EACH ROW\n\
             BEGIN\n\
             \x20   DELETE FROM {COMMENTS_TABLE}\n\
             \x20    WHERE commentable_type = '{commentable_type}'\n\
             \x20      AND commentable_id = OLD.id;\n\
             END;\n"
        ),
    }
}

/// The `DROP TRIGGER` undoing [`parent_cleanup_sql`].
///
/// Backend-split because the syntax differs: `PostgreSQL` names the table
/// (`DROP TRIGGER … ON <table>`), `SQLite` does not — triggers are global there.
#[must_use]
pub fn parent_cleanup_down_sql(
    backend: autumn_web::config::DatabaseBackend,
    parent_table: &str,
) -> String {
    match backend {
        autumn_web::config::DatabaseBackend::Postgres => format!(
            "DROP TRIGGER IF EXISTS {parent_table}_delete_{COMMENTS_TABLE} ON {parent_table};\n"
        ),
        autumn_web::config::DatabaseBackend::Sqlite => {
            format!("DROP TRIGGER IF EXISTS {parent_table}_delete_{COMMENTS_TABLE};\n")
        }
    }
}

/// Every migration's `up.sql`, lowercased with SQL comments stripped.
fn migration_up_sql(project_root: &Path) -> Vec<String> {
    migration_up_sql_where(project_root, |_, _| true)
}

/// [`migration_up_sql`], keeping only the migrations `keep(dir, raw_sql)` accepts.
fn migration_up_sql_where(project_root: &Path, keep: impl Fn(&Path, &str) -> bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_root.join("migrations")) else {
        return Vec::new();
    };
    // Sorted by directory name, which carries the timestamp prefix diesel
    // orders by. Read order from the filesystem is unspecified, and this scan
    // now depends on sequence: a create followed by a drop is not the same
    // history as a drop followed by a create.
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|dir| {
            let sql = std::fs::read_to_string(dir.join("up.sql")).ok()?;
            keep(&dir, &sql).then_some(sql)
        })
        .map(|sql| strip_sql_comments(&sql))
        .collect()
}

/// `sql` with `--` line comments and `/* … */` blocks removed.
///
/// Matching runs over raw text, so a commented-out example — `-- CREATE TABLE
/// comments (commentable_type …)` in a migration's header, which is exactly the
/// kind of thing a header explaining the shared table would contain — would
/// otherwise read as the real table and make the generator emit nothing.
/// Whether the quote at `quote` opens a `PostgreSQL` escape string (`E'…'`).
///
/// Only these honour backslash escapes; an ordinary literal treats `\` as data
/// under the default `standard_conforming_strings = on`. The `E` must be the
/// prefix and not the tail of a longer word, so the character before it has to
/// be a non-identifier one — otherwise a column whose name ends in `e` sitting
/// next to a literal would turn escaping on by accident.
fn is_escape_string_prefix(sql: &str, quote: usize) -> bool {
    let bytes = sql.as_bytes();
    let Some(prev) = quote.checked_sub(1) else {
        return false;
    };
    if !matches!(bytes[prev], b'E' | b'e') {
        return false;
    }
    prev.checked_sub(1)
        .map(|before| bytes[before])
        .is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
}

/// Length of the `$tag$` opening a dollar-quoted literal, if `text` starts one.
///
/// `$$…$$` and `$tag$…$tag$` are `PostgreSQL`'s quote-free string syntax. The tag
/// is an identifier: letters, digits and underscores, but it may NOT begin with
/// a digit — `$1abc$x$1abc$` is rejected as "trailing junk after parameter",
/// which is exactly why `$1` and `$2` placeholders must pass through untouched
/// rather than being read as the start of a literal. Verified against
/// `PostgreSQL` rather than recalled.
fn dollar_tag_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'$' => return Some(i + 1),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => i += 1,
            // A digit is fine inside the tag, never at its head.
            b'0'..=b'9' if i > 1 => i += 1,
            _ => return None,
        }
    }
    None
}

fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            // A single-quoted STRING LITERAL is data, never DDL. Its contents
            // are blanked rather than copied, because every matcher downstream
            // scans this text for keywords, table names, columns and
            // parentheses — and a literal can contain all four. Without this,
            // `VALUES ('DROP TABLE comments;')` reads as a drop, and
            // `DEFAULT ')'` closes a column list early.
            //
            // Length is preserved (one space per byte) so byte offsets stay
            // valid: `replay_migration_history` sorts events by position, and
            // the body slice is taken by index.
            b'\'' => {
                // In an E-STRING — and ONLY there — a backslash escapes the
                // next character, so `E'can\'t; DROP TABLE comments;'` does not
                // end at that apostrophe. Closing it there left the rest of the
                // literal to be replayed as DDL.
                //
                // The restriction to E-strings is the important half. Under
                // `standard_conforming_strings = on` (the default, checked)
                // PostgreSQL treats a backslash in an ORDINARY literal as an
                // ordinary character: `'can\'t X'` is an ERROR precisely
                // because the quote still closes, and `'ends_with_backslash\'`
                // closes normally. Honouring escapes everywhere would swallow
                // the statements after such a literal — the same bug pointing
                // the other way.
                let escapes = is_escape_string_prefix(sql, i);
                out.push('\'');
                i += 1;
                while i < bytes.len() {
                    if escapes && bytes[i] == b'\\' && i + 1 < bytes.len() {
                        // The backslash and whatever it escapes are both data.
                        let ch = sql[i + 1..].chars().next().unwrap_or(' ');
                        for _ in 0..=ch.len_utf8() {
                            out.push(' ');
                        }
                        i += 1 + ch.len_utf8();
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        // A doubled quote is an escaped one, still inside.
                        if bytes.get(i + 1) == Some(&b'\'') {
                            out.push_str("  ");
                            i += 2;
                            continue;
                        }
                        out.push('\'');
                        i += 1;
                        break;
                    }
                    let ch = sql[i..].chars().next().unwrap_or(' ');
                    // One space per BYTE, not per char, to hold the offset.
                    for _ in 0..ch.len_utf8() {
                        out.push(' ');
                    }
                    i += ch.len_utf8();
                }
            }
            // A DOLLAR-QUOTED literal is data as much as a single-quoted one,
            // and blanking only `'…'` left it exposed: PostgreSQL's
            // `$msg$DROP TABLE comments;$msg$` is an ordinary way to store a
            // string containing quotes, and its contents were being scanned as
            // executable DDL. A migration that merely LOGS such a line emitted
            // a false `Drop`, `already_migrated` went false, and the next
            // scaffold wrote a duplicate `CREATE TABLE comments`.
            b'$' => {
                if let Some(tag_len) = dollar_tag_len(&sql[i..]) {
                    let tag = &sql[i..i + tag_len];
                    out.push_str(tag);
                    i += tag_len;
                    if let Some(rel) = sql[i..].find(tag) {
                        // One space per BYTE, holding the offset.
                        for _ in 0..rel {
                            out.push(' ');
                        }
                        out.push_str(tag);
                        i += rel + tag_len;
                    } else {
                        // Unterminated: whatever follows is inside the literal,
                        // so none of it is DDL.
                        for _ in i..bytes.len() {
                            out.push(' ');
                        }
                        i = bytes.len();
                    }
                } else {
                    // `$1` is a parameter placeholder, not a tag — PostgreSQL
                    // rejects `$1abc$…$1abc$`, so a digit cannot START a tag.
                    out.push('$');
                    i += 1;
                }
            }
            // A double-quoted IDENTIFIER is a name — exactly what the matchers
            // are looking for — so it is copied through intact. `"comments"`
            // and `"commentable_type"` have to stay findable.
            //
            // Case is preserved here and folded everywhere else, because
            // quoting is what makes an identifier case-SENSITIVE: PostgreSQL
            // treats `"Comments"` and `comments` as different relations, so
            // lowercasing the whole file would report a table that the runtime
            // cannot find.
            b'"' => {
                out.push('"');
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            out.push_str(&sql[i..=i + 1]);
                            i += 2;
                            continue;
                        }
                        out.push('"');
                        i += 1;
                        break;
                    }
                    let ch = sql[i..].chars().next().unwrap_or('"');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let end = sql[i..].find('\n').map_or(bytes.len(), |nl| i + nl);
                // Blanked, not dropped, so offsets survive.
                for _ in i..end {
                    out.push(' ');
                }
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = sql[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |e| i + 2 + e + 2);
                for _ in i..end {
                    out.push(' ');
                }
                i = end;
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap_or(' ');
                // Unquoted SQL is case-insensitive, so it folds here — leaving
                // the quoted branches above as the only case-preserving path.
                //
                // ASCII fold specifically: `char::to_lowercase` can expand one
                // char into several (İ → i̇), which would shift every byte
                // offset after it — and offsets are load-bearing, since events
                // are ordered by position and the CREATE body is sliced by
                // index. SQL keywords are ASCII, so nothing is lost.
                out.push(ch.to_ascii_lowercase());
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Push the shared comments migration onto `plan`, unless the project already
/// has one.
///
/// Pushed on a **revert** plan too, and that is load-bearing: `Plan::revert`
/// discovers the migration directories it may remove from the plan's `Create`
/// actions under `migrations/`, so omitting it would leave the directory behind
/// after `autumn destroy scaffold`.
///
/// Returns whether the migration was emitted, so the caller can surface the
/// "already there, reusing it" case as a warning rather than silence.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] when a `comments` table exists but is not
/// the shared one (#2283). A revert never refuses.
pub fn push_commentable_migration(
    plan: &mut Plan,
    project_root: &Path,
    timestamp: &str,
    backend: autumn_web::config::DatabaseBackend,
    for_revert: bool,
) -> Result<bool, GenerateError> {
    // On a revert plan the directory is (by construction) already on disk from
    // the generate run being undone, so the table always reads as present and
    // the revert would never take it back out.
    if !for_revert {
        let table = comments_table(project_root);
        if let CommentsTable::Conflicting {
            missing,
            columns_unknown,
        } = &table
        {
            return Err(conflicting_table_error(missing, *columns_unknown));
        }
        // Absent or shared: the table is fine, but a model still bound to it
        // would insert rows without the shared columns.
        if let Some(model) = model_using_comments_table(project_root) {
            return Err(stale_model_error(project_root, &model));
        }
        if table == CommentsTable::Shared {
            return Ok(false);
        }
    }
    // …but only take out a migration this generator actually WROTE. A project
    // whose polymorphic `comments` table predates the scaffold got no migration
    // from us at all (generation skipped it), and `Plan::revert` matches by the
    // `_create_comments` suffix — so pushing the action unconditionally would
    // have `destroy scaffold` delete a hand-written migration, or block on it,
    // for a file this tool never created. `--force` would delete it outright.
    //
    // Ownership is judged by content: byte-identical to what `up_sql` emits for
    // this backend means ours. An edited copy (the author added the `author_id`
    // foreign key the header suggests, say) is left alone — leaving a file
    // behind is recoverable, deleting one is not.
    if for_revert && !generator_owned_comments_migration(project_root, backend) {
        return Ok(false);
    }
    let dir = project_root
        .join("migrations")
        .join(migration_dir_name(timestamp));
    plan.create(dir.join("up.sql"), up_sql(backend));
    plan.create(dir.join("down.sql"), down_sql());
    Ok(true)
}

/// Whether any `.rs` file under `dir`, other than `destroying_file`, declares
/// `#[commentable]`.
///
/// Recursive: model layout below `src/models/` is the app's business, not the
/// generator's, and a missed declaration here costs a surviving model its table.
fn commentable_declared_below(dir: &Path, excluding: &[std::path::PathBuf]) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if commentable_declared_below(&path, excluding) {
                return true;
            }
            continue;
        }
        // Compared as a PATH, not a basename. `src/models/admin/post.rs` is a
        // different model from `src/models/post.rs` and must still count as a
        // survivor when the flat one is destroyed — skipping it by shared
        // filename would delete the shared migration out from under it.
        if excluding.contains(&path) {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        if std::fs::read_to_string(&path).is_ok_and(|src| declares_commentable_on_comments(&src)) {
            return true;
        }
    }
    false
}

/// Whether some migration's `up.sql` is byte-identical to what [`up_sql`]
/// emits for `backend` — i.e. this generator wrote it.
#[must_use]
fn generator_owned_comments_migration(
    project_root: &Path,
    backend: autumn_web::config::DatabaseBackend,
) -> bool {
    let ours = up_sql(backend);
    let Ok(entries) = std::fs::read_dir(project_root.join("migrations")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        std::fs::read_to_string(entry.path().join("up.sql")).is_ok_and(|sql| sql == ours)
    })
}

/// Whether **another** model in `project_root` still declares
/// `#[commentable]`, ignoring `destroying_model`.
///
/// The comments table is shared, so `autumn destroy scaffold Post` in a project
/// where `Photo` is also commentable must NOT take the migration with it —
/// `Plan::revert` finds migration directories to remove from the plan's own
/// `Create` actions, so without this guard destroying one model silently breaks
/// every other one. Mirrors the `mail_unsubscribes_migration_still_needed_elsewhere`
/// guard in [`super::emit`], which exists for the same reason.
#[must_use]
pub fn another_model_is_still_commentable(project_root: &Path, destroying_model: &str) -> bool {
    let models_dir = project_root.join("src").join("models");
    let destroying_file = format!("{destroying_model}.rs");

    // Per-file layout (`src/models/<snake>.rs`), the one the generators emit —
    // walked RECURSIVELY. A hand-organised app may keep models in nested
    // modules (`src/models/admin/post.rs`), and reading a directory entry as a
    // file simply fails, so a flat scan would conclude nobody else needs the
    // shared table and delete the migration out from under a model that does.
    // The cost of the mistake is a deployment with no storage for a live model.
    if commentable_declared_below(&models_dir, &[models_dir.join(&destroying_file)]) {
        return true;
    }

    // Single-file layout (`src/models.rs`), which hand-written apps use. The
    // file being destroyed from is the same file, so count declarations: more
    // than one means somebody else still needs the table.
    std::fs::read_to_string(project_root.join("src").join("models.rs"))
        .is_ok_and(|src| src.matches("#[commentable").count() > 1)
}

/// Whether the generated model for `snake_name` declares `#[commentable]`.
///
/// `destroy scaffold Post` is typed without the field tokens the generate run
/// carried, so the revert plan cannot learn from its arguments that this model
/// brought the shared comments table. It can read the model file, which is
/// still on disk when the plan is computed — the same "recover it from what was
/// written" move the nested-resource revert makes.
#[must_use]
pub fn model_declares_commentable(project_root: &Path, snake_name: &str) -> bool {
    let src = project_root.join("src");
    if let Ok(per_file) =
        std::fs::read_to_string(src.join("models").join(format!("{snake_name}.rs")))
    {
        return per_file.contains("#[commentable");
    }
    false
}

/// The app's author model, for `#[commentable(by = <Model>)]`.
///
/// `by` is what lets the generated code resolve an author display name, and a
/// typo'd (or absent) model name would be a compile error in a file the author
/// did not write — so it is emitted **only** when the model actually exists.
/// `autumn generate auth` produces `src/models/user.rs`; a project that keeps
/// its models in one `src/models.rs` is matched by the struct declaration.
/// Anything else gets a bare `#[commentable]`, which compiles, plus a warning
/// naming the one word to add.
#[must_use]
pub fn detect_author_model(project_root: &Path) -> Option<&'static str> {
    let src = project_root.join("src");
    if src.join("models").join("user.rs").is_file() {
        return Some("User");
    }
    let single_file = std::fs::read_to_string(src.join("models.rs")).ok()?;
    single_file.contains("pub struct User ").then_some("User")
}

#[cfg(test)]
mod tests {
    use super::*;

    use autumn_web::config::DatabaseBackend;

    /// A `comments` table exists, but it is not the shared one.
    fn conflicting_comments_table(project_root: &Path) -> bool {
        matches!(
            comments_table(project_root),
            CommentsTable::Conflicting { .. }
        )
    }

    #[test]
    fn up_sql_declares_the_polymorphic_key_and_the_threading_column() {
        let sql = up_sql(DatabaseBackend::Postgres);
        assert!(sql.contains("CREATE TABLE comments"));
        assert!(
            !sql.contains("CREATE TABLE IF NOT EXISTS"),
            "a colliding table is a conflict to resolve, not a silent no-op"
        );
        assert!(sql.contains("commentable_type TEXT NOT NULL"));
        assert!(sql.contains("commentable_id BIGINT NOT NULL"));
        assert!(sql.contains("parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE"));
        assert!(sql.contains("deleted_at TIMESTAMP"));
        assert!(sql.contains("(commentable_type, commentable_id, created_at, id)"));
        // The polymorphic column must NOT gain a foreign key — a single column
        // cannot reference two tables, and pretending otherwise would break the
        // second commentable model.
        assert!(!sql.contains("commentable_id BIGINT NOT NULL REFERENCES"));
    }

    /// A `SQLite` project takes the same token, so the shared table has to be
    /// spelled for it too — `BIGSERIAL`/`NOW()` would fail `diesel migration
    /// run` on the very first migrate.
    #[test]
    fn up_sql_is_spelled_for_sqlite_too() {
        let sql = up_sql(DatabaseBackend::Sqlite);
        assert!(!sql.contains("BIGSERIAL"), "{sql}");
        assert!(!sql.contains("NOW()"), "{sql}");
        assert!(sql.contains("INTEGER PRIMARY KEY AUTOINCREMENT"), "{sql}");
        assert!(sql.contains("DEFAULT CURRENT_TIMESTAMP"), "{sql}");
        // The polymorphic key and the threading self-FK are backend-independent.
        assert!(sql.contains("commentable_type TEXT NOT NULL"), "{sql}");
        assert!(sql.contains("commentable_id INTEGER NOT NULL"), "{sql}");
        assert!(
            sql.contains("parent_id INTEGER REFERENCES comments(id) ON DELETE CASCADE"),
            "{sql}"
        );
    }

    #[test]
    fn down_sql_is_idempotent() {
        assert!(down_sql().contains("DROP TABLE IF EXISTS comments"));
    }

    /// The version must sort after the scaffold's own and after a
    /// `--counter-cache` migration taken in the same run, and before the next
    /// second's scaffold.
    #[test]
    fn migration_version_sorts_between_this_second_and_the_next() {
        let this = "20260621000000";
        let next = "20260621000001";
        let ours = migration_dir_name(this);
        assert!(ours.starts_with(this));
        assert!(this < ours.as_str());
        assert!(format!("{this}1_add_comment_count_to_posts").as_str() < ours.as_str());
        assert!(ours.as_str() < next);
    }

    #[test]
    fn detect_author_model_finds_both_model_layouts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect_author_model(tmp.path()), None);

        let per_file = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(per_file.path().join("src/models")).expect("mkdir");
        std::fs::write(per_file.path().join("src/models/user.rs"), "").expect("write");
        assert_eq!(detect_author_model(per_file.path()), Some("User"));

        let single = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(single.path().join("src")).expect("mkdir");
        std::fs::write(
            single.path().join("src/models.rs"),
            "#[autumn_web::model]\npub struct User {}\n",
        )
        .expect("write");
        assert_eq!(detect_author_model(single.path()), Some("User"));
    }

    /// Detection is by content, so a renamed directory still counts…
    #[test]
    fn already_migrated_finds_a_renamed_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_comments_v2");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));

        let empty = tempfile::tempdir().expect("tempdir");
        assert!(!already_migrated(empty.path()));
    }

    /// …and a `Comment` resource scaffolded the ordinary way does NOT, even
    /// though it produces a directory with the very same name. Matching on the
    /// name alone would skip the shared table and then fail at runtime on the
    /// missing discriminator columns.
    #[test]
    fn a_scaffolded_comment_model_does_not_look_like_the_shared_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp
            .path()
            .join("migrations")
            .join("20260820000000_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL,\n    \
             parent_id BIGINT,\n    author_id BIGINT,\n    \
             created_at TIMESTAMP,\n    deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a post_id-keyed comments table is not the polymorphic one"
        );
    }

    /// An unrelated *idempotent* `comments` table is not this migration either.
    /// The discriminator pair is what identifies it, whichever spelling of
    /// `CREATE TABLE` the file uses.
    #[test]
    fn an_unrelated_idempotent_comments_table_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE IF NOT EXISTS comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "IF NOT EXISTS does not make an unrelated table polymorphic"
        );
    }

    /// A table whose name merely *starts* with `comments` is a different
    /// table, even when it carries the discriminator columns.
    #[test]
    fn a_comments_prefixed_table_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_archive");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments_archive (\n    id BIGSERIAL PRIMARY KEY,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL,\n    id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "`comments_archive` is not `comments`"
        );

        // …while the real thing, quoted or not, still is.
        for sql in [
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
            "CREATE TABLE \"comments\" (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
            "CREATE TABLE IF NOT EXISTS comments(commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_real");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
            assert!(already_migrated(tmp.path()), "{sql}");
        }
    }

    /// A migration that *drops* the comments table has not created it, however
    /// many discriminator columns appear later in the file.
    #[test]
    fn dropping_the_comments_table_is_not_creating_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_retire");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "DROP TABLE comments;\nCREATE TABLE comments_archive (\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL,\n    id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a dropped table is not a reusable one"
        );
    }

    /// A commented-out example is not a table. A migration header explaining
    /// the shared table is a realistic place to find exactly this.
    #[test]
    fn a_commented_out_create_table_is_not_the_shared_one() {
        for sql in [
            "-- CREATE TABLE comments (\n--   commentable_type TEXT NOT NULL,\n\
             --   commentable_id BIGINT NOT NULL\n-- );\nCREATE TABLE notes (id BIGSERIAL);\n",
            "/* CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP); */\n\
             CREATE TABLE notes (id BIGSERIAL);\n",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_notes");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "a commented-out CREATE is not the shared table:\n{sql}"
            );
        }

        // The real migration still registers, comments in its header and all.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_real");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// The discriminator columns have to belong to the `comments` statement
    /// itself. A file that creates an ordinary `comments` table next to an
    /// unrelated table carrying those columns is not the shared migration.
    #[test]
    fn discriminator_columns_on_another_table_do_not_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_mixed");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n\
             CREATE TABLE audit_log (\n    id BIGSERIAL PRIMARY KEY,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL,\n    id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the discriminator columns belong to audit_log, not to comments"
        );
    }

    /// …and a column list with its own parentheses still parses.
    #[test]
    fn a_nested_paren_in_the_column_list_does_not_truncate_the_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             score NUMERIC(10, 2) NOT NULL DEFAULT 0,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL,\n    id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "NUMERIC(10, 2) must not end the column list early"
        );
    }

    /// A project that CONVERTED an existing `comments` table to polymorphic
    /// storage did it across two migrations: one created the table, a later one
    /// added the discriminator columns by `ALTER TABLE`. `examples/reddit-clone`
    /// is exactly this shape. Requiring both in one file would emit a second
    /// `CREATE TABLE comments` and fail the next `migrate`.
    #[test]
    fn a_table_made_polymorphic_by_a_later_migration_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("20260419000000_create_app");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL,\n    \
             parent_id BIGINT,\n    author_id BIGINT,\n    \
             created_at TIMESTAMP,\n    deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");

        // The CREATE alone is not the shared table…
        assert!(!already_migrated(tmp.path()));

        let converted = migrations.join("20260820000000_polymorphic_comments");
        std::fs::create_dir_all(&converted).expect("mkdir");
        std::fs::write(
            converted.join("up.sql"),
            "ALTER TABLE comments ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE comments ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");

        // …but the accumulated history is.
        assert!(
            already_migrated(tmp.path()),
            "a comments table converted by a later ALTER is still the shared table"
        );
    }

    /// …and an `ALTER` on a *different* table does not convert `comments`.
    #[test]
    fn altering_another_table_does_not_make_comments_polymorphic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let dir = migrations.join("0001_app");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, parent_id BIGINT, author_id BIGINT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE comments_archive ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE comments_archive ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "comments_archive is not comments"
        );
    }

    /// Nothing requires a conversion to add both discriminator columns in the
    /// same migration, so the two ALTERs have to accumulate across the history
    /// as well.
    #[test]
    fn discriminator_columns_altered_in_separate_migrations_still_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        for (dir, sql) in [
            (
                "0001_create",
                "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, parent_id BIGINT, author_id BIGINT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
            ),
            (
                "0002_add_type",
                "ALTER TABLE comments ADD COLUMN commentable_type TEXT;\n",
            ),
        ] {
            let path = migrations.join(dir);
            std::fs::create_dir_all(&path).expect("mkdir");
            std::fs::write(path.join("up.sql"), sql).expect("write");
        }

        // Only one of the two columns so far.
        assert!(!already_migrated(tmp.path()));

        let path = migrations.join("0003_add_id");
        std::fs::create_dir_all(&path).expect("mkdir");
        std::fs::write(
            path.join("up.sql"),
            "ALTER TABLE comments ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the columns may arrive in separate migrations"
        );
    }

    /// A history is a sequence, not a bag of facts: a later `DROP TABLE
    /// comments` undoes an earlier polymorphic create, and the generator must
    /// emit the table again rather than skip it.
    #[test]
    fn a_later_drop_undoes_an_earlier_polymorphic_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("20260101000000_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(created.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the table exists at this point"
        );

        let dropped = migrations.join("20260202000000_retire");
        std::fs::create_dir_all(&dropped).expect("mkdir");
        std::fs::write(dropped.join("up.sql"), "DROP TABLE comments;\n").expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a dropped table is not available to reuse"
        );

        // …and recreating it later brings it back.
        let again = migrations.join("20260303000000_recreate");
        std::fs::create_dir_all(&again).expect("mkdir");
        std::fs::write(again.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// Order matters *within* a file too, not only between them.
    #[test]
    fn a_drop_after_a_create_in_one_file_leaves_no_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_churn");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             DROP TABLE comments;\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        // The reverse order does leave one.
        std::fs::write(
            dir.join("up.sql"),
            "DROP TABLE IF EXISTS comments;\n\
             CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// Column names need the same identifier-boundary rule the table name
    /// gets: `legacy_commentable_type` is not `commentable_type`.
    #[test]
    fn similarly_named_columns_do_not_look_like_the_discriminator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_legacy");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             legacy_commentable_type TEXT NOT NULL,\n    \
             legacy_commentable_id BIGINT NOT NULL,\n    parent_id BIGINT,\n    author_id BIGINT,\n    body TEXT,\n    created_at TIMESTAMP,\n    deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "legacy_* columns are not the discriminator pair"
        );

        // A trailing suffix is no better than a leading one.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    commentable_type_old TEXT,\n    \
             commentable_id_old BIGINT\n);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        // …and the real columns still register, quoted or not.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    \"commentable_type\" TEXT NOT NULL,\n    \
             id BIGINT,\n    commentable_id BIGINT NOT NULL,\n    parent_id BIGINT,\n    author_id BIGINT,\n    body TEXT,\n    created_at TIMESTAMP,\n    deleted_at TIMESTAMP\n);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// An ALTER naming a discriminator column may be REMOVING it. Treating
    /// every mention as an add would let a dropped column read as present.
    #[test]
    fn dropping_or_renaming_a_discriminator_column_undoes_it() {
        let base = "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";
        for removal in [
            "ALTER TABLE comments DROP COLUMN commentable_type;\n",
            "ALTER TABLE comments RENAME COLUMN commentable_type TO legacy_kind;\n",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let migrations = tmp.path().join("migrations");
            let created = migrations.join("0001_create");
            std::fs::create_dir_all(&created).expect("mkdir");
            std::fs::write(created.join("up.sql"), base).expect("write");
            assert!(already_migrated(tmp.path()));

            let changed = migrations.join("0002_change");
            std::fs::create_dir_all(&changed).expect("mkdir");
            std::fs::write(changed.join("up.sql"), removal).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "the table is no longer polymorphic after:\n{removal}"
            );
        }

        // A rename that CREATES the column is the opposite, and must count.
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (kind TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        let renamed = migrations.join("0002_rename");
        std::fs::create_dir_all(&renamed).expect("mkdir");
        std::fs::write(
            renamed.join("up.sql"),
            "ALTER TABLE comments RENAME COLUMN kind TO commentable_type;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "a rename INTO the discriminator name adds it"
        );
    }

    /// `public.comments` is the same relation as `comments` under the default
    /// search path, so a migration spelling it that way already has the table.
    #[test]
    fn a_schema_qualified_comments_table_is_recognised() {
        for create in [
            "CREATE TABLE public.comments",
            "CREATE TABLE \"public\".\"comments\"",
            "CREATE TABLE public.\"comments\"",
            "CREATE TABLE IF NOT EXISTS public.comments",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_qualified");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(
                dir.join("up.sql"),
                format!(
                    "{create} (\n    id BIGSERIAL PRIMARY KEY,\n    \
                     commentable_type TEXT NOT NULL,\n    \
                     commentable_id BIGINT NOT NULL,\n    parent_id BIGINT,\n    author_id BIGINT,\n    body TEXT,\n    created_at TIMESTAMP,\n    deleted_at TIMESTAMP\n);\n"
                ),
            )
            .expect("write");
            assert!(already_migrated(tmp.path()), "{create}");
        }

        // A qualified DROP still undoes a qualified CREATE.
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE public.comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        let dropped = migrations.join("0002_drop");
        std::fs::create_dir_all(&dropped).expect("mkdir");
        std::fs::write(dropped.join("up.sql"), "DROP TABLE public.comments;\n").expect("write");
        assert!(!already_migrated(tmp.path()));

        // …but another schema is a different table.
        let other = tempfile::tempdir().expect("tempdir");
        let dir = other.path().join("migrations").join("0001_other");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE archive.comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(other.path()),
            "archive.comments is not public.comments"
        );
    }

    /// A comment marker inside a quoted literal is text, not a comment.
    /// Treating it as one truncated the statement and dropped the discriminator
    /// columns that followed.
    #[test]
    fn a_comment_marker_inside_a_literal_is_not_a_comment() {
        for sql in [
            "CREATE TABLE comments (note TEXT DEFAULT '--', \
             commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
            "CREATE TABLE comments (note TEXT DEFAULT '/* x */', \
             commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
            // A doubled quote is an escaped one, still inside the literal.
            "CREATE TABLE comments (note TEXT DEFAULT 'it''s -- fine', \
             commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
            // …and the same inside a quoted identifier.
            "CREATE TABLE comments (\"od--d\" TEXT, commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_literal");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
            assert!(already_migrated(tmp.path()), "{sql}");
        }

        // A real comment still hides what follows it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_commented");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             -- commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));
    }

    /// Model layout below `src/models/` is the app's business. A surviving
    /// `#[commentable]` model in a nested module still needs the shared table,
    /// and missing it costs that model its storage.
    #[test]
    fn a_commentable_model_in_a_nested_module_still_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("src").join("models").join("admin");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(
            tmp.path().join("src/models/post.rs"),
            "#[autumn_web::model]\n#[commentable(by = User)]\npub struct Post {}\n",
        )
        .expect("write");
        std::fs::write(
            nested.join("announcement.rs"),
            "#[autumn_web::model]\n#[commentable(by = User)]\npub struct Announcement {}\n",
        )
        .expect("write");

        assert!(
            another_model_is_still_commentable(tmp.path(), "post"),
            "the nested Announcement still needs the shared table"
        );

        // …and with the nested one gone, nothing else does.
        std::fs::remove_file(nested.join("announcement.rs")).expect("rm");
        assert!(!another_model_is_still_commentable(tmp.path(), "post"));

        // A same-named model in another module is a DIFFERENT model, and it
        // still needs the table. Matching by bare filename skipped it too.
        std::fs::write(
            nested.join("post.rs"),
            "#[autumn_web::model]\n#[commentable(by = User)]\npub struct Post {}\n",
        )
        .expect("write");
        assert!(
            another_model_is_still_commentable(tmp.path(), "post"),
            "`admin/post.rs` is a different file from `models/post.rs`, so destroying \
             the flat one must not take the shared migration with it"
        );
    }

    /// A string literal is data, not DDL. Every matcher downstream scans this
    /// text for keywords, names, columns and parentheses — and a literal can
    /// contain all four.
    #[test]
    fn sql_string_literals_are_not_read_as_ddl() {
        let real = "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        // A `)` inside a literal must not close the column list early.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_paren");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (note TEXT DEFAULT ')', \
             commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "a quoted ')' must not end the column list"
        );

        // A DROP inside a literal must not read as a drop.
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(created.join("up.sql"), real).expect("write");
        let logged = migrations.join("0002_log");
        std::fs::create_dir_all(&logged).expect("mkdir");
        std::fs::write(
            logged.join("up.sql"),
            "INSERT INTO audit_log(message) VALUES ('DROP TABLE comments;');\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "a DROP quoted inside a value is not a DROP"
        );

        // …but a real DROP still is one.
        std::fs::write(logged.join("up.sql"), "DROP TABLE comments;\n").expect("write");
        assert!(!already_migrated(tmp.path()));

        // A quoted IDENTIFIER is a name and must stay findable.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_quoted");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE \"comments\" (\"commentable_type\" TEXT, \"commentable_id\" BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()), "quoted identifiers are names");
    }

    /// Renaming the table away removes it just as surely as dropping it — and
    /// renaming it back restores it, columns and all (#2282).
    #[test]
    fn renaming_the_comments_table_moves_the_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));

        let renamed = migrations.join("0002_rename");
        std::fs::create_dir_all(&renamed).expect("mkdir");
        std::fs::write(
            renamed.join("up.sql"),
            "ALTER TABLE comments RENAME TO archived_comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "only archived_comments remains; `comments` has to be created again"
        );

        // Renaming a table INTO `comments` brings one back — with no columns
        // claimed, so it is not polymorphic until an ALTER adds them.
        // Renaming it back restores the polymorphic table: the rename carries
        // the source table's columns across (#2282) — the old scan landed on
        // an empty column list here and could not know them.
        let back = migrations.join("0003_rename_back");
        std::fs::create_dir_all(&back).expect("mkdir");
        std::fs::write(
            back.join("up.sql"),
            "ALTER TABLE archived_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the rename carried the discriminator columns back with the table"
        );
    }

    /// A table renamed INTO `comments` brings its columns with it (#2282).
    #[test]
    fn a_rename_into_comments_carries_the_source_columns_across() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the renamed table already carried the full discriminator schema"
        );
    }

    /// …but a rename cannot conjure columns the source never had.
    #[test]
    fn a_rename_into_comments_without_the_columns_is_not_the_shared_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (id BIGSERIAL PRIMARY KEY, body TEXT);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the source table carried no discriminator columns"
        );
        assert!(
            conflicting_comments_table(tmp.path()),
            "the name is taken, loudly, rather than silently reused"
        );
    }

    /// `RENAME TO` keeps the table in its schema: a table renamed within
    /// another schema is not the default-schema `comments`.
    #[test]
    fn a_rename_within_another_schema_stays_in_that_schema() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE archive.legacy_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, \
             parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, \
             deleted_at TIMESTAMP);\n\
             ALTER TABLE archive.legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the renamed table is archive.comments, not the shared comments table"
        );
    }

    /// A parenthesis-free `CREATE TABLE … AS` does not borrow the column list of
    /// a later statement.
    #[test]
    fn a_create_table_as_does_not_borrow_a_later_column_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments AS SELECT 1 AS id;\n\
             CREATE TABLE other_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, \
             parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, \
             deleted_at TIMESTAMP);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "legacy_comments never had the discriminator columns"
        );
    }

    /// The parens of a `CREATE TABLE … AS SELECT` query are not a column list,
    /// even when the query names every discriminator column.
    #[test]
    fn a_create_table_as_query_is_not_read_as_a_column_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments AS SELECT jsonb_build_object(\
             commentable_type, commentable_id, id, parent_id, author_id, body, \
             created_at, deleted_at) AS payload FROM source;\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the table only has `payload`, whatever its query mentions"
        );
    }

    /// A column the body only REFERENCES is not declared: `REFERENCES
    /// users(id)` does not give the table an `id` column.
    #[test]
    fn a_referenced_column_is_not_a_declared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (commentable_type TEXT, commentable_id BIGINT, \
             parent_id BIGINT, author_id BIGINT REFERENCES users(id), body TEXT, \
             created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "legacy_comments has no `id` column of its own"
        );
    }

    #[test]
    fn declares_column_reads_element_heads_only() {
        let body = "id BIGSERIAL PRIMARY KEY, \"check\" TEXT, note TEXT DEFAULT 'a, id', \
                    CONSTRAINT body_fk FOREIGN KEY (body) REFERENCES t(body), \
                    PRIMARY KEY (parent_id)";
        assert!(declares_column(body, "id"));
        assert!(declares_column(body, "check"));
        assert!(declares_column(body, "note"));
        assert!(
            !declares_column(body, "body"),
            "only named inside a constraint"
        );
        assert!(
            !declares_column(body, "parent_id"),
            "only named in PRIMARY KEY (…)"
        );
    }

    /// Columns added to the source table BEFORE the rename carry across too:
    /// the replay tracks every table, not just `comments`.
    #[test]
    fn columns_added_before_the_rename_carry_across() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (id BIGSERIAL PRIMARY KEY);\n\
             ALTER TABLE legacy_comments ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE legacy_comments ADD COLUMN commentable_id BIGINT;\n\
             ALTER TABLE legacy_comments ADD COLUMN parent_id BIGINT;\n\
             ALTER TABLE legacy_comments ADD COLUMN author_id BIGINT;\n\
             ALTER TABLE legacy_comments ADD COLUMN body TEXT;\n\
             ALTER TABLE legacy_comments ADD COLUMN created_at TIMESTAMP;\n\
             ALTER TABLE legacy_comments ADD COLUMN deleted_at TIMESTAMP;\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "ALTERs on the old name count once the table is renamed in"
        );
    }

    /// Rename in, then drop a discriminator column: not the shared table.
    #[test]
    fn dropping_a_column_after_a_rename_in_unmakes_the_shared_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n\
             ALTER TABLE comments DROP COLUMN commentable_id;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the discriminator column is gone even though the rename brought it"
        );
    }

    /// Rename in, then rename out again: no `comments` table remains.
    #[test]
    fn renaming_out_again_after_a_rename_in_leaves_no_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE legacy_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE legacy_comments RENAME TO comments;\n\
             ALTER TABLE comments RENAME TO archived_comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the table moved away again; `comments` has to be created again"
        );
    }

    /// Two renames into `comments` in one file: the last one wins.
    #[test]
    fn two_renames_into_comments_in_one_file_last_wins() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_two_renames");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE good_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             CREATE TABLE bare_comments (id BIGSERIAL PRIMARY KEY);\n\
             ALTER TABLE good_comments RENAME TO comments;\n\
             ALTER TABLE bare_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the second rename replaced the polymorphic table with a bare one"
        );

        // …and in the other order the polymorphic one stands.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE bare_comments (id BIGSERIAL PRIMARY KEY);\n\
             CREATE TABLE good_comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE bare_comments RENAME TO comments;\n\
             ALTER TABLE good_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the last rename brought the full discriminator schema"
        );
    }

    /// A rename from a table the history never created: the conservative read
    /// is "present, columns unknown" — loud, not silent.
    #[test]
    fn a_rename_from_an_unknown_table_is_present_but_not_polymorphic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the scan cannot verify columns it never saw"
        );
        assert!(
            conflicting_comments_table(tmp.path()),
            "the name is taken: generation refuses rather than claiming a reuse"
        );
    }

    /// One file may legitimately recreate the table. Recording only the first
    /// CREATE, while every DROP is recorded, ended the replay on the drop.
    #[test]
    fn every_create_in_a_file_is_replayed_not_just_the_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_recreate");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             DROP TABLE comments;\n\
             CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the LAST create is the one that stands"
        );

        // …and the reverse order still ends with no table.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             DROP TABLE comments;\n\
             CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the last create declares no discriminator columns"
        );
    }

    /// `ONLY` sits where `IF EXISTS` does, and a converted table is commonly
    /// spelled with it.
    #[test]
    fn alter_table_only_is_recognised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        let altered = migrations.join("0002_convert");
        std::fs::create_dir_all(&altered).expect("mkdir");
        std::fs::write(
            altered.join("up.sql"),
            "ALTER TABLE ONLY comments ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE ONLY comments ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "ALTER TABLE ONLY converts the table just as ALTER TABLE does"
        );

        // …and ONLY does not make another table's name match.
        let other = tempfile::tempdir().expect("tempdir");
        let dir = other.path().join("migrations").join("0001_other");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n\
             ALTER TABLE ONLY comments_archive ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE ONLY comments_archive ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(!already_migrated(other.path()));
    }

    /// A deleted parent must take its comments with it. The polymorphic key
    /// cannot be a foreign key, so a trigger is the cascade — and without one
    /// the thread outlives the parent and resurfaces if the id is reused.
    #[test]
    fn each_commentable_parent_gets_a_cleanup_trigger() {
        // The function travels WITH the trigger, not in the shared migration.
        // Diesel orders by version prefix, and the shared table's `{ts}2` sorts
        // after a parent's `{ts}` — so a trigger depending on the shared
        // migration would be created before its function existed and
        // `CREATE TRIGGER` would fail outright.
        let shared = up_sql(DatabaseBackend::Postgres);
        assert!(
            !shared.contains("comments_delete_for_parent"),
            "the shared migration must NOT be the trigger's dependency:\n{shared}"
        );
        let trigger = parent_cleanup_sql(DatabaseBackend::Postgres, "posts", "Post");
        assert!(
            trigger.contains("CREATE OR REPLACE FUNCTION comments_delete_for_parent()"),
            "the parent carries its own function, so ordering cannot break it:\n{trigger}"
        );
        assert!(
            trigger.find("CREATE OR REPLACE FUNCTION").unwrap()
                < trigger.find("CREATE TRIGGER").unwrap(),
            "and defines it BEFORE the trigger that calls it:\n{trigger}"
        );
        assert!(trigger.contains("AFTER DELETE ON posts"));

        // The trigger's discriminator is its ONLY link to the comments, and
        // nothing checks it at run time: a later `#[commentable(type_name =
        // "Article")]` on the model would leave this trigger deleting `'Post'`
        // rows that never existed, orphaning the thread silently. The generator
        // cannot see a future edit, so it says so where the editor will look.
        for backend in [DatabaseBackend::Postgres, DatabaseBackend::Sqlite] {
            let sql = parent_cleanup_sql(backend, "posts", "Post");
            assert!(
                sql.contains("KEEP IN SYNC WITH `#[commentable]` ON THIS MODEL."),
                "{backend:?} migration must say the coupling out loud:\n{sql}"
            );
            assert!(sql.contains("type_name"), "{sql}");
        }
        assert!(
            trigger.contains("EXECUTE FUNCTION comments_delete_for_parent('Post')"),
            "the discriminator is passed, so one function serves every model:\n{trigger}"
        );

        // SQLite has no parameterised trigger functions, so it inlines instead
        // — and must NOT reference the Postgres-only shared function.
        let sqlite_shared = up_sql(DatabaseBackend::Sqlite);
        assert!(
            !sqlite_shared.contains("plpgsql"),
            "no plpgsql on SQLite:\n{sqlite_shared}"
        );
        let sqlite = parent_cleanup_sql(DatabaseBackend::Sqlite, "posts", "Post");
        assert!(sqlite.contains("AFTER DELETE ON posts"));
        assert!(sqlite.contains("commentable_type = 'Post'"));
        assert!(
            !sqlite.contains("EXECUTE FUNCTION"),
            "SQLite has no EXECUTE FUNCTION:\n{sqlite}"
        );

        // The down side is backend-split: SQLite's DROP TRIGGER takes no ON.
        assert!(
            parent_cleanup_down_sql(DatabaseBackend::Postgres, "posts").contains(" ON posts;"),
            "Postgres names the table"
        );
        assert!(
            !parent_cleanup_down_sql(DatabaseBackend::Sqlite, "posts").contains(" ON "),
            "SQLite must not"
        );
    }

    /// `COLUMN` is optional in a DROP too, and one ALTER may carry several
    /// drops — only some of which remove a column.
    #[test]
    fn a_column_drop_is_recognised_with_or_without_the_column_keyword() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_d");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let create = "CREATE TABLE comments (commentable_type TEXT NOT NULL, \
                      commentable_id BIGINT NOT NULL, id BIGINT, parent_id BIGINT, \
                      author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        for drop in [
            "ALTER TABLE comments DROP COLUMN commentable_type;",
            "ALTER TABLE comments DROP commentable_type;",
            "ALTER TABLE comments DROP COLUMN IF EXISTS commentable_type;",
            // The discriminator drop trails a constraint drop in one statement.
            "ALTER TABLE comments DROP CONSTRAINT ck, DROP COLUMN commentable_type;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{drop}\n")).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "the discriminator was dropped by: {drop}"
            );
        }

        // Dropping something that is NOT a column leaves the table polymorphic
        // — including a property OF the discriminator column, which names it.
        for keep in [
            "ALTER TABLE comments ALTER COLUMN commentable_type DROP NOT NULL;",
            "ALTER TABLE comments ALTER COLUMN commentable_type DROP DEFAULT;",
            "ALTER TABLE comments DROP CONSTRAINT comments_pkey;",
            // A different column goes; the discriminator is only added here.
            "ALTER TABLE comments DROP COLUMN kind;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{keep}\n")).expect("write");
            assert!(
                already_migrated(tmp.path()),
                "the discriminator survives: {keep}"
            );
        }
    }

    /// `PostgreSQL` makes `COLUMN` optional in a column rename, so both spellings
    /// have to be read the same way — in both directions.
    #[test]
    fn a_column_rename_is_recognised_with_or_without_the_column_keyword() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_c");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let create = "CREATE TABLE comments (commentable_type TEXT NOT NULL, \
                      commentable_id BIGINT NOT NULL, id BIGINT, parent_id BIGINT, \
                      author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        // Renaming the discriminator AWAY leaves the table non-polymorphic,
        // whichever spelling the migration used.
        for rename in [
            "ALTER TABLE comments RENAME COLUMN commentable_type TO kind;",
            "ALTER TABLE comments RENAME commentable_type TO kind;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{rename}\n")).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "the discriminator was renamed away by: {rename}"
            );
        }

        // Renaming another column INTO the discriminator name ADDS it — the
        // `to` side is what decides, so neither spelling may read as a removal.
        let bare = "CREATE TABLE comments (kind TEXT NOT NULL, commentable_id BIGINT NOT NULL, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";
        for rename in [
            "ALTER TABLE comments RENAME COLUMN kind TO commentable_type;",
            "ALTER TABLE comments RENAME kind TO commentable_type;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{bare}{rename}\n")).expect("write");
            assert!(
                already_migrated(tmp.path()),
                "the discriminator was renamed INTO place by: {rename}"
            );
        }

        // A TABLE rename and a CONSTRAINT rename touch no column at all.
        std::fs::write(
            dir.join("up.sql"),
            format!("{create}ALTER TABLE comments RENAME CONSTRAINT ck TO ck2;\n"),
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "renaming a constraint leaves the columns alone"
        );
    }

    /// Backslash escapes belong to `E'…'` and to nothing else. Both halves
    /// were checked against a real `PostgreSQL`.
    #[test]
    fn backslash_escapes_are_honoured_only_in_escape_strings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_estring");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let create = "CREATE TABLE comments (id BIGINT, commentable_type TEXT, \
                      commentable_id BIGINT, parent_id BIGINT, author_id BIGINT, \
                      body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        // `E'can\'t; DROP TABLE comments;'` is ONE literal: the escaped
        // apostrophe does not end it, so nothing inside is DDL.
        for literal in [
            r"INSERT INTO audit_log(msg) VALUES (E'can\'t; DROP TABLE comments;');",
            r"INSERT INTO audit_log(msg) VALUES (e'lower\'s; DROP TABLE comments;');",
            // A backslash escaping something else, with a real quote after.
            r"INSERT INTO audit_log(msg) VALUES (E'tab\there; DROP TABLE comments;');",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{literal}\n")).expect("write");
            assert!(
                already_migrated(tmp.path()),
                "`{literal}` is one literal, not a drop"
            );
        }

        // The other half, and the one a careless fix breaks: in an ORDINARY
        // literal a backslash is DATA, so the quote still closes. PostgreSQL
        // returns `ends_with_backslash\` for the first value here — if the
        // masker swallowed onward, the following DROP would vanish.
        std::fs::write(
            dir.join("up.sql"),
            format!(
                "{create}INSERT INTO audit_log(msg) VALUES ('ends\\');\nDROP TABLE comments;\n"
            ),
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a backslash does not escape the closing quote of an ordinary literal"
        );

        // An identifier merely ENDING in `e` must not turn escaping on.
        std::fs::write(
            dir.join("up.sql"),
            format!("{create}INSERT INTO audit_log(role)VALUES('x\\');\nDROP TABLE comments;\n"),
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "`role` ending in `e` is not an E-string prefix"
        );

        // A REAL drop after a genuine E-string still registers.
        std::fs::write(
            dir.join("up.sql"),
            format!(
                "{create}INSERT INTO audit_log(msg) VALUES (E'hi\\'there');\nDROP TABLE comments;\n"
            ),
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()), "the real drop still counts");
    }

    /// `$tag$…$tag$` is a string, not DDL. Every case here was checked
    /// against a real `PostgreSQL` before being encoded.
    #[test]
    fn dollar_quoted_literals_are_not_read_as_ddl() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_dollar");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let create = "CREATE TABLE comments (id BIGINT, commentable_type TEXT, \
                      commentable_id BIGINT, parent_id BIGINT, author_id BIGINT, \
                      body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        // DDL text stored as data must not move the replay.
        for literal in [
            "INSERT INTO audit_log(msg) VALUES ($msg$DROP TABLE comments;$msg$);",
            "INSERT INTO audit_log(msg) VALUES ($$DROP TABLE comments;$$);",
            "INSERT INTO audit_log(msg) VALUES ($tag_1$DROP TABLE comments;$tag_1$);",
            // A function body is the usual home for this.
            "CREATE FUNCTION f() RETURNS void AS $do$ BEGIN DROP TABLE comments; END $do$ \
             LANGUAGE plpgsql;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{literal}\n")).expect("write");
            assert!(
                already_migrated(tmp.path()),
                "`{literal}` is data, not a drop"
            );
        }

        // A REAL drop after such a literal still registers — the masking must
        // not swallow the rest of the file.
        std::fs::write(
            dir.join("up.sql"),
            format!(
                "{create}INSERT INTO audit_log(msg) VALUES ($msg$hello$msg$);\n\
                     DROP TABLE comments;\n"
            ),
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()), "the real drop still counts");

        // `$1` is a parameter placeholder, not a tag: a digit cannot start one,
        // so the text after it must still be scanned.
        std::fs::write(
            dir.join("up.sql"),
            format!("{create}PREPARE p AS SELECT $1;\nDROP TABLE comments;\n"),
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "`$1` must not open a literal that swallows the drop"
        );
    }

    /// `DROP TABLE name [, ...]` — the shared table may sit anywhere in the
    /// list, and a similarly-named neighbour must not be mistaken for it.
    #[test]
    fn a_drop_finds_comments_anywhere_in_the_name_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_drop");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let create = "CREATE TABLE comments (id BIGINT, commentable_type TEXT, \
                      commentable_id BIGINT, parent_id BIGINT, author_id BIGINT, \
                      body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n";

        for drop in [
            "DROP TABLE comments;",
            "DROP TABLE audit_log, comments;",
            "DROP TABLE comments, audit_log;",
            "DROP TABLE IF EXISTS a, public.comments CASCADE;",
            "DROP TABLE a, \"comments\", b RESTRICT;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{drop}\n")).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "`{drop}` takes the shared table away"
            );
        }

        // A list that does NOT name it must leave the table standing —
        // including neighbours whose names merely contain "comments".
        for drop in [
            "DROP TABLE audit_log;",
            "DROP TABLE comments_archive, audit_log;",
            "DROP TABLE a, archive.comments;",
            "DROP TABLE old_comments;",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create}{drop}\n")).expect("write");
            assert!(
                already_migrated(tmp.path()),
                "`{drop}` leaves the shared table alone"
            );
        }
    }

    /// `UNLOGGED` still occupies the name; `TEMPORARY` does not. Both halves
    /// were checked against a real `PostgreSQL` before being encoded here.
    #[test]
    fn an_unlogged_comments_table_counts_and_a_temporary_one_does_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_unlogged");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let columns = "id BIGINT, commentable_type TEXT, commentable_id BIGINT, \
                       parent_id BIGINT, author_id BIGINT, body TEXT, \
                       created_at TIMESTAMP, deleted_at TIMESTAMP";

        // Persistent under a different durability setting: a later
        // `CREATE TABLE comments` would fail with "already exists".
        for create in [
            "CREATE UNLOGGED TABLE comments",
            "CREATE UNLOGGED TABLE IF NOT EXISTS comments",
            "CREATE UNLOGGED TABLE public.comments",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create} ({columns});\n")).expect("write");
            assert!(already_migrated(tmp.path()), "`{create}` occupies the name");
        }

        // A temp table lives in a session-local schema and does NOT collide,
        // so it must not suppress the shared migration.
        for create in [
            "CREATE TEMP TABLE comments",
            "CREATE TEMPORARY TABLE comments",
            "CREATE LOCAL TEMP TABLE comments",
        ] {
            std::fs::write(dir.join("up.sql"), format!("{create} ({columns});\n")).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "`{create}` is session-local and collides with nothing"
            );
        }
    }

    /// `ALTER TABLE [ IF EXISTS ] [ ONLY ] name [ * ]` — the modifiers COMBINE,
    /// and the `*` that may follow the name is part of the grammar too. Every
    /// spelling names the same table, so every one must be recognised.
    #[test]
    fn every_alter_table_modifier_combination_is_recognised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_alter");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // A table with everything BUT the discriminator pair, so the ALTER
        // under test is what completes it.
        let base = "CREATE TABLE comments (id BIGINT, parent_id BIGINT, \
                    author_id BIGINT, body TEXT, created_at TIMESTAMP, \
                    deleted_at TIMESTAMP);\n";

        for alter in [
            "ALTER TABLE comments",
            "ALTER TABLE ONLY comments",
            "ALTER TABLE IF EXISTS comments",
            "ALTER TABLE IF EXISTS ONLY comments",
            "ALTER TABLE IF EXISTS ONLY public.comments",
            // The inheritance marker sits AFTER the name.
            "ALTER TABLE comments *",
        ] {
            std::fs::write(
                dir.join("up.sql"),
                format!(
                    "{base}{alter} ADD COLUMN commentable_type TEXT;\n\
                     {alter} ADD COLUMN commentable_id BIGINT;\n"
                ),
            )
            .expect("write");
            assert!(
                already_migrated(tmp.path()),
                "`{alter}` names the shared table"
            );
        }

        // A DIFFERENT table under the same modifiers must still not count.
        for alter in [
            "ALTER TABLE IF EXISTS ONLY comments_archive",
            "ALTER TABLE ONLY archive.comments",
        ] {
            std::fs::write(
                dir.join("up.sql"),
                format!(
                    "{base}{alter} ADD COLUMN commentable_type TEXT;\n\
                     {alter} ADD COLUMN commentable_id BIGINT;\n"
                ),
            )
            .expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "`{alter}` is a different table"
            );
        }
    }

    /// The discriminator pair is not the contract. A table carrying it but
    /// missing a column the helpers query is NOT the shared table: reusing it
    /// suppresses the migration and every comment operation then fails at run
    /// time on `42703 undefined_column` — after generation said it was reusing.
    #[test]
    fn a_polymorphic_table_missing_a_required_column_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rails");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // Rails-style: the pair is there, but the author column is `user_id`.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGINT, commentable_type TEXT, \
             commentable_id BIGINT, parent_id BIGINT, user_id BIGINT, body TEXT, \
             created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "`user_id` is not `author_id`, so the helpers would 42703"
        );
        // It IS a name collision, so generation refuses rather than emitting
        // a second `CREATE TABLE comments`.
        assert!(conflicting_comments_table(tmp.path()));

        // Every other required column, one at a time, for the same reason.
        for missing in REQUIRED_COLUMNS.iter().copied() {
            let columns: Vec<String> = REQUIRED_COLUMNS
                .iter()
                .filter(|column| **column != missing)
                .map(|column| format!("{column} BIGINT"))
                .collect();
            std::fs::write(
                dir.join("up.sql"),
                format!("CREATE TABLE comments ({});\n", columns.join(", ")),
            )
            .expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "a table with no `{missing}` is not the shared table"
            );
        }

        // …and the complete set IS the shared table.
        let columns: Vec<String> = REQUIRED_COLUMNS
            .iter()
            .map(|column| format!("{column} BIGINT"))
            .collect();
        std::fs::write(
            dir.join("up.sql"),
            format!("CREATE TABLE comments ({});\n", columns.join(", ")),
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
        assert!(!conflicting_comments_table(tmp.path()));
    }

    /// A `Comment` model scaffolded the ordinary way owns a `comments` table
    /// with no discriminator columns. That is neither "already migrated" (the
    /// helpers would query columns that are not there) nor a clean slate — the
    /// names collide, so the caller must be able to say so.
    #[test]
    fn a_plain_comments_table_is_reported_as_conflicting() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, parent_id BIGINT, author_id BIGINT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()), "no discriminator columns");
        assert!(
            conflicting_comments_table(tmp.path()),
            "a same-named non-polymorphic table must be reported"
        );

        // The polymorphic table is NOT a conflict — it is the thing we reuse.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (commentable_type TEXT NOT NULL, commentable_id BIGINT NOT NULL, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
        assert!(
            !conflicting_comments_table(tmp.path()),
            "the shared table is reused, not a collision"
        );

        // And no comments table at all is neither.
        let empty = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(empty.path().join("migrations")).expect("mkdir");
        assert!(!already_migrated(empty.path()));
        assert!(!conflicting_comments_table(empty.path()));
    }

    /// Quoting makes an identifier case-SENSITIVE: `PostgreSQL` treats
    /// `"Comments"` and `comments` as different relations, so folding the whole
    /// file would report a table the runtime cannot find.
    #[test]
    fn a_quoted_case_sensitive_table_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_cased");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE \"Comments\" (commentable_type TEXT, commentable_id BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "\"Comments\" is a different relation from comments"
        );

        // Unquoted SQL stays case-INsensitive, so keywords in any case match.
        std::fs::write(
            dir.join("up.sql"),
            "CrEaTe TaBlE Comments (CommentAble_Type TEXT, COMMENTABLE_ID BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "unquoted identifiers and keywords fold"
        );

        // …and the quoted lowercase spelling is still the shared table.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE \"comments\" (\"commentable_type\" TEXT, \"commentable_id\" BIGINT, id BIGINT, parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, deleted_at TIMESTAMP);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// The shared migration must survive `destroy scaffold` while any other
    /// model still declares `#[commentable]`.
    #[test]
    fn another_commentable_model_keeps_the_shared_migration_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let models = tmp.path().join("src").join("models");
        std::fs::create_dir_all(&models).expect("mkdir");
        std::fs::write(models.join("post.rs"), "#[commentable(by = User)]\n").expect("write");
        assert!(
            !another_model_is_still_commentable(tmp.path(), "post"),
            "the model being destroyed does not count as another one"
        );

        std::fs::write(models.join("photo.rs"), "#[commentable(by = User)]\n").expect("write");
        assert!(another_model_is_still_commentable(tmp.path(), "post"));

        // Single-file layout: two declarations in one file.
        let single = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(single.path().join("src")).expect("mkdir");
        std::fs::write(
            single.path().join("src").join("models.rs"),
            "#[commentable(by = User)]\npub struct Post {}\n",
        )
        .expect("write");
        assert!(!another_model_is_still_commentable(single.path(), "post"));
        std::fs::write(
            single.path().join("src").join("models.rs"),
            "#[commentable(by = User)]\npub struct Post {}\n\
             #[commentable(by = User)]\npub struct Photo {}\n",
        )
        .expect("write");
        assert!(another_model_is_still_commentable(single.path(), "post"));
    }

    /// A plain `comments` table, as a `Comment` scaffold makes it (#2283).
    fn project_with_a_plain_comments_table() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, \
             created_at TIMESTAMP NOT NULL, updated_at TIMESTAMP NOT NULL);\n",
        )
        .expect("write");
        tmp
    }

    /// #2283: a second `CREATE TABLE comments` stops `migrate`. Generation
    /// must refuse, name the missing columns, and plan no file.
    #[test]
    fn a_plain_comments_table_blocks_the_shared_migration() {
        let tmp = project_with_a_plain_comments_table();
        let mut plan = Plan::new(tmp.path());
        let err = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Postgres,
            false,
        )
        .expect_err("a plain `comments` table must block generation");

        let message = err.to_string();
        for missing in [
            "commentable_type",
            "commentable_id",
            "parent_id",
            "author_id",
            "deleted_at",
        ] {
            assert!(message.contains(&format!("`{missing}`")), "{message}");
        }
        for present in ["`id`", "`body`", "`created_at`"] {
            assert!(!message.contains(present), "{present} exists:\n{message}");
        }
        assert!(message.contains("Rename or drop"), "{message}");
        // Adding the columns would leave the `Comment` writer inserting rows
        // without them, so the error does not offer it.
        assert!(!message.contains("Add the missing columns"), "{message}");
        assert!(message.contains("update or remove the model"), "{message}");
        assert!(plan.actions.is_empty(), "a refusal plans no file");
    }

    /// `destroy` must not refuse: it only removes what `generate` wrote.
    #[test]
    fn a_revert_ignores_a_plain_comments_table() {
        let tmp = project_with_a_plain_comments_table();
        let mut plan = Plan::new(tmp.path());
        let emitted = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Postgres,
            true,
        )
        .expect("a revert never refuses");
        assert!(!emitted, "the generator did not write this table");
        assert!(plan.actions.is_empty());
    }

    /// Detection is by columns: a table completed by a later `ALTER` is the
    /// shared one, and generation reuses it.
    #[test]
    fn a_table_completed_by_an_alter_is_reused() {
        let tmp = project_with_a_plain_comments_table();
        let dir = tmp.path().join("migrations").join("0002_adopt_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "ALTER TABLE comments\n\
             \x20   ADD COLUMN commentable_type TEXT NOT NULL,\n\
             \x20   ADD COLUMN commentable_id BIGINT NOT NULL,\n\
             \x20   ADD COLUMN parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE,\n\
             \x20   ADD COLUMN author_id BIGINT NOT NULL,\n\
             \x20   ADD COLUMN deleted_at TIMESTAMP;\n",
        )
        .expect("write");

        let mut plan = Plan::new(tmp.path());
        let emitted = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Postgres,
            false,
        )
        .expect("a complete table is reused, not refused");
        assert!(!emitted);
        assert!(plan.actions.is_empty());
    }

    /// A `SQLite` `Comment` scaffold spells the plain table differently. The
    /// check reads column names only, so it refuses the same way.
    #[test]
    fn a_plain_sqlite_comments_table_blocks_the_shared_migration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);\n",
        )
        .expect("write");
        let mut plan = Plan::new(tmp.path());
        let err = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Sqlite,
            false,
        )
        .expect_err("a plain `comments` table must block generation");
        assert!(err.to_string().contains("`commentable_type`"), "{err}");
        assert!(plan.actions.is_empty());
    }

    /// A rename from a table the history never creates hides the columns.
    /// The error says so instead of listing columns it cannot see.
    #[test]
    fn a_rename_from_an_unknown_table_says_the_columns_are_unknown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_rename_in");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "ALTER TABLE legacy_comments RENAME TO comments;\n",
        )
        .expect("write");
        let mut plan = Plan::new(tmp.path());
        let message = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Postgres,
            false,
        )
        .expect_err("the name is taken")
        .to_string();
        assert!(message.contains("cannot read"), "{message}");
        assert!(!message.contains("Add the missing columns"), "{message}");
        assert!(message.contains("Rename or drop"), "{message}");
    }

    /// `CREATE TABLE IF NOT EXISTS` does nothing when the table exists, so it
    /// must not replace the replayed table.
    #[test]
    fn a_create_if_not_exists_keeps_the_existing_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&first).expect("mkdir");
        std::fs::write(first.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        let second = tmp.path().join("migrations").join("0002_noop");
        std::fs::create_dir_all(&second).expect("mkdir");
        std::fs::write(
            second.join("up.sql"),
            "CREATE TABLE IF NOT EXISTS comments (id BIGINT, body TEXT);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the IF NOT EXISTS create was a no-op"
        );

        // On an absent table, the same statement creates it.
        std::fs::remove_dir_all(&first).expect("rm");
        assert!(conflicting_comments_table(tmp.path()));
    }

    /// Plant `migrations/<dir>/up.sql` for each pair, and a `Post` model that
    /// is still `#[commentable]`.
    fn project_with(migrations: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        for (dir, sql) in migrations {
            let dir = tmp.path().join("migrations").join(dir);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
        }
        let models = tmp.path().join("src").join("models");
        std::fs::create_dir_all(&models).expect("mkdir");
        std::fs::write(
            models.join("post.rs"),
            "#[commentable]\npub struct Post {}\n",
        )
        .expect("write");
        tmp
    }

    /// A `Comment` migration that declares every shared column is the only
    /// `CREATE TABLE comments`, so `destroy` must keep it.
    #[test]
    fn a_full_column_comment_migration_is_still_needed() {
        let full = format!(
            "CREATE TABLE comments ({});\n",
            REQUIRED_COLUMNS
                .iter()
                .map(|column| format!("{column} BIGINT"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let tmp = project_with(&[("0001_create_comments", &full)]);
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        assert!(comments_migration_still_needed(tmp.path(), &dir, &[]));

        // The `Post` model is part of the same `destroy`: nothing needs it.
        let post = tmp.path().join("src").join("models").join("post.rs");
        assert!(!comments_migration_still_needed(tmp.path(), &dir, &[post]));
    }

    /// The generator's shared table was renamed away, then a plain table was
    /// adopted. The live table starts with the plain migration.
    #[test]
    fn an_adopted_table_after_a_renamed_shared_one_is_still_needed() {
        let ours = up_sql(DatabaseBackend::Postgres);
        let tmp = project_with(&[
            ("0001_create_comments", &ours),
            (
                "0002_retire",
                "ALTER TABLE comments RENAME TO legacy_comments;\n",
            ),
            (
                "0003_create_comments",
                "CREATE TABLE comments (id BIGINT, body TEXT, created_at TIMESTAMP);\n",
            ),
            (
                "0004_adopt",
                "ALTER TABLE comments ADD COLUMN commentable_type TEXT, ADD COLUMN commentable_id BIGINT, \
                 ADD COLUMN parent_id BIGINT, ADD COLUMN author_id BIGINT, ADD COLUMN deleted_at TIMESTAMP;\n",
            ),
        ]);
        let plain = tmp.path().join("migrations").join("0003_create_comments");
        assert!(comments_migration_still_needed(tmp.path(), &plain, &[]));
    }

    /// Only a `*_create_comments` migration is ever kept.
    #[test]
    fn another_migration_is_never_kept() {
        let tmp = project_with(&[("0001_create_posts", "CREATE TABLE posts (id BIGINT);\n")]);
        let dir = tmp.path().join("migrations").join("0001_create_posts");
        assert!(!comments_migration_still_needed(tmp.path(), &dir, &[]));
    }

    /// Without the plain migration, the adoption `ALTER` runs on no table and
    /// a fresh `migrate` stops there, even though a later migration creates
    /// a new shared table. `destroy` must keep the plain migration.
    #[test]
    fn a_migration_a_later_alter_needs_is_still_needed() {
        let ours = up_sql(DatabaseBackend::Postgres);
        let tmp = project_with(&[
            (
                "0001_create_comments",
                "CREATE TABLE comments (id BIGINT, body TEXT, created_at TIMESTAMP);\n",
            ),
            (
                "0002_adopt",
                "ALTER TABLE comments ADD COLUMN commentable_type TEXT, ADD COLUMN commentable_id BIGINT, \
                 ADD COLUMN parent_id BIGINT, ADD COLUMN author_id BIGINT, ADD COLUMN deleted_at TIMESTAMP;\n",
            ),
            (
                "0003_retire",
                "ALTER TABLE comments RENAME TO legacy_comments;\n",
            ),
            ("0004_create_comments", &ours),
        ]);
        let plain = tmp.path().join("migrations").join("0001_create_comments");
        assert!(comments_migration_still_needed(tmp.path(), &plain, &[]));
    }

    /// After the rename, the `Comment` model still uses `comments`, now the
    /// shared table. Its inserts lack the shared columns, so generation must
    /// refuse until that model is removed or retargeted.
    #[test]
    fn a_model_still_on_the_comments_table_blocks_the_shared_migration() {
        let tmp = project_with_a_plain_comments_table();
        let rename = tmp.path().join("migrations").join("0002_rename");
        std::fs::create_dir_all(&rename).expect("mkdir");
        std::fs::write(
            rename.join("up.sql"),
            "ALTER TABLE comments RENAME TO notes;\n",
        )
        .expect("write");
        let models = tmp.path().join("src").join("models");
        std::fs::create_dir_all(&models).expect("mkdir");
        std::fs::write(
            models.join("comment.rs"),
            "use crate::schema::comments;\n\n#[autumn_web::model]\npub struct Comment {}\n",
        )
        .expect("write");

        let mut plan = Plan::new(tmp.path());
        let message = push_commentable_migration(
            &mut plan,
            tmp.path(),
            "20260101000000",
            DatabaseBackend::Postgres,
            false,
        )
        .expect_err("the `Comment` model still writes to `comments`")
        .to_string();
        assert!(message.contains("comment.rs"), "{message}");
        assert!(plan.actions.is_empty());

        // Retargeted at the renamed table, the model is no longer in the way.
        std::fs::write(
            models.join("comment.rs"),
            "use crate::schema::notes;\n\n#[autumn_web::model(table = \"notes\")]\npub struct Comment {}\n",
        )
        .expect("write");
        let mut plan = Plan::new(tmp.path());
        assert!(
            push_commentable_migration(
                &mut plan,
                tmp.path(),
                "20260101000000",
                DatabaseBackend::Postgres,
                false,
            )
            .expect("no model uses `comments` now")
        );
    }

    /// Without the candidate, a later plain `DROP TABLE comments` runs on no
    /// table and a fresh `migrate` stops there.
    #[test]
    fn a_migration_a_later_drop_needs_is_still_needed() {
        let ours = up_sql(DatabaseBackend::Postgres);
        let migrations = |drop: &str| {
            project_with(&[
                (
                    "0001_create_comments",
                    "CREATE TABLE comments (id BIGINT, body TEXT);\n",
                ),
                ("0002_drop", drop),
                ("0003_create_comments", &ours),
            ])
        };
        let tmp = migrations("DROP TABLE comments;\n");
        let plain = tmp.path().join("migrations").join("0001_create_comments");
        assert!(comments_migration_still_needed(tmp.path(), &plain, &[]));

        // `IF EXISTS` runs fine on no table, so the candidate is not needed.
        let tmp = migrations("DROP TABLE IF EXISTS comments;\n");
        let plain = tmp.path().join("migrations").join("0001_create_comments");
        assert!(!comments_migration_still_needed(tmp.path(), &plain, &[]));
    }

    /// The `#[model]` struct binds the table, not the import: `table = "…"`,
    /// or the name the macro infers from the struct. Imports, comments,
    /// strings and reads do not count. `examples/reddit-clone/src/models.rs`
    /// mentions `schema::comments` and maps no model to it.
    #[test]
    fn only_a_model_struct_on_the_comments_table_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        let models = src.join("models.rs");
        let check = |source: &str| {
            std::fs::write(&models, source).expect("write");
            model_using_comments_table(tmp.path())
        };

        assert_eq!(
            check(
                "// `crate::schema::comments` is kept, but no `#[model]` maps it.\n\
                 use crate::schema::{comments, posts};\n\
                 const NOTE: &str = \"crate::schema::comments;\";\n\
                 fn count() { crate::schema::comments::table; }\n\
                 #[autumn_web::model]\npub struct Post { pub id: i64 }\n",
            ),
            None
        );
        // A glob import and the inferred name.
        assert_eq!(
            check(
                "use crate::schema::*;\n#[autumn_web::model]\npub struct Comment { pub id: i64 }\n"
            ),
            Some(models.clone())
        );
        // An explicit table under another name.
        assert_eq!(
            check("#[model(table = \"comments\")]\npub struct Remark { pub id: i64 }\n"),
            Some(models.clone())
        );
        // A `Comment` model moved to another table.
        assert_eq!(
            check("#[autumn_web::model(table = \"notes\")]\npub struct Comment { pub id: i64 }\n"),
            None
        );
        // Inside an inline module too.
        assert_eq!(
            check("mod inner {\n#[autumn_web::model]\npub struct Comment { pub id: i64 }\n}\n"),
            Some(models.clone())
        );
        // A model with every shared column writes the shared table correctly.
        let fields = REQUIRED_COLUMNS
            .iter()
            .map(|column| format!("pub {column}: i64"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            check(&format!(
                "#[autumn_web::model]\npub struct Comment {{ {fields} }}\n"
            )),
            None
        );
        // Nullable and defaulted columns may be left out: inserts still work.
        assert_eq!(
            check(
                "#[autumn_web::model]\npub struct Comment { pub id: i64, \
                 pub commentable_type: String, pub commentable_id: i64, \
                 pub author_id: i64, pub body: String }\n",
            ),
            None
        );
        // A Diesel rename names the column, not the Rust field.
        let renamed = fields.replacen(
            "pub id: i64",
            "#[diesel(column_name = id)] pub comment_id: i64",
            1,
        );
        assert_eq!(
            check(&format!(
                "#[autumn_web::model]\npub struct Comment {{ {renamed} }}\n"
            )),
            None
        );
    }

    /// `ALTER TABLE IF EXISTS` on no table does nothing, so it needs no
    /// earlier `CREATE`.
    #[test]
    fn an_alter_if_exists_does_not_need_the_candidate() {
        let ours = up_sql(DatabaseBackend::Postgres);
        let tmp = project_with(&[
            (
                "0001_create_comments",
                "CREATE TABLE comments (id BIGINT, body TEXT);\n",
            ),
            (
                "0002_tweak",
                "ALTER TABLE IF EXISTS comments ADD COLUMN commentable_type TEXT;\n",
            ),
            ("0003_drop", "DROP TABLE IF EXISTS comments;\n"),
            ("0004_create_comments", &ours),
        ]);
        let plain = tmp.path().join("migrations").join("0001_create_comments");
        assert!(!comments_migration_still_needed(tmp.path(), &plain, &[]));
    }

    /// Only a live `#[commentable]` on the `comments` table needs it. A
    /// commented-out one, or one on another table, does not.
    #[test]
    fn only_a_live_commentable_on_comments_needs_the_table() {
        let tmp = project_with(&[(
            "0001_create_comments",
            "CREATE TABLE comments (id BIGINT, commentable_type TEXT, commentable_id BIGINT, \
             parent_id BIGINT, author_id BIGINT, body TEXT, created_at TIMESTAMP, \
             deleted_at TIMESTAMP);\n",
        )]);
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        let post = tmp.path().join("src").join("models").join("post.rs");
        assert!(comments_migration_still_needed(tmp.path(), &dir, &[]));

        std::fs::write(&post, "// #[commentable]\npub struct Post {}\n").expect("write");
        assert!(!comments_migration_still_needed(tmp.path(), &dir, &[]));

        std::fs::write(
            &post,
            "#[commentable(table = remarks)]\npub struct Post {}\n",
        )
        .expect("write");
        assert!(!comments_migration_still_needed(tmp.path(), &dir, &[]));
    }

    /// Any `ALTER TABLE comments` needs the table, even one on a column the
    /// shared schema does not track.
    #[test]
    fn an_alter_on_an_untracked_column_needs_the_candidate() {
        let ours = up_sql(DatabaseBackend::Postgres);
        let tmp = project_with(&[
            (
                "0001_create_comments",
                "CREATE TABLE comments (id BIGINT, body TEXT);\n",
            ),
            (
                "0002_moderate",
                "ALTER TABLE comments ADD COLUMN moderation_state TEXT;\n",
            ),
            ("0003_drop", "DROP TABLE IF EXISTS comments;\n"),
            ("0004_create_comments", &ours),
        ]);
        let plain = tmp.path().join("migrations").join("0001_create_comments");
        assert!(comments_migration_still_needed(tmp.path(), &plain, &[]));
    }
}
