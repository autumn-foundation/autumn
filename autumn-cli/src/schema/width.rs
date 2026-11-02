//! Diesel table-width audit: `autumn schema width` and the `autumn build`
//! warning.
//!
//! # Why 32
//!
//! Diesel implements its column-tuple traits (`Queryable`, `Selectable`,
//! `Insertable`, `table!`'s `AllColumns`, …) once per tuple arity, up to a cap
//! picked by a Cargo feature: `32-column-tables` (the default),
//! `64-column-tables`, or `128-column-tables`. Cargo unifies features across the
//! whole graph, so **one** table wider than 32 columns makes every crate that
//! depends on `diesel` compile — and type-check — the larger set of impls. The
//! cost is not linear. Measured on one production Autumn app's database crate:
//!
//! | tuple cap | peak rustc memory | build time |
//! |-----------|-------------------|------------|
//! | 32        | 1.16 GB           | 1m 18s     |
//! | 64        | 2.92 GB           | 2m 04s     |
//! | 72        | 3.55 GB           | 3m 26s     |
//! | 128       | 9.95 GB           | 8m 16s     |
//!
//! The cap applies to each **`table!` declaration**, not to the physical
//! table. Two declarations can map one table, so a 40-column table compiles at
//! the default cap when it is declared in halves:
//!
//! ```ignore
//! diesel::table! { wide (id) { id -> Int4, state -> Text, a -> Int4 } }
//! diesel::table! {
//!     #[sql_name = "wide"]
//!     wide_ext (id) { id -> Int4, paused -> Nullable<Text>, b -> Int4 }
//! }
//! ```
//!
//! So the framework holds every Diesel declaration it ships to
//! [`DIESEL_DEFAULT_COLUMN_LIMIT`] columns (a hard CI gate: `autumn schema
//! width .` in the `lint` job), and `autumn build` *warns* an application that
//! crosses it. The warning never
//! stops the build: a wide table is the app's call to make, it just should be
//! a call it makes knowingly. See `docs/guide/table-width.md`.
//!
//! # What it reads
//!
//! Three sources, each reported separately so the finding points at the file
//! to change:
//!
//! - **Migrations.** Every directory that holds `<version>_<name>/up.sql`
//!   steps is a migration set. Its `up.sql` files are replayed in version order
//!   through a small DDL reader (`CREATE TABLE`, `ALTER TABLE … ADD/DROP/RENAME
//!   COLUMN`, `ALTER TABLE … RENAME TO`, `DROP TABLE`), and the final width of
//!   each table is what counts. Sets replay independently: the Postgres and
//!   `SQLite` twins of one schema are two sets, not one. A wide physical table
//!   is a finding only when no `table!` or `#[model]` maps to it: then
//!   `diesel print-schema` would write one wide `table!`. Once the Rust side
//!   declares it, the declarations are what count.
//! - **`table!` blocks.** Every `diesel::table!` invocation in a `.rs` file; one
//!   column per `name -> Type` entry, mapped to its `#[sql_name]` table.
//! - **`#[model]` structs.** The columns the declarative-schema parser derives
//!   from the struct, the same view `autumn schema snapshot` takes.
//!
//! The DDL reader is deliberately small. A statement it does not understand
//! (`CREATE TABLE … AS SELECT`, `PARTITION OF`, a `DO` block) changes no width;
//! the `table!` and `#[model]` sources still see the Rust side of that table.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use autumn_schema_core::Backend;

/// The widest table Diesel's default `32-column-tables` feature supports, and
/// the widest table the framework ships.
pub const DIESEL_DEFAULT_COLUMN_LIMIT: usize = 32;

/// The diesel features that raise the tuple cap past
/// [`DIESEL_DEFAULT_COLUMN_LIMIT`], widest first.
pub const WIDE_DIESEL_FEATURES: [&str; 2] = ["128-column-tables", "64-column-tables"];

/// Where a table's width was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WidthSource {
    /// The final shape after replaying a migration set's `up.sql` files.
    Migration,
    /// A `diesel::table!` declaration.
    TableMacro,
    /// A `#[model]` struct.
    Model,
}

impl WidthSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Migration => "migrations",
            Self::TableMacro => "table!",
            Self::Model => "#[model]",
        }
    }
}

/// One table's column count, from one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableWidth {
    /// The name Rust code uses: the `table!` identifier, the `#[model]`'s
    /// Diesel table, or the migration's (schema-qualified) table.
    pub table: String,
    /// The database table it maps to: a `table!`'s `#[sql_name = "…"]` when it
    /// has one, else its name. Two declarations can share one physical table,
    /// and that is how a wide table stays under the cap.
    pub physical: String,
    /// The Cargo package that owns the file (the nearest directory with a
    /// `Cargo.toml`, relative to the scan root). A declaration maps only the
    /// migrations of its own package, so two apps in one tree that both have
    /// a `users` table never vouch for each other. Empty outside a scan.
    pub package: PathBuf,
    /// How many columns the table has.
    pub columns: usize,
    /// Where the count came from.
    pub source: WidthSource,
    /// The file to change: the migration that last widened the table, or the
    /// `.rs` file holding the `table!` / `#[model]`.
    pub path: PathBuf,
}

/// Everything the audit found under one root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WidthReport {
    /// Every table seen, from every source.
    pub tables: Vec<TableWidth>,
    /// The wide diesel features cargo resolves for this workspace (empty when
    /// diesel builds at the default cap, or when the workspace was not read).
    pub wide_diesel_features: Vec<String>,
}

