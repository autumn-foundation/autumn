//! `TestDb` on the `SQLite` runtime backend (issue #2555).
//!
//! Under `--features sqlite`, `autumn_web::test::TestDb` is a throwaway
//! tempfile `SQLite` database. It has the same API as the Postgres harness
//! and needs no Docker.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_test_db`.
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::test::TestDb;
use diesel::sql_types::BigInt;
use diesel_async::RunQueryDsl as _;

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

async fn count(db: &TestDb, table: &str) -> i64 {
    let mut conn = db.pool().get().await.expect("connection");
    diesel::sql_query(format!("SELECT COUNT(*) AS n FROM {table}"))
        .get_result::<Count>(&mut *conn)
        .await
        .expect("count")
        .n
}

#[tokio::test]
async fn shared_returns_the_same_database() {
    let a = TestDb::shared().await;
    let b = TestDb::shared().await;
    assert!(std::ptr::eq(a, b));
    assert!(a.url().starts_with("sqlite://"), "{}", a.url());
}

#[tokio::test]
async fn execute_sql_runs_against_the_pool() {
    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS td_rows (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
    )
    .await;
    db.execute_sql("INSERT INTO td_rows (v) VALUES ('a')").await;
    assert!(count(db, "td_rows").await >= 1);
}

#[tokio::test]
async fn reset_table_empties_it_and_restarts_ids() {
    let db = TestDb::new().await;
    db.execute_sql("CREATE TABLE td_reset (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .await;
    db.execute_sql("INSERT INTO td_reset (v) VALUES ('a'), ('b')")
        .await;
    db.reset_table("td_reset").await;
    assert_eq!(count(&db, "td_reset").await, 0);
    db.execute_sql("INSERT INTO td_reset (v) VALUES ('c')")
        .await;
    let mut conn = db.pool().get().await.expect("connection");
    let id = diesel::sql_query("SELECT id AS n FROM td_reset")
        .get_result::<Count>(&mut *conn)
        .await
        .expect("id")
        .n;
    assert_eq!(id, 1, "ids restart after reset_table");
}

#[tokio::test]
async fn new_databases_are_isolated() {
    let a = TestDb::new().await;
    let b = TestDb::new().await;
    a.execute_sql("CREATE TABLE td_iso (id INTEGER PRIMARY KEY)")
        .await;
    b.execute_sql("CREATE TABLE td_iso (id INTEGER PRIMARY KEY)")
        .await;
    a.execute_sql("INSERT INTO td_iso (id) VALUES (1)").await;
    assert_eq!(count(&a, "td_iso").await, 1);
    assert_eq!(count(&b, "td_iso").await, 0);
}

#[tokio::test]
async fn dropping_a_database_removes_its_file() {
    let db = TestDb::new().await;
    let path = db.url().trim_start_matches("sqlite://").to_owned();
    assert!(std::path::Path::new(&path).exists());
    drop(db);
    assert!(!std::path::Path::new(&path).exists());
}

#[tokio::test]
async fn reset_table_works_when_no_table_uses_autoincrement() {
    let db = TestDb::new().await;
    db.execute_sql("CREATE TABLE td_plain (id INTEGER PRIMARY KEY, v TEXT)")
        .await;
    db.execute_sql("INSERT INTO td_plain (v) VALUES ('a')")
        .await;
    db.reset_table("td_plain").await;
    assert_eq!(count(&db, "td_plain").await, 0);
}

#[tokio::test]
async fn reset_table_leaves_other_tables_counters_alone() {
    let db = TestDb::new().await;
    db.execute_sql("CREATE TABLE td_a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .await;
    db.execute_sql("CREATE TABLE td_b (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .await;
    db.execute_sql("INSERT INTO td_a (v) VALUES ('a'), ('b')")
        .await;
    db.execute_sql("INSERT INTO td_b (v) VALUES ('a'), ('b')")
        .await;
    db.reset_table("td_a").await;
    db.execute_sql("DELETE FROM td_b").await;
    db.execute_sql("INSERT INTO td_b (v) VALUES ('c')").await;
    let mut conn = db.pool().get().await.expect("connection");
    let id = diesel::sql_query("SELECT id AS n FROM td_b")
        .get_result::<Count>(&mut *conn)
        .await
        .expect("id")
        .n;
    assert_eq!(id, 3, "the counter of td_b continues");
}

#[tokio::test]
#[should_panic(expected = "plain identifier")]
async fn reset_table_rejects_sql_in_the_name() {
    let db = TestDb::new().await;
    db.reset_table("t; DROP TABLE x").await;
}

#[tokio::test]
#[should_panic(expected = "plain identifier")]
async fn reset_table_rejects_an_empty_name() {
    let db = TestDb::new().await;
    db.reset_table("").await;
}
