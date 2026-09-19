//! Recover a table's existing indexes from earlier migrations (issue #1906).
//!
//! `SQLite` refuses `ALTER TABLE … DROP COLUMN` while any index still names the
//! column, so a `Remove…From…` migration must drop those indexes first. The
//! generator names the indexes it creates itself (`idx_<table>_<col>`), but a
//! composite, partial, expression or hand-named index from an earlier migration
//! is invisible to it. This module replays `migrations/*/up.sql` in timestamp
//! order to recover the indexes that are live on a table when the new migration
//! runs.
//!
//! Static text analysis only: `generate migration` is offline and has no
//! database to introspect.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::migrate::safety::{normalize_statement, split_statements, strip_block_comments};

/// A `CREATE INDEX` on the target table that no later migration dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorIndex {
    /// Index name, as written in the migration.
    pub name: String,
    /// The whole `CREATE INDEX …` statement, so a rollback can re-create it.
    pub create_sql: String,
    /// Lowercased identifier tokens from everything after `ON <table>`: the key
    /// columns, any expression operands, and a partial index's `WHERE` columns.
    /// `SQLite` refuses to drop a column named by any of them.
    tokens: Vec<String>,
}

impl PriorIndex {
    /// Whether this index names `column`, so `SQLite` would refuse to drop that
    /// column while the index exists.
    #[must_use]
    pub fn covers(&self, column: &str) -> bool {
        let needle = column.to_lowercase();
        self.tokens.contains(&needle)
    }
}

/// Indexes live on `table` after replaying every migration in `migrations_dir`.
///
/// Returns them in creation order. An unreadable directory yields an empty list:
/// this is a best-effort improvement to the emitted DDL, never a hard failure of
/// `generate migration`.
#[must_use]
pub fn scan_prior_indexes(migrations_dir: &Path, table: &str) -> Vec<PriorIndex> {
    let statements = migration_statements(migrations_dir);
    // The table's name at each point in history, and the first statement that
    // belongs to this incarnation of it.
    let (names_at, from) = table_identity(&statements, table);

    // Insertion-ordered by a monotonic sequence, so the result keeps creation
    // order while `DROP INDEX` can still remove by name.
    let mut live: BTreeMap<String, (usize, PriorIndex)> = BTreeMap::new();
    // Index names are global in SQLite, not per table. `IF NOT EXISTS` no-ops
    // against a name taken by ANY table, so track every live name — not only the
    // ones on the table being scanned.
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut seq = 0usize;

    for (i, (raw, normalized)) in statements.iter().enumerate().skip(from) {
        let name = &names_at[i];
        if let Some(header) = parse_index_header(normalized) {
            let key = normalize_identifier(header.name);
            // SQLite keeps the ORIGINAL definition here; replacing it would let
            // a later `Remove…From…` drop an index it never looked at.
            if header.if_not_exists && taken.contains(&key) {
                continue;
            }
            taken.insert(key.clone());
            if let Some(index) = parse_create_index(raw, &header, name) {
                live.insert(key, (seq, index));
                seq += 1;
            } else {
                // A create on some other table still claims the name.
                live.remove(&key);
            }
        } else if let Some(dropped) = parse_dropped_index_name(normalized) {
            taken.remove(&dropped);
            live.remove(&dropped);
        } else if let Some((old, new)) = parse_column_rename(normalized, name) {
            // SQLite rewrites an index's column references on RENAME COLUMN, so
            // both the recorded tokens and the replay SQL must follow. A stale
            // `create_sql` would make the rollback re-create the index against a
            // column name that no longer exists.
            for (_, index) in live.values_mut() {
                for token in &mut index.tokens {
                    if *token == old {
                        token.clone_from(&new);
                    }
                }
                index.create_sql = rename_identifier(&index.create_sql, &old, &new);
            }
        } else if let Some((old, new)) = parse_table_rename(normalized)
            && old == *name
        {
            // The index moves with its table, so its replay SQL must name the
            // new table or the rollback re-creates it against one that is gone.
            for (_, index) in live.values_mut() {
                index.create_sql = rename_identifier(&index.create_sql, &old, &new);
            }
        } else if drops_table(normalized, name) {
            // Every index on the table goes with it.
            live.clear();
        }
    }

    let mut out: Vec<(usize, PriorIndex)> = live.into_values().collect();
    out.sort_by_key(|(seq, _)| *seq);
    out.into_iter().map(|(_, index)| index).collect()
}

