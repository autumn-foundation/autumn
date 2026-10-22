//! Find the indexes that block a `SQLite` `DROP COLUMN` (issues #1906, #2579).
//!
//! `SQLite` refuses `ALTER TABLE … DROP COLUMN` while an index names the column.
//! A `Remove…From…` migration must drop those indexes first.
//!
//! This module does not parse SQL. It replays `migrations/*/up.sql` into an
//! in-memory database, then asks `SQLite` to drop each column. An index blocks
//! the column when the drop fails with that index present and succeeds
//! without it. Every probe is rolled back.
//!
//! Limits (the project database is not read):
//! - An index made by hand, outside `migrations/`, is not seen.
//! - An index behind an inline `UNIQUE` or `PRIMARY KEY` cannot be dropped.
//!   The scan returns an error for it.

use std::fmt::Write as _;
use std::path::Path;

use diesel::connection::SimpleConnection;
use diesel::sql_types::Text;
use diesel::{Connection, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

/// An index on the target table that blocks the drop of a scanned column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorIndex {
    /// Index name, quoted when it is not a plain identifier.
    pub name: String,
    /// The `CREATE INDEX …` statement, so a rollback can re-create it.
    pub create_sql: String,
    /// Lowercase names of the scanned columns that this index blocks.
    blocks: Vec<String>,
}

impl PriorIndex {
    /// Whether this index blocks the drop of `column`.
    #[must_use]
    pub fn covers(&self, column: &str) -> bool {
        let needle = column.to_lowercase();
        self.blocks.contains(&needle)
    }
}

#[derive(QueryableByName)]
struct IndexRow {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    sql: String,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// The indexes on `table` that block the drop of any of `columns`, after
/// every migration in `migrations_dir` is applied. Indexes keep creation order.
///
/// A missing directory, table or column gives no indexes.
///
/// # Errors
/// Returns a message when a migration does not replay, or when a column
/// cannot be dropped even with all its indexes removed (a `PRIMARY KEY`,
/// an inline `UNIQUE`, a trigger or view that uses it).
pub fn scan_prior_indexes(
    migrations_dir: &Path,
    table: &str,
    columns: &[&str],
) -> Result<Vec<PriorIndex>, String> {
    let mut conn = SqliteConnection::establish(":memory:")
        .map_err(|e| format!("could not open an in-memory SQLite database: {e}"))?;
    replay(&mut conn, migrations_dir)?;

    let indexes = table_indexes(&mut conn, table)?;
    let mut found: Vec<PriorIndex> = indexes
        .into_iter()
        .map(|row| PriorIndex {
            name: render_name(&row.name),
            create_sql: terminated(&row.sql),
            blocks: Vec::new(),
        })
        .collect();

    for column in columns {
        if !has_column(&mut conn, table, column)? {
            continue;
        }
        let drop_column = format!("ALTER TABLE {} DROP COLUMN {}", quote(table), quote(column));
        // Control: with every index gone, the drop must work.
        let all: Vec<usize> = (0..found.len()).collect();
        if let Err(e) = probe(&mut conn, &found, &all, &drop_column) {
            return Err(format!(
                "SQLite cannot drop `{table}.{column}` even without its indexes: {e}. \
                 Rebuild the table by hand"
            ));
        }
        for i in 0..found.len() {
            let others: Vec<usize> = (0..found.len()).filter(|j| *j != i).collect();
            if probe(&mut conn, &found, &others, &drop_column).is_err() {
                found[i].blocks.push(column.to_lowercase());
            }
        }
    }
    found.retain(|i| !i.blocks.is_empty());
    Ok(found)
}

/// Apply each `up.sql` in name order. Names start with a timestamp.
fn replay(conn: &mut SqliteConnection, migrations_dir: &Path) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(migrations_dir) else {
        return Ok(());
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Ok(sql) = std::fs::read_to_string(dir.join("up.sql")) else {
            continue;
        };
        conn.batch_execute(&sql).map_err(|e| {
            format!(
                "cannot replay {} to find the table's indexes: {e}",
                dir.join("up.sql").display()
            )
        })?;
    }
    Ok(())
}

/// Named indexes on `table`, in creation order. An index that `SQLite` makes
/// for a constraint has no SQL and no name to drop, so it is left out.
fn table_indexes(conn: &mut SqliteConnection, table: &str) -> Result<Vec<IndexRow>, String> {
    sql_query(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'index' AND sql IS NOT NULL AND lower(tbl_name) = lower(?) \
         ORDER BY rowid",
    )
    .bind::<Text, _>(table)
    .load(conn)
    .map_err(|e| format!("could not read the replayed indexes: {e}"))
}

