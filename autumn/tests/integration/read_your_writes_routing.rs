//! Issue #1201: generated `#[repository]` read methods route to the primary
//! when a read-your-own-writes (RYWW) pin is active — either because the
//! current request acquired a primary write connection, or because a fresh
//! signed `autumn.ryw` session cookie arrived from a previous write request.
//!
//! All tests run without a live database: pools are created lazily (no TCP
//! connection until a checkout), sized 5/2 so `pool.status().max_size`
//! distinguishes primary from replica, mirroring `repository_replica_routing.rs`.
//!
//! Cookie-parsing unit tests live in `autumn/src/read_your_writes.rs` where
//! `ResolvedSigningKeys` (pub(crate)) is accessible.

#![cfg(feature = "db")]

use autumn_web::AppState;
use autumn_web::config::{DatabaseConfig, ReadYourWrites};
use autumn_web::db;
use autumn_web::middleware::MetricsCollector;
use autumn_web::read_your_writes::{self, RequestPin};
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;
use autumn_web::reexports::http::Request;
use autumn_web::repository::ReadRoute;

mod schema {
    autumn_web::reexports::diesel::table! {
        ryw_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use schema::ryw_notes;

#[autumn_web::model(table = "ryw_notes")]
pub struct RywNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::repository(RywNote, table = "ryw_notes")]
pub trait RywNoteRepository {
    fn find_by_content(content: String) -> Vec<RywNote>;
}

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

fn read_pool_size(route: &ReadRoute) -> Option<usize> {
    match route {
        ReadRoute::ReadPool(pool) => Some(pool.status().max_size),
        ReadRoute::Primary | ReadRoute::Unavailable => None,
    }
}

// ── AC 1: request mode — reads stay on replica before any write ─────────────

#[tokio::test]
async fn request_mode_reads_replica_before_write() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;
    let pin = RequestPin::new(ReadYourWrites::Request);

    read_your_writes::scope(pin, async {
        assert_eq!(
            read_pool_size(&repo.__autumn_effective_read_route()),
            Some(REPLICA_POOL_SIZE),
            "before any write, reads must still route to the replica"
        );
    })
    .await;
}

// ── AC 2: request mode — reads pin to primary after mark_write() ────────────

#[tokio::test]
async fn request_mode_pins_reads_after_mark_write() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;
    let pin = RequestPin::new(ReadYourWrites::Request);

    read_your_writes::scope(pin, async {
        read_your_writes::mark_write();

        assert!(
            matches!(repo.__autumn_effective_read_route(), ReadRoute::Primary),
            "after mark_write(), reads must be pinned to the primary"
        );
    })
    .await;
}

// ── AC 3: concurrent isolation — second scope without write is not pinned ────

#[tokio::test]
async fn concurrent_scopes_are_isolated() {
    let state1 = two_pool_state();
    let state2 = two_pool_state();

    let (wrote_result, clean_result) = tokio::join!(
        tokio::spawn(async move {
            let repo: PgRywNoteRepository = extract(&state1).await;
            let pin = RequestPin::new(ReadYourWrites::Request);
            read_your_writes::scope(pin, async move {
                read_your_writes::mark_write();
                matches!(repo.__autumn_effective_read_route(), ReadRoute::Primary)
            })
            .await
        }),
        tokio::spawn(async move {
            let repo: PgRywNoteRepository = extract(&state2).await;
            let pin = RequestPin::new(ReadYourWrites::Request);
            read_your_writes::scope(pin, async move {
                read_pool_size(&repo.__autumn_effective_read_route())
            })
            .await
        })
    );

    assert!(
        wrote_result.unwrap(),
        "task that wrote must have reads pinned to primary"
    );
    assert_eq!(
        clean_result.unwrap(),
        Some(REPLICA_POOL_SIZE),
        "task that did not write must keep reading from the replica"
    );
}

// ── AC 4: off mode (no scope) — existing behavior preserved ─────────────────

