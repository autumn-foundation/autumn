//! `--dev-url` shadow-database replay (#1975, Decision 2).
//!
//! `autumn schema diff --dev-url <URL>` uses a database, not the snapshot, as
//! the diff baseline. [`replay`] applies every migration in `migrations/` to
//! the dev database inside one transaction, reads the schema back, and rolls
//! the transaction back. The dev database must be empty, and it stays empty.
//!
//! The replayed schema shows what the migrations really make, including what
//! the offline snapshot cannot see (for example a `#[belongs_to]` foreign key
//! or a hand-written migration).
//!
//! A migration that cannot run in a transaction (for example
//! `CREATE INDEX CONCURRENTLY`) makes the replay fail with its error.

use std::collections::BTreeSet;
use std::path::Path;

use autumn_schema_core::{Backend, Table};
use diesel::{Connection, QueryableByName, RunQueryDsl as _};
use diesel_migrations::{FileBasedMigrations, MigrationHarness};

/// The schema that the replayed migrations make.
#[derive(Debug)]
pub struct Replay {
    /// The tables (the diff baseline).
    pub tables: Vec<Table>,
    /// The names of the other relations (views, sequences, ...). They share
    /// the table namespace, so a new or renamed table cannot use them.
    pub other_relations: BTreeSet<String>,
}

/// Replay the migrations in `migrations_dir` on the empty dev database at
/// `url` and return its schema. The database is not changed.
///
/// # Errors
///
/// Returns a message when `url` is not a database URL for `backend`, when the
/// database is unreachable or not empty, or when a migration fails. A message
/// never contains the password. A connection error shows only the host and
/// port (or the `SQLite` file path).
pub fn replay(backend: Backend, url: &str, migrations_dir: &Path) -> Result<Replay, String> {
    let dev_backend = match autumn_web::config::DatabaseBackend::detect(url) {
        Some(autumn_web::config::DatabaseBackend::Postgres) => Backend::Postgres,
        Some(autumn_web::config::DatabaseBackend::Sqlite) => Backend::Sqlite,
        None => {
            return Err("--dev-url must be a `postgres://` or `sqlite:` database URL".to_owned());
        }
    };
    if dev_backend != backend {
        return Err(format!(
            "--dev-url is a {} database, but the schema backend is {}",
            backend_label(dev_backend),
            backend_label(backend)
        ));
    }
    check_replayable(migrations_dir)?;
    match backend {
        Backend::Postgres => replay_postgres(url, migrations_dir),
        Backend::Sqlite => replay_sqlite(url, migrations_dir),
    }
}

/// Set `managed` on each replayed table.
///
/// Introspection marks every table managed. But a replay also reads tables that
/// hand-written migrations make for unmanaged models, and the diff must never
/// drop those. So a replayed table is managed only when:
/// - the snapshot records it as managed, or
/// - a managed model declares it, or renames it with `#[renamed_from]`.
///
/// Thus with no snapshot, the diff never drops a table.
pub fn adopt_managed_flags(
    replayed: &mut [Table],
    snapshot: Option<&[Table]>,
    models: &crate::schema::parse::ParsedSchema,
) {
    let managed_model = |name: &str| models.tables.iter().any(|t| t.managed && t.name == name);
    for table in replayed {
        let name = table.name.as_str();
        table.managed = snapshot
            .unwrap_or_default()
            .iter()
            .any(|s| s.name == name && s.managed)
            || managed_model(name)
            || models
                .renames
                .iter()
                .any(|h| h.column.is_none() && h.from == name && managed_model(&h.table));
    }
}

