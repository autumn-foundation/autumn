//! A `sum` derivation whose partial sums leave `i64` on `SQLite` (#2663).
//!
//! `SQLite`'s integer `SUM` raises `integer overflow` when the running total
//! leaves `i64`, also when the final total fits. The delta paths fold in
//! `i128`, so they accept `MAX, 1, -1`. The set-based paths (drift, recompute,
//! backfill, bulk delete) must accept the same rows.
//!
//! What it pins:
//!
//! * **Repair agrees with the delta paths.** After `save_many` folds
//!   `MAX, 1, -1`, drift is 0, `recompute` rebuilds `MAX`, and `delete_many`
//!   goes back to 0.
//! * **A new derivation finishes its backfill** over the same rows.
//! * **A total that does not fit is still an error.** It is not a REAL and
//!   not a clamped value.
//!
//! Its own binary, so this file registers one derivation only. Run:
//! `cargo test -p autumn-web --features sqlite --test sqlite_derivation_wide_sum`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::derivation::{
    BackfillOptions, DerivationDef, drift, ensure_derivations, recompute, registered_derivations,
    run_backfill,
};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::sql_types::{BigInt, Text};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        ws_posts (id) {
            id -> Int8,
            title -> Text,
            weight_total -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        ws_weights (id) {
            id -> Int8,
            post_id -> Int8,
            weight -> Int8,
        }
    }
}

use schema::{ws_posts, ws_weights};

#[autumn_web::model(table = "ws_posts")]
pub struct WsPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub weight_total: i64,
}

#[autumn_web::repository(WsPost, table = "ws_posts")]
pub trait WsPostRepository {}

// No filter: a negative weight must reach the sum.
#[autumn_web::model(table = "ws_weights")]
#[derivation(WsPost, column = "weight_total", fk = post_id, transform = sum(weight))]
pub struct WsWeight {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub weight: i64,
}

#[autumn_web::repository(WsWeight, table = "ws_weights")]
pub trait WsWeightRepository {}

const DERIVATION: &str = "ws_posts.weight_total";

const DERIVATIONS_DDL: &str =
    include_str!("../derivation_migrations_sqlite/20260907101530_create_derivations/up.sql");

const DDL: &[&str] = &[
    "CREATE TABLE ws_posts (\
         id INTEGER PRIMARY KEY, \
         title TEXT NOT NULL, \
         weight_total BIGINT NOT NULL DEFAULT 0\
     )",
    "CREATE TABLE ws_weights (\
         id INTEGER PRIMARY KEY, \
         post_id BIGINT NOT NULL REFERENCES ws_posts(id), \
         weight BIGINT NOT NULL\
     )",
];

async fn boot_pool(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(2),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    for stmt in DDL.iter().copied().chain([DERIVATIONS_DDL]) {
        diesel::sql_query(stmt)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
    }
    drop(conn);
    pool
}

#[derive(diesel::QueryableByName)]
struct ValueRow {
    #[diesel(sql_type = BigInt)]
    value: i64,
}

#[derive(diesel::QueryableByName)]
struct TypeRow {
    #[diesel(sql_type = Text)]
    kind: String,
}

/// The derived column, read with raw SQL.
async fn derived(pool: &SqlitePool, post: i64) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT weight_total AS value FROM ws_posts WHERE id = ?")
        .bind::<BigInt, _>(post)
        .get_result::<ValueRow>(&mut *conn)
        .await
        .expect("read derived column")
        .value
}

/// The storage class of the derived column. A REAL here means the sum
/// overflowed into a float.
async fn derived_type(pool: &SqlitePool, post: i64) -> String {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT typeof(weight_total) AS kind FROM ws_posts WHERE id = ?")
        .bind::<BigInt, _>(post)
        .get_result::<TypeRow>(&mut *conn)
        .await
        .expect("read derived column type")
        .kind
}