/// Every `up.sql` statement under `migrations_dir`, in migration order, as
/// `(raw, normalized)`.
///
/// Block comments are stripped before splitting: a `;` inside `/* … */` would
/// otherwise cut a statement in half. String literals are blanked before
/// normalizing, so a `--` inside a value cannot truncate the rest of the
/// statement.
fn migration_statements(migrations_dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(migrations_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // Migration dirs are timestamp-prefixed, so name order is chronological.
    dirs.sort();

    let mut out = Vec::new();
    for dir in dirs {
        let Ok(sql) = std::fs::read_to_string(dir.join("up.sql")) else {
            continue;
        };
        for raw in split_statements(&strip_block_comments(&sql)) {
            let normalized = normalize_statement(&blank_string_literals(&raw));
            out.push((raw, normalized));
        }
    }
    out
}

/// The name `table` carried before each statement, and the index of the first
/// statement that belongs to this incarnation of the table.
///
/// The incarnation begins at the `CREATE TABLE` that made it, or just after the
/// `DROP TABLE` that ended the previous one — whichever the backward walk meets
/// first. Without the `CREATE TABLE` boundary, a history that renames `posts`
/// to `archived_posts` and then creates a NEW `posts` would attribute the
/// archived table's indexes to the new one.
///
/// A timeless set of every name the table ever had is not enough: a rename frees
/// the old name, and a later `CREATE TABLE` can reuse it for an unrelated table.
/// Matching by name alone would then attribute that table's indexes to this one
/// and emit a `DROP INDEX` against it. So the history is walked backwards from
/// the final name, one statement at a time. A `DROP TABLE` of the name in force
/// at that point ends the search: everything before it was a different table.
fn table_identity(statements: &[(String, String)], table: &str) -> (Vec<String>, usize) {
    let mut names_at = vec![String::new(); statements.len()];
    let mut current = table.to_lowercase();
    let mut boundary: Option<usize> = None;
    for i in (0..statements.len()).rev() {
        let normalized = &statements[i].1;
        // Apply the rename before recording: at this statement the table still
        // had its earlier name.
        if let Some((old, new)) = parse_table_rename(normalized)
            && new == current
        {
            current = old;
        }
        names_at[i].clone_from(&current);
        if boundary.is_none() {
            if creates_table(normalized, &current) {
                // This incarnation of the table begins here.
                boundary = Some(i);
            } else if drops_table(normalized, &current) {
                // The previous incarnation ended here.
                boundary = Some(i + 1);
            }
        }
    }
    (names_at, boundary.unwrap_or(0))
}

/// Whether the statement creates `table`.
fn creates_table(normalized: &str, table: &str) -> bool {
    let Some(rest) = normalized.strip_prefix("create table ") else {
        return false;
    };
    let rest = rest.strip_prefix("if not exists ").unwrap_or(rest);
    rest.split([' ', '(', ';'])
        .next()
        .is_some_and(|named| normalize_identifier(named) == table)
}

/// `(old, new)` column names for an `ALTER TABLE <table> RENAME [COLUMN] <old>
/// TO <new>` on `table`, lowercased. `None` for a table rename.
fn parse_column_rename(normalized: &str, table: &str) -> Option<(String, String)> {
    let rest = normalized.strip_prefix("alter table ")?;
    let (named, rest) = rest.split_once(" rename ")?;
    if normalize_identifier(named) != table {
        return None;
    }
    // `RENAME TO <table>` is a table rename, not a column rename.
    let rest = rest.strip_prefix("column ").unwrap_or(rest);
    let (old, rest) = rest.split_once(" to ")?;
    if old.trim().is_empty() {
        return None;
    }
    let new = rest.split([' ', ';']).next()?;
    Some((normalize_identifier(old), normalize_identifier(new)))
}

/// `(old, new)` for an `ALTER TABLE <old> RENAME TO <new>` statement.
fn parse_table_rename(normalized: &str) -> Option<(String, String)> {
    let rest = normalized.strip_prefix("alter table ")?;
    let (old, rest) = rest.split_once(" rename to ")?;
    let new = rest.split([' ', ';']).next()?;
    Some((normalize_identifier(old), normalize_identifier(new)))
}

/// Build a [`PriorIndex`] from an already-parsed `header` when the index targets
/// `table`. `raw` is the original statement text, kept for the rollback
/// re-create.
fn parse_create_index(raw: &str, header: &IndexHeader<'_>, table: &str) -> Option<PriorIndex> {
    let after_table = strip_on_table(header.after_name, table)?;
    Some(PriorIndex {
        // Take the name from the raw statement, so the emitted `DROP INDEX`
        // keeps the original casing and quoting.
        name: raw_token_matching(raw, header.name),
        create_sql: recreate_sql(raw),
        tokens: identifier_tokens(after_table),
    })
}

/// The name and options of a `CREATE INDEX`, before its target table is known.
struct IndexHeader<'a> {
    /// Index name, lowercased as it appears in the normalized statement.
    name: &'a str,
    /// Whether the statement carries `IF NOT EXISTS`.
    if_not_exists: bool,
    /// The rest of the statement, starting at `on <table>`.
    after_name: &'a str,
}