impl WidthReport {
    /// The tables wider than `max`, widest first.
    ///
    /// Diesel's cap applies to each `table!` declaration, not to the physical
    /// table: a 40-column table declared as `wide` and
    /// `#[sql_name = "wide"] wide_ext` compiles at the default 32. So a
    /// `table!` or `#[model]` over `max` is always a finding, and a migration
    /// over `max` is one only when no Rust declaration maps to that table, which
    /// means a generated `schema.rs` would emit one wide `table!` for it.
    #[must_use]
    pub fn over(&self, max: usize) -> Vec<&TableWidth> {
        let declared = self.declared_physical();
        let mut over: Vec<&TableWidth> = self
            .tables
            .iter()
            .filter(|t| t.columns > max)
            .filter(|t| {
                t.source != WidthSource::Migration
                    || !declared.contains(&(t.package.clone(), t.physical.clone()))
            })
            .collect();
        over.sort_by(|a, b| {
            b.columns
                .cmp(&a.columns)
                .then_with(|| a.table.cmp(&b.table))
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.path.cmp(&b.path))
        });
        over
    }

    /// The physical tables some `table!` or `#[model]` declares. A model names
    /// a Diesel table, so it maps through that `table!`'s `sql_name`.
    fn declared_physical(&self) -> std::collections::BTreeSet<(PathBuf, String)> {
        // A `table!` keeps its own physical table: two `users` declarations in
        // different schemas map two tables. Only a `#[model]` resolves through
        // the `table!` it names, and if that name is declared more than once
        // in the package, through each of them.
        let mut macros: BTreeMap<(&Path, &str), Vec<&str>> = BTreeMap::new();
        for t in self
            .tables
            .iter()
            .filter(|t| t.source == WidthSource::TableMacro)
        {
            macros
                .entry((t.package.as_path(), t.table.as_str()))
                .or_default()
                .push(t.physical.as_str());
        }
        let mut declared = std::collections::BTreeSet::new();
        for t in &self.tables {
            match t.source {
                WidthSource::Migration => {}
                WidthSource::TableMacro => {
                    declared.insert((t.package.clone(), t.physical.clone()));
                }
                WidthSource::Model => match macros.get(&(t.package.as_path(), t.table.as_str())) {
                    Some(physicals) => declared.extend(
                        physicals
                            .iter()
                            .map(|p| (t.package.clone(), (*p).to_owned())),
                    ),
                    None => {
                        declared.insert((t.package.clone(), t.physical.clone()));
                    }
                },
            }
        }
        declared
    }

    /// Whether `t` is a migration whose table some Rust declaration maps:
    /// its width is then the declarations' business, not its own.
    #[must_use]
    pub fn is_declared_migration(&self, t: &TableWidth) -> bool {
        t.source == WidthSource::Migration
            && self
                .declared_physical()
                .contains(&(t.package.clone(), t.physical.clone()))
    }

    /// The widest table that counts toward the cap, if any: every `table!` and
    /// `#[model]`, and each migration table no declaration maps.
    #[must_use]
    pub fn widest(&self) -> Option<&TableWidth> {
        let declared = self.declared_physical();
        self.tables
            .iter()
            .filter(|t| {
                t.source != WidthSource::Migration
                    || !declared.contains(&(t.package.clone(), t.physical.clone()))
            })
            .max_by_key(|t| t.columns)
    }

    /// Whether anything is over `max` or a wide diesel feature is on.
    #[must_use]
    pub fn has_findings(&self, max: usize) -> bool {
        !self.over(max).is_empty() || !self.features_over(max).is_empty()
    }

    /// The resolved wide diesel features whose cap is above `max`. At the
    /// default 32 both are findings; under `--max 64` only `128-column-tables`
    /// is, so an app that chose the 64 cap on purpose can still gate on it.
    #[must_use]
    pub fn features_over(&self, max: usize) -> Vec<&str> {
        self.wide_diesel_features
            .iter()
            .map(String::as_str)
            .filter(|f| feature_cap(f).is_none_or(|cap| cap > max))
            .collect()
    }
}

/// The tuple cap a diesel `<N>-column-tables` feature sets.
fn feature_cap(feature: &str) -> Option<usize> {
    feature.strip_suffix("-column-tables")?.parse().ok()
}

// ── SQL ────────────────────────────────────────────────────────────────────

/// A DDL token. Keywords are compared on [`Tok::Word`] only, so a quoted
/// identifier named `"table"` is never read as the keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// An unquoted word, lowercased.
    Word(String),
    /// A `"quoted"` identifier, case kept.
    Quoted(String),
    /// A string literal or a dollar-quoted body; its contents never matter.
    Literal,
    /// Any other single character.
    Punct(char),
}

impl Tok {
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self, Self::Word(w) if w == kw)
    }

    const fn is_punct(&self, c: char) -> bool {
        matches!(self, Self::Punct(p) if *p == c)
    }

    fn ident(&self) -> Option<&str> {
        match self {
            Self::Word(w) | Self::Quoted(w) => Some(w),
            _ => None,
        }
    }
}

/// Read a dollar-quote tag (`$$`, `$body$`) at `start`, returning the tag
/// including both dollars.
fn dollar_tag(chars: &[char], start: usize) -> Option<String> {
    if chars.get(start) != Some(&'$') {
        return None;
    }
    let mut end = start + 1;
    while let Some(&c) = chars.get(end) {
        if c == '$' {
            let tag: String = chars[start..=end].iter().collect();
            // `$1` is a parameter, not a tag: a tag cannot start with a digit.
            if tag.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
            return Some(tag);
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            end += 1;
        } else {
            return None;
        }
    }
    None
}

/// Tokenize SQL, dropping comments and folding literals to [`Tok::Literal`].
#[allow(clippy::too_many_lines)]
fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // Postgres block comments nest.
            let mut depth = 0usize;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == '\'' {
            // `''` is an escaped quote inside the literal.
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            toks.push(Tok::Literal);
        } else if c == '"' {
            let mut name = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        name.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                name.push(chars[i]);
                i += 1;
            }
            toks.push(Tok::Quoted(name));
        } else if let Some(tag) = dollar_tag(&chars, i) {
            let tag_chars: Vec<char> = tag.chars().collect();
            i += tag_chars.len();
            while i < chars.len() && !chars[i..].starts_with(&tag_chars) {
                i += 1;
            }
            i = (i + tag_chars.len()).min(chars.len());
            toks.push(Tok::Literal);
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut word = String::new();
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                word.push(chars[i].to_ascii_lowercase());
                i += 1;
            }
            toks.push(Tok::Word(word));
        } else {
            toks.push(Tok::Punct(c));
            i += 1;
        }
    }
    toks
}

/// Read a possibly schema-qualified name at `i`. Returns the replay key and
/// the index after the name.
///
/// The key keeps the schema, so `a.t` and `b.t` are two tables, and drops a
/// leading `public.`, so `public.t` and `t` stay one (the default
/// `search_path`).
fn read_name(toks: &[Tok], mut i: usize) -> Option<(String, usize)> {
    let mut segments = vec![toks.get(i)?.ident()?];
    i += 1;
    while toks.get(i).is_some_and(|t| t.is_punct('.')) {
        segments.push(toks.get(i + 1)?.ident()?);
        i += 2;
    }
    if segments.len() > 1 && segments[0] == "public" {
        segments.remove(0);
    }
    Some((segments.join("."), i))
}

/// Skip the keyword sequence `kws` at `i` if every keyword is there.
fn skip_kws(toks: &[Tok], i: usize, kws: &[&str]) -> usize {
    if kws
        .iter()
        .enumerate()
        .all(|(k, kw)| toks.get(i + k).is_some_and(|t| t.is_kw(kw)))
    {
        i + kws.len()
    } else {
        i
    }
}

/// Split `toks` at depth-0 occurrences of `sep`.
fn split_top_level(toks: &[Tok], sep: char) -> Vec<&[Tok]> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t {
            // `[` nests too: `DEFAULT ARRAY[1, 2]` is one column, not two.
            Tok::Punct('(' | '[') => depth += 1,
            Tok::Punct(')' | ']') => depth = depth.saturating_sub(1),
            Tok::Punct(c) if *c == sep && depth == 0 => {
                parts.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&toks[start..]);
    parts
}