async fn seed_post(pool: &SqlitePool, title: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("INSERT INTO ws_posts (title) VALUES (?) RETURNING id AS value")
        .bind::<Text, _>(title)
        .get_result::<ValueRow>(&mut *conn)
        .await
        .expect("seed post")
        .value
}

/// Insert weights in row order, with no derivation maintenance.
async fn seed_raw_weights(pool: &SqlitePool, post: i64, weights: &[i64]) {
    let mut conn = pool.get().await.expect("conn");
    for weight in weights {
        diesel::sql_query("INSERT INTO ws_weights (post_id, weight) VALUES (?, ?)")
            .bind::<BigInt, _>(post)
            .bind::<BigInt, _>(*weight)
            .execute(&mut *conn)
            .await
            .expect("seed weight");
    }
}

fn def() -> &'static DerivationDef {
    registered_derivations()
        .into_iter()
        .find(|def| def.name == DERIVATION)
        .unwrap_or_else(|| panic!("`{DERIVATION}` must be a registered derivation"))
}

#[tokio::test]
async fn repair_accepts_a_partial_sum_that_leaves_i64() {
    let pool = boot_pool("ws_repair").await;
    let repo = PgWsWeightRepository::with_pool_untracked(pool.clone());
    let post = seed_post(&pool, "wide").await;

    // Row order MAX, 1, -1: the running total passes MAX after row two.
    let saved = repo
        .save_many(&[i64::MAX, 1, -1].map(|weight| NewWsWeight {
            post_id: post,
            weight,
        }))
        .await
        .expect("save_many folds the batch in i128");
    assert_eq!(derived(&pool, post).await, i64::MAX);

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        drift(&mut conn, def()).await.expect("the drift probe runs"),
        0
    );

    diesel::sql_query("UPDATE ws_posts SET weight_total = 0 WHERE id = ?")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("corrupt the derived value");
    assert_eq!(
        recompute(&mut conn, DERIVATION)
            .await
            .expect("recompute runs"),
        1
    );
    assert_eq!(derived(&pool, post).await, i64::MAX);
    assert_eq!(derived_type(&pool, post).await, "integer");

    let ids: Vec<i64> = saved.iter().map(|row| row.id).collect();
    repo.delete_many(&ids).await.expect("bulk delete runs");
    assert_eq!(derived(&pool, post).await, 0);
}

#[tokio::test]
async fn a_new_derivation_finishes_its_backfill_over_wide_partial_sums() {
    let pool = boot_pool("ws_backfill").await;
    let high = seed_post(&pool, "high").await;
    let low = seed_post(&pool, "low").await;
    seed_raw_weights(&pool, high, &[i64::MAX, 1, -1]).await;
    seed_raw_weights(&pool, low, &[i64::MIN, -1, 1]).await;

    let mut conn = pool.get().await.expect("conn");
    ensure_derivations(&mut conn).await.expect("enqueue");
    let report = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("the backfill runs");
    assert_eq!(report.completed, vec![DERIVATION.to_owned()], "{report:?}");
    assert_eq!(report.rows_repaired, 2, "{report:?}");
    assert_eq!(derived(&pool, high).await, i64::MAX);
    assert_eq!(derived(&pool, low).await, i64::MIN);
    assert_eq!(drift(&mut conn, def()).await.expect("drift"), 0);
}

#[tokio::test]
async fn a_total_that_leaves_i64_is_still_an_error() {
    let pool = boot_pool("ws_too_wide").await;
    let post = seed_post(&pool, "too wide").await;
    seed_raw_weights(&pool, post, &[i64::MAX, 1]).await;

    let mut conn = pool.get().await.expect("conn");
    let err = recompute(&mut conn, DERIVATION)
        .await
        .expect_err("a total past MAX has no i64 value to store");
    assert!(err.to_string().contains("integer overflow"), "{err}");
    assert!(drift(&mut conn, def()).await.is_err());
    assert_eq!(derived(&pool, post).await, 0, "nothing is written");
    assert_eq!(derived_type(&pool, post).await, "integer");
}