/// Refuse a migration that the replay transaction cannot hold, before any
/// connection: one with `run_in_transaction = false`, or one with its own
/// transaction-control statement (a `COMMIT` would end the replay transaction
/// and keep the changes).
fn check_replayable(migrations_dir: &Path) -> Result<(), String> {
    if !migrations_dir.exists() {
        return Ok(());
    }
    let read_err =
        |path: &Path, e: std::io::Error| format!("failed to read {}: {e}", path.display());
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(migrations_dir)
        .map_err(|e| read_err(migrations_dir, e))?
        .map(|entry| {
            entry
                .map(|e| e.path())
                .map_err(|e| read_err(migrations_dir, e))
        })
        .collect::<Result<_, _>>()?;
    dirs.sort();
    let why = "the --dev-url replay runs every migration in one transaction and rolls it back";
    for dir in dirs.iter().filter(|d| d.join("up.sql").is_file()) {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let metadata = dir.join("metadata.toml");
        if metadata.is_file() {
            let text = std::fs::read_to_string(&metadata).map_err(|e| read_err(&metadata, e))?;
            let no_transaction = text
                .parse::<toml::Table>()
                .ok()
                .and_then(|t| t.get("run_in_transaction").and_then(toml::Value::as_bool))
                == Some(false);
            if no_transaction {
                return Err(format!(
                    "migration `{name}` sets run_in_transaction = false; {why}"
                ));
            }
        }
        let up = dir.join("up.sql");
        let sql = std::fs::read_to_string(&up).map_err(|e| read_err(&up, e))?;
        if let Some(keyword) = transaction_control(&sql) {
            return Err(format!(
                "migration `{name}` has a `{keyword}` statement; {why}"
            ));
        }
    }
    Ok(())
}

/// The first transaction-control statement (`BEGIN`, `START TRANSACTION`,
/// `COMMIT`, `END`, `ROLLBACK`, `ABORT`) in `sql`, if any. Comments, string
/// literals, quoted names and dollar-quoted bodies do not count. A `BEGIN`
/// inside a statement opens a body (`BEGIN ATOMIC`, a `SQLite` trigger) that
/// ends at a standalone `END`; that `END` does not count either.
fn transaction_control(sql: &str) -> Option<String> {
    let code = strip_sql_noise(sql);
    let mut in_body = false;
    for statement in code.split(';') {
        let words: Vec<String> = statement
            .split_whitespace()
            .map(str::to_ascii_uppercase)
            .collect();
        if in_body {
            in_body = words != ["END"];
            continue;
        }
        let first = words.first().map_or("", String::as_str);
        if first != "BEGIN" && words.iter().any(|w| w == "BEGIN") {
            in_body = true;
            continue;
        }
        let hit = match first {
            "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "ABORT" => true,
            "START" => words.get(1).is_some_and(|w| w == "TRANSACTION"),
            _ => false,
        };
        if hit {
            return Some(first.to_owned());
        }
    }
    None
}

/// `sql` with comments, `'...'`, `E'...'` and `"..."` literals, and
/// `$tag$...$tag$` bodies replaced by a space.
fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        let skip = if rest.starts_with("--") {
            rest.find('\n').unwrap_or(rest.len())
        } else if rest.starts_with("/*") {
            rest.find("*/").map_or(rest.len(), |i| i + 2)
        } else if matches!(c, 'E' | 'e')
            && rest[1..].starts_with('\'')
            && !out.ends_with(|p: char| p.is_ascii_alphanumeric() || p == '_')
        {
            // A Postgres escape string: a backslash escapes the next character.
            1 + escape_string_len(&rest[1..])
        } else if c == '\'' || c == '"' {
            crate::schema::rename::quoted_len(rest, c)
        } else if let Some(tag) = dollar_tag(rest) {
            rest[tag.len()..]
                .find(tag)
                .map_or(rest.len(), |i| tag.len() + i + tag.len())
        } else {
            0
        };
        if skip == 0 {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        } else {
            out.push(' ');
            rest = &rest[skip..];
        }
    }
    out
}

/// The byte length of the escape-string body at the start of `s` (it starts at
/// the opening `'`), through the closing quote. `\x` and `''` are escapes. An
/// unclosed body runs to the end.
fn escape_string_len(s: &str) -> usize {
    let mut iter = s.char_indices().skip(1).peekable();
    while let Some((i, c)) = iter.next() {
        match c {
            '\\' => {
                iter.next();
            }
            '\'' if iter.peek().is_some_and(|&(_, n)| n == '\'') => {
                iter.next();
            }
            '\'' => return i + 1,
            _ => {}
        }
    }
    s.len()
}

