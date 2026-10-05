//! `#[renamed_from("old")]` support (#1975, Decision 5).
//!
//! A hint turns a drop + add into a rename. The diff engine:
//!
//! 1. Reads the hints and makes [`SchemaChange::RenameTable`],
//!    [`SchemaChange::RenameColumn`] and [`SchemaChange::RenameIndex`] changes
//!    ([`rename_changes`]).
//! 2. Applies those changes to a copy of the baseline ([`renamed_baseline`]).
//! 3. Diffs the renamed baseline against the models as usual.
//!
//! A hint is **active** only when the old name is in the baseline and the new
//! name is not. Thus a hint that stays in the source after its migration is
//! inert. An active hint that is ambiguous gives a
//! [`SchemaChange::RenameConflict`] marker. The guard refuses it and
//! `--allow-destructive` does not override it.
//!
//! An index follows a rename only when its name uses the parser convention
//! (`idx_<table>_<field>` or `idx_<table>_<field>_unique`) and the models
//! declare the new name. Index definitions and `CHECK` expressions are
//! rewritten by a word-bounded token match (best effort).

use std::collections::{BTreeMap, BTreeSet};

use autumn_schema_core::Table;

use crate::schema::diff::SchemaChange;
use crate::schema::parse::{ParsedSchema, RenameHint};

/// The rename changes (and conflict markers) the `desired` hints ask for,
/// relative to `baseline`. Table renames come first, then column renames,
/// then index renames.
#[must_use]
pub fn rename_changes(baseline: &[Table], desired: &ParsedSchema) -> Vec<SchemaChange> {
    let managed: BTreeMap<&str, &Table> = desired
        .tables
        .iter()
        .filter(|t| t.managed)
        .map(|t| (t.name.as_str(), t))
        .collect();
    let mut work = baseline.to_vec();
    let mut out = Vec::new();

    let tables = table_renames(baseline, desired, &managed, &mut out);
    for (from, to) in &tables {
        push_applied(
            &mut work,
            &mut out,
            SchemaChange::RenameTable {
                from: from.clone(),
                to: to.clone(),
            },
        );
    }

    let columns = column_renames(&work, desired, &managed, &mut out);
    for (table, pairs) in &columns {
        for (from, to) in pairs {
            push_applied(
                &mut work,
                &mut out,
                SchemaChange::RenameColumn {
                    table: table.clone(),
                    from: from.clone(),
                    to: to.clone(),
                },
            );
        }
    }

    for change in index_renames(&work, &managed, &tables, &columns) {
        push_applied(&mut work, &mut out, change);
    }
    out
}

/// The `(from, to)` table renames. Activity uses the original baseline names.
fn table_renames(
    baseline: &[Table],
    desired: &ParsedSchema,
    managed: &BTreeMap<&str, &Table>,
    out: &mut Vec<SchemaChange>,
) -> Vec<(String, String)> {
    let base_tables: BTreeSet<&str> = baseline.iter().map(|t| t.name.as_str()).collect();
    let declared: BTreeSet<&str> = desired.tables.iter().map(|t| t.name.as_str()).collect();
    let hints: Vec<&RenameHint> = desired
        .renames
        .iter()
        .filter(|h| h.column.is_none() && managed.contains_key(h.table.as_str()))
        .collect();
    select(
        &hints,
        |name| base_tables.contains(name),
        |h| declared.contains(h.from.as_str()),
        "table",
        out,
    )
}

/// The `(from, to)` column renames per table, against the table-renamed
/// baseline `work`.
fn column_renames(
    work: &[Table],
    desired: &ParsedSchema,
    managed: &BTreeMap<&str, &Table>,
    out: &mut Vec<SchemaChange>,
) -> BTreeMap<String, Vec<(String, String)>> {
    let mut renames = BTreeMap::new();
    for (name, want) in managed {
        let Some(base) = work.iter().find(|t| t.name == *name) else {
            continue;
        };
        let base_cols: BTreeSet<&str> = base.columns.iter().map(|c| c.name.as_str()).collect();
        let want_cols: BTreeSet<&str> = want.columns.iter().map(|c| c.name.as_str()).collect();
        let (hints, unread): (Vec<&RenameHint>, Vec<&RenameHint>) = desired
            .renames
            .iter()
            .filter(|h| h.table == *name && h.column.is_some())
            .partition(|h| h.column.as_deref().is_some_and(|c| want_cols.contains(c)));
        // A hint on a field the parser skipped (an unsupported type) cannot
        // apply. Refuse it, so the user does not get the "add #[renamed_from]"
        // advice for a hint that is already there.
        for h in unread {
            let field = h.column.as_deref().unwrap_or_default();
            let skipped = desired
                .diagnostics
                .iter()
                .any(|d| d.table == *name && d.field == field);
            if skipped && base_cols.contains(h.from.as_str()) {
                out.push(SchemaChange::RenameConflict {
                    table: (*name).to_owned(),
                    reason: format!(
                        "the parser cannot read field `{field}` (unsupported type), so its \
                         hint cannot apply; write this rename as a manual migration"
                    ),
                });
            }
        }
        let selected = select(
            &hints,
            |col| base_cols.contains(col),
            |h| want_cols.contains(h.from.as_str()),
            "column",
            out,
        );
        if !selected.is_empty() {
            renames.insert((*name).to_owned(), selected);
        }
    }
    renames
}

