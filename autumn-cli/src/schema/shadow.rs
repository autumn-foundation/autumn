//! `--dev-url` shadow-database replay (#1975, Decision 2).
//!
//! `autumn schema diff --dev-url <URL>` uses a database, not the snapshot, as
//! the diff baseline. [`replay`] applies every migration in `migrations/` to a
//! scratch database and reads the schema back:
//!
//! - **Postgres:** the command creates a scratch database on the server of
//!   `URL` (the role needs `CREATEDB`), applies the migrations there, reads the
//!   schema, and drops the scratch database. The database in `URL` is not
//!   changed.
//! - **`SQLite`:** the command replays into an in-memory database. `URL` only
//!   selects `SQLite`.
//!
//! Because the scratch database is thrown away, a migration can do anything a
//! real apply does (`COMMIT`, `run_in_transaction = false`,
//! `CREATE INDEX CONCURRENTLY`): nothing it does can reach the dev database.
//!
//! The replayed schema shows what the migrations really make, including what
//! the offline snapshot cannot see (for example a `#[belongs_to]` foreign key
//! or a hand-written migration).

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

/// Replay the migrations in `migrations_dir` on a scratch database (see the
/// module docs) and return its schema. The database in `url` is not changed.
///
/// # Errors
///
/// Returns a message when `url` is not a database URL for `backend`, when the
/// server is unreachable or refuses the scratch database, or when a migration
/// fails. A message never contains the password. A connection error shows
/// only the host and port.
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
    match backend {
        Backend::Postgres => replay_postgres(url, migrations_dir),
        Backend::Sqlite => replay_sqlite(migrations_dir),
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

const fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Postgres => "Postgres",
        Backend::Sqlite => "SQLite",
    }
}

/// A unique scratch database name: `autumn_replay_<pid>_<nanos>`.
fn scratch_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("autumn_replay_{}_{nanos}", std::process::id())
}

/// `url` with its database name set to `name`. Query parameters stay.
fn scratch_url(url: &str, name: &str) -> Result<String, String> {
    let mut parsed = url::Url::parse(url).map_err(|_| {
        "--dev-url must be a `postgres://` URL (a key-value connection string is not \
         supported)"
            .to_owned()
    })?;
    parsed.set_path(&format!("/{name}"));
    Ok(parsed.to_string())
}

/// A scratch database on the dev server. Dropping it drops the database.
struct ScratchDatabase {
    admin: diesel::PgConnection,
    name: String,
}

impl Drop for ScratchDatabase {
    fn drop(&mut self) {
        // `WITH (FORCE)` (Postgres 13+) also ends any left-over connection.
        let forced = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name);
        let plain = format!("DROP DATABASE IF EXISTS {}", self.name);
        let dropped = diesel::sql_query(forced)
            .execute(&mut self.admin)
            .or_else(|_| diesel::sql_query(plain).execute(&mut self.admin));
        if dropped.is_err() {
            eprintln!(
                "warning: could not drop the scratch database `{}`; drop it by hand",
                self.name
            );
        }
    }
}

fn replay_postgres(url: &str, migrations_dir: &Path) -> Result<Replay, String> {
    use crate::schema::introspect;
    let name = scratch_name();
    let target = scratch_url(url, &name)?;
    let mut admin = introspect::connect_postgres(url).map_err(|e| e.to_string())?;
    diesel::sql_query(format!("CREATE DATABASE {name}"))
        .execute(&mut admin)
        .map_err(|e| {
            format!(
                "could not create the scratch database `{name}` on the --dev-url server \
                 (the role needs CREATEDB): {e}"
            )
        })?;
    // Declared before `conn`, so it is dropped (and the database with it) last.
    let _scratch = ScratchDatabase { admin, name };
    let mut conn = introspect::connect_postgres(&target).map_err(|e| e.to_string())?;
    // Introspection reads `public`, so the migrations must write there.
    diesel::sql_query("SET search_path TO public")
        .execute(&mut conn)
        .map_err(|e| format!("could not set the replay search_path: {e}"))?;
    apply_migrations(&mut conn, migrations_dir)?;
    Ok(Replay {
        tables: introspect::introspect_postgres_conn(&mut conn).map_err(|e| e.to_string())?,
        other_relations: relation_names(
            &mut conn,
            "SELECT c.relname AS name FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind IN ('v', 'm', 'S', 'f', 'c')",
        )?,
    })
}

#[cfg(feature = "sqlite")]
fn replay_sqlite(migrations_dir: &Path) -> Result<Replay, String> {
    use crate::schema::introspect;
    let mut conn = diesel::SqliteConnection::establish(":memory:")
        .map_err(|e| format!("could not open an in-memory SQLite database: {e}"))?;
    apply_migrations(&mut conn, migrations_dir)?;
    Ok(Replay {
        tables: introspect::introspect_sqlite_conn(&mut conn).map_err(|e| e.to_string())?,
        other_relations: relation_names(
            &mut conn,
            "SELECT name FROM sqlite_master WHERE type = 'view'",
        )?,
    })
}

/// The default build targets Postgres only and has no `SQLite` driver.
#[cfg(not(feature = "sqlite"))]
fn replay_sqlite(_migrations_dir: &Path) -> Result<Replay, String> {
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

/// Apply the migrations in `migrations_dir` (none when it does not exist), as
/// `autumn schema migrate` does.
fn apply_migrations<C>(conn: &mut C, migrations_dir: &Path) -> Result<(), String>
where
    C: Connection + MigrationHarness<C::Backend>,
    FileBasedMigrations: diesel::migration::MigrationSource<C::Backend>,
{
    if !migrations_dir.exists() {
        return Ok(());
    }
    let migrations = FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
        format!(
            "failed to read migrations directory {}: {e}",
            migrations_dir.display()
        )
    })?;
    conn.run_pending_migrations(migrations)
        .map(|_| ())
        .map_err(|e| format!("the --dev-url replay of the migrations failed: {e}"))
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

    #[test]
    fn scratch_url_replaces_only_the_database_name() {
        assert_eq!(
            scratch_url(
                "postgres://u:pw@db:5433/dev?sslmode=disable",
                "autumn_replay_1_2"
            )
            .expect("url"),
            "postgres://u:pw@db:5433/autumn_replay_1_2?sslmode=disable"
        );
        let err = scratch_url("host=db dbname=dev", "x").unwrap_err();
        assert!(err.contains("postgres://"), "{err}");
        let name = scratch_name();
        assert!(
            name.starts_with("autumn_replay_")
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{name}"
        );
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
        fn replay_reads_the_schema_and_never_opens_the_dev_file() {
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
            let url = format!("sqlite://{}", db.display());

            let tables = replay(Backend::Sqlite, &url, &migrations)
                .expect("replay")
                .tables;
            let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names, vec!["posts", "users"]);
            let user_id = tables[0]
                .columns
                .iter()
                .find(|c| c.name == "user_id")
                .expect("user_id");
            assert_eq!(
                user_id.references.as_ref().map(|fk| fk.table.as_str()),
                Some("users")
            );
            assert!(!db.exists(), "the replay must not touch the dev file");
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