/// The index of the `)` closing the `(` at `open`.
fn close_paren(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Table-level elements of `CREATE TABLE (…)` and `ALTER TABLE … ADD` that are
/// not columns.
const CONSTRAINT_KWS: [&str; 7] = [
    "constraint",
    "primary",
    "unique",
    "check",
    "foreign",
    "exclude",
    "like",
];

fn starts_constraint(element: &[Tok]) -> bool {
    element
        .first()
        .is_some_and(|t| CONSTRAINT_KWS.iter().any(|kw| t.is_kw(kw)))
}

/// The tables of one migration set as its migrations are replayed.
#[derive(Debug, Default)]
struct Replay {
    /// table → (column names in order, the migration that last widened it).
    tables: BTreeMap<String, (Vec<String>, PathBuf)>,
}

impl Replay {
    fn add_column(&mut self, table: &str, column: String, origin: &Path) {
        if let Some((columns, last)) = self.tables.get_mut(table)
            && !columns.contains(&column)
        {
            columns.push(column);
            origin.clone_into(last);
        }
    }

    fn drop_column(&mut self, table: &str, column: &str) {
        if let Some((columns, _)) = self.tables.get_mut(table) {
            columns.retain(|c| c != column);
        }
    }

    fn rename_column(&mut self, table: &str, from: &str, to: String) {
        if let Some((columns, _)) = self.tables.get_mut(table)
            && let Some(slot) = columns.iter_mut().find(|c| *c == from)
        {
            *slot = to;
        }
    }

    /// Apply one migration's SQL.
    fn apply(&mut self, sql: &str, origin: &Path) {
        let toks = tokenize(sql);
        for stmt in split_top_level(&toks, ';') {
            self.apply_statement(stmt, origin);
        }
    }

    fn apply_statement(&mut self, stmt: &[Tok], origin: &Path) {
        let Some(first) = stmt.first() else { return };
        if first.is_kw("create") {
            self.apply_create(stmt, origin);
        } else if first.is_kw("alter") && stmt.get(1).is_some_and(|t| t.is_kw("table")) {
            self.apply_alter(stmt, origin);
        } else if first.is_kw("drop") && stmt.get(1).is_some_and(|t| t.is_kw("table")) {
            let i = skip_kws(stmt, 2, &["if", "exists"]);
            for part in split_top_level(&stmt[i..], ',') {
                if let Some((name, _)) = read_name(part, 0) {
                    self.tables.remove(&name);
                }
            }
        }
    }

    fn apply_create(&mut self, stmt: &[Tok], origin: &Path) {
        let mut i = 1;
        // A temporary table is not part of the schema Diesel sees.
        if stmt
            .get(i)
            .is_some_and(|t| t.is_kw("temp") || t.is_kw("temporary"))
            || stmt
                .get(i + 1)
                .is_some_and(|t| t.is_kw("temp") || t.is_kw("temporary"))
        {
            return;
        }
        i = skip_kws(stmt, i, &["global"]);
        i = skip_kws(stmt, i, &["local"]);
        i = skip_kws(stmt, i, &["unlogged"]);
        if !stmt.get(i).is_some_and(|t| t.is_kw("table")) {
            return;
        }
        let if_not_exists = skip_kws(stmt, i + 1, &["if", "not", "exists"]);
        let guarded = if_not_exists != i + 1;
        i = if_not_exists;
        let Some((name, after)) = read_name(stmt, i) else {
            return;
        };
        // The database leaves an existing table alone, so the replay does too.
        if guarded && self.tables.contains_key(&name) {
            return;
        }
        // `CREATE TABLE x AS …` / `PARTITION OF …` carry no column list.
        if !stmt.get(after).is_some_and(|t| t.is_punct('(')) {
            return;
        }
        let Some(close) = close_paren(stmt, after) else {
            return;
        };
        let mut columns = Vec::new();
        for element in split_top_level(&stmt[after + 1..close], ',') {
            if starts_constraint(element) {
                continue;
            }
            if let Some(column) = element.first().and_then(Tok::ident)
                && !columns.iter().any(|c| c == column)
            {
                columns.push(column.to_owned());
            }
        }
        self.tables.insert(name, (columns, origin.to_path_buf()));
    }

    fn apply_alter(&mut self, stmt: &[Tok], origin: &Path) {
        let mut i = skip_kws(stmt, 2, &["if", "exists"]);
        i = skip_kws(stmt, i, &["only"]);
        let Some((mut table, after)) = read_name(stmt, i) else {
            return;
        };
        for action in split_top_level(&stmt[after..], ',') {
            let Some(verb) = action.first() else { continue };
            if verb.is_kw("add") {
                if starts_constraint(&action[1..]) {
                    continue;
                }
                let mut j = skip_kws(action, 1, &["column"]);
                j = skip_kws(action, j, &["if", "not", "exists"]);
                if let Some(column) = action.get(j).and_then(Tok::ident) {
                    self.add_column(&table, column.to_owned(), origin);
                }
            } else if verb.is_kw("drop") {
                if action.get(1).is_some_and(|t| t.is_kw("constraint")) {
                    continue;
                }
                let mut j = skip_kws(action, 1, &["column"]);
                j = skip_kws(action, j, &["if", "exists"]);
                if let Some(column) = action.get(j).and_then(Tok::ident) {
                    self.drop_column(&table, column);
                }
            } else if verb.is_kw("rename") {
                if action.get(1).is_some_and(|t| t.is_kw("constraint")) {
                    continue;
                }
                if action.get(1).is_some_and(|t| t.is_kw("to")) {
                    if let Some((to, _)) = read_name(action, 2)
                        && let Some(state) = self.tables.remove(&table)
                    {
                        // `RENAME TO` names no schema: the table stays in its
                        // own, so `a.t RENAME TO u` is `a.u`.
                        let to = match table.rsplit_once('.') {
                            Some((schema, _)) if !to.contains('.') => format!("{schema}.{to}"),
                            _ => to,
                        };
                        self.tables.insert(to.clone(), state);
                        table = to;
                    }
                    continue;
                }
                let j = skip_kws(action, 1, &["column"]);
                if let (Some(from), Some(true), Some(to)) = (
                    action.get(j).and_then(Tok::ident),
                    action.get(j + 1).map(|t| t.is_kw("to")),
                    action.get(j + 2).and_then(Tok::ident),
                ) {
                    self.rename_column(&table, from, to.to_owned());
                }
            }
        }
    }
}

/// Replay one migration set — `(up.sql path, its SQL)` in version order — and
/// return each table's final width.
#[must_use]
pub fn replay_migration_set(steps: &[(PathBuf, String)]) -> Vec<TableWidth> {
    let mut replay = Replay::default();
    for (path, sql) in steps {
        replay.apply(sql, path);
    }
    replay
        .tables
        .into_iter()
        .map(|(table, (columns, path))| TableWidth {
            // The replay key: schema kept, `public.` dropped, so `a.users`
            // and `b.users` stay two tables when matched to declarations.
            physical: table.clone(),
            package: PathBuf::new(),
            table,
            columns: columns.len(),
            source: WidthSource::Migration,
            path,
        })
        .collect()
}

// ── Rust ───────────────────────────────────────────────────────────────────

/// One table in a `table!` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MacroTable {
    /// The Rust identifier.
    name: String,
    /// The `#[sql_name]`, else the identifier.
    physical: String,
    /// One per `name -> Type` entry.
    columns: usize,
}

