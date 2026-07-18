//! Issue #971: generated `#[repository]` read methods route to the replica
//! pool automatically when `database.replica_url` is configured, while
//! mutating methods stay on the primary.
//!
//! Uses the existing two-pool test topology: pools are created lazily (no
//! live database needed) and distinguished by `status().max_size`, the same
//! technique as the `AppState::read_pool` tests in `src/state.rs`.

#![cfg(feature = "db")]

use autumn_web::AppState;
use autumn_web::config::{DatabaseConfig, ReplicaFallback};
use autumn_web::db;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;
use autumn_web::reexports::http::Request;
use autumn_web::repository::ReadRoute;

mod schema {
    autumn_web::reexports::diesel::table! {
        replica_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        pinned_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use schema::{pinned_notes, replica_notes};

#[autumn_web::model(table = "replica_notes")]
pub struct ReplicaNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::model(table = "pinned_notes")]
pub struct PinnedNote {
    #[id]
    pub id: i64,
    pub content: String,
}

/// Default routing: reads go to the replica when one is configured.
#[autumn_web::repository(ReplicaNote, table = "replica_notes")]
pub trait ReplicaNoteRepository {
    fn find_by_content(content: String) -> Vec<ReplicaNote>;
}

/// Per-repository opt-out: reads pinned to the primary.
#[autumn_web::repository(PinnedNote, table = "pinned_notes", primary_reads)]
pub trait PinnedNoteRepository {}

const PRIMARY_POOL_SIZE: usize = 5;
const REPLICA_POOL_SIZE: usize = 2;

fn make_pool(database: &str, pool_size: usize) -> Pool<AsyncPgConnection> {
    let config = DatabaseConfig {
        url: Some(format!("postgres://localhost/{database}")),
        pool_size,
        ..Default::default()
    };
    db::create_pool(&config)
        .expect("pool config is valid")
        .expect("url is set")
}

fn two_pool_state() -> AppState {
    AppState::for_test()
        .with_pool(make_pool("primary", PRIMARY_POOL_SIZE))
        .with_replica_pool(make_pool("replica", REPLICA_POOL_SIZE))
}

async fn extract<R>(state: &AppState) -> R
where
    R: FromRequestParts<AppState, Rejection = autumn_web::AutumnError>,
{
    let (mut parts, ()) = Request::builder().body(()).unwrap().into_parts();
    R::from_request_parts(&mut parts, state)
        .await
        .expect("repository extraction succeeds")
}

/// Resolves the pool a generated read method would acquire from, or `None`
/// when reads are pinned to the primary / unavailable.
fn read_pool_size(route: &ReadRoute) -> Option<usize> {
    match route {
        ReadRoute::ReadPool(pool) => Some(pool.status().max_size),
        ReadRoute::Primary | ReadRoute::Unavailable => None,
    }
}

#[tokio::test]
async fn generated_reads_target_replica_and_writes_target_primary() {
    let state = two_pool_state();
    let repo: PgReplicaNoteRepository = extract(&state).await;

    // Generated read-only methods acquire from the replica pool…
    assert_eq!(
        read_pool_size(repo.__autumn_read_route()),
        Some(REPLICA_POOL_SIZE),
        "read route must snapshot the replica pool"
    );
    // …while mutating methods stay on the primary pool.
    assert_eq!(
        repo.__autumn_write_pool().status().max_size,
        PRIMARY_POOL_SIZE,
        "write pool must remain the primary"
    );
}

#[tokio::test]
async fn reads_use_primary_when_no_replica_is_configured() {
    let state = AppState::for_test().with_pool(make_pool("primary", PRIMARY_POOL_SIZE));
    let repo: PgReplicaNoteRepository = extract(&state).await;

    assert!(
        matches!(repo.__autumn_read_route(), ReadRoute::Primary),
        "single-pool apps must keep current behavior (reads on primary)"
    );
}

#[tokio::test]
async fn primary_reads_attribute_pins_reads_to_primary() {
    let state = two_pool_state();
    let repo: PgPinnedNoteRepository = extract(&state).await;

    assert!(
        matches!(repo.__autumn_read_route(), ReadRoute::Primary),
        "primary_reads repositories must never read from the replica"
    );
    assert_eq!(
        repo.__autumn_write_pool().status().max_size,
        PRIMARY_POOL_SIZE
    );
}

#[tokio::test]
async fn on_primary_escape_hatch_pins_reads_per_call() {
    let state = two_pool_state();
    let repo: PgReplicaNoteRepository = extract(&state).await;

    let pinned = repo.on_primary();
    assert!(
        matches!(pinned.__autumn_read_route(), ReadRoute::Primary),
        "on_primary() must force reads onto the primary pool"
    );
    // The original repository keeps routing reads to the replica.
    assert_eq!(
        read_pool_size(repo.__autumn_read_route()),
        Some(REPLICA_POOL_SIZE),
        "on_primary() returns a pinned clone without mutating the original"
    );
}

#[tokio::test]
async fn reads_fall_back_to_primary_when_replica_unready_and_policy_allows() {
    let state = two_pool_state();
    state
        .probes()
        .configure_replica_dependency(ReplicaFallback::Primary);
    state
        .probes()
        .mark_replica_unready("replica migrations lag primary");

    let repo: PgReplicaNoteRepository = extract(&state).await;
    assert_eq!(
        read_pool_size(repo.__autumn_read_route()),
        Some(PRIMARY_POOL_SIZE),
        "fallback policy routes reads to the primary pool"
    );
}

#[tokio::test]
async fn reads_error_when_replica_unready_and_fallback_forbidden() {
    let state = two_pool_state();
    state
        .probes()
        .configure_replica_dependency(ReplicaFallback::FailReadiness);
    state
        .probes()
        .mark_replica_unready("replica connection failed");

    let repo: PgReplicaNoteRepository = extract(&state).await;
    assert!(
        matches!(repo.__autumn_read_route(), ReadRoute::Unavailable),
        "FailReadiness policy must not silently fall back to the primary"
    );

    // A generated read method fails fast — before touching any pool — so this
    // runs without a live database.
    let err = repo
        .find_all()
        .await
        .expect_err("reads must be unavailable");
    assert!(
        err.to_string().to_lowercase().contains("replica"),
        "error should explain the replica is unavailable, got: {err}"
    );
}

/// Issue #835: `preload` runs against the same read role as the parent finder.
/// It acquires its connection through the repository's `ReadRoute`, so under a
/// `FailReadiness` policy with an unready replica it fails fast with the same
/// "replica unavailable" error a finder would — before touching any pool, and
/// without ever falling back to the primary. (Runs without a live database.)
#[tokio::test]
async fn preload_routes_through_read_role_and_fails_fast_on_unavailable() {
    let state = two_pool_state();
    state
        .probes()
        .configure_replica_dependency(ReplicaFallback::FailReadiness);
    state
        .probes()
        .mark_replica_unready("replica connection failed");

    let repo: PgReplicaNoteRepository = extract(&state).await;
    assert!(matches!(repo.__autumn_read_route(), ReadRoute::Unavailable));

    let err = repo
        .preload(
            vec![ReplicaNote {
                id: 1,
                content: "x".into(),
            }],
            ReplicaNote::preload(),
        )
        .await
        .expect_err("preload must honor the unavailable read route");
    assert!(
        err.to_string().to_lowercase().contains("replica"),
        "preload should route through the read role and report the replica is \
         unavailable, got: {err}"
    );
}