/// Parse the leading `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS]
/// <name>` of a normalized statement, whatever table it targets.
fn parse_index_header(normalized: &str) -> Option<IndexHeader<'_>> {
    let rest = normalized
        .strip_prefix("create unique index ")
        .or_else(|| normalized.strip_prefix("create index "))?;
    // `CONCURRENTLY` is Postgres-only. Accept the spelling anyway: a Postgres
    // migration history is still worth reading correctly.
    let rest = rest.strip_prefix("concurrently ").unwrap_or(rest);
    let (rest, if_not_exists) = rest
        .strip_prefix("if not exists ")
        .map_or((rest, false), |r| (r, true));
    let (name, after_name) = split_index_name(rest)?;
    Some(IndexHeader {
        name,
        if_not_exists,
        after_name,
    })
}

/// Split an index name off the front of `rest`, honoring a double-quoted name
/// that contains spaces. Returns the name and the remainder.
fn split_index_name(rest: &str) -> Option<(&str, &str)> {
    if let Some(body) = rest.strip_prefix('"') {
        let close = body.find('"')?;
        // +2 for the two quote characters.
        return Some((&rest[..close + 2], rest[close + 2..].trim_start()));
    }
    let end = rest.find([' ', '('])?;
    Some((&rest[..end], rest[end..].trim_start()))
}

/// Strip a leading `on <table>` from `rest`, returning what follows, or `None`
/// when the index targets a different table.
///
/// Accepts a double-quoted table name and a `schema.` prefix, the two forms a
/// hand-written migration realistically uses.
fn strip_on_table<'a>(rest: &'a str, table: &str) -> Option<&'a str> {
    let after_on = rest.strip_prefix("on ")?;
    // The table name runs to the key list, the `USING` clause, or end of input.
    let end = after_on.find(['(', ' ']).unwrap_or(after_on.len());
    let (named, tail) = after_on.split_at(end);
    (normalize_identifier(named) == table).then_some(tail)
}

/// Index name dropped by a `DROP INDEX [CONCURRENTLY] [IF EXISTS] <name>`
/// statement, lowercased and unquoted.
fn parse_dropped_index_name(normalized: &str) -> Option<String> {
    let rest = normalized.strip_prefix("drop index ")?;
    let rest = rest.strip_prefix("concurrently ").unwrap_or(rest);
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    let name = split_index_name(rest).map_or_else(|| rest.trim(), |(name, _)| name);
    (!name.is_empty()).then(|| normalize_identifier(name))
}

/// Whether the statement drops `table` outright, taking its indexes.
fn drops_table(normalized: &str, table: &str) -> bool {
    let Some(rest) = normalized.strip_prefix("drop table ") else {
        return false;
    };
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    rest.split([' ', ';', ','])
        .next()
        .is_some_and(|named| normalize_identifier(named) == table)
}

/// Lowercase an SQL identifier: drop double quotes, a trailing `;`, and any
/// `schema.` qualifier.
fn normalize_identifier(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches(';').trim_matches('"');
    trimmed
        .rsplit_once('.')
        .map_or(trimmed, |(_, name)| name)
        .trim_matches('"')
        .to_lowercase()
}

