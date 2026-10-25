//! `TestDb::reset_table` on Postgres (issue #2555).
//!
//! Needs Docker (testcontainers), so the test is `#[ignore]`d. CI runs it in
//! the Docker sweep.
#![cfg(all(feature = "db", feature = "test-support", not(feature = "sqlite")))]

use autumn_web::test::TestDb;

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reset_table_empties_the_table_and_restarts_ids() {
    use diesel::sql_types::BigInt;
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct Id {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }

    let db = TestDb::new().await;
    db.execute_sql("CREATE TABLE td_reset (id BIGSERIAL PRIMARY KEY, v TEXT)")
        .await;
    db.execute_sql("INSERT INTO td_reset (v) VALUES ('a'), ('b')")
        .await;
    db.reset_table("td_reset").await;
    db.execute_sql("INSERT INTO td_reset (v) VALUES ('c')")
        .await;

    let mut conn = db.pool().get().await.expect("connection");
    let rows: Vec<Id> = diesel::sql_query("SELECT id FROM td_reset")
        .load(&mut *conn)
        .await
        .expect("select");
    assert_eq!(rows.len(), 1, "the old rows are gone");
    assert_eq!(rows[0].id, 1, "the id sequence restarts");
}