/// Count the columns of every table in one `table!` invocation's tokens.
///
/// A table is `[#[attrs]] [schema.]name [(pk, …)] { col -> Type, … }`; one
/// invocation can hold several, plus `use` lines. A column is one top-level
/// `->` inside the brace group.
fn table_macro_tables(tokens: proc_macro2::TokenStream) -> Vec<MacroTable> {
    use proc_macro2::{Delimiter, Spacing, TokenTree};

    let mut out = Vec::new();
    // `b.users` is the path `["b", "users"]`; a bare `users` is one segment.
    let mut path: Vec<String> = Vec::new();
    let mut prev_dot = false;
    let mut sql_name: Option<String> = None;
    for tree in tokens {
        let is_dot = matches!(&tree, TokenTree::Punct(p) if p.as_char() == '.');
        match tree {
            TokenTree::Ident(ident) => {
                if !prev_dot {
                    path.clear();
                }
                path.push(ident.to_string().trim_start_matches("r#").to_owned());
            }
            // `#[sql_name = "wide"]` before the table: the physical name.
            TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                if let [
                    TokenTree::Ident(key),
                    TokenTree::Punct(eq),
                    TokenTree::Literal(lit),
                ] = inner.as_slice()
                    && key == "sql_name"
                    && eq.as_char() == '='
                {
                    sql_name = Some(lit.to_string().trim_matches('"').to_owned());
                }
            }
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                let mut columns = 0;
                let mut prev_minus = false;
                for inner in group.stream() {
                    let is_arrow_head =
                        matches!(&inner, TokenTree::Punct(p) if p.as_char() == '>' && prev_minus);
                    if is_arrow_head {
                        columns += 1;
                    }
                    prev_minus = matches!(
                        &inner,
                        TokenTree::Punct(p) if p.as_char() == '-' && p.spacing() == Spacing::Joint
                    );
                }
                if let Some((name, schema)) = path.split_last() {
                    let table = sql_name.take().unwrap_or_else(|| name.clone());
                    // Same key shape as the migration replay: the schema
                    // stays, `public` goes.
                    let physical = match schema {
                        [] => table,
                        [s] if s == "public" => table,
                        _ => format!("{}.{table}", schema.join(".")),
                    };
                    out.push(MacroTable {
                        name: name.clone(),
                        physical,
                        columns,
                    });
                }
                path.clear();
                sql_name = None;
            }
            // The `(pk)` group leaves the name alone; `;` (after a `use`
            // line) resets it.
            TokenTree::Punct(p) if p.as_char() == ';' => {
                path.clear();
                sql_name = None;
            }
            _ => {}
        }
        prev_dot = is_dot;
    }
    out
}

/// Visit every `table!` macro in a parsed file, nested modules included.
fn collect_table_macros(items: &[syn::Item], out: &mut Vec<MacroTable>) {
    for item in items {
        match item {
            syn::Item::Macro(m)
                if m.mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|seg| seg.ident == "table") =>
            {
                out.extend(table_macro_tables(m.mac.tokens.clone()));
            }
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_table_macros(nested, out);
                }
            }
            _ => {}
        }
    }
}

/// The widths one `.rs` file declares: its `table!` blocks and its `#[model]`
/// structs. A file that is not valid Rust yields nothing (rustc reports it).
#[must_use]
pub fn rust_source_widths(path: &Path, src: &str) -> Vec<TableWidth> {
    let mut out = Vec::new();
    let mentions_table = src.contains("table!");
    // `#[model]`, `#[model(…)]`, `#[autumn_web::model]`: cheap text filter so
    // only a file that can hold a model pays for a parse.
    let mentions_model = src.contains("model]") || src.contains("model(");
    if !mentions_table && !mentions_model {
        return out;
    }
    if mentions_table && let Ok(file) = syn::parse_file(src) {
        let mut tables = Vec::new();
        collect_table_macros(&file.items, &mut tables);
        out.extend(tables.into_iter().map(|t| TableWidth {
            table: t.name,
            physical: t.physical,
            package: PathBuf::new(),
            columns: t.columns,
            source: WidthSource::TableMacro,
            path: path.to_path_buf(),
        }));
    }
    if mentions_model {
        out.extend(model_widths(path, src));
        // `parse_model_source` reads top-level items only; a `#[model]` inside
        // an inline `mod x { … }` is re-read from that module's tokens.
        if let Ok(file) = syn::parse_file(src) {
            let mut modules = Vec::new();
            collect_inline_modules(&file.items, &mut modules);
            for items in modules {
                let tokens = quote::quote! { #(#items)* }.to_string();
                out.extend(model_widths(path, &tokens));
            }
        }
    }
    out
}

/// The `#[model]` widths among the top-level items of `src`.
///
/// Each model is parsed on its own: the parser's implicit-column and
/// diagnostic records are keyed by table, so two models of one table in one
/// file would otherwise count each other's.
fn model_widths(path: &Path, src: &str) -> Vec<TableWidth> {
    let Ok(file) = syn::parse_file(src) else {
        return Vec::new();
    };
    file.items
        .iter()
        .filter(|item| {
            matches!(item, syn::Item::Struct(st) if st.attrs.iter().any(|a| {
                a.path().segments.last().is_some_and(|seg| seg.ident == "model")
            }))
        })
        .flat_map(|item| single_model_width(path, &quote::quote! { #item }.to_string()))
        .collect()
}

/// The width of the one `#[model]` in `src`.
fn single_model_width(path: &Path, src: &str) -> Vec<TableWidth> {
    let mut out = Vec::new();
    let Ok(parsed) = super::parse::parse_model_source(src, Backend::Postgres) else {
        return out;
    };
    for table in parsed.tables {
        // The parser adds a conventional `created_at` the macro never reads.
        let implicit = parsed
            .implicit_columns
            .iter()
            .filter(|(t, _)| *t == table.name)
            .count();
        // A field whose SQL type the parser cannot rebuild (a custom enum)
        // is skipped from `columns` with a diagnostic, but it is still a
        // column the macro's Diesel derives carry. Width needs no type.
        let mut skipped: Vec<&str> = parsed
            .diagnostics
            .iter()
            .filter(|d| d.table == table.name)
            .map(|d| d.field.as_str())
            .filter(|f| !table.columns.iter().any(|c| c.name == *f))
            .collect();
        skipped.sort_unstable();
        skipped.dedup();
        out.push(TableWidth {
            columns: table.columns.len().saturating_sub(implicit) + skipped.len(),
            physical: table.name.clone(),
            package: PathBuf::new(),
            table: table.name,
            source: WidthSource::Model,
            path: path.to_path_buf(),
        });
    }
    out
}

/// Every inline module's item list, at any depth.
fn collect_inline_modules<'a>(items: &'a [syn::Item], out: &mut Vec<&'a [syn::Item]>) {
    for item in items {
        if let syn::Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            out.push(nested);
            collect_inline_modules(nested, out);
        }
    }
}

// ── Discovery ──────────────────────────────────────────────────────────────

/// Whether the walk skips a directory: build output, VCS and tool state, and
/// vendored JavaScript.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules" | "dist")
}

/// The `up.sql` steps of `dir` in version order, when `dir` is a migration set.
fn migration_steps(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut steps: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path().join("up.sql"))
        .filter(|up| up.is_file())
        .collect();
    steps.sort();
    steps
}

/// Every migration set and every `.rs` file under `root`.
fn discover(root: &Path) -> (Vec<Vec<PathBuf>>, Vec<PathBuf>) {
    let mut sets = Vec::new();
    let mut rust = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let steps = migration_steps(&dir);
        if !steps.is_empty() {
            sets.push(steps);
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(skip_dir)
                {
                    stack.push(path);
                }
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "rs") {
                rust.push(path);
            }
        }
    }
    sets.sort();
    rust.sort();
    (sets, rust)
}

