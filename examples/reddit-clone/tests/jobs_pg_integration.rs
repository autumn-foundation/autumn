//! Integration tests: Postgres-backed job queue on the reddit-clone.
//!
//! Covers two scenarios from issue #675 AC #6:
//!
//! 1. `pg_job_enqueue_participates_in_transaction` – proves that an
//!    autumn-jobs INSERT is part of the caller's Diesel transaction; rolling
//!    the transaction back removes both the domain row and the job row.
//!
//! 2. `user_onboarding_enqueue_run_ack_cycle` – proves the full
//!    enqueue → claim (SKIP LOCKED) → run handler → ack cycle on Postgres
//!    with no Redis or in-memory queue involved.
//!
//! Both tests require Docker and are marked `#[ignore]` so they are skipped
//! in standard `cargo test` runs. Run them explicitly with:
//!
//! ```text
//! cargo test -p reddit-clone -- --ignored
//! ```

use autumn_web::AppState;
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use reddit_clone::jobs::{UserOnboardingArgs, user_onboarding};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ── SQL for schema setup ────────────────────────────────────────────────────

const CREATE_AUTUMN_JOBS: &str =
    include_str!("../../../autumn/migrations/20260513000000_create_job_queue/up.sql");

const CREATE_USERS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS users (
    id            BIGSERIAL   PRIMARY KEY,
    username      TEXT        NOT NULL UNIQUE,
    password_hash TEXT        NOT NULL,
    karma         BIGINT      NOT NULL DEFAULT 0,
    role          TEXT        NOT NULL DEFAULT 'user',
    created_at    TIMESTAMP   NOT NULL DEFAULT NOW()
);";

// ── Diesel QueryableByName helpers ─────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct KarmaRow {
    #[diesel(sql_type = BigInt)]
    karma: i64,
}

#[derive(diesel::QueryableByName)]
struct StatusRow {
    #[diesel(sql_type = Text)]
    status: String,
}

#[derive(diesel::QueryableByName)]
struct ClaimedIdRow {
    #[diesel(sql_type = Text)]
    id: String,
}

// ── Helper: start a Postgres container and build a pool ───────────────────

async fn start_postgres() -> (impl std::any::Any, Pool<AsyncPgConnection>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("build pool");
    (container, pool)
}

// ── Test 1: job INSERT rolls back atomically with the domain write ──────────

/// Demonstrates that enqueuing a job inside a Diesel transaction is atomic:
/// rolling back the transaction also removes the job row, so there are no
/// orphan jobs pointing at domain objects that were never committed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_job_enqueue_participates_in_transaction() {
    let (_container, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("get connection");

    diesel::sql_query(CREATE_AUTUMN_JOBS)
        .execute(&mut *conn)
        .await
        .expect("create autumn_jobs");
    diesel::sql_query(CREATE_USERS_TABLE)
        .execute(&mut *conn)
        .await
        .expect("create users");

    // Open a transaction, insert a user + a job row, then roll back.
    let _: Result<(), diesel::result::Error> = conn
        .transaction(async |conn| {
            diesel::sql_query(
                "INSERT INTO users (username, password_hash) VALUES ('ferris', 'hash')",
            )
            .execute(conn)
            .await?;

            diesel::sql_query(
                "INSERT INTO autumn_jobs \
                 (id, name, payload, status, attempt, max_attempts, initial_backoff_ms, \
                  enqueued_at, run_at) \
                 VALUES ('rollback-test-id', 'user_onboarding', '{}'::JSONB, \
                         'enqueued', 1, 5, 500, NOW(), NOW())",
            )
            .execute(conn)
            .await?;

            Err(diesel::result::Error::RollbackTransaction)
        })
        .await;

    // Both the user row and the job row must have disappeared.
    let job_count: i64 = diesel::sql_query(
        "SELECT COUNT(*)::BIGINT AS count FROM autumn_jobs WHERE id = 'rollback-test-id'",
    )
    .get_result::<CountRow>(&mut *conn)
    .await
    .expect("count autumn_jobs")
    .count;

    let user_count: i64 =
        diesel::sql_query("SELECT COUNT(*)::BIGINT AS count FROM users WHERE username = 'ferris'")
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count users")
            .count;

    assert_eq!(job_count, 0, "job row must roll back with the transaction");
    assert_eq!(
        user_count, 0,
        "user row must roll back with the transaction"
    );
}

