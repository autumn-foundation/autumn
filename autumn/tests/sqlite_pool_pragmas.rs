//! Per-connection PRAGMA proof for the `SQLite` runtime pool (issue #1614).
//!
//! The file-backed pool defaults to size 10, so ordinary overlapping writes on
//! separate pooled connections would hit SQLite's default busy handler, which
//! returns `SQLITE_BUSY` *immediately* when another connection holds the writer
//! lock — surfacing as spurious 5xx instead of waiting briefly (Codex P1).
//! `build_sqlite_pool` now installs `PRAGMA busy_timeout` (plus a deliberate
//! WAL `journal_mode` with `synchronous = NORMAL`, mirroring `crate::sync::store`)
//! in the manager's per-connection setup callback, so every pool-issued
//! connection waits on the lock rather than failing fast.
//!
//! Directly asserting "no `SQLITE_BUSY` under contention" is inherently racy;
//! the reliable proof is that a pool-issued connection reports the configured,
//! non-zero `busy_timeout` (and the WAL journal mode) when queried. This test
//! builds the real pool through the public [`autumn_web::db::create_pool`] path
//! and reads both pragmas back off a *separately checked-out* pooled connection.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_pool_pragmas
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct BusyTimeout {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    timeout: i32,
}

#[derive(diesel::QueryableByName)]
struct JournalMode {
    #[diesel(sql_type = diesel::sql_types::Text)]
    journal_mode: String,
}

#[tokio::test]
async fn pooled_sqlite_connections_configure_busy_timeout_and_journal_mode() {
    // A tempfile-backed (not `:memory:`) database: `journal_mode = WAL` only
    // takes effect for a real file, and a file pool is where overlapping writes
    // across pooled connections actually contend.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("pragmas.db");
    let url = format!("sqlite://{}", db_path.display());

    let config = DatabaseConfig {
        url: Some(url),
        // A multi-slot pool so the pragma read happens on a genuine pool-issued
        // connection, not the single-slot in-memory special case.
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    // Warm one connection so the file exists and WAL conversion has run.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&mut *conn)
            .await
            .expect("create table");
    }

    // On a freshly checked-out pooled connection, `busy_timeout` must report the
    // configured non-zero value — proof the setup callback ran on *this*
    // connection, not just the first one.
    let mut conn = pool.get().await.expect("checkout a second connection");

    let rows: Vec<BusyTimeout> = diesel::sql_query("PRAGMA busy_timeout")
        .load(&mut *conn)
        .await
        .expect("read busy_timeout pragma");
    let timeout = rows
        .into_iter()
        .next()
        .expect("busy_timeout pragma returns a row")
        .timeout;
    assert_eq!(
        timeout, 5000,
        "every pooled connection must carry the configured busy_timeout (ms), \
         mirroring crate::sync::store"
    );

    // The journal mode must be the deliberately-set WAL for a file database.
    let rows: Vec<JournalMode> = diesel::sql_query("PRAGMA journal_mode")
        .load(&mut *conn)
        .await
        .expect("read journal_mode pragma");
    let mode = rows
        .into_iter()
        .next()
        .expect("journal_mode pragma returns a row")
        .journal_mode;
    assert_eq!(
        mode.to_ascii_lowercase(),
        "wal",
        "file-backed pooled connections must use the deliberate WAL journal mode"
    );
}