/// The dollar-quote opener (`$$` or `$tag$`) at the start of `s`, if any.
fn dollar_tag(s: &str) -> Option<&str> {
    let body = s.strip_prefix('$')?;
    let end = body.find('$')?;
    let tag = &body[..end];
    let valid = tag
        .chars()
        .next()
        .is_none_or(|c| c.is_ascii_alphabetic() || c == '_')
        && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| &s[..end + 2])
}

const fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Postgres => "Postgres",
        Backend::Sqlite => "SQLite",
    }
}

fn replay_postgres(url: &str, migrations_dir: &Path) -> Result<Replay, String> {
    use crate::schema::introspect;
    let mut conn = introspect::connect_postgres(url).map_err(|e| e.to_string())?;
    // Any table or view in any user schema makes the database not empty.
    ensure_empty(
        &mut conn,
        "SELECT n.nspname || '.' || c.relname AS name FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
         AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         AND n.nspname NOT LIKE 'pg\\_%' ORDER BY 1 LIMIT 1",
    )?;
    in_rolled_back_transaction(
        &mut conn,
        migrations_dir,
        |conn| {
            // Introspection reads `public`, so the migrations must write there.
            diesel::sql_query("SET LOCAL search_path TO public")
                .execute(conn)
                .map(|_| ())
                .map_err(|e| format!("could not set the replay search_path: {e}"))
        },
        |conn| {
            let rows: Vec<NameRow> = diesel::sql_query("SELECT txid_current()::text AS name")
                .load(conn)
                .map_err(|e| format!("could not read the replay transaction id: {e}"))?;
            Ok(rows.into_iter().next().map(|r| r.name).unwrap_or_default())
        },
        |conn| {
            Ok(Replay {
                tables: introspect::introspect_postgres_conn(conn).map_err(|e| e.to_string())?,
                other_relations: relation_names(
                    conn,
                    "SELECT c.relname AS name FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = 'public' AND c.relkind IN ('v', 'm', 'S', 'f', 'c')",
                )?,
            })
        },
    )
}

#[cfg(feature = "sqlite")]
fn replay_sqlite(url: &str, migrations_dir: &Path) -> Result<Replay, String> {
    use crate::schema::introspect;
    let target = introspect::sqlite_target(url);
    // `establish` creates a missing file. Refuse instead of making a stray file.
    if let Some(path) = introspect::sqlite_existence_check_path(&target)
        && !Path::new(&path).exists()
    {
        return Err(format!("the SQLite dev database {path} does not exist"));
    }
    let mut conn = diesel::SqliteConnection::establish(&target)
        .map_err(|_| format!("could not open the SQLite dev database at {target}"))?;
    ensure_empty(
        &mut conn,
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') \
         AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 1",
    )?;
    in_rolled_back_transaction(
        &mut conn,
        migrations_dir,
        |_| Ok(()),
        |_| Ok(String::new()),
        |conn| {
            Ok(Replay {
                tables: introspect::introspect_sqlite_conn(conn).map_err(|e| e.to_string())?,
                other_relations: relation_names(
                    conn,
                    "SELECT name FROM sqlite_master WHERE type = 'view'",
                )?,
            })
        },
    )
}

/// The default build targets Postgres only and has no `SQLite` driver.
#[cfg(not(feature = "sqlite"))]
fn replay_sqlite(_url: &str, _migrations_dir: &Path) -> Result<Replay, String> {
    Err(
        "--dev-url with a SQLite URL needs a CLI built with `--features sqlite` \
         (this build targets Postgres only)"
            .to_owned(),
    )
}

#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// The `name` column of `query`, as a set.
fn relation_names<C: Connection>(conn: &mut C, query: &str) -> Result<BTreeSet<String>, String>
where
    NameRow: QueryableByName<C::Backend>,
    diesel::query_builder::SqlQuery: diesel::query_dsl::LoadQuery<'static, C, NameRow>,
{
    let rows: Vec<NameRow> = diesel::sql_query(query)
        .load(conn)
        .map_err(|e| format!("could not read the replayed relations: {e}"))?;
    Ok(rows.into_iter().map(|r| r.name).collect())
}

