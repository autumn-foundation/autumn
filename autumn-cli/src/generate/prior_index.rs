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

use crate::migrate::safety::{normalize_statement, split_statements};

/// A `CREATE INDEX` on the target table that no later migration dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorIndex {
    /// Index name, as written in the migration.
    pub name: String,
    /// The whole `CREATE INDEX …` statement, so a rollback can re-create it.
    pub create_sql: String,
    /// Lowercased identifier tokens from everything after `ON <table>` — the
    /// key columns plus any expression operands and partial-index `WHERE`
    /// columns. `SQLite` blocks the column drop for every one of them.
    tokens: Vec<String>,
}

impl PriorIndex {
    /// Whether this index names `column`, so `SQLite` would refuse to drop it.
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

    // Insertion-ordered by a monotonic sequence so the result keeps creation
    // order while `DROP INDEX` can still remove by name.
    let mut live: BTreeMap<String, (usize, PriorIndex)> = BTreeMap::new();
    let mut seq = 0usize;

    for dir in dirs {
        let Ok(sql) = std::fs::read_to_string(dir.join("up.sql")) else {
            continue;
        };
        for stmt in split_statements(&sql) {
            let normalized = normalize_statement(&stmt);
            if let Some(index) = parse_create_index(&stmt, &normalized, table) {
                live.insert(index.name.to_lowercase(), (seq, index));
                seq += 1;
            } else if let Some(name) = parse_dropped_index_name(&normalized) {
                live.remove(&name);
            } else if drops_table(&normalized, table) {
                // Every index on the table goes with it.
                live.clear();
            }
        }
    }

    let mut out: Vec<(usize, PriorIndex)> = live.into_values().collect();
    out.sort_by_key(|(seq, _)| *seq);
    out.into_iter().map(|(_, index)| index).collect()
}

/// Parse a `CREATE [UNIQUE] INDEX [IF NOT EXISTS] <name> ON <table> …`
/// statement targeting `table`. `raw` is the original text (kept verbatim for
/// the rollback re-create); `normalized` is its comment-stripped, lowercased,
/// whitespace-collapsed form.
fn parse_create_index(raw: &str, normalized: &str, table: &str) -> Option<PriorIndex> {
    let rest = normalized
        .strip_prefix("create unique index ")
        .or_else(|| normalized.strip_prefix("create index "))?;
    // `CONCURRENTLY` is Postgres-only, but a Postgres migration history is
    // still worth reading correctly.
    let rest = rest.strip_prefix("concurrently ").unwrap_or(rest);
    let rest = rest.strip_prefix("if not exists ").unwrap_or(rest);

    let (name, after_name) = rest.split_once(' ')?;
    let after_table = strip_on_table(after_name, table)?;

    Some(PriorIndex {
        // Take the name from the raw statement, so the emitted `DROP INDEX`
        // keeps the original casing and quoting.
        name: raw_token_matching(raw, name),
        create_sql: recreate_sql(raw),
        tokens: identifier_tokens(after_table),
    })
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
    (normalize_identifier(named) == table.to_lowercase()).then_some(tail)
}

/// Index name dropped by a `DROP INDEX [CONCURRENTLY] [IF EXISTS] <name>`
/// statement, lowercased and unquoted.
fn parse_dropped_index_name(normalized: &str) -> Option<String> {
    let rest = normalized.strip_prefix("drop index ")?;
    let rest = rest.strip_prefix("concurrently ").unwrap_or(rest);
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    let name = rest.split([' ', ';', ',']).next()?;
    (!name.is_empty()).then(|| normalize_identifier(name))
}

/// Whether the statement drops `table` outright, taking its indexes with it.
fn drops_table(normalized: &str, table: &str) -> bool {
    let Some(rest) = normalized.strip_prefix("drop table ") else {
        return false;
    };
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    rest.split([' ', ';', ','])
        .next()
        .is_some_and(|named| normalize_identifier(named) == table.to_lowercase())
}

/// The statement as a replayable `CREATE INDEX …;`.
///
/// [`split_statements`] hands back the chunk between semicolons, so any comment
/// lines that preceded the statement ride along and the terminator is gone.
/// Drop the leading comments and restore the `;`.
fn recreate_sql(raw: &str) -> String {
    let body: Vec<&str> = raw
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.trim().starts_with("--"))
        .collect();
    format!("{};", body.join("\n").trim().trim_end_matches(';'))
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
/// differently (a comment or line break inside the name).
fn raw_token_matching(raw: &str, lowercased: &str) -> String {
    raw.split_whitespace()
        .find(|t| t.to_lowercase() == lowercased)
        .map_or_else(|| lowercased.to_owned(), str::to_owned)
}

/// Lowercased identifier-shaped tokens in `sql`.
///
/// Splitting on everything but `[A-Za-z0-9_]` keeps `deleted_at` whole, so a
/// column name can never match a longer identifier that merely contains it.
fn identifier_tokens(sql: &str) -> Vec<String> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
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
}
