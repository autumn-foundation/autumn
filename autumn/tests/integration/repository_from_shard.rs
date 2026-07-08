//! Issue #1273: `from_shard` constructor and `with_pool_untracked` rename.
//!
//! Tests that:
//!  - `with_pool_untracked` is the new name of the raw pool escape hatch and
//!    initialises repos with framework defaults (no timeout, 500ms slow
//!    threshold, unknown route label).
//!  - The instrumentation getters (`__autumn_statement_timeout_ms`,
//!    `__autumn_slow_threshold`, `__autumn_route_label`) are reachable and
//!    accurate for both the renamed escape hatch and extractor-built repos.
//!
//! Runs without a live database (pools are created lazily by deadpool).

#![cfg(feature = "db")]

use autumn_web::AppState;
use autumn_web::config::DatabaseConfig;
use autumn_web::db;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;
use autumn_web::reexports::http::Request;

mod schema {
    autumn_web::reexports::diesel::table! {
        shard_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use schema::shard_notes;

#[autumn_web::model(table = "shard_notes")]
pub struct ShardNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::repository(ShardNote, table = "shard_notes")]
pub trait ShardNoteRepository {}

const POOL_SIZE: usize = 3;

fn make_pool(database: &str) -> Pool<AsyncPgConnection> {
    let config = DatabaseConfig {
        url: Some(format!("postgres://localhost/{database}")),
        pool_size: POOL_SIZE,
        ..Default::default()
    };
    db::create_pool(&config)
        .expect("pool config is valid")
        .expect("url is set")
}

async fn extract_repo(state: &AppState) -> PgShardNoteRepository {
    let (mut parts, ()) = Request::builder()
        .uri("/notes")
        .body(())
        .unwrap()
        .into_parts();
    PgShardNoteRepository::from_request_parts(&mut parts, state)
        .await
        .expect("extraction succeeds")
}

// ── with_pool_untracked defaults ────────────────────────────────────────────

#[tokio::test]
async fn with_pool_untracked_sets_zero_timeout() {
    let pool = make_pool("primary");
    let repo = PgShardNoteRepository::with_pool_untracked(pool);
    assert_eq!(
        repo.__autumn_statement_timeout_ms(),
        0,
        "with_pool_untracked: statement timeout must be 0 (no limit)"
    );
}

#[tokio::test]
async fn with_pool_untracked_sets_default_slow_threshold() {
    let pool = make_pool("primary");
    let repo = PgShardNoteRepository::with_pool_untracked(pool);
    assert_eq!(
        repo.__autumn_slow_threshold(),
        std::time::Duration::from_millis(500),
        "with_pool_untracked: slow-query threshold must be the 500ms default"
    );
}

#[tokio::test]
async fn with_pool_untracked_has_unknown_route_label() {
    let pool = make_pool("primary");
    let repo = PgShardNoteRepository::with_pool_untracked(pool);
    assert_eq!(
        repo.__autumn_route_label(),
        "unknown",
        "with_pool_untracked: route label must be 'unknown' (no request context)"
    );
}

#[tokio::test]
async fn with_pool_untracked_write_pool_matches_supplied_pool() {
    let pool = make_pool("primary");
    let repo = PgShardNoteRepository::with_pool_untracked(pool);
    assert_eq!(
        repo.__autumn_write_pool().status().max_size,
        POOL_SIZE,
        "with_pool_untracked: write pool must be the supplied pool"
    );
}

// ── extractor captures instrumentation ──────────────────────────────────────

#[tokio::test]
async fn extractor_captures_state_slow_threshold() {
    let state = AppState::for_test().with_pool(make_pool("primary"));
    let repo = extract_repo(&state).await;
    // Default AppState slow threshold is 500ms; the repo must snapshot it.
    assert_eq!(
        repo.__autumn_slow_threshold(),
        std::time::Duration::from_millis(500),
    );
}

#[tokio::test]
async fn extractor_sets_zero_timeout_when_no_override() {
    let state = AppState::for_test().with_pool(make_pool("primary"));
    let repo = extract_repo(&state).await;
    assert_eq!(repo.__autumn_statement_timeout_ms(), 0);
}
