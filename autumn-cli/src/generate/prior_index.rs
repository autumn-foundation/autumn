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

use std::collections::BTreeMap;
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
    // A table carries its indexes through a rename, so match every name it has
    // ever had. Renames are collected first because they happen after the
    // `CREATE INDEX` statements that used the older name.
    let names = table_aliases(&statements, table);

    // Insertion-ordered by a monotonic sequence, so the result keeps creation
    // order while `DROP INDEX` can still remove by name.
    let mut live: BTreeMap<String, (usize, PriorIndex)> = BTreeMap::new();
    let mut seq = 0usize;

    for (raw, normalized) in &statements {
        if let Some(index) = parse_create_index(raw, normalized, &names) {
            live.insert(normalize_identifier(&index.name), (seq, index));
            seq += 1;
        } else if let Some(name) = parse_dropped_index_name(normalized) {
            live.remove(&name);
        } else if drops_table(normalized, &names) {
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

/// Every name `table` has had, following `ALTER TABLE … RENAME TO` backwards.
fn table_aliases(statements: &[(String, String)], table: &str) -> Vec<String> {
    let renames: Vec<(String, String)> = statements
        .iter()
        .filter_map(|(_, n)| parse_table_rename(n))
        .collect();
    let mut names = vec![table.to_lowercase()];
    // Walk the chain backwards: the last rename into a known name reveals the
    // name the table had before it.
    for (from, to) in renames.iter().rev() {
        if names.contains(to) && !names.contains(from) {
            names.push(from.clone());
        }
    }
    names
}

/// `(old, new)` for an `ALTER TABLE <old> RENAME TO <new>` statement.
fn parse_table_rename(normalized: &str) -> Option<(String, String)> {
    let rest = normalized.strip_prefix("alter table ")?;
    let (old, rest) = rest.split_once(" rename to ")?;
    let new = rest.split([' ', ';']).next()?;
    Some((normalize_identifier(old), normalize_identifier(new)))
}

/// Parse a `CREATE [UNIQUE] INDEX [IF NOT EXISTS] <name> ON <table> …`
/// statement targeting any of `tables`. `raw` is the original text (kept for the
/// rollback re-create); `normalized` is its comment-stripped, lowercased,
/// whitespace-collapsed form.
fn parse_create_index(raw: &str, normalized: &str, tables: &[String]) -> Option<PriorIndex> {
    let rest = normalized
        .strip_prefix("create unique index ")
        .or_else(|| normalized.strip_prefix("create index "))?;
    // `CONCURRENTLY` is Postgres-only. Accept the spelling anyway: a Postgres
    // migration history is still worth reading correctly.
    let rest = rest.strip_prefix("concurrently ").unwrap_or(rest);
    let rest = rest.strip_prefix("if not exists ").unwrap_or(rest);

    let (name, after_name) = split_index_name(rest)?;
    let after_table = strip_on_table(after_name, tables)?;

    Some(PriorIndex {
        // Take the name from the raw statement, so the emitted `DROP INDEX`
        // keeps the original casing and quoting.
        name: raw_token_matching(raw, name),
        create_sql: recreate_sql(raw),
        tokens: identifier_tokens(after_table),
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
/// when the index targets some other table.
///
/// Accepts a double-quoted table name and a `schema.` prefix, the two forms a
/// hand-written migration realistically uses.
fn strip_on_table<'a>(rest: &'a str, tables: &[String]) -> Option<&'a str> {
    let after_on = rest.strip_prefix("on ")?;
    // The table name runs to the key list, the `USING` clause, or end of input.
    let end = after_on.find(['(', ' ']).unwrap_or(after_on.len());
    let (named, tail) = after_on.split_at(end);
    tables
        .contains(&normalize_identifier(named))
        .then_some(tail)
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

/// Whether the statement drops one of `tables` outright, taking its indexes.
fn drops_table(normalized: &str, tables: &[String]) -> bool {
    let Some(rest) = normalized.strip_prefix("drop table ") else {
        return false;
    };
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    rest.split([' ', ';', ','])
        .next()
        .is_some_and(|named| tables.contains(&normalize_identifier(named)))
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
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in sql.char_indices() {
        let is_ident = c.is_alphanumeric() || c == '_';
        if is_ident {
            start.get_or_insert(i);
            continue;
        }
        if let Some(from) = start.take() {
            push_identifier(&mut out, &sql[from..i], bytes.get(i).copied());
        }
    }
    if let Some(from) = start {
        push_identifier(&mut out, &sql[from..], None);
    }
    out
}

/// Record `token` unless the character that ended it opens a call.
fn push_identifier(out: &mut Vec<String>, token: &str, terminator: Option<u8>) {
    if terminator == Some(b'(') {
        return;
    }
    out.push(token.to_lowercase());
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