/// The convention-named index renames on every table that a table or column
/// rename touched. The models must declare the new name with the same shape;
/// otherwise the index diff drops the old index and adds the new one.
fn index_renames(
    work: &[Table],
    managed: &BTreeMap<&str, &Table>,
    tables: &[(String, String)],
    columns: &BTreeMap<String, Vec<(String, String)>>,
) -> Vec<SchemaChange> {
    let old_table: BTreeMap<&str, &str> = tables
        .iter()
        .map(|(from, to)| (to.as_str(), from.as_str()))
        .collect();
    let touched: BTreeSet<&str> = old_table
        .keys()
        .copied()
        .chain(columns.keys().map(String::as_str))
        .collect();
    let mut out = Vec::new();
    for name in touched {
        let (Some(want), Some(base)) = (managed.get(name), work.iter().find(|t| t.name == name))
        else {
            continue;
        };
        let old = old_table.get(name).copied().unwrap_or(name);
        let cols = columns.get(name).map_or(&[][..], Vec::as_slice);
        for idx in base.indexes.iter().filter(|i| i.definition.is_none()) {
            let Some(new_name) = convention_index_name(&idx.name, old, name, cols) else {
                continue;
            };
            let declared = want.indexes.iter().any(|i| {
                i.name == new_name
                    && i.columns == idx.columns
                    && i.unique == idx.unique
                    && i.definition.is_none()
            });
            // Index names are global in the schema, so check every table.
            let taken = work
                .iter()
                .any(|t| t.indexes.iter().any(|i| i.name == new_name));
            if new_name != idx.name && declared && !taken {
                let mut index = idx.clone();
                index.name = new_name;
                out.push(SchemaChange::RenameIndex {
                    table: name.to_owned(),
                    from: idx.name.clone(),
                    index,
                });
            }
        }
    }
    out
}

/// Pick the active hints in one scope (the tables, or one table's columns) as
/// `(from, to)` pairs. `in_base` tells if a name is in the baseline. A hint is
/// active when its old name is in the baseline and its new name is not.
///
/// A conflict ([`SchemaChange::RenameConflict`], pushed to `out`) is:
/// - an active hint whose old name is still declared;
/// - two active hints with one old name;
/// - a chain or swap: a hint whose old and new names are both in the baseline,
///   while another hint renames its new name away.
fn select(
    hints: &[&RenameHint],
    in_base: impl Fn(&str) -> bool,
    still_declared: impl Fn(&RenameHint) -> bool,
    kind: &str,
    out: &mut Vec<SchemaChange>,
) -> Vec<(String, String)> {
    let target = |h: &RenameHint| h.column.clone().unwrap_or_else(|| h.table.clone());
    let conflict = |h: &RenameHint, reason: String| SchemaChange::RenameConflict {
        table: h.table.clone(),
        reason,
    };
    for h in hints {
        let to = target(h);
        if in_base(&h.from) && in_base(&to) && hints.iter().any(|o| o.from == to) {
            out.push(conflict(
                h,
                format!(
                    "`{}` -> `{to}` is part of a chain or swap of renames; apply each \
                     rename in its own migration",
                    h.from
                ),
            ));
        }
    }
    let active: Vec<&RenameHint> = hints
        .iter()
        .copied()
        .filter(|h| in_base(&h.from) && !in_base(&target(h)))
        .collect();
    let mut by_from: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &active {
        *by_from.entry(h.from.as_str()).or_default() += 1;
    }
    let mut picked = Vec::new();
    for h in &active {
        let to = target(h);
        if still_declared(h) {
            out.push(conflict(
                h,
                format!(
                    "{kind} `{}` is still declared, so `{to}` cannot be renamed from it",
                    h.from
                ),
            ));
        } else if by_from[h.from.as_str()] > 1 {
            out.push(conflict(
                h,
                format!("more than one {kind} is renamed from `{}`", h.from),
            ));
        } else {
            picked.push((h.from.clone(), to));
        }
    }
    picked
}

/// Apply `change` to `work` and record it.
fn push_applied(work: &mut [Table], out: &mut Vec<SchemaChange>, change: SchemaChange) {
    apply_rename(work, &change);
    out.push(change);
}