/// The `raw` statement's spelling of the token whose normalized form is
/// `lowercased`, falling back to `lowercased` when the raw text splits
/// differently (a line break inside a quoted name).
fn raw_token_matching(raw: &str, lowercased: &str) -> String {
    raw.split_whitespace()
        .find(|t| t.to_lowercase() == lowercased)
        .map_or_else(|| lowercased.to_owned(), str::to_owned)
}

/// The statement as a replayable `CREATE INDEX …;`.
///
/// [`split_statements`] hands back the chunk between semicolons, so comment
/// lines that preceded the statement ride along and the terminator is gone.
/// Drop those comments and restore the `;`. When the last line still carries a
/// `--` comment, the `;` goes on its own line: appending it would put the
/// terminator inside the comment.
fn recreate_sql(raw: &str) -> String {
    let body: Vec<&str> = raw
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.trim().starts_with("--"))
        .collect();
    let joined = body.join("\n");
    let trimmed = joined.trim().trim_end_matches(';').trim_end();
    let last_line_is_commented = trimmed
        .lines()
        .next_back()
        .is_some_and(|l| blank_string_literals(l).contains("--"));
    if last_line_is_commented {
        format!("{trimmed}\n;")
    } else {
        format!("{trimmed};")
    }
}

/// Replace whole-identifier occurrences of `old` with `new` in `sql`.
///
/// Only complete identifiers are rewritten, so renaming `title` leaves
/// `title_slug` alone. Single-quoted literals are skipped, as is a name followed
/// by `(` — that is a function call, not a column. Double quotes sit outside the
/// identifier, so a quoted `"title"` is rewritten and stays quoted.
fn rename_identifier(sql: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut pending = String::new();
    let mut in_literal = false;
    let flush = |out: &mut String, pending: &mut String, next: Option<char>| {
        if !pending.is_empty() {
            if pending.to_lowercase() == old && next != Some('(') {
                out.push_str(new);
            } else {
                out.push_str(pending);
            }
            pending.clear();
        }
    };
    for c in sql.chars() {
        if in_literal {
            if c == '\'' {
                in_literal = false;
            }
            out.push(c);
            continue;
        }
        if c == '\'' {
            flush(&mut out, &mut pending, Some(c));
            in_literal = true;
            out.push(c);
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            pending.push(c);
            continue;
        }
        flush(&mut out, &mut pending, Some(c));
        out.push(c);
    }
    flush(&mut out, &mut pending, None);
    out
}

/// Replace the contents of single-quoted literals with spaces, keeping the
/// quotes. A `--`, a `;` or a column-like word inside a value then cannot be
/// mistaken for SQL.
fn blank_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_literal = false;
    for c in sql.chars() {
        match c {
            '\'' => {
                in_literal = !in_literal;
                out.push(c);
            }
            _ if in_literal => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Lowercased identifier tokens in `sql`, excluding function names.
///
/// Splitting on non-alphanumeric characters keeps `deleted_at` and `título`
/// whole, so a column name can never match a longer identifier that merely
/// contains it. A token followed by `(` is a call, not a column, so
/// `date(created_at)` yields `created_at` alone.
fn identifier_tokens(sql: &str) -> Vec<String> {
    let sql = blank_string_literals(sql);
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in sql.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            start.get_or_insert(i);
            continue;
        }
        if let Some(from) = start.take() {
            push_identifier(&mut out, &sql[from..i], &sql[i..]);
        }
    }
    if let Some(from) = start {
        push_identifier(&mut out, &sql[from..], "");
    }
    out
}

/// Words that are `CREATE INDEX` grammar, not column names. A table may well
/// have a column called `desc` or `where`; recording the keyword as a reference
/// would drop an index that never mentioned that column.
const INDEX_GRAMMAR_KEYWORDS: &[&str] = &[
    "asc", "collate", "desc", "where", "and", "or", "not", "is", "null", "in", "like", "glob",
    "between", "on", "using", "true", "false",
];