fn has_column(conn: &mut SqliteConnection, table: &str, column: &str) -> Result<bool, String> {
    let rows: Vec<CountRow> =
        sql_query("SELECT count(*) AS n FROM pragma_table_xinfo(?) WHERE lower(name) = lower(?)")
            .bind::<Text, _>(table)
            .bind::<Text, _>(column)
            .load(conn)
            .map_err(|e| format!("could not read the replayed columns: {e}"))?;
    Ok(rows.first().is_some_and(|r| r.n > 0))
}

/// Drop `removed` indexes, run `statement`, and undo everything.
fn probe(
    conn: &mut SqliteConnection,
    indexes: &[PriorIndex],
    removed: &[usize],
    statement: &str,
) -> Result<(), String> {
    conn.batch_execute("SAVEPOINT autumn_probe")
        .map_err(|e| e.to_string())?;
    let mut script = String::new();
    for i in removed {
        let _ = writeln!(script, "DROP INDEX {};", indexes[*i].name);
    }
    script.push_str(statement);
    script.push(';');
    let result = conn.batch_execute(&script).map_err(|e| e.to_string());
    conn.batch_execute("ROLLBACK TO autumn_probe; RELEASE autumn_probe")
        .map_err(|e| e.to_string())?;
    result
}

/// A double-quoted identifier.
fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `name` as written in SQL: bare when it is a plain identifier.
fn render_name(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain { name.to_owned() } else { quote(name) }
}

