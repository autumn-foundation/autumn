//! Read-only transaction safety proof for the `SQLite` runtime (issue #1614,
//! Codex P2).
//!
//! `Db::tx_with`'s `SQLite` arm runs a single plain, writable transaction and
//! cannot enforce `READ ONLY` (there is no `SQLite` transaction builder that
//! sets read-only access mode the way the Postgres arm does via
//! `SET TRANSACTION READ ONLY`). Silently honoring a caller's
//! `TxOptions::read_only()` by running a writable transaction anyway would let
//! writes succeed and commit under a contract that promised none — a safety
//! regression for any code relying on a read-only transaction to prevent
//! mutation.
//!
//! The fix rejects the request up front: under `--features sqlite`, a
//! `tx_with` call carrying `read_only` returns an unsupported-options error
//! **before the closure runs**, so no write can slip through. A normal
//! read-write transaction is unchanged and still commits its writes.
//!
//! This test drives the real pool + `Db` and asserts both halves: a read-only
//! transaction is rejected and commits nothing, while a plain read-write
//! transaction commits its insert.
//!
//! Only meaningful under `--features sqlite`; it also needs `test-support` for
//! the public [`autumn_web::db::Db::connect_for_test`] harness helper, so the
//! file is `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a
//! default `cargo test` (and a bare `--features sqlite` run) compiles it to an
//! empty (passing) binary. Run explicitly (never via a members-enable edge —
//! that would trip the feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_read_only_tx
//! ```
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{Db, RuntimeConnection, TxOptions, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::sql_types::BigInt;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use scoped_futures::ScopedFutureExt as _;

type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct RowCount {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Count the rows in `t` on a freshly checked-out pooled connection (a
/// connection distinct from the `Db` under test), so the assertion reflects
/// what was actually committed rather than any in-transaction view.
async fn row_count(pool: &SqlitePool) -> i64 {
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    diesel::sql_query("SELECT COUNT(*) AS n FROM t")
        .get_result::<RowCount>(&mut *conn)
        .await
        .expect("count rows")
        .n
}

#[tokio::test]
async fn sqlite_read_only_tx_is_rejected_and_read_write_still_commits() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("rotx.db");
    let url = format!("sqlite://{}", db_path.display());

    let config = DatabaseConfig {
        url: Some(url),
        // A multi-slot pool so the `Db` under test and the out-of-band
        // row-count checkouts can be different pooled connections.
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    // Schema: a single-column table we try to write into.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&mut *conn)
            .await
            .expect("create table t");
    }

    // (1) A read-only transaction that attempts a write must be REJECTED before
    // the closure runs — so the write never happens and nothing commits.
    {
        let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
        let result: Result<(), autumn_web::AutumnError> = db
            .tx_with(TxOptions::default().read_only(), move |conn| {
                async move {
                    // If the arm silently ran a writable transaction, this
                    // insert would succeed and commit — the regression under test.
                    diesel::sql_query("INSERT INTO t (id) VALUES (1)")
                        .execute(conn)
                        .await?;
                    Ok::<_, autumn_web::AutumnError>(())
                }
                .scope_boxed()
            })
            .await;

        let err = result.expect_err("read-only tx under sqlite must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("read-only") && message.contains("SQLite"),
            "expected an unsupported read-only-options error naming SQLite, got: {message}"
        );
    }

    // The rejected read-only transaction must not have committed anything.
    assert_eq!(
        row_count(&pool).await,
        0,
        "a rejected read-only transaction must not commit a write"
    );

    // (2) A normal read-write transaction still commits its write unchanged.
    {
        let mut db = Db::connect_for_test(&pool).await.expect("db checkout");
        db.tx_with(TxOptions::default(), move |conn| {
            async move {
                diesel::sql_query("INSERT INTO t (id) VALUES (1)")
                    .execute(conn)
                    .await?;
                Ok::<_, autumn_web::AutumnError>(())
            }
            .scope_boxed()
        })
        .await
        .expect("a read-write sqlite transaction commits its write");
    }

    assert_eq!(
        row_count(&pool).await,
        1,
        "a read-write transaction must commit its insert"
    );
}