/// Record `token` unless what follows it opens a call, or it is index grammar.
/// `lower (title)` is valid SQL, so whitespace before the parenthesis is
/// skipped.
fn push_identifier(out: &mut Vec<String>, token: &str, rest: &str) {
    if rest.trim_start().starts_with('(') {
        return;
    }
    let lowered = token.to_lowercase();
    if INDEX_GRAMMAR_KEYWORDS.contains(&lowered.as_str()) {
        return;
    }
    out.push(lowered);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrations tree whose dirs are named in the given order.
    fn tree(migrations: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (name, up) in migrations {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("up.sql"), up).unwrap();
        }
        tmp
    }

    #[test]
    fn finds_a_composite_index_covering_the_column() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_author_title ON posts (author_id, title);",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].name, "idx_posts_author_title");
        assert!(found[0].covers("title"));
        assert!(found[0].covers("author_id"));
        assert!(!found[0].covers("body"));
    }

    #[test]
    fn recreate_sql_is_the_original_statement() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE UNIQUE INDEX idx_posts_slug ON posts (slug);",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(
            found[0].create_sql,
            "CREATE UNIQUE INDEX idx_posts_slug ON posts (slug);"
        );
    }

    #[test]
    fn ignores_an_index_on_another_table() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_comments_title ON comments (title);",
        )]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn a_later_drop_index_removes_it() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_posts_title ON posts (title);",
            ),
            ("2026_01_02_000000_b", "DROP INDEX idx_posts_title;"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn a_later_drop_table_removes_every_index_on_it() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_posts_title ON posts (title);",
            ),
            ("2026_01_02_000000_b", "DROP TABLE posts;"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn covers_a_partial_index_where_column() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_live ON posts (title) WHERE deleted_at IS NULL;",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("deleted_at"), "got {found:?}");
    }

    #[test]
    fn covers_an_expression_index_operand() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_lower_title ON posts (lower(title));",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn quoted_and_if_not_exists_forms_are_parsed() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX IF NOT EXISTS \"idx_posts_title\" ON \"posts\" (\"title\");",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].name, "\"idx_posts_title\"");
        assert!(found[0].covers("title"));
    }

    #[test]
    fn a_commented_out_create_index_is_ignored() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "-- CREATE INDEX idx_posts_title ON posts (title);\n",
        )]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn an_absent_directory_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_prior_indexes(&tmp.path().join("nope"), "posts").is_empty());
    }

    #[test]
    fn migrations_replay_in_timestamp_order_not_readdir_order() {
        // The drop is chronologically later even if the filesystem hands the
        // dirs back in another order.
        let t = tree(&[
            ("2026_02_01_000000_z_create", "CREATE INDEX i ON posts (a);"),
            ("2026_03_01_000000_a_drop", "DROP INDEX i;"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    // ── review regressions (#1906) ────────────────────────────────────────

    #[test]
    fn a_quoted_create_matches_an_unquoted_later_drop() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX \"idx_posts_title\" ON posts (title);",
            ),
            ("2026_01_02_000000_b", "DROP INDEX idx_posts_title;"),
        ]);
        assert!(
            scan_prior_indexes(t.path(), "posts").is_empty(),
            "quoting must not defeat the drop"
        );
    }

    #[test]
    fn a_block_comment_containing_a_semicolon_does_not_hide_the_index() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "/* fast; titles */\nCREATE INDEX idx_posts_title ON posts (title);",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn a_string_literal_in_a_where_clause_is_not_a_column() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_featured ON posts (status) WHERE status = 'title';",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(!found[0].covers("title"), "got {found:?}");
        assert!(found[0].covers("status"));
    }

    #[test]
    fn a_function_name_in_an_expression_index_is_not_a_column() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_day ON posts (date(created_at));",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(!found[0].covers("date"), "got {found:?}");
        assert!(found[0].covers("created_at"));
    }

    #[test]
    fn a_non_ascii_column_name_survives_tokenizing() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_titulo ON posts (\"título\");",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("título"), "got {found:?}");
    }

    #[test]
    fn a_double_dash_inside_a_literal_does_not_truncate_the_statement() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_live ON posts (slug) \
             WHERE tag = 'a--b' AND title IS NOT NULL;",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn a_trailing_comment_does_not_swallow_the_terminator() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_title ON posts (title)\n-- TODO: make partial\n;\n",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(
            found[0].create_sql.ends_with("\n;"),
            "the `;` must not land inside the comment: {:?}",
            found[0].create_sql
        );
    }

    #[test]
    fn an_index_survives_a_later_table_rename() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_articles_title ON articles (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE articles RENAME TO posts;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("title"));
    }

    #[test]
    fn an_index_follows_a_later_column_rename() {
        // SQLite rewrites the index's column reference on RENAME COLUMN, so a
        // later `RemoveHeadlineFromPosts` must still find the index.
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_posts_title ON posts (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE posts RENAME COLUMN title TO headline;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("headline"), "got {found:?}");
        assert!(!found[0].covers("title"), "the old name is gone: {found:?}");
    }

    #[test]
    fn a_reused_old_name_does_not_capture_the_new_tables_indexes() {
        // `articles` is renamed to `posts`, then a DIFFERENT `articles` is
        // created. Its index must not be attributed to `posts` — a generated
        // Remove…From… would otherwise DROP INDEX against the unrelated table.
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_old_title ON articles (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE articles RENAME TO posts;",
            ),
            (
                "2026_01_03_000000_c",
                "CREATE TABLE articles (id INTEGER PRIMARY KEY, title TEXT);\n\
                 CREATE UNIQUE INDEX idx_new_title ON articles (title);",
            ),
        ]);
        let names: Vec<String> = scan_prior_indexes(t.path(), "posts")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["idx_old_title"], "got {names:?}");
    }

    #[test]
    fn a_table_dropped_and_recreated_starts_from_the_new_one() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_gone ON posts (title);",
            ),
            ("2026_01_02_000000_b", "DROP TABLE posts;"),
            (
                "2026_01_03_000000_c",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                 CREATE INDEX idx_fresh ON posts (title);",
            ),
        ]);
        let names: Vec<String> = scan_prior_indexes(t.path(), "posts")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["idx_fresh"], "got {names:?}");
    }

    #[test]
    fn index_grammar_keywords_are_not_column_references() {
        // A table may have a column called `desc`; dropping it must not take an
        // unrelated unique index on `title` with it.
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE UNIQUE INDEX ux ON posts (title DESC) WHERE body IS NOT NULL;",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        for keyword in ["desc", "where", "is", "not", "null"] {
            assert!(!found[0].covers(keyword), "{keyword}: {found:?}");
        }
        assert!(found[0].covers("title"));
        assert!(found[0].covers("body"));
    }

    #[test]
    fn a_reused_name_after_a_rename_away_starts_a_new_incarnation() {
        // The old `posts` is renamed aside and a NEW `posts` created. The
        // archived table's index must not be attributed to the new table.
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                 CREATE UNIQUE INDEX ux_old ON posts (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE posts RENAME TO archived_posts;",
            ),
            (
                "2026_01_03_000000_c",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                 CREATE INDEX ix_new ON posts (title);",
            ),
        ]);
        let names: Vec<String> = scan_prior_indexes(t.path(), "posts")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["ix_new"], "got {names:?}");
    }

    #[test]
    fn a_table_rename_rewrites_the_replay_sql() {
        // The rollback would otherwise re-create the index on a table that no
        // longer exists under that name.
        let t = tree(&[
            ("2026_01_01_000000_a", "CREATE INDEX i ON articles (title);"),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE articles RENAME TO posts;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(
            found[0].create_sql, "CREATE INDEX i ON posts (title);",
            "got {:?}",
            found[0].create_sql
        );
    }

    #[test]
    fn if_not_exists_keeps_the_original_definition() {
        // SQLite no-ops the second CREATE, leaving the index on `body`. Taking
        // the second definition would make RemoveTitleFromPosts drop a live
        // index on a column it never touched.
        let t = tree(&[
            ("2026_01_01_000000_a", "CREATE INDEX i ON posts (body);"),
            (
                "2026_01_02_000000_b",
                "CREATE INDEX IF NOT EXISTS i ON posts (title);",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("body"), "got {found:?}");
        assert!(!found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn if_not_exists_respects_a_name_taken_by_another_table() {
        // Index names are global in SQLite, so this create no-ops too.
        let t = tree(&[
            ("2026_01_01_000000_a", "CREATE INDEX i ON comments (body);"),
            (
                "2026_01_02_000000_b",
                "CREATE INDEX IF NOT EXISTS i ON posts (title);",
            ),
        ]);
        assert!(
            scan_prior_indexes(t.path(), "posts").is_empty(),
            "the name was already taken"
        );
    }

    #[test]
    fn a_plain_create_on_another_table_reclaims_the_name() {
        // Without IF NOT EXISTS the name is re-pointed, so `posts` loses it.
        let t = tree(&[
            ("2026_01_01_000000_a", "CREATE INDEX i ON posts (title);"),
            ("2026_01_02_000000_b", "CREATE INDEX i ON comments (body);"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn a_spaced_function_call_is_not_a_column() {
        // `lower (title)` is valid SQL; recording `lower` as a column would drop
        // this unique index when a column named `lower` is removed.
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE UNIQUE INDEX ux ON posts (lower (title));",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(!found[0].covers("lower"), "got {found:?}");
        assert!(found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn a_column_rename_rewrites_the_replay_sql_too() {
        // A stale `create_sql` would make the rollback re-create the index
        // against a column name that no longer exists.
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX i ON posts (title, title_slug) WHERE title IS NOT NULL;",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE posts RENAME COLUMN title TO headline;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(
            found[0].create_sql,
            "CREATE INDEX i ON posts (headline, title_slug) WHERE headline IS NOT NULL;",
            "`title_slug` must not be touched"
        );
    }

    #[test]
    fn a_column_rename_leaves_string_literals_and_calls_alone() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX i ON posts (title(x)) WHERE tag = 'title';",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE posts RENAME COLUMN title TO headline;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(
            found[0].create_sql, "CREATE INDEX i ON posts (title(x)) WHERE tag = 'title';",
            "a call name and a literal are not the column"
        );
    }

    #[test]
    fn a_column_rename_on_another_table_is_ignored() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_posts_title ON posts (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE comments RENAME COLUMN title TO headline;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn a_table_rename_is_not_read_as_a_column_rename() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX idx_articles_title ON articles (title);",
            ),
            (
                "2026_01_02_000000_b",
                "ALTER TABLE articles RENAME TO posts;",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert!(found[0].covers("title"), "got {found:?}");
    }

    #[test]
    fn a_quoted_index_name_containing_a_space_is_parsed() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE INDEX \"my idx\" ON posts (title);",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].name, "\"my idx\"");
    }

    #[test]
    fn drop_index_if_exists_and_concurrently_forms_are_honored() {
        for drop in [
            "DROP INDEX IF EXISTS idx_posts_title;",
            "DROP INDEX CONCURRENTLY idx_posts_title;",
            "DROP INDEX CONCURRENTLY IF EXISTS idx_posts_title;",
        ] {
            let t = tree(&[
                (
                    "2026_01_01_000000_a",
                    "CREATE INDEX idx_posts_title ON posts (title);",
                ),
                ("2026_01_02_000000_b", drop),
            ]);
            assert!(
                scan_prior_indexes(t.path(), "posts").is_empty(),
                "not dropped by: {drop}"
            );
        }
    }

    #[test]
    fn drop_table_if_exists_clears_indexes_and_another_table_does_not() {
        let create = (
            "2026_01_01_000000_a",
            "CREATE INDEX idx_posts_title ON posts (title);",
        );
        let t = tree(&[
            create,
            ("2026_01_02_000000_b", "DROP TABLE IF EXISTS posts;"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());

        let t = tree(&[create, ("2026_01_02_000000_b", "DROP TABLE comments;")]);
        assert_eq!(scan_prior_indexes(t.path(), "posts").len(), 1);
    }

    #[test]
    fn a_leading_comment_is_stripped_from_the_recreate_sql() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "-- speeds up the archive page\nCREATE INDEX idx_posts_title ON posts (title);",
        )]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(
            found[0].create_sql,
            "CREATE INDEX idx_posts_title ON posts (title);"
        );
    }

    #[test]
    fn a_schema_qualified_create_matches_a_bare_drop() {
        let t = tree(&[
            (
                "2026_01_01_000000_a",
                "CREATE INDEX main.idx_posts_title ON main.posts (title);",
            ),
            ("2026_01_02_000000_b", "DROP INDEX idx_posts_title;"),
        ]);
        assert!(scan_prior_indexes(t.path(), "posts").is_empty());
    }

    #[test]
    fn a_recreated_index_keeps_its_latest_definition() {
        let t = tree(&[
            ("2026_01_01_000000_a", "CREATE INDEX i ON posts (title);"),
            (
                "2026_01_02_000000_b",
                "DROP INDEX i;\nCREATE INDEX i ON posts (title, body);",
            ),
        ]);
        let found = scan_prior_indexes(t.path(), "posts");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("body"));
    }
}