/// Every table width under `root`: migration sets, `table!` blocks and
/// `#[model]` structs. Paths are reported relative to `root`.
#[must_use]
pub fn scan_tables(root: &Path) -> Vec<TableWidth> {
    let relative = |p: &Path| p.strip_prefix(root).unwrap_or(p).to_path_buf();
    let (sets, rust) = discover(root);
    let mut tables = Vec::new();
    for set in sets {
        let steps: Vec<(PathBuf, String)> = set
            .into_iter()
            .filter_map(|p| {
                let sql = std::fs::read_to_string(&p).ok()?;
                Some((relative(&p), sql))
            })
            .collect();
        tables.extend(replay_migration_set(&steps));
    }
    for path in rust {
        if let Ok(src) = std::fs::read_to_string(&path) {
            tables.extend(rust_source_widths(&relative(&path), &src));
        }
    }
    let mut packages: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for t in &mut tables {
        let dir = t.path.parent().map(Path::to_path_buf).unwrap_or_default();
        packages
            .entry(dir.clone())
            .or_insert_with(|| owning_package(root, &dir))
            .clone_into(&mut t.package);
    }
    tables
}

/// The nearest directory at or above `dir` (relative to `root`) that holds a
/// `Cargo.toml`, relative to `root`; empty (the root) when none does.
fn owning_package(root: &Path, dir: &Path) -> PathBuf {
    let mut current = Some(dir);
    while let Some(d) = current {
        if root.join(d).join("Cargo.toml").is_file() {
            return d.to_path_buf();
        }
        current = d.parent();
    }
    PathBuf::new()
}

// ── Cargo ──────────────────────────────────────────────────────────────────

/// The wide diesel features cargo resolves, read from full `cargo metadata`
/// output (`resolve.nodes[].features`). Widest first, deduplicated.
#[must_use]
pub fn wide_diesel_features(metadata: &serde_json::Value) -> Vec<String> {
    let diesel_ids: Vec<&str> = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["name"].as_str() == Some("diesel"))
        .filter_map(|p| p["id"].as_str())
        .collect();
    let mut found = Vec::new();
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if !node["id"]
            .as_str()
            .is_some_and(|id| diesel_ids.contains(&id))
        {
            continue;
        }
        for feature in node["features"].as_array().into_iter().flatten() {
            if let Some(f) = feature.as_str()
                && WIDE_DIESEL_FEATURES.contains(&f)
                && !found.iter().any(|x: &String| x == f)
            {
                found.push(f.to_owned());
            }
        }
    }
    found.sort_by_key(|f| WIDE_DIESEL_FEATURES.iter().position(|w| w == f));
    found
}

/// Ask cargo which diesel features the workspace at `root` resolves. `None`
/// when there is no manifest or cargo cannot answer (offline, broken lock):
/// the table scan still stands on its own.
fn resolve_wide_diesel_features(root: &Path) -> Option<Vec<String>> {
    if !root.join("Cargo.toml").is_file() {
        return None;
    }
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(wide_diesel_features(&metadata))
}

/// Scan `root`: every table width, plus the diesel features cargo resolves.
#[must_use]
pub fn scan(root: &Path) -> WidthReport {
    WidthReport {
        tables: scan_tables(root),
        wide_diesel_features: resolve_wide_diesel_features(root).unwrap_or_default(),
    }
}

// ── Reporting ──────────────────────────────────────────────────────────────

/// Where the warning sends an app: the app has no `docs/` of its own.
const GUIDE: &str =
    "https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/guide/table-width.md";

fn format_findings(report: &WidthReport, max: usize, out: &mut String) {
    for t in report.over(max) {
        let _ = writeln!(
            out,
            "  {} has {} columns ({} > {max}) — {}: {}",
            t.table,
            t.columns,
            t.columns,
            t.source.label(),
            t.path.display()
        );
        if t.source == WidthSource::Migration {
            let _ = writeln!(
                out,
                "    no `table!` maps to it, so a generated schema.rs declares all {} columns \
                 in one `table!`",
                t.columns
            );
        }
    }
    for feature in report.features_over(max) {
        let _ = writeln!(
            out,
            "  diesel is built with `{feature}`: every crate that depends on diesel compiles \
             the wider tuple impls"
        );
    }
}

/// The `autumn build` warning, or `None` when every table fits Diesel's
/// default cap and no wide feature is on. Never an error: the app decides.
#[must_use]
pub fn format_build_warning(report: &WidthReport) -> Option<String> {
    let max = DIESEL_DEFAULT_COLUMN_LIMIT;
    if !report.has_findings(max) {
        return None;
    }
    let mut out = format!(
        "warning: Diesel table width over {max} columns\n\
         \x20 A Diesel `table!` wider than {max} columns needs diesel's `64-column-tables` or \
         `128-column-tables` feature,\n\
         \x20 and that feature makes every crate that uses diesel slower and hungrier to \
         compile\n\
         \x20 (measured: 32 → 1.2 GB / 1m18s, 64 → 2.9 GB / 2m04s, 128 → 10 GB / 8m16s).\n"
    );
    format_findings(report, max, &mut out);
    let _ = write!(
        out,
        "  Split the Diesel declaration: a second `table!` with `#[sql_name = \"<table>\"]`\n\
         \x20 maps the same table, and each declaration stays at {max} or fewer.\n\
         \x20 The build continues. `autumn schema width` re-runs this check.\n\
         \x20 See {GUIDE}"
    );
    Some(out)
}