/// Refuse a dev database that has a table: the replay must start from empty.
fn ensure_empty<C: Connection>(conn: &mut C, query: &str) -> Result<(), String>
where
    NameRow: QueryableByName<C::Backend>,
    diesel::query_builder::SqlQuery: diesel::query_dsl::LoadQuery<'static, C, NameRow>,
{
    let rows: Vec<NameRow> = diesel::sql_query(query)
        .load(conn)
        .map_err(|e| format!("could not read the dev database catalog: {e}"))?;
    rows.first().map_or(Ok(()), |row| {
        Err(format!(
            "the dev database is not empty (it has table `{}`); --dev-url needs an \
             empty database",
            row.name
        ))
    })
}

/// Apply the migrations in a test transaction, run `read`, and roll back: a
/// test transaction is never committed, so the dev database does not change.
///
/// `setup` runs first in the transaction. `xact_id` reads the transaction id
/// before and after the migrations; a change means a migration ended the
/// transaction, so the replay fails.
fn in_rolled_back_transaction<C>(
    conn: &mut C,
    migrations_dir: &Path,
    setup: impl FnOnce(&mut C) -> Result<(), String>,
    xact_id: impl Fn(&mut C) -> Result<String, String>,
    read: impl FnOnce(&mut C) -> Result<Replay, String>,
) -> Result<Replay, String>
where
    C: Connection + MigrationHarness<C::Backend>,
    FileBasedMigrations: diesel::migration::MigrationSource<C::Backend>,
{
    conn.begin_test_transaction()
        .map_err(|e| format!("could not start the replay transaction: {e}"))?;
    setup(conn)?;
    let before = xact_id(conn)?;
    if migrations_dir.exists() {
        let migrations = FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
            format!(
                "failed to read migrations directory {}: {e}",
                migrations_dir.display()
            )
        })?;
        conn.run_pending_migrations(migrations)
            .map_err(|e| format!("the --dev-url replay of the migrations failed: {e}"))?;
    }
    if xact_id(conn)? != before {
        return Err(
            "a migration ended the --dev-url replay transaction, so the dev database \
             may now hold its changes; remove the transaction control from the migration"
                .to_owned(),
        );
    }
    read(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replayed_tables_are_managed_when_the_snapshot_or_a_model_says_so() {
        let mut snap_users = Table::new("users", Backend::Postgres);
        snap_users.managed = true;
        let mut snap_logs = Table::new("logs", Backend::Postgres);
        snap_logs.managed = false;
        let snapshot = vec![snap_users, snap_logs];
        let mut posts = Table::new("posts", Backend::Postgres);
        posts.managed = true;
        let mut models = crate::schema::parse::ParsedSchema::from_tables(vec![posts]);
        models.renames.push(crate::schema::parse::RenameHint {
            table: "posts".to_owned(),
            column: None,
            from: "articles".to_owned(),
        });

        let names = ["users", "logs", "audit", "posts", "articles"];
        let mut replayed: Vec<Table> = names
            .iter()
            .map(|n| Table::new(*n, Backend::Postgres))
            .collect();
        adopt_managed_flags(&mut replayed, Some(&snapshot), &models);
        let flags: Vec<bool> = replayed.iter().map(|t| t.managed).collect();
        assert_eq!(flags, vec![true, false, false, true, true]);

        adopt_managed_flags(&mut replayed, None, &models);
        let flags: Vec<bool> = replayed.iter().map(|t| t.managed).collect();
        assert_eq!(flags, vec![false, false, false, true, true]);
    }

    fn migration_dir(root: &Path, name: &str, up: &str, metadata: Option<&str>) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("up.sql"), up).expect("up.sql");
        std::fs::write(dir.join("down.sql"), "SELECT 1;").expect("down.sql");
        if let Some(meta) = metadata {
            std::fs::write(dir.join("metadata.toml"), meta).expect("metadata.toml");
        }
    }

    #[test]
    fn transaction_control_is_found_outside_bodies_strings_and_comments() {
        for sql in [
            "BEGIN; CREATE TABLE t (id INT); COMMIT;",
            "CREATE TABLE t (id INT);\ncommit;",
            "START TRANSACTION;",
            "ALTER TYPE s ADD VALUE 'x'; END;",
            "ROLLBACK",
            "SELECT E'a\\'b'; COMMIT;",
        ] {
            assert!(transaction_control(sql).is_some(), "{sql}");
        }
        for sql in [
            "DO $$ BEGIN PERFORM 1; END $$;",
            "CREATE FUNCTION f() RETURNS trigger AS $body$ BEGIN RETURN NEW; END; $body$ LANGUAGE plpgsql;",
            "CREATE FUNCTION g() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END;",
            "CREATE TRIGGER \"t_assign\" AFTER INSERT ON \"t\" BEGIN\n  UPDATE \"t\" SET p = \
             (SELECT CASE WHEN 1 THEN 0 ELSE 1 END) WHERE id = new.id;\nEND;",
            "CREATE TRIGGER IF NOT EXISTS t_del AFTER DELETE ON t BEGIN DELETE FROM c \
             WHERE p = old.id; DELETE FROM d WHERE p = old.id; END;",
            "INSERT INTO t VALUES ('BEGIN; COMMIT;'); -- COMMIT;\n/* ROLLBACK; */",
            "CREATE TABLE t (id INT, \"commit\" TEXT);",
            "SELECT E'it\\'s; COMMIT;';",
            "SELECT e'\\\\'; SELECT 1;",
        ] {
            assert_eq!(transaction_control(sql), None, "{sql}");
        }
    }

    #[test]
    fn a_migration_that_controls_transactions_is_refused_before_connecting() {
        let root = tempfile::tempdir().expect("tempdir");
        migration_dir(
            root.path(),
            "2026-01-01-000000_enum",
            "BEGIN; SELECT 1; COMMIT;",
            None,
        );
        let err = check_replayable(root.path()).unwrap_err();
        assert!(
            err.contains("2026-01-01-000000_enum") && err.contains("BEGIN"),
            "{err}"
        );

        let root = tempfile::tempdir().expect("tempdir");
        migration_dir(
            root.path(),
            "2026-01-01-000000_concurrent",
            "CREATE INDEX CONCURRENTLY i ON t (x);",
            Some("run_in_transaction = false\n"),
        );
        let err = check_replayable(root.path()).unwrap_err();
        assert!(err.contains("run_in_transaction"), "{err}");

        let root = tempfile::tempdir().expect("tempdir");
        migration_dir(
            root.path(),
            "2026-01-01-000000_ok",
            "CREATE TABLE t (id INT);",
            None,
        );
        check_replayable(root.path()).expect("plain migration");
        check_replayable(&root.path().join("absent")).expect("no migrations directory");
    }

    #[test]
    fn a_dev_url_for_another_backend_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = replay(Backend::Postgres, "sqlite://dev.db", dir.path()).unwrap_err();
        assert!(err.contains("SQLite") && err.contains("Postgres"), "{err}");
        let err = replay(Backend::Sqlite, "postgres://localhost/dev", dir.path()).unwrap_err();
        assert!(err.contains("SQLite") && err.contains("Postgres"), "{err}");
    }

    #[test]
    fn a_dev_url_that_is_not_a_database_url_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = replay(Backend::Postgres, "mysql://u:pw@host/db", dir.path()).unwrap_err();
        assert!(err.contains("--dev-url"), "{err}");
        assert!(!err.contains("pw@"), "the URL must not leak: {err}");
    }

    #[test]
    fn an_unreachable_dev_database_does_not_leak_the_password() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = replay(
            Backend::Postgres,
            "postgres://user:s3cr3t_pw@127.0.0.1:1/dev",
            dir.path(),
        )
        .unwrap_err();
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(
            !err.contains("s3cr3t_pw"),
            "the password must not leak: {err}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn sqlite_replay_needs_the_sqlite_feature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = replay(Backend::Sqlite, "sqlite::memory:", dir.path()).unwrap_err();
        assert!(err.contains("--features sqlite"), "{err}");
    }

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;

        fn migration(root: &Path, name: &str, up: &str) {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), up).expect("up.sql");
            std::fs::write(dir.join("down.sql"), "SELECT 1;").expect("down.sql");
        }

        #[test]
        fn replay_reads_the_schema_and_leaves_the_dev_database_empty() {
            let root = tempfile::tempdir().expect("tempdir");
            let migrations = root.path().join("migrations");
            migration(
                &migrations,
                "2026-01-01-000000_users",
                "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL);",
            );
            migration(
                &migrations,
                "2026-01-02-000000_posts",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 user_id BIGINT NOT NULL REFERENCES users(id));",
            );
            let db = root.path().join("dev.db");
            std::fs::File::create(&db).expect("create dev.db");
            let url = format!("sqlite://{}", db.display());

            let tables = replay(Backend::Sqlite, &url, &migrations)
                .expect("replay")
                .tables;
            let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names, vec!["posts", "users"]);
            let posts = &tables[0];
            let user_id = posts
                .columns
                .iter()
                .find(|c| c.name == "user_id")
                .expect("user_id");
            assert_eq!(
                user_id.references.as_ref().map(|fk| fk.table.as_str()),
                Some("users")
            );

            // The transaction rolled back: a second replay sees an empty database.
            assert_eq!(
                replay(Backend::Sqlite, &url, &migrations)
                    .expect("again")
                    .tables
                    .len(),
                2
            );
            let pulled = crate::schema::introspect::introspect_sqlite(&url).expect("introspect");
            assert!(
                pulled.is_empty(),
                "the dev database must stay empty: {pulled:?}"
            );
        }

        #[test]
        fn a_dev_database_that_is_not_empty_is_refused() {
            let root = tempfile::tempdir().expect("tempdir");
            let db = root.path().join("dev.db");
            {
                use diesel::{Connection as _, RunQueryDsl as _};
                let mut conn =
                    diesel::SqliteConnection::establish(&db.display().to_string()).expect("open");
                diesel::sql_query("CREATE TABLE leftover (id INTEGER PRIMARY KEY)")
                    .execute(&mut conn)
                    .expect("create");
            }
            let url = format!("sqlite://{}", db.display());
            let err = replay(Backend::Sqlite, &url, &root.path().join("migrations")).unwrap_err();
            assert!(
                err.contains("not empty") && err.contains("leftover"),
                "{err}"
            );
        }

        #[test]
        fn a_failing_migration_names_the_replay() {
            let root = tempfile::tempdir().expect("tempdir");
            let migrations = root.path().join("migrations");
            migration(&migrations, "2026-01-01-000000_bad", "CREATE TABLE (;");
            let err = replay(Backend::Sqlite, "sqlite::memory:", &migrations).unwrap_err();
            assert!(err.contains("replay"), "{err}");
        }

        #[test]
        fn replay_lists_views_as_other_relations() {
            let root = tempfile::tempdir().expect("tempdir");
            let migrations = root.path().join("migrations");
            migration(
                &migrations,
                "2026-01-01-000000_report",
                "CREATE TABLE t (id INTEGER PRIMARY KEY); CREATE VIEW report AS SELECT id FROM t;",
            );
            let replayed = replay(Backend::Sqlite, "sqlite::memory:", &migrations).expect("replay");
            assert!(
                replayed.other_relations.contains("report"),
                "{:?}",
                replayed.other_relations
            );
            assert_eq!(replayed.tables.len(), 1);
        }

        #[test]
        fn a_missing_sqlite_dev_file_is_refused_and_not_created() {
            let root = tempfile::tempdir().expect("tempdir");
            let db = root.path().join("absent.db");
            let url = format!("sqlite://{}", db.display());
            let err = replay(Backend::Sqlite, &url, &root.path().join("migrations")).unwrap_err();
            assert!(err.contains("absent.db"), "{err}");
            assert!(!db.exists(), "the replay must not create the file");
        }

        #[test]
        fn no_migrations_directory_gives_an_empty_baseline() {
            let root = tempfile::tempdir().expect("tempdir");
            let tables = replay(
                Backend::Sqlite,
                "sqlite::memory:",
                &root.path().join("migrations"),
            )
            .expect("replay")
            .tables;
            assert!(tables.is_empty());
        }
    }
}