#[tokio::test]
async fn off_mode_no_scope_preserves_replica_routing() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;

    assert!(
        !read_your_writes::is_pinned(),
        "is_pinned() must be false outside any scope"
    );
    assert_eq!(
        read_pool_size(&repo.__autumn_effective_read_route()),
        read_pool_size(repo.__autumn_read_route()),
        "__autumn_effective_read_route() must match snapshot outside any scope"
    );
}

// ── AC 5: config parsing round-trips ────────────────────────────────────────

#[test]
fn read_your_writes_from_str_round_trips() {
    use std::str::FromStr;

    assert!(matches!(
        ReadYourWrites::from_str("off").unwrap(),
        ReadYourWrites::Off
    ));
    assert!(matches!(
        ReadYourWrites::from_str("request").unwrap(),
        ReadYourWrites::Request
    ));
    assert!(matches!(
        ReadYourWrites::from_str("session").unwrap(),
        ReadYourWrites::Session
    ));
    assert!(ReadYourWrites::from_str("invalid").is_err());
}

#[test]
fn database_config_defaults() {
    let config = DatabaseConfig::default();
    assert!(
        matches!(config.read_your_writes, ReadYourWrites::Off),
        "default mode must be off"
    );
    assert_eq!(config.pin_after_write_secs, 5, "default window must be 5 s");
}

// ── AC 6: session-mode incoming_pin (cookie-based) pins reads to primary ────
//
// Cookie parsing is unit-tested in `read_your_writes.rs`. Here we verify the
// routing effect using `RequestPin::with_incoming_pin`.

#[tokio::test]
async fn session_mode_incoming_pin_routes_reads_to_primary() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;

    let pin = RequestPin::with_incoming_pin(ReadYourWrites::Session, true);
    read_your_writes::scope(pin, async {
        assert!(
            matches!(repo.__autumn_effective_read_route(), ReadRoute::Primary),
            "incoming_pin=true must route reads to primary without an explicit write"
        );
    })
    .await;
}

#[tokio::test]
async fn session_mode_expired_pin_routes_reads_to_replica() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;

    // incoming_pin=false simulates an expired or absent cookie.
    let pin = RequestPin::with_incoming_pin(ReadYourWrites::Session, false);
    read_your_writes::scope(pin, async {
        assert_eq!(
            read_pool_size(&repo.__autumn_effective_read_route()),
            Some(REPLICA_POOL_SIZE),
            "incoming_pin=false without a write must still use the replica"
        );
    })
    .await;
}

// ── AC 7: metrics incremented on each pin-redirected read ───────────────────

#[tokio::test]
async fn metrics_incremented_on_each_pin_redirect() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;
    let metrics = MetricsCollector::new();

    let pin = RequestPin::new_with_metrics(ReadYourWrites::Request, metrics.clone());
    read_your_writes::scope(pin, async {
        read_your_writes::mark_write();

        // Two pin-redirected reads.
        let _ = repo.__autumn_effective_read_route();
        let _ = repo.__autumn_effective_read_route();
    })
    .await;

    let snapshot = metrics.snapshot();
    assert_eq!(
        snapshot.read_your_writes_pins_total, 2,
        "each redirected read must increment the counter"
    );
}

// ── Regression: existing snapshot accessor must be unaffected ───────────────

#[tokio::test]
async fn snapshot_accessor_unaffected_by_ryw_pin() {
    let state = two_pool_state();
    let repo: PgRywNoteRepository = extract(&state).await;
    let pin = RequestPin::new(ReadYourWrites::Request);

    read_your_writes::scope(pin, async {
        read_your_writes::mark_write();

        // Even when the effective route is Primary, the snapshot is unchanged.
        assert_eq!(
            read_pool_size(repo.__autumn_read_route()),
            Some(REPLICA_POOL_SIZE),
            "__autumn_read_route() must always return the immutable snapshot"
        );
    })
    .await;
}