fn terminated(sql: &str) -> String {
    let sql = sql.trim();
    if sql.ends_with(';') {
        sql.to_owned()
    } else {
        format!("{sql};")
    }
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

    const POSTS: &str = "CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER, \
                         title TEXT, body TEXT, status TEXT);";

    fn scan(migrations: &[(&str, &str)], columns: &[&str]) -> Vec<PriorIndex> {
        let t = tree(migrations);
        scan_prior_indexes(t.path(), "posts", columns).expect("scan")
    }

    fn with_posts(index_sql: &str) -> Vec<(&'static str, String)> {
        vec![
            ("2026_01_01_000000_a", POSTS.to_owned()),
            ("2026_01_02_000000_b", index_sql.to_owned()),
        ]
    }

    fn scan_after(index_sql: &str, columns: &[&str]) -> Vec<PriorIndex> {
        let owned = with_posts(index_sql);
        let refs: Vec<(&str, &str)> = owned.iter().map(|(a, b)| (*a, b.as_str())).collect();
        scan(&refs, columns)
    }

    #[test]
    fn finds_a_composite_index_covering_the_column() {
        let found = scan_after(
            "CREATE INDEX idx_posts_author_title ON posts (author_id, title);",
            &["title", "body"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].name, "idx_posts_author_title");
        assert!(found[0].covers("title"));
        assert!(!found[0].covers("body"));
    }

    #[test]
    fn covers_only_the_scanned_columns() {
        let found = scan_after("CREATE INDEX i ON posts (author_id, title);", &["title"]);
        assert!(found[0].covers("title"));
        assert!(!found[0].covers("author_id"));
    }

    #[test]
    fn create_sql_is_ready_to_run_again() {
        let found = scan_after(
            "CREATE UNIQUE INDEX IF NOT EXISTS i ON posts (title) WHERE status = 'live'",
            &["title"],
        );
        assert_eq!(
            found[0].create_sql,
            "CREATE UNIQUE INDEX i ON posts (title) WHERE status = 'live';"
        );
    }

    #[test]
    fn ignores_an_index_on_another_table() {
        let found = scan(
            &[
                ("2026_01_01_000000_a", POSTS),
                (
                    "2026_01_02_000000_b",
                    "CREATE TABLE comments (id INTEGER PRIMARY KEY, title TEXT);\n\
                     CREATE INDEX idx_c ON comments (title);",
                ),
            ],
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn a_later_drop_index_removes_it() {
        let found = scan_after(
            "CREATE INDEX i ON posts (title);\nDROP INDEX i;",
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn a_later_drop_table_removes_every_index() {
        let found = scan(
            &[
                ("2026_01_01_000000_a", POSTS),
                ("2026_01_02_000000_b", "CREATE INDEX i ON posts (title);"),
                ("2026_01_03_000000_c", "DROP TABLE posts;"),
            ],
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn a_partial_index_where_column_blocks_the_drop() {
        let found = scan_after(
            "CREATE INDEX i ON posts (title) WHERE status = 'x';",
            &["status"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn an_expression_index_operand_blocks_the_drop() {
        let found = scan_after("CREATE INDEX i ON posts (lower (title));", &["title"]);
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn a_literal_a_function_name_and_desc_are_not_columns() {
        let found = scan_after(
            "CREATE INDEX i ON posts (lower (author_id), body DESC) WHERE status = 'title';",
            &["title", "lower", "desc"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn if_not_exists_keeps_the_first_definition() {
        let found = scan_after(
            "CREATE INDEX i ON posts (title);\nCREATE INDEX IF NOT EXISTS i ON posts (body);",
            &["title", "body"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("title"));
        assert!(!found[0].covers("body"));
    }

    #[test]
    fn a_quoted_create_matches_an_unquoted_drop() {
        let found = scan_after(
            "CREATE INDEX \"idx\" ON posts (title);\nDROP INDEX idx;",
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn comments_and_literals_cannot_confuse_the_replay() {
        let found = scan_after(
            "/* fast; titles */ CREATE INDEX i ON posts (title) -- x; y\n\
             WHERE status != 'a--b';",
            &["title"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn a_trigger_body_with_semicolons_does_not_hide_the_index() {
        let found = scan_after(
            "CREATE TRIGGER t AFTER INSERT ON posts BEGIN UPDATE posts SET body = 'x'; END;\n\
             CREATE INDEX i ON posts (title);",
            &["title"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn a_non_ascii_column_name_is_found() {
        let found = scan(
            &[
                (
                    "2026_01_01_000000_a",
                    "CREATE TABLE posts (id INTEGER PRIMARY KEY, \"título\" TEXT);",
                ),
                (
                    "2026_01_02_000000_b",
                    "CREATE INDEX i ON posts (\"título\");",
                ),
            ],
            &["título"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn column_names_ignore_case() {
        let found = scan_after("CREATE INDEX i ON posts (TITLE);", &["Title"]);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("TITLE"));
    }

    #[test]
    fn a_name_that_needs_quotes_is_quoted() {
        let found = scan_after("CREATE INDEX \"my idx\" ON posts (title);", &["title"]);
        assert_eq!(found[0].name, "\"my idx\"");
    }

    #[test]
    fn migrations_replay_in_name_order_not_readdir_order() {
        let found = scan(
            &[
                ("2026_01_02_000000_b", "DROP INDEX i;"),
                (
                    "2026_01_01_000000_a",
                    &format!("{POSTS}\nCREATE INDEX i ON posts (title);"),
                ),
            ],
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn indexes_keep_creation_order() {
        let found = scan_after(
            "CREATE INDEX z_first ON posts (title);\nCREATE INDEX a_second ON posts (title);",
            &["title"],
        );
        let names: Vec<_> = found.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["z_first", "a_second"]);
    }

    #[test]
    fn an_absent_directory_table_or_column_gives_nothing() {
        let missing = Path::new("/nonexistent/migrations");
        assert!(
            scan_prior_indexes(missing, "posts", &["title"])
                .unwrap()
                .is_empty()
        );
        let t = tree(&[("2026_01_01_000000_a", POSTS)]);
        assert!(
            scan_prior_indexes(t.path(), "nope", &["title"])
                .unwrap()
                .is_empty()
        );
        assert!(
            scan_prior_indexes(t.path(), "posts", &["nope"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_directory_without_up_sql_is_skipped() {
        let t = tree(&[("2026_01_01_000000_a", POSTS)]);
        std::fs::create_dir_all(t.path().join("2026_01_02_000000_empty")).unwrap();
        assert!(
            scan_prior_indexes(t.path(), "posts", &["title"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_table_rename_moves_the_index_and_its_sql() {
        let found = scan(
            &[
                (
                    "2026_01_01_000000_a",
                    "CREATE TABLE old_posts (id INTEGER PRIMARY KEY, title TEXT);",
                ),
                (
                    "2026_01_02_000000_b",
                    "CREATE INDEX i ON old_posts (title);",
                ),
                (
                    "2026_01_03_000000_c",
                    "ALTER TABLE old_posts RENAME TO posts;",
                ),
            ],
            &["title"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(
            found[0].create_sql.contains("ON \"posts\""),
            "{}",
            found[0].create_sql
        );
    }

    #[test]
    fn a_column_rename_moves_the_index_and_its_sql() {
        let found = scan_after(
            "CREATE INDEX i ON posts (title);\nALTER TABLE posts RENAME COLUMN title TO headline;",
            &["title", "headline"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("headline"));
        assert!(
            found[0].create_sql.contains("headline"),
            "{}",
            found[0].create_sql
        );
    }

    #[test]
    fn an_archived_table_does_not_lend_its_indexes_to_a_new_one() {
        let found = scan(
            &[
                ("2026_01_01_000000_a", POSTS),
                ("2026_01_02_000000_b", "CREATE INDEX i ON posts (title);"),
                (
                    "2026_01_03_000000_c",
                    "ALTER TABLE posts RENAME TO archived_posts;",
                ),
                ("2026_01_04_000000_d", POSTS),
            ],
            &["title"],
        );
        assert!(found.is_empty(), "got {found:?}");
    }

    #[test]
    fn a_rebuilt_table_keeps_the_index_made_on_the_staging_name() {
        let found = scan(
            &[
                ("2026_01_01_000000_a", POSTS),
                (
                    "2026_01_02_000000_b",
                    "CREATE TABLE posts_new (id INTEGER PRIMARY KEY, title TEXT);\n\
                     CREATE INDEX idx_new ON posts_new (title);\n\
                     DROP TABLE posts;\nALTER TABLE posts_new RENAME TO posts;",
                ),
            ],
            &["title"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn a_fts5_migration_replays() {
        let found = scan_after(
            "CREATE VIRTUAL TABLE posts_fts USING fts5(title, content='posts', content_rowid='id');\n\
             CREATE INDEX i ON posts (title);",
            &["title"],
        );
        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn an_inline_unique_column_is_an_error_not_a_silent_miss() {
        let t = tree(&[(
            "2026_01_01_000000_a",
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, slug TEXT UNIQUE);",
        )]);
        let err = scan_prior_indexes(t.path(), "posts", &["slug"]).unwrap_err();
        assert!(err.contains("posts.slug"), "{err}");
        assert!(err.contains("Rebuild the table"), "{err}");
    }

    #[test]
    fn a_primary_key_column_is_an_error() {
        let t = tree(&[("2026_01_01_000000_a", POSTS)]);
        let err = scan_prior_indexes(t.path(), "posts", &["id"]).unwrap_err();
        assert!(err.contains("posts.id"), "{err}");
    }

    #[test]
    fn a_migration_that_does_not_replay_is_an_error() {
        let t = tree(&[("2026_01_01_000000_bad", "CREATE TABEL posts (id INTEGER);")]);
        let err = scan_prior_indexes(t.path(), "posts", &["title"]).unwrap_err();
        assert!(err.contains("2026_01_01_000000_bad"), "{err}");
    }

    #[test]
    fn probing_changes_nothing() {
        let t = tree(&with_posts_refs("CREATE INDEX i ON posts (title);"));
        let a = scan_prior_indexes(t.path(), "posts", &["title", "body"]).unwrap();
        let b = scan_prior_indexes(t.path(), "posts", &["title", "body"]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 1);
    }

    fn with_posts_refs(index_sql: &str) -> Vec<(&'static str, &str)> {
        vec![
            ("2026_01_01_000000_a", POSTS),
            ("2026_01_02_000000_b", index_sql),
        ]
    }

    #[test]
    fn one_composite_index_blocks_two_columns() {
        let found = scan_after("CREATE INDEX i ON posts (title, body);", &["title", "body"]);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(found[0].covers("title") && found[0].covers("body"));
    }

    #[test]
    fn dropping_the_found_indexes_lets_sqlite_drop_the_column() {
        let t = tree(&with_posts_refs(
            "CREATE INDEX a ON posts (author_id, title);\n\
             CREATE INDEX b ON posts (lower(title)) WHERE status = 'x';\n\
             CREATE INDEX keep ON posts (body);",
        ));
        let found = scan_prior_indexes(t.path(), "posts", &["title"]).unwrap();
        let names: Vec<_> = found.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        replay(&mut conn, t.path()).unwrap();
        for i in &found {
            conn.batch_execute(&format!("DROP INDEX {};", i.name))
                .unwrap();
        }
        conn.batch_execute("ALTER TABLE posts DROP COLUMN title;")
            .unwrap();
        for i in &found {
            conn.batch_execute(&i.create_sql).unwrap_err();
        }
    }
}
