//! Foreign-key enforcement proof for the `SQLite` runtime pool (issue #1614).
//!
//! SQLite leaves `foreign_keys` OFF by default on every new connection, so a
//! bare pool manager would hand out connections that silently accept orphan
//! rows and violate referential integrity (Codex P1). `build_sqlite_pool` now
//! runs `PRAGMA foreign_keys = ON` in the manager's per-connection setup
//! callback (mirroring `crate::sync::store`), so every pool-issued connection
//! enforces `REFERENCES` constraints.
//!
//! This test builds the real pool through the public
//! [`autumn_web::db::create_pool`] path, creates a parent/child schema with a
//! foreign key, then attempts to insert a child row referencing a non-existent
//! parent on a *separately checked-out* pooled connection and asserts the
//! insert is rejected with a foreign-key constraint violation.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_foreign_keys
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

#[tokio::test]
async fn pooled_sqlite_connections_enforce_foreign_keys() {
    // A tempfile-backed database so a second pooled connection observes the
    // schema created on the first — proving FK enforcement holds on *every*
    // connection the pool hands out, not just the one that ran the DDL.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("fk.db");
    let url = format!("sqlite://{}", db_path.display());

    let config = DatabaseConfig {
        url: Some(url),
        // A multi-slot pool so the schema-setup checkout and the orphan-insert
        // checkout can be different pooled connections.
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    // (1) Create a parent table and a child table with a REFERENCES FK.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("CREATE TABLE parent (id INTEGER PRIMARY KEY)")
            .execute(&mut *conn)
            .await
            .expect("create parent table");
        diesel::sql_query(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER \
             REFERENCES parent(id))",
        )
        .execute(&mut *conn)
        .await
        .expect("create child table");
    }

    // (2) On a freshly checked-out pooled connection, an insert referencing a
    // non-existent parent must FAIL — this only happens when the pool turned
    // `foreign_keys` ON during that connection's setup.
    let mut conn = pool.get().await.expect("checkout a second connection");
    let result = diesel::sql_query("INSERT INTO child (id, parent_id) VALUES (1, 999)")
        .execute(&mut *conn)
        .await;

    let err = result.expect_err("orphan insert must be rejected when foreign_keys is ON");
    let message = err.to_string();
    assert!(
        message.to_ascii_uppercase().contains("FOREIGN KEY"),
        "expected a foreign-key constraint violation, got: {message}"
    );

    // (3) Sanity check: a valid insert (parent exists) is accepted, so the FK
    // pragma rejects only genuine violations rather than blocking all writes.
    diesel::sql_query("INSERT INTO parent (id) VALUES (1)")
        .execute(&mut *conn)
        .await
        .expect("insert parent row");
    diesel::sql_query("INSERT INTO child (id, parent_id) VALUES (2, 1)")
        .execute(&mut *conn)
        .await
        .expect("valid child insert is accepted");
}
