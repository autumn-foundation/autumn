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

use std::path::Path;

use autumn_schema_core::{Backend, Table};
use diesel::{Connection, QueryableByName, RunQueryDsl as _};
use diesel_migrations::{FileBasedMigrations, MigrationHarness};

/// Replay the migrations in `migrations_dir` on the empty dev database at
/// `url` and return its schema. The database is not changed.
///
/// # Errors
///
/// Returns a message when `url` is not a database URL for `backend`, when the
/// database is unreachable or not empty, or when a migration fails. A message
/// never contains the URL or its password.
pub fn replay(backend: Backend, url: &str, migrations_dir: &Path) -> Result<Vec<Table>, String> {
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
    match backend {
        Backend::Postgres => replay_postgres(url, migrations_dir),
        Backend::Sqlite => replay_sqlite(url, migrations_dir),
    }
}

/// Set `managed` on each replayed table from the checked-in snapshot.
///
/// Introspection marks every table managed. But a replay also reads tables that
/// hand-written migrations make for unmanaged models, and the diff must never
/// drop those. So a replayed table is managed only when the snapshot records it
/// as managed. With no snapshot, no replayed table is managed: the diff can
/// add and alter, but never drop a table.
pub fn adopt_managed_flags(replayed: &mut [Table], snapshot: Option<&[Table]>) {
    for table in replayed {
        table.managed = snapshot
            .unwrap_or_default()
            .iter()
            .any(|s| s.name == table.name && s.managed);
    }
}

const fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Postgres => "Postgres",
        Backend::Sqlite => "SQLite",
    }
}

fn replay_postgres(url: &str, migrations_dir: &Path) -> Result<Vec<Table>, String> {
    use crate::schema::introspect;
    let mut conn = introspect::connect_postgres(url).map_err(|e| e.to_string())?;
    ensure_empty(
        &mut conn,
        "SELECT table_name AS name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         ORDER BY table_name LIMIT 1",
    )?;
    in_rolled_back_transaction(&mut conn, migrations_dir, |conn| {
        introspect::introspect_postgres_conn(conn).map_err(|e| e.to_string())
    })
}

#[cfg(feature = "sqlite")]
fn replay_sqlite(url: &str, migrations_dir: &Path) -> Result<Vec<Table>, String> {
    use crate::schema::introspect;
    let target = introspect::sqlite_target(url);
    let mut conn = diesel::SqliteConnection::establish(&target)
        .map_err(|_| format!("could not open the SQLite dev database at {target}"))?;
    ensure_empty(
        &mut conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 1",
    )?;
    in_rolled_back_transaction(&mut conn, migrations_dir, |conn| {
        introspect::introspect_sqlite_conn(conn).map_err(|e| e.to_string())
    })
}

/// The default build targets Postgres only and has no `SQLite` driver.
#[cfg(not(feature = "sqlite"))]
fn replay_sqlite(_url: &str, _migrations_dir: &Path) -> Result<Vec<Table>, String> {
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

/// Refuse a dev database that has a table: the replay must start from empty.
fn ensure_empty<C: Connection>(conn: &mut C, query: &str) -> Result<(), String>
where
    NameRow: QueryableByName<C::Backend>,
    diesel::query_builder::SqlQuery: diesel::query_dsl::LoadQuery<'static, C, NameRow>,
{
    let rows: Vec<NameRow> = diesel::sql_query(query)
        .load(conn)
        .map_err(|e| format!("could not read the dev database catalog: {e}"))?;
    match rows.first() {
        Some(row) => Err(format!(
            "the dev database is not empty (it has table `{}`); --dev-url needs an \
             empty database",
            row.name
        )),
        None => Ok(()),
    }
}

/// Apply the migrations in a test transaction, run `read`, and roll back: a
/// test transaction is never committed, so the dev database does not change.
fn in_rolled_back_transaction<C>(
    conn: &mut C,
    migrations_dir: &Path,
    read: impl FnOnce(&mut C) -> Result<Vec<Table>, String>,
) -> Result<Vec<Table>, String>
where
    C: Connection + MigrationHarness<C::Backend>,
    FileBasedMigrations: diesel::migration::MigrationSource<C::Backend>,
{
    conn.begin_test_transaction()
        .map_err(|e| format!("could not start the replay transaction: {e}"))?;
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
    read(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replayed_tables_are_managed_only_when_the_snapshot_says_so() {
        let mut snap_users = Table::new("users", Backend::Postgres);
        snap_users.managed = true;
        let mut snap_logs = Table::new("logs", Backend::Postgres);
        snap_logs.managed = false;
        let snapshot = vec![snap_users, snap_logs];

        let mut replayed: Vec<Table> = ["users", "logs", "audit"]
            .iter()
            .map(|n| Table::new(*n, Backend::Postgres))
            .collect();
        adopt_managed_flags(&mut replayed, Some(&snapshot));
        let flags: Vec<bool> = replayed.iter().map(|t| t.managed).collect();
        assert_eq!(flags, vec![true, false, false]);

        adopt_managed_flags(&mut replayed, None);
        assert!(replayed.iter().all(|t| !t.managed));
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

            let tables = replay(Backend::Sqlite, &url, &migrations).expect("replay");
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
        fn no_migrations_directory_gives_an_empty_baseline() {
            let root = tempfile::tempdir().expect("tempdir");
            let tables = replay(
                Backend::Sqlite,
                "sqlite::memory:",
                &root.path().join("migrations"),
            )
            .expect("replay");
            assert!(tables.is_empty());
        }
    }
}