/// The new name of a convention index (`idx_<table>_<field>` or
/// `idx_<table>_<field>_unique`) after the table `old` becomes `new` and the
/// column renames `cols` apply. `None` when `name` does not use the convention.
fn convention_index_name(
    name: &str,
    old: &str,
    new: &str,
    cols: &[(String, String)],
) -> Option<String> {
    let rest = name.strip_prefix(&format!("idx_{old}_"))?;
    let rest = cols
        .iter()
        .find_map(|(from, to)| {
            if rest == from {
                Some(to.clone())
            } else {
                rest.strip_suffix("_unique")
                    .filter(|field| field == from)
                    .map(|_| format!("{to}_unique"))
            }
        })
        .unwrap_or_else(|| rest.to_owned());
    Some(format!("idx_{new}_{rest}"))
}

/// `baseline` with every rename change in `changes` applied. Other changes are
/// ignored.
#[must_use]
pub fn renamed_baseline(baseline: &[Table], changes: &[SchemaChange]) -> Vec<Table> {
    let mut tables = baseline.to_vec();
    for change in changes {
        apply_rename(&mut tables, change);
    }
    tables
}

/// Apply one rename change to `tables`, as the database applies it: foreign
/// keys, index columns, index definitions and `CHECK` expressions follow the
/// new name. A non-rename change is a no-op.
pub fn apply_rename(tables: &mut [Table], change: &SchemaChange) {
    match change {
        SchemaChange::RenameTable { from, to } => {
            for t in tables.iter_mut() {
                if t.name == *from {
                    t.name.clone_from(to);
                    for idx in &mut t.indexes {
                        if let Some(def) = &mut idx.definition {
                            *def = rename_index_target(def, from, to);
                        }
                    }
                }
                for c in &mut t.columns {
                    if let Some(fk) = &mut c.references
                        && fk.table == *from
                    {
                        fk.table.clone_from(to);
                    }
                }
            }
        }
        SchemaChange::RenameColumn { table, from, to } => {
            let swap = |name: &mut String| {
                if name == from {
                    name.clone_from(to);
                }
            };
            for t in tables.iter_mut() {
                if t.name == *table {
                    t.columns.iter_mut().for_each(|c| swap(&mut c.name));
                    t.primary_key.iter_mut().for_each(swap);
                    for idx in &mut t.indexes {
                        idx.columns.iter_mut().for_each(swap);
                        idx.key_columns.iter_mut().for_each(swap);
                        if let Some(def) = &mut idx.definition {
                            *def = rename_index_column(def, from, to);
                        }
                    }
                    for check in &mut t.checks {
                        check.expression = replace_word(&check.expression, from, to);
                    }
                }
                for c in &mut t.columns {
                    if let Some(fk) = &mut c.references
                        && fk.table == *table
                        && fk.column == *from
                    {
                        fk.column.clone_from(to);
                    }
                }
            }
        }
        SchemaChange::RenameIndex { table, from, index } => {
            for t in tables.iter_mut().filter(|t| t.name == *table) {
                for idx in &mut t.indexes {
                    if idx.name == *from {
                        *idx = index.clone();
                    }
                }
            }
        }
        _ => {}
    }
}

/// Whether `name` is a safe, unquoted SQL identifier: `[a-z_][a-z0-9_]*`, at
/// most 63 bytes (the Postgres limit).
#[must_use]
pub fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
}

/// Split an index or constraint definition at its table target:
/// `(head, target token, rest)`. The target follows `ON [ONLY]` in a
/// `CREATE INDEX`, or `ALTER TABLE [ONLY]` in an introspected `EXCLUDE`
/// constraint. `None` when there is no target.
fn split_index_target(def: &str) -> Option<(&str, &str, &str)> {
    let upper = def.to_ascii_uppercase();
    let mut start = if upper.starts_with("ALTER TABLE ") {
        "ALTER TABLE ".len()
    } else {
        // The first `ON` keyword with whitespace on both sides.
        let bytes = upper.as_bytes();
        (1..upper.len().saturating_sub(2)).find(|&i| {
            &upper[i..i + 2] == "ON"
                && bytes[i - 1].is_ascii_whitespace()
                && bytes[i + 2].is_ascii_whitespace()
        })? + 3
    };
    start += def[start..].len() - def[start..].trim_start().len();
    if upper[start..].starts_with("ONLY ") {
        start += 5;
    }
    let len = def[start..]
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(def.len() - start);
    Some((&def[..start], &def[start..start + len], &def[start + len..]))
}

/// Rename the table in an index definition's `ON` target. The schema prefix
/// and quoting stay.
fn rename_index_target(def: &str, from: &str, to: &str) -> String {
    let Some((head, target, rest)) = split_index_target(def) else {
        return def.to_owned();
    };
    let (schema, name) = target
        .rsplit_once('.')
        .map_or(("", target), |(s, n)| (s, n));
    let quoted = name.len() > 1
        && ((name.starts_with('"') && name.ends_with('"'))
            || (name.starts_with('[') && name.ends_with(']')));
    let bare = if quoted {
        &name[1..name.len() - 1]
    } else {
        name
    };
    if !bare.eq_ignore_ascii_case(from) {
        return def.to_owned();
    }
    let dot = if schema.is_empty() { "" } else { "." };
    let new = if quoted {
        format!("{}{to}{}", &name[..1], &name[name.len() - 1..])
    } else {
        to.to_owned()
    };
    format!("{head}{schema}{dot}{new}{rest}")
}