/// Run `autumn schema width`: print every table over `max`, and exit non-zero
/// when there is one or a wide diesel feature is resolved. With `all`, print
/// every table, not only the wide ones.
///
/// # Errors
///
/// Returns the failure summary when the audit finds anything.
pub fn run_width(root: &Path, max: usize, all: bool) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let report = scan(root);
    if all {
        let mut tables: Vec<&TableWidth> = report.tables.iter().collect();
        tables.sort_by(|a, b| {
            b.columns
                .cmp(&a.columns)
                .then_with(|| a.table.cmp(&b.table))
        });
        for t in tables {
            // A wide table the Rust side splits is listed, but marked: its
            // declarations are what Diesel compiles.
            let note = if report.is_declared_migration(t) {
                "  (declared in Rust)"
            } else {
                ""
            };
            println!(
                "{:>4}  {:<40} {:<10} {}{note}",
                t.columns,
                t.table,
                t.source.label(),
                t.path.display()
            );
        }
    }
    if report.has_findings(max) {
        let mut out = format!("Diesel table width: findings over the {max}-column cap\n");
        format_findings(&report, max, &mut out);
        let _ = write!(
            out,
            "Keep every table at {max} columns or fewer so diesel builds with its default \
             `32-column-tables` feature. See {GUIDE}."
        );
        return Err(out);
    }
    let widest = report.widest().map_or_else(String::new, |t| {
        format!(" (widest: {} at {})", t.table, t.columns)
    });
    println!(
        "\u{2713} {} table declarations, all at {max} columns or fewer{widest}",
        report.tables.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(sql_steps: &[&str]) -> BTreeMap<String, usize> {
        let steps: Vec<(PathBuf, String)> = sql_steps
            .iter()
            .enumerate()
            .map(|(i, s)| (PathBuf::from(format!("{i:02}_m/up.sql")), (*s).to_owned()))
            .collect();
        replay_migration_set(&steps)
            .into_iter()
            .map(|t| (t.table, t.columns))
            .collect()
    }

    fn wide_create(name: &str, columns: usize) -> String {
        let cols: Vec<String> = (0..columns).map(|i| format!("c{i} TEXT")).collect();
        format!("CREATE TABLE {name} ({});", cols.join(", "))
    }

    #[test]
    fn create_table_counts_columns_not_constraints() {
        let w = widths(&["CREATE TABLE IF NOT EXISTS public.\"Posts\" (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL CHECK (length(title) > 0),
            body TEXT DEFAULT 'a, b; c',
            score NUMERIC(10, 2),
            CONSTRAINT posts_title UNIQUE (title),
            PRIMARY KEY (id),
            UNIQUE (title, body),
            CHECK (score >= 0),
            FOREIGN KEY (id) REFERENCES other(id)
        );"]);
        assert_eq!(w.get("Posts"), Some(&4));
    }

    #[test]
    fn alter_add_drop_rename_replay_in_order() {
        let w = widths(&[
            "CREATE TABLE jobs (id BIGINT, name TEXT);",
            "ALTER TABLE jobs ADD COLUMN attempts INT, ADD COLUMN IF NOT EXISTS name TEXT, \
             ADD CONSTRAINT jobs_pk PRIMARY KEY (id);",
            "ALTER TABLE ONLY jobs ADD trace TEXT; ALTER TABLE jobs DROP COLUMN IF EXISTS name;",
            "ALTER TABLE jobs RENAME COLUMN trace TO trace_id; ALTER TABLE jobs RENAME TO tasks;",
            "ALTER TABLE tasks DROP CONSTRAINT jobs_pk; ALTER TABLE tasks ADD COLUMN extra TEXT;",
        ]);
        assert_eq!(w.get("jobs"), None);
        // id, attempts, trace_id, extra
        assert_eq!(w.get("tasks"), Some(&4));
    }

    #[test]
    fn drop_table_removes_it_and_temp_tables_never_count() {
        let w = widths(&[
            "CREATE TABLE a (x INT); CREATE TABLE b (y INT);",
            "DROP TABLE IF EXISTS a, b CASCADE;",
            "CREATE TEMP TABLE scratch (z INT); CREATE TEMPORARY TABLE s2 (z INT);",
        ]);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn comments_strings_and_function_bodies_are_not_ddl() {
        let w = widths(&["-- CREATE TABLE fake (a INT);
            /* CREATE TABLE fake2 (a INT); /* nested */ */
            CREATE TABLE real (a INT);
            CREATE FUNCTION f() RETURNS trigger AS $body$
              BEGIN ALTER TABLE real ADD COLUMN sneaky INT; END
            $body$ LANGUAGE plpgsql;
            DO $$ BEGIN ALTER TABLE real ADD COLUMN sneaky2 INT; END $$;
            INSERT INTO real VALUES ('ALTER TABLE real ADD COLUMN s3 INT;');"]);
        assert_eq!(w.get("fake"), None);
        assert_eq!(w.get("fake2"), None);
        assert_eq!(w.get("real"), Some(&1));
    }

    #[test]
    fn array_literals_do_not_split_a_column() {
        let w = widths(&["CREATE TABLE t (
            a INT[] DEFAULT ARRAY[1, 2, 3],
            b INT[][] DEFAULT '{{1,2},{3,4}}',
            c TEXT
        );
        ALTER TABLE t ADD COLUMN d INT[] DEFAULT ARRAY[4, 5], ADD COLUMN e INT;"]);
        assert_eq!(w.get("t"), Some(&5));
    }

    #[test]
    fn max_flag_accepts_the_feature_the_app_chose() {
        let report = |features: &[&str]| WidthReport {
            tables: replay_migration_set(&[(PathBuf::from("01/up.sql"), wide_create("t", 40))]),
            wide_diesel_features: features.iter().map(|f| (*f).to_owned()).collect(),
        };
        // At the default cap, both wide features are findings.
        assert_eq!(
            report(&["128-column-tables", "64-column-tables"]).features_over(32),
            vec!["128-column-tables", "64-column-tables"]
        );
        // An app that chose 64 on purpose passes `--max 64`.
        assert!(!report(&["64-column-tables"]).has_findings(64));
        // ...but not if something pulls in 128.
        assert_eq!(
            report(&["128-column-tables", "64-column-tables"]).features_over(64),
            vec!["128-column-tables"]
        );
        assert!(report(&["128-column-tables"]).has_findings(64));
        // The build warning always measures against 32.
        assert!(format_build_warning(&report(&["64-column-tables"])).is_some());
    }

    #[test]
    fn create_if_not_exists_keeps_the_existing_table() {
        let w = widths(&[
            wide_create("t", 33).as_str(),
            "CREATE TABLE IF NOT EXISTS t (id INT);",
            "CREATE TABLE IF NOT EXISTS fresh (id INT, name TEXT);",
        ]);
        assert_eq!(w.get("t"), Some(&33));
        assert_eq!(w.get("fresh"), Some(&2));
    }

    #[test]
    fn schemas_keep_same_named_tables_apart() {
        let cols: Vec<String> = (0..33).map(|i| format!("c{i} TEXT")).collect();
        let w = widths(&[
            format!("CREATE TABLE a.t ({});", cols.join(", ")).as_str(),
            "CREATE TABLE b.t (id INT);",
            "ALTER TABLE b.t ADD COLUMN x INT;",
            "CREATE TABLE public.p (id INT); ALTER TABLE p ADD COLUMN y INT;",
        ]);
        assert_eq!(w.get("a.t"), Some(&33));
        assert_eq!(w.get("b.t"), Some(&2));
        assert_eq!(w.get("p"), Some(&2));
    }

    #[test]
    fn model_fields_with_unresolved_types_still_count() {
        let fields: Vec<String> = (0..31).map(|i| format!("pub f{i}: String,")).collect();
        let src = format!(
            "#[model]\npub struct Wide {{\n#[id]\npub id: i64,\n{}\npub status: MyEnum,\n}}",
            fields.join("\n")
        );
        let found = rust_source_widths(Path::new("models.rs"), &src);
        let model = found
            .iter()
            .find(|t| t.source == WidthSource::Model)
            .expect("model width");
        // id + 31 + the enum field the parser cannot type.
        assert_eq!(model.columns, 33);
    }

    #[test]
    fn rename_keeps_the_schema() {
        let cols: Vec<String> = (0..32).map(|i| format!("c{i} TEXT")).collect();
        let w = widths(&[
            format!("CREATE TABLE a.t ({});", cols.join(", ")).as_str(),
            "ALTER TABLE a.t RENAME TO u;",
            "ALTER TABLE a.u ADD COLUMN one_more TEXT;",
        ]);
        assert_eq!(w.get("a.t"), None);
        assert_eq!(w.get("u"), None);
        assert_eq!(w.get("a.u"), Some(&33));
    }

    #[test]
    fn models_in_inline_modules_count() {
        let fields: Vec<String> = (0..33).map(|i| format!("pub f{i}: String,")).collect();
        let src = format!(
            "mod models {{ mod deeper {{ #[model]\npub struct Wide {{\n#[id]\npub id: i64,\n{}\n}} }} }}",
            fields.join("\n")
        );
        let found = rust_source_widths(Path::new("lib.rs"), &src);
        let model = found
            .iter()
            .find(|t| t.source == WidthSource::Model)
            .expect("nested model width");
        assert_eq!(model.columns, 34);
    }

    fn report_of(sql: &str, rust: &[(&str, &str)]) -> WidthReport {
        let mut tables = replay_migration_set(&[(PathBuf::from("01/up.sql"), sql.to_owned())]);
        for (path, src) in rust {
            tables.extend(rust_source_widths(Path::new(path), src));
        }
        WidthReport {
            tables,
            wide_diesel_features: Vec::new(),
        }
    }

    fn decl(name: &str, sql_name: Option<&str>, columns: std::ops::Range<usize>) -> String {
        let attr = sql_name.map_or_else(String::new, |n| format!("#[sql_name = \"{n}\"]"));
        let cols: Vec<String> = columns.map(|i| format!("c{i} -> Text,")).collect();
        format!(
            "diesel::table! {{ {attr} {name} (c0) {{ {} }} }}",
            cols.join(" ")
        )
    }

    #[test]
    fn sql_name_maps_a_declaration_to_its_table() {
        let src = r#"
            diesel::table! { wide (id) { id -> Int4, state -> Text, a -> Int4 } }
            diesel::table! {
                #[sql_name = "wide"]
                wide_ext (id) { id -> Int4, paused -> Nullable<Text>, b -> Int4 }
            }
        "#;
        let found = rust_source_widths(Path::new("schema.rs"), src);
        let pairs: Vec<(&str, &str, usize)> = found
            .iter()
            .map(|t| (t.table.as_str(), t.physical.as_str(), t.columns))
            .collect();
        assert_eq!(pairs, vec![("wide", "wide", 3), ("wide_ext", "wide", 3)]);
    }

    #[test]
    fn a_wide_table_split_across_declarations_is_not_a_finding() {
        let sql = wide_create("wide", 40);
        let split = format!(
            "{}\n{}",
            decl("wide", None, 0..20),
            decl("wide_ext", Some("wide"), 20..40)
        );
        let report = report_of(&sql, &[("schema.rs", &split)]);
        assert!(
            report.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{report:?}"
        );
        assert!(format_build_warning(&report).is_none());
    }

    #[test]
    fn a_wide_table_no_declaration_maps_is_a_finding() {
        let report = report_of(&wide_create("wide", 40), &[]);
        let over = report.over(DIESEL_DEFAULT_COLUMN_LIMIT);
        assert_eq!(over.len(), 1);
        let warning = format_build_warning(&report).expect("warning");
        assert!(warning.contains("no `table!` maps to it"), "{warning}");
        assert!(warning.contains("#[sql_name"), "{warning}");
    }

    #[test]
    fn one_wide_declaration_is_a_finding_even_when_split_elsewhere() {
        // The physical table is split, but one declaration still carries 33.
        let split = format!(
            "{}\n{}",
            decl("wide", None, 0..33),
            decl("wide_ext", Some("wide"), 33..40)
        );
        let report = report_of(&wide_create("wide", 40), &[("schema.rs", &split)]);
        let over = report.over(DIESEL_DEFAULT_COLUMN_LIMIT);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].source, WidthSource::TableMacro);
        assert_eq!(over[0].table, "wide");
    }

    #[test]
    fn a_model_on_the_split_half_maps_through_its_sql_name() {
        let fields: Vec<String> = (0..20).map(|i| format!("pub f{i}: String,")).collect();
        let model = format!(
            "#[model(table = \"wide_ext\")]\npub struct WideExt {{\n#[id]\npub id: i64,\n{}\n}}",
            fields.join("\n")
        );
        // Only the model's half is declared, via `wide_ext` → `wide`.
        let report = report_of(
            &wide_create("wide", 40),
            &[
                ("schema.rs", &decl("wide_ext", Some("wide"), 0..21)),
                ("models.rs", &model),
            ],
        );
        assert!(
            report.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn widest_skips_a_split_physical_table() {
        let split = format!(
            "{}\n{}",
            decl("wide", None, 0..20),
            decl("wide_ext", Some("wide"), 20..40)
        );
        let report = report_of(&wide_create("wide", 40), &[("schema.rs", &split)]);
        let widest = report.widest().expect("widest");
        assert_eq!(widest.columns, 20);
        assert_eq!(widest.source, WidthSource::TableMacro);
        let migration = report
            .tables
            .iter()
            .find(|t| t.source == WidthSource::Migration)
            .expect("migration");
        assert!(report.is_declared_migration(migration));
    }

    #[test]
    fn two_models_of_one_table_count_independently() {
        let wide: Vec<String> = (0..32).map(|i| format!("pub f{i}: String,")).collect();
        let src = format!(
            "#[model(table = \"t\")]\npub struct Wide {{\n#[id]\npub id: i64,\n{}\n}}\n\
             #[model(table = \"t\")]\npub struct Narrow {{\n#[id]\npub id: i64,\npub status: MyEnum,\n}}",
            wide.join("\n")
        );
        let mut widths: Vec<usize> = rust_source_widths(Path::new("models.rs"), &src)
            .iter()
            .filter(|t| t.source == WidthSource::Model)
            .map(|t| t.columns)
            .collect();
        widths.sort_unstable();
        // `Narrow`'s enum diagnostic and both implicit `created_at`s stay
        // with their own model.
        assert_eq!(widths, vec![2, 33]);
    }

    #[test]
    fn a_declaration_vouches_only_for_its_own_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let write = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, body).expect("write");
        };
        write("app_a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(
            "app_a/migrations/2026_users/up.sql",
            &wide_create("users", 33),
        );
        write("app_b/Cargo.toml", "[package]\nname = \"b\"\n");
        write(
            "app_b/src/schema.rs",
            "diesel::table! { users (id) { id -> Int8, } }",
        );
        let report = WidthReport {
            tables: scan_tables(root),
            wide_diesel_features: Vec::new(),
        };
        let over = report.over(DIESEL_DEFAULT_COLUMN_LIMIT);
        assert_eq!(over.len(), 1, "{report:?}");
        assert_eq!(over[0].package, PathBuf::from("app_a"));

        // App A declaring its own table (split) clears it.
        write(
            "app_a/src/schema.rs",
            &format!(
                "{}\n{}",
                decl("users", None, 0..20),
                decl("users_ext", Some("users"), 20..33)
            ),
        );
        let report = WidthReport {
            tables: scan_tables(root),
            wide_diesel_features: Vec::new(),
        };
        assert!(
            report.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn a_declaration_in_one_schema_does_not_cover_another() {
        let cols: Vec<String> = (0..33).map(|i| format!("c{i} TEXT")).collect();
        let sql = format!(
            "CREATE TABLE a.users ({}); CREATE TABLE b.users (id INT);",
            cols.join(", ")
        );
        let only_b = "diesel::table! { b.users (id) { id -> Int4, } }";
        let report = report_of(&sql, &[("schema.rs", only_b)]);
        let over = report.over(DIESEL_DEFAULT_COLUMN_LIMIT);
        assert_eq!(over.len(), 1, "{report:?}");
        assert_eq!(over[0].table, "a.users");

        // Declaring `a.users` (and `public.x` / bare `x` agreeing) clears it.
        let both = format!(
            "{only_b}\n{}\n{}",
            "diesel::table! { a.users (c0) { c0 -> Text, } }",
            "diesel::table! { #[sql_name = \"users\"] a.users_ext (c1) { c1 -> Text, } }"
        );
        let report = report_of(&sql, &[("schema.rs", &both)]);
        assert!(
            report.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{report:?}"
        );
        let public = report_of(
            &wide_create("public.p", 33),
            &[("schema.rs", "diesel::table! { p (c0) { c0 -> Text, } }")],
        );
        assert!(
            public.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{public:?}"
        );
    }

    #[test]
    fn same_named_declarations_in_two_schemas_keep_their_own_tables() {
        let cols: Vec<String> = (0..33).map(|i| format!("c{i} TEXT")).collect();
        let sql = format!(
            "CREATE TABLE a.users ({}); CREATE TABLE b.users (id INT);",
            cols.join(", ")
        );
        // `a.users` is declared in one module and `b.users` in another, both
        // as `users`: the last one must not overwrite the first's mapping.
        let schema = "mod a { diesel::table! { a.users (c0) { c0 -> Text, } } } \
                      mod b { diesel::table! { b.users (id) { id -> Int4, } } }";
        let report = report_of(&sql, &[("schema.rs", schema)]);
        assert!(
            report.over(DIESEL_DEFAULT_COLUMN_LIMIT).is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn create_table_as_select_changes_no_width() {
        let w = widths(&["CREATE TABLE copy AS SELECT * FROM real;"]);
        assert!(w.is_empty());
    }

    #[test]
    fn the_33rd_column_from_an_alter_is_the_finding() {
        let report = WidthReport {
            tables: replay_migration_set(&[
                (PathBuf::from("01_create/up.sql"), wide_create("wide", 32)),
                (
                    PathBuf::from("02_grow/up.sql"),
                    "ALTER TABLE wide ADD COLUMN one_more TEXT;".to_owned(),
                ),
            ]),
            wide_diesel_features: Vec::new(),
        };
        let over = report.over(DIESEL_DEFAULT_COLUMN_LIMIT);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].columns, 33);
        assert_eq!(over[0].path, PathBuf::from("02_grow/up.sql"));
    }

    #[test]
    fn exactly_32_is_allowed() {
        let report = WidthReport {
            tables: replay_migration_set(&[(PathBuf::from("01/up.sql"), wide_create("t", 32))]),
            wide_diesel_features: Vec::new(),
        };
        assert!(!report.has_findings(DIESEL_DEFAULT_COLUMN_LIMIT));
        assert!(format_build_warning(&report).is_none());
    }

    #[test]
    fn table_macro_counts_each_table_in_one_invocation() {
        let src = r#"
            diesel::table! {
                use diesel::sql_types::*;
                #[sql_name = "people"]
                users (id) {
                    id -> Int8,
                    name -> Nullable<Text>,
                    tags -> Array<Nullable<Text>>,
                    #[sql_name = "type"]
                    kind -> Text,
                }
                public.posts (id, slug) { id -> Int8, slug -> Text, }
            }
            mod nested { table! { r#type (id) { id -> Int4, } } }
        "#;
        let mut found: Vec<(String, usize)> = rust_source_widths(Path::new("schema.rs"), src)
            .into_iter()
            .map(|t| (t.table, t.columns))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("posts".to_owned(), 2),
                ("type".to_owned(), 1),
                ("users".to_owned(), 4)
            ]
        );
    }

    #[test]
    fn model_struct_width_matches_its_fields() {
        let fields: Vec<String> = (0..33).map(|i| format!("pub f{i}: String,")).collect();
        let src = format!(
            "#[model]\npub struct Wide {{\n#[id]\npub id: i64,\n{}\n}}",
            fields.join("\n")
        );
        let found = rust_source_widths(Path::new("models.rs"), &src);
        let model = found
            .iter()
            .find(|t| t.source == WidthSource::Model)
            .expect("model width");
        assert_eq!(model.table, "wides");
        assert_eq!(model.columns, 34);
    }

    #[test]
    fn invalid_rust_yields_nothing() {
        assert!(rust_source_widths(Path::new("x.rs"), "table! { fn (").is_empty());
    }

    #[test]
    fn wide_features_read_from_the_resolve_graph() {
        let metadata = serde_json::json!({
            "packages": [
                {"name": "diesel", "id": "diesel 2.2.0"},
                {"name": "not-diesel", "id": "other 1.0.0"}
            ],
            "resolve": {"nodes": [
                {"id": "diesel 2.2.0", "features": ["32-column-tables", "64-column-tables", "128-column-tables", "postgres"]},
                {"id": "other 1.0.0", "features": ["128-column-tables"]}
            ]}
        });
        assert_eq!(
            wide_diesel_features(&metadata),
            vec![
                "128-column-tables".to_owned(),
                "64-column-tables".to_owned()
            ]
        );
        let default_only = serde_json::json!({
            "packages": [{"name": "diesel", "id": "d"}],
            "resolve": {"nodes": [{"id": "d", "features": ["32-column-tables"]}]}
        });
        assert!(wide_diesel_features(&default_only).is_empty());
        assert!(wide_diesel_features(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn build_warning_names_the_table_the_file_and_the_feature() {
        let report = WidthReport {
            tables: vec![TableWidth {
                table: "accounts".to_owned(),
                physical: "accounts".to_owned(),
                package: PathBuf::new(),
                columns: 40,
                source: WidthSource::Migration,
                path: PathBuf::from("migrations/2026_wide/up.sql"),
            }],
            wide_diesel_features: vec!["64-column-tables".to_owned()],
        };
        let warning = format_build_warning(&report).expect("a warning");
        assert!(warning.starts_with("warning:"), "{warning}");
        assert!(warning.contains("accounts has 40 columns"), "{warning}");
        assert!(warning.contains("migrations/2026_wide/up.sql"), "{warning}");
        assert!(warning.contains("`64-column-tables`"), "{warning}");
        assert!(warning.contains("The build continues"), "{warning}");
        assert!(warning.contains(GUIDE), "{warning}");
    }

    #[test]
    fn discovery_finds_sets_and_skips_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let write = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, body).expect("write");
        };
        write("migrations/2026_a/up.sql", &wide_create("wide", 33));
        write(
            "migrations/2026_b/up.sql",
            "ALTER TABLE wide DROP COLUMN c0;",
        );
        write("plugin/migrations/2026_c/up.sql", &wide_create("plug", 34));
        write(
            "target/debug/migrations/2026_x/up.sql",
            &wide_create("built", 99),
        );
        write(".git/migrations/2026_x/up.sql", &wide_create("vcs", 99));
        write("src/schema.rs", "diesel::table! { t (id) { id -> Int8, } }");

        let tables = scan_tables(root);
        let by_name: BTreeMap<&str, usize> = tables
            .iter()
            .map(|t| (t.table.as_str(), t.columns))
            .collect();
        assert_eq!(by_name.get("wide"), Some(&32));
        assert_eq!(by_name.get("plug"), Some(&34));
        assert_eq!(by_name.get("t"), Some(&1));
        assert!(!by_name.contains_key("built"));
        assert!(!by_name.contains_key("vcs"));
        assert!(run_width(root, DIESEL_DEFAULT_COLUMN_LIMIT, false).is_err());
        assert!(run_width(root, 34, false).is_ok());
    }
}