// ── Test 2: enqueue → claim (SKIP LOCKED) → run handler → ack ──────────────

/// Demonstrates the full enqueue → claim → run → ack cycle on the Postgres
/// backend, exercising the real `user_onboarding` handler and verifying the
/// karma update. No Redis or in-process queue is involved.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn user_onboarding_enqueue_run_ack_cycle() {
    let (_container, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("get connection");

    diesel::sql_query(CREATE_AUTUMN_JOBS)
        .execute(&mut *conn)
        .await
        .expect("create autumn_jobs");
    diesel::sql_query(CREATE_USERS_TABLE)
        .execute(&mut *conn)
        .await
        .expect("create users");

    // Insert a user with karma=0.
    let user_id: i64 = diesel::sql_query(
        "INSERT INTO users (username, password_hash) VALUES ('rustacean', 'hash') RETURNING id",
    )
    .get_result::<IdRow>(&mut *conn)
    .await
    .expect("insert user")
    .id;

    // ── Enqueue ────────────────────────────────────────────────────────────
    let job_id = Uuid::new_v4().to_string();
    let args = UserOnboardingArgs {
        user_id,
        username: "rustacean".to_owned(),
    };
    diesel::sql_query(
        "INSERT INTO autumn_jobs \
         (id, name, payload, status, attempt, max_attempts, initial_backoff_ms, enqueued_at, run_at) \
         VALUES ($1, 'user_onboarding', $2::JSONB, 'enqueued', 1, 5, 500, NOW(), NOW())",
    )
    .bind::<Text, _>(&job_id)
    .bind::<Text, _>(&serde_json::to_string(&args).unwrap())
    .execute(&mut *conn)
    .await
    .expect("enqueue job");

    // ── Claim (SELECT … FOR UPDATE SKIP LOCKED) ────────────────────────────
    let claimed_id: String = diesel::sql_query(
        "UPDATE autumn_jobs \
         SET status = 'running', started_at = NOW(), claimed_by = 'test-worker', claimed_at = NOW() \
         WHERE id = ( \
           SELECT id FROM autumn_jobs \
           WHERE status = 'enqueued' AND run_at <= NOW() \
           ORDER BY run_at ASC \
           LIMIT 1 \
           FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id",
    )
    .get_result::<ClaimedIdRow>(&mut *conn)
    .await
    .expect("claim job")
    .id;

    assert_eq!(
        claimed_id, job_id,
        "claimed job should be the one we enqueued"
    );

    // ── Run handler ────────────────────────────────────────────────────────
    let state = AppState::detached().with_pool(pool.clone());
    user_onboarding(state, args)
        .await
        .expect("user_onboarding handler should succeed");

    // ── Ack ────────────────────────────────────────────────────────────────
    diesel::sql_query(
        "UPDATE autumn_jobs SET status = 'completed', finished_at = NOW() WHERE id = $1",
    )
    .bind::<Text, _>(&claimed_id)
    .execute(&mut *conn)
    .await
    .expect("ack job");

    // ── Verify ────────────────────────────────────────────────────────────
    let status = diesel::sql_query("SELECT status FROM autumn_jobs WHERE id = $1")
        .bind::<Text, _>(&claimed_id)
        .get_result::<StatusRow>(&mut *conn)
        .await
        .expect("query job status")
        .status;
    assert_eq!(status, "completed");

    let karma: i64 = diesel::sql_query("SELECT karma FROM users WHERE id = $1")
        .bind::<BigInt, _>(user_id)
        .get_result::<KarmaRow>(&mut *conn)
        .await
        .expect("query karma")
        .karma;
    assert_eq!(karma, 5, "user_onboarding should award 5 starter karma");
}