/// Rename a column in the part of an index definition after its `ON` target.
fn rename_index_column(def: &str, from: &str, to: &str) -> String {
    match split_index_target(def) {
        Some((head, target, rest)) => format!("{head}{target}{}", replace_word(rest, from, to)),
        None => def.to_owned(),
    }
}

/// The byte length of the quoted token at the start of `rest` (its opening
/// character is a quote), through the `close` character. A doubled quote is an
/// escape, except for `]`. An unclosed token runs to the end.
pub fn quoted_len(rest: &str, close: char) -> usize {
    let mut iter = rest.char_indices().skip(1).peekable();
    while let Some((i, d)) = iter.next() {
        if d == close {
            if close != ']' && iter.peek().is_some_and(|&(_, n)| n == close) {
                iter.next();
            } else {
                return i + 1;
            }
        }
    }
    rest.len()
}

/// Replace each whole-word identifier `from` with `to` in a SQL fragment.
///
/// - An unquoted match ignores case, as SQL does.
/// - A quoted identifier (`"from"` or `[from]`) matches too.
/// - String literals (`'...'`) do not change.
/// - A function name (a word before `(`), a typed-literal type (a word before
///   `'`) or a cast type (a word after `::`) does not change.
fn replace_word(sql: &str, from: &str, to: &str) -> String {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        let close = match c {
            '\'' | '"' => Some(c),
            '[' => Some(']'),
            _ => None,
        };
        let len = close.map_or_else(
            || {
                if is_word(c) {
                    rest.find(|d: char| !is_word(d)).unwrap_or(rest.len())
                } else {
                    c.len_utf8()
                }
            },
            |close| quoted_len(rest, close),
        );
        let token = &rest[..len];
        let next = rest[len..].trim_start();
        // A word before `(` is a function; before `'` it is a typed literal
        // (`DATE '2020-01-01'`).
        let is_call = next.starts_with('(') || next.starts_with('\'');
        let is_cast = out.ends_with("::");
        let quoted_match = matches!(close, Some('"' | ']'))
            && token.len() > 1
            && &token[1..token.len() - 1] == from;
        if close.is_none() && token.eq_ignore_ascii_case(from) && !is_call && !is_cast {
            out.push_str(to);
        } else if quoted_match {
            out.push_str(&token[..1]);
            out.push_str(to);
            out.push_str(&token[token.len() - 1..]);
        } else {
            out.push_str(token);
        }
        rest = &rest[len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_schema_core::{Backend, CheckConstraint, Column, ColumnType, ForeignKey, Index};

    use crate::schema::diff::{
        DiffError, DiffOptions, MigrationPlan, SchemaContext, diff_schema,
        emit_down_sql_with_context, emit_up_sql_with_context, guard_plan,
    };
    use crate::schema::parse::RenameHint;

    const OPTS: DiffOptions = DiffOptions {
        allow_destructive: false,
        definitions_authoritative: false,
    };
    const ALLOW: DiffOptions = DiffOptions {
        allow_destructive: true,
        definitions_authoritative: false,
    };

    fn table(name: &str, backend: Backend, columns: &[(&str, ColumnType)]) -> Table {
        let mut t = Table::new(name, backend);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        for (c, ty) in columns {
            t.columns.push(Column::new(*c, ty.clone()));
        }
        t
    }

    fn col_hint(table: &str, column: &str, from: &str) -> RenameHint {
        RenameHint {
            table: table.to_owned(),
            column: Some(column.to_owned()),
            from: from.to_owned(),
        }
    }

    fn table_hint(table: &str, from: &str) -> RenameHint {
        RenameHint {
            table: table.to_owned(),
            column: None,
            from: from.to_owned(),
        }
    }

    fn desired(tables: Vec<Table>, renames: Vec<RenameHint>) -> ParsedSchema {
        let mut p = ParsedSchema::from_tables(tables);
        p.renames = renames;
        p
    }

    /// Diff, guard, and render both legs with the command's context.
    fn render(baseline: &[Table], want: &ParsedSchema) -> (MigrationPlan, String, String) {
        let plan = diff_schema(baseline, want, OPTS);
        guard_plan(&plan, OPTS).expect("plan passes the guard");
        let ctx =
            SchemaContext::from_tables(&want.tables, &renamed_baseline(baseline, &plan.changes));
        let up = emit_up_sql_with_context(&plan, &ctx).expect("up");
        let down = emit_down_sql_with_context(&plan, &ctx).expect("down");
        (plan, up, down)
    }

    fn pos(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not in:\n{haystack}"))
    }

    #[test]
    fn column_hint_emits_a_rename_not_a_drop_and_add() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("headline", ColumnType::Text)],
            )],
            vec![col_hint("posts", "headline", "title")],
        );
        let (plan, up, down) = render(&base, &want);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::RenameColumn {
                table: "posts".to_owned(),
                from: "title".to_owned(),
                to: "headline".to_owned(),
            }]
        );
        assert_eq!(
            up.trim(),
            "ALTER TABLE posts RENAME COLUMN title TO headline;"
        );
        assert_eq!(
            down.trim(),
            "ALTER TABLE posts RENAME COLUMN headline TO title;"
        );
    }

    #[test]
    fn table_hint_emits_a_table_rename() {
        let base = vec![table(
            "articles",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("title", ColumnType::Text)],
            )],
            vec![table_hint("posts", "articles")],
        );
        let (plan, up, down) = render(&base, &want);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::RenameTable {
                from: "articles".to_owned(),
                to: "posts".to_owned(),
            }]
        );
        assert_eq!(up.trim(), "ALTER TABLE articles RENAME TO posts;");
        assert_eq!(down.trim(), "ALTER TABLE posts RENAME TO articles;");
    }

    #[test]
    fn table_and_column_hints_compose() {
        let base = vec![table(
            "articles",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("headline", ColumnType::Text)],
            )],
            vec![
                col_hint("posts", "headline", "title"),
                table_hint("posts", "articles"),
            ],
        );
        let (_, up, down) = render(&base, &want);
        assert!(
            pos(&up, "RENAME TO posts")
                < pos(&up, "ALTER TABLE posts RENAME COLUMN title TO headline"),
            "{up}"
        );
        assert!(
            pos(&down, "RENAME COLUMN headline TO title")
                < pos(&down, "ALTER TABLE posts RENAME TO articles"),
            "{down}"
        );
    }

    #[test]
    fn applied_hint_is_inert() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("headline", ColumnType::Text)],
        )];
        let want = desired(
            base.clone(),
            vec![
                col_hint("posts", "headline", "title"),
                table_hint("posts", "articles"),
            ],
        );
        assert!(diff_schema(&base, &want, OPTS).is_empty());
    }

    #[test]
    fn hint_on_an_unmanaged_model_is_ignored() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let mut t = table(
            "posts",
            Backend::Postgres,
            &[("headline", ColumnType::Text)],
        );
        t.managed = false;
        let want = desired(vec![t], vec![col_hint("posts", "headline", "title")]);
        assert!(rename_changes(&base, &want).is_empty());
    }

    #[test]
    fn hint_whose_old_name_is_still_declared_is_refused() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("title", ColumnType::Text), ("headline", ColumnType::Text)],
            )],
            vec![col_hint("posts", "headline", "title")],
        );
        let plan = diff_schema(&base, &want, OPTS);
        let err = guard_plan(&plan, ALLOW).unwrap_err();
        assert!(matches!(err, DiffError::RenameConflict { .. }), "{err}");
        assert!(err.to_string().contains("title"), "{err}");
    }

    #[test]
    fn two_hints_with_one_old_name_are_refused() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("title", ColumnType::Text)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("a", ColumnType::Text), ("b", ColumnType::Text)],
            )],
            vec![
                col_hint("posts", "a", "title"),
                col_hint("posts", "b", "title"),
            ],
        );
        let plan = diff_schema(&base, &want, OPTS);
        assert!(matches!(
            guard_plan(&plan, ALLOW),
            Err(DiffError::RenameConflict { .. })
        ));
    }

    #[test]
    fn table_hint_whose_old_table_is_still_declared_is_refused() {
        let base = vec![table("articles", Backend::Postgres, &[])];
        let want = desired(
            vec![
                table("articles", Backend::Postgres, &[]),
                table("posts", Backend::Postgres, &[]),
            ],
            vec![table_hint("posts", "articles")],
        );
        let plan = diff_schema(&base, &want, OPTS);
        assert!(matches!(
            guard_plan(&plan, ALLOW),
            Err(DiffError::RenameConflict { .. })
        ));
    }

    fn with_unique_email(mut t: Table, column: &str) -> Table {
        t.indexes.push(Index::new(
            format!("idx_{}_{column}_unique", t.name),
            vec![column.to_owned()],
            true,
        ));
        t
    }

    #[test]
    fn unique_index_follows_a_column_rename_on_postgres() {
        let base = vec![with_unique_email(
            table("users", Backend::Postgres, &[("email", ColumnType::Text)]),
            "email",
        )];
        let want = desired(
            vec![with_unique_email(
                table("users", Backend::Postgres, &[("mail", ColumnType::Text)]),
                "mail",
            )],
            vec![col_hint("users", "mail", "email")],
        );
        let (plan, up, down) = render(&base, &want);
        assert_eq!(plan.changes.len(), 2, "{:?}", plan.changes);
        assert!(
            up.contains("ALTER INDEX idx_users_email_unique RENAME TO idx_users_mail_unique;"),
            "{up}"
        );
        assert!(
            down.contains("ALTER INDEX idx_users_mail_unique RENAME TO idx_users_email_unique;"),
            "{down}"
        );
        assert!(
            pos(&down, "ALTER INDEX") < pos(&down, "RENAME COLUMN mail TO email"),
            "{down}"
        );
    }

    #[test]
    fn unique_index_follows_a_table_rename_on_sqlite() {
        let base = vec![with_unique_email(
            table("people", Backend::Sqlite, &[("email", ColumnType::Text)]),
            "email",
        )];
        let want = desired(
            vec![with_unique_email(
                table("users", Backend::Sqlite, &[("email", ColumnType::Text)]),
                "email",
            )],
            vec![table_hint("users", "people")],
        );
        let (_, up, down) = render(&base, &want);
        assert!(up.contains("ALTER TABLE people RENAME TO users;"), "{up}");
        assert!(up.contains("DROP INDEX idx_people_email_unique;"), "{up}");
        assert!(
            up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "{up}"
        );
        assert!(
            down.contains("DROP INDEX idx_users_email_unique;"),
            "{down}"
        );
        assert!(
            down.contains("CREATE UNIQUE INDEX idx_people_email_unique ON users (email);"),
            "{down}"
        );
        assert!(
            pos(&down, "idx_people_email_unique")
                < pos(&down, "ALTER TABLE users RENAME TO people"),
            "{down}"
        );
    }

    #[test]
    fn foreign_keys_follow_a_table_rename() {
        let mut posts = table("posts", Backend::Postgres, &[]);
        let mut user_id = Column::new("user_id", ColumnType::Int64);
        user_id.references = Some(ForeignKey::new("users", "id"));
        posts.columns.push(user_id);
        let base = vec![table("users", Backend::Postgres, &[]), posts.clone()];

        let mut want_posts = posts;
        want_posts.columns[1].references = Some(ForeignKey::new("accounts", "id"));
        let want = desired(
            vec![table("accounts", Backend::Postgres, &[]), want_posts],
            vec![table_hint("accounts", "users")],
        );
        let (plan, _, _) = render(&base, &want);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::RenameTable {
                from: "users".to_owned(),
                to: "accounts".to_owned(),
            }]
        );
    }

    #[test]
    fn foreign_keys_follow_a_column_rename() {
        let mut posts = table("posts", Backend::Postgres, &[]);
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "uid"));
        posts.columns.push(author);
        let mut users = table("users", Backend::Postgres, &[("uid", ColumnType::Int64)]);
        users.name = "users".to_owned();
        let base = vec![users.clone(), posts.clone()];

        let mut want_users = users;
        want_users.columns[1].name = "user_key".to_owned();
        let mut want_posts = posts;
        want_posts.columns[1].references = Some(ForeignKey::new("users", "user_key"));
        let want = desired(
            vec![want_users, want_posts],
            vec![col_hint("users", "user_key", "uid")],
        );
        let plan = diff_schema(&base, &want, OPTS);
        assert_eq!(plan.changes.len(), 1, "{:?}", plan.changes);
    }

    #[test]
    fn a_rename_with_a_type_change_renames_first() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("n", ColumnType::Int32)],
        )];
        let want = desired(
            vec![table(
                "posts",
                Backend::Postgres,
                &[("count", ColumnType::Int64)],
            )],
            vec![col_hint("posts", "count", "n")],
        );
        let (plan, up, down) = render(&base, &want);
        assert!(plan.changes.iter().any(|c| matches!(
            c,
            SchemaChange::AlterColumnType { column, .. } if column == "count"
        )));
        assert!(
            pos(&up, "RENAME COLUMN n TO count") < pos(&up, "ALTER COLUMN count TYPE"),
            "{up}"
        );
        assert!(
            pos(&down, "ALTER COLUMN count TYPE") < pos(&down, "RENAME COLUMN count TO n"),
            "{down}"
        );
    }

    #[test]
    fn sqlite_rename_is_never_folded_into_a_rebuild() {
        let base = vec![table("posts", Backend::Sqlite, &[("n", ColumnType::Int64)])];
        let mut want_t = table("posts", Backend::Sqlite, &[("count", ColumnType::Int64)]);
        want_t.columns[1].nullable = true;
        let want = desired(vec![want_t], vec![col_hint("posts", "count", "n")]);
        let (_, up, down) = render(&base, &want);
        assert!(
            pos(&up, "RENAME COLUMN n TO count") < pos(&up, "posts__autumn_new"),
            "{up}"
        );
        assert!(
            pos(&down, "posts__autumn_new") < pos(&down, "RENAME COLUMN count TO n"),
            "{down}"
        );
    }

    #[test]
    fn renamed_baseline_rewrites_checks_definitions_and_keys() {
        let mut t = table("posts", Backend::Postgres, &[("title", ColumnType::Text)]);
        t.checks.push(CheckConstraint {
            name: Some("title_len".to_owned()),
            expression: "length(title) > 0 AND note <> 'title'".to_owned(),
        });
        let mut expr = Index::new("idx_lower_title", vec!["title".to_owned()], false);
        expr.definition = Some(
            "CREATE INDEX idx_lower_title ON public.posts USING btree (lower(title))".to_owned(),
        );
        t.indexes.push(expr);
        let renamed = renamed_baseline(
            &[t],
            &[
                SchemaChange::RenameColumn {
                    table: "posts".to_owned(),
                    from: "title".to_owned(),
                    to: "headline".to_owned(),
                },
                SchemaChange::RenameTable {
                    from: "posts".to_owned(),
                    to: "articles".to_owned(),
                },
            ],
        );
        let t = &renamed[0];
        assert_eq!(t.name, "articles");
        assert_eq!(
            t.checks[0].expression,
            "length(headline) > 0 AND note <> 'title'"
        );
        assert_eq!(t.indexes[0].columns, vec!["headline".to_owned()]);
        assert_eq!(
            t.indexes[0].definition.as_deref(),
            Some("CREATE INDEX idx_lower_title ON public.articles USING btree (lower(headline))")
        );
    }

    #[test]
    fn postgres_down_recreates_retained_indexes_before_undoing_renames() {
        let mut base_t = table(
            "posts",
            Backend::Postgres,
            &[("title", ColumnType::Text), ("slug", ColumnType::Text)],
        );
        let mut live = Index::new(
            "posts_live",
            vec!["title".to_owned(), "slug".to_owned()],
            false,
        );
        live.definition = Some(
            "CREATE INDEX posts_live ON public.posts USING btree (title) WHERE (slug IS NOT NULL)"
                .to_owned(),
        );
        base_t.indexes.push(live);
        let base = vec![base_t];
        let want = desired(
            vec![table(
                "articles",
                Backend::Postgres,
                &[("title", ColumnType::Text)],
            )],
            vec![table_hint("articles", "posts")],
        );
        let plan = diff_schema(&base, &want, ALLOW);
        guard_plan(&plan, ALLOW).expect("guard");
        let ctx = SchemaContext::from_tables(&want.tables, &renamed_baseline(&base, &plan.changes));
        let down = emit_down_sql_with_context(&plan, &ctx).expect("down");
        assert!(
            pos(&down, "CREATE INDEX posts_live ON public.articles")
                < pos(&down, "ALTER TABLE articles RENAME TO posts"),
            "{down}"
        );
    }

    #[test]
    fn an_applied_hint_does_not_block_a_new_rename_into_its_old_name() {
        // `full_name` was renamed from `name` earlier; now `name` comes from `nickname`.
        let base = vec![table(
            "users",
            Backend::Postgres,
            &[
                ("full_name", ColumnType::Text),
                ("nickname", ColumnType::Text),
            ],
        )];
        let want = desired(
            vec![table(
                "users",
                Backend::Postgres,
                &[("full_name", ColumnType::Text), ("name", ColumnType::Text)],
            )],
            vec![
                col_hint("users", "full_name", "name"),
                col_hint("users", "name", "nickname"),
            ],
        );
        let plan = diff_schema(&base, &want, OPTS);
        guard_plan(&plan, OPTS).expect("no conflict");
        assert_eq!(
            plan.changes,
            vec![SchemaChange::RenameColumn {
                table: "users".to_owned(),
                from: "nickname".to_owned(),
                to: "name".to_owned(),
            }]
        );
    }

    #[test]
    fn a_swap_or_a_chain_is_refused() {
        let cols = [("a", ColumnType::Text), ("b", ColumnType::Text)];
        let base = vec![table("t", Backend::Postgres, &cols)];
        let swap = desired(
            vec![table("t", Backend::Postgres, &cols)],
            vec![col_hint("t", "a", "b"), col_hint("t", "b", "a")],
        );
        assert!(matches!(
            guard_plan(&diff_schema(&base, &swap, OPTS), ALLOW),
            Err(DiffError::RenameConflict { .. })
        ));
        // Chain: b -> a and c -> b.
        let base = vec![table(
            "t",
            Backend::Postgres,
            &[("b", ColumnType::Text), ("c", ColumnType::Text)],
        )];
        let chain = desired(
            vec![table("t", Backend::Postgres, &cols)],
            vec![col_hint("t", "a", "b"), col_hint("t", "b", "c")],
        );
        assert!(matches!(
            guard_plan(&diff_schema(&base, &chain, OPTS), ALLOW),
            Err(DiffError::RenameConflict { .. })
        ));
    }

    #[test]
    fn a_hint_on_a_skipped_field_is_refused() {
        let base = vec![table(
            "posts",
            Backend::Postgres,
            &[("state", ColumnType::Text)],
        )];
        let mut want = desired(
            vec![table("posts", Backend::Postgres, &[])],
            vec![col_hint("posts", "status", "state")],
        );
        want.diagnostics
            .push(crate::schema::parse::SchemaDiagnostic {
                model: "Post".to_owned(),
                table: "posts".to_owned(),
                field: "status".to_owned(),
                rust_type: "PostStatus".to_owned(),
                message: "skipped".to_owned(),
            });
        let err = guard_plan(&diff_schema(&base, &want, OPTS), ALLOW).unwrap_err();
        assert!(matches!(err, DiffError::RenameConflict { .. }), "{err}");
        assert!(err.to_string().contains("status"), "{err}");
    }

    #[test]
    fn an_index_whose_shape_changes_is_not_renamed() {
        let base = vec![with_unique_email(
            table("users", Backend::Postgres, &[("email", ColumnType::Text)]),
            "email",
        )];
        let mut want_t = table("users", Backend::Postgres, &[("mail", ColumnType::Text)]);
        want_t.indexes.push(Index::new(
            "idx_users_mail_unique",
            vec!["mail".to_owned(), "id".to_owned()],
            true,
        ));
        let want = desired(vec![want_t], vec![col_hint("users", "mail", "email")]);
        let plan = diff_schema(&base, &want, OPTS);
        assert!(
            !plan
                .changes
                .iter()
                .any(|c| matches!(c, SchemaChange::RenameIndex { .. })),
            "{:?}",
            plan.changes
        );
    }

    #[test]
    fn an_index_name_taken_on_another_table_is_not_reused() {
        let users = with_unique_email(
            table("users", Backend::Postgres, &[("email", ColumnType::Text)]),
            "email",
        );
        // Another table already owns the target name.
        let mut other = table("audit", Backend::Postgres, &[("x", ColumnType::Text)]);
        other.indexes.push(Index::new(
            "idx_users_mail_unique",
            vec!["x".to_owned()],
            true,
        ));
        let base = vec![users, other.clone()];
        let want = desired(
            vec![
                with_unique_email(
                    table("users", Backend::Postgres, &[("mail", ColumnType::Text)]),
                    "mail",
                ),
                other,
            ],
            vec![col_hint("users", "mail", "email")],
        );
        let changes = rename_changes(&base, &want);
        assert!(
            !changes
                .iter()
                .any(|c| matches!(c, SchemaChange::RenameIndex { .. })),
            "{changes:?}"
        );
    }

    #[test]
    fn renamed_baseline_rewrites_exclude_constraints() {
        let mut t = table("posts", Backend::Postgres, &[("span", ColumnType::Text)]);
        let mut ex = Index::new("posts_span_excl", vec!["span".to_owned()], true);
        ex.definition = Some(
            "ALTER TABLE posts ADD CONSTRAINT posts_span_excl EXCLUDE USING gist (span WITH &&)"
                .to_owned(),
        );
        t.indexes.push(ex);
        let renamed = renamed_baseline(
            &[t],
            &[
                SchemaChange::RenameColumn {
                    table: "posts".to_owned(),
                    from: "span".to_owned(),
                    to: "period".to_owned(),
                },
                SchemaChange::RenameTable {
                    from: "posts".to_owned(),
                    to: "bookings".to_owned(),
                },
            ],
        );
        assert_eq!(
            renamed[0].indexes[0].definition.as_deref(),
            Some(
                "ALTER TABLE bookings ADD CONSTRAINT posts_span_excl EXCLUDE USING gist (period WITH &&)"
            )
        );
    }

    #[test]
    fn word_rewrite_skips_functions_and_casts_and_ignores_case() {
        assert_eq!(
            replace_word(
                "date >= date('2000-01-01') AND x::date IS NOT NULL",
                "date",
                "published_on"
            ),
            "published_on >= date('2000-01-01') AND x::date IS NOT NULL"
        );
        assert_eq!(
            replace_word("date >= DATE '2020-01-01'", "date", "published_on"),
            "published_on >= DATE '2020-01-01'"
        );
        assert_eq!(
            replace_word("lower(TITLE)", "title", "headline"),
            "lower(headline)"
        );
        assert_eq!(
            replace_word("[title] > 0", "title", "headline"),
            "[headline] > 0"
        );
        assert_eq!(
            rename_index_column("CREATE INDEX i\nON posts (title)", "title", "headline"),
            "CREATE INDEX i\nON posts (headline)"
        );
    }

    #[test]
    fn plain_identifier_rule() {
        for ok in ["title", "_x", "a1_b2"] {
            assert!(is_plain_identifier(ok), "{ok}");
        }
        for bad in ["", "Title", "1a", "a b", "a;b", "a\"b", "a-b"] {
            assert!(!is_plain_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn rename_is_described_in_the_plan() {
        let text = crate::schema::diff::describe_plan(&MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::RenameColumn {
                table: "posts".to_owned(),
                from: "title".to_owned(),
                to: "headline".to_owned(),
            }],
        });
        assert!(
            text.contains("RENAME COLUMN posts.title TO headline"),
            "{text}"
        );
    }
}
