//! Read-only `SQLite` pool proof (issue #1614, Codex P2).
//!
//! `build_sqlite_pool`'s per-connection `custom_setup` batch includes
//! `PRAGMA journal_mode = WAL`, which WRITES to the database (it rewrites the
//! file header and creates the `-wal`/`-shm` sidecars). Against a **read-only**
//! URI target such as `sqlite://file:/srv/reference.db?mode=ro` that fails with
//! "attempt to write a readonly database"; `custom_setup` propagates the error,
//! so the pool can hand out no connection and `Db` routes 503 even for pure
//! reads.
//!
//! The fix detects a read-only URI target (`mode=ro` / `immutable`) and installs
//! only the non-writing pragmas (`busy_timeout`, `foreign_keys`), skipping the
//! write-affecting ones. This test seeds a file database in the default
//! (rollback) journal mode via a raw read-write connection — deliberately NOT
//! through the RW pool, which would persist WAL mode — then opens the SAME file
//! read-only through the public [`autumn_web::db::create_pool`] path and proves
//! the pool builds, serves a committed read, still applies the non-writing
//! `foreign_keys` pragma, and rejects writes.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_read_only_pool
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::sql_types::{BigInt, Integer, Text};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection as _, RunQueryDsl as _};

type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct NameRow {
    #[diesel(sql_type = Text)]
    name: String,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(diesel::QueryableByName)]
struct ForeignKeysRow {
    #[diesel(sql_type = Integer)]
    foreign_keys: i32,
}

#[tokio::test]
async fn read_only_sqlite_pool_builds_and_serves_reads() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("reference.db");

    // (1) Seed the file with schema + data via a raw read-write connection.
    //     Deliberately NOT through create_pool's RW pool: that sets WAL journal
    //     mode persistently, and we want the file left in the default (rollback)
    //     journal mode so that opening it read-only later genuinely exercises
    //     the WAL-would-write path the fix skips.
    {
        let mut seed = RuntimeConnection::establish(&db_path.display().to_string())
            .await
            .expect("establish a raw read-write sqlite connection");
        diesel::sql_query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .execute(&mut seed)
            .await
            .expect("create table t");
        diesel::sql_query("INSERT INTO t (id, name) VALUES (1, 'alpha')")
            .execute(&mut seed)
            .await
            .expect("insert seed row");
    }

    // (2) Build a pool over the SAME file opened READ-ONLY via a `file:` URI.
    //     With the pre-fix full pragma batch, `custom_setup` would run
    //     `PRAGMA journal_mode = WAL`, fail with "attempt to write a readonly
    //     database", and the pool could not hand out any connection.
    let ro_url = format!("sqlite://file:{}?mode=ro", db_path.display());
    let config = DatabaseConfig {
        url: Some(ro_url),
        // Multi-slot so the read below runs on a genuine pool-issued connection,
        // not an in-memory single-slot special case.
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("read-only sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    // (3) custom_setup must succeed on a real pool-issued connection (no WAL
    //     "readonly database" error), and a SELECT must return the seeded row.
    let mut conn = pool
        .get()
        .await
        .expect("checkout a read-only pooled connection — custom_setup must not fail on WAL");

    let rows: Vec<NameRow> = diesel::sql_query("SELECT name FROM t WHERE id = 1")
        .load(&mut *conn)
        .await
        .expect("read from the read-only pool");
    assert_eq!(
        rows.into_iter().next().expect("one row").name,
        "alpha",
        "a read-only pool must serve committed reads"
    );

    // The non-writing `foreign_keys` pragma must still be applied on the
    // read-only connection — the fix keeps it in the reduced batch.
    let fk: Vec<ForeignKeysRow> = diesel::sql_query("PRAGMA foreign_keys")
        .load(&mut *conn)
        .await
        .expect("read foreign_keys pragma");
    assert_eq!(
        fk.into_iter()
            .next()
            .expect("foreign_keys row")
            .foreign_keys,
        1,
        "the non-writing foreign_keys pragma must still be applied read-only"
    );

    // (4) A write must be rejected — the database really is read-only.
    let write = diesel::sql_query("INSERT INTO t (id, name) VALUES (2, 'beta')")
        .execute(&mut *conn)
        .await;
    assert!(
        write.is_err(),
        "a write against a read-only sqlite pool must be rejected"
    );

    // The rejected write committed nothing.
    let count: Vec<CountRow> = diesel::sql_query("SELECT COUNT(*) AS n FROM t")
        .load(&mut *conn)
        .await
        .expect("count rows");
    assert_eq!(
        count.into_iter().next().expect("count row").n,
        1,
        "a rejected write must not have been committed to the read-only database"
    );
}
