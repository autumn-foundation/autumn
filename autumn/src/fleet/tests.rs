//! A test engine with a hand-written extractor over [`resolve`]. The tests
//! do not need `sqlite`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::routing::get;
use http::{Request, StatusCode};
use tower::ServiceExt as _;

use super::{FleetLease, fleet_of, ownership_store, resolve, resolve_key};
use crate::AppState;
use crate::config::AutumnConfig;
use crate::config::DatabaseFleetConfig;
use crate::db::fleet::test_engine::{TextEngine, TextHandle};
use crate::db::fleet::{DatabaseFleet, FleetDatabase, FleetError, FleetLifecycle};
use crate::db::fleet_ownership::{FleetOwnership, MemoryOwnership, OwnerId, SlotRange};
use crate::error::AutumnError;
use crate::fleet_layout::{FleetDbKey, FleetMode};
use crate::sharding::ShardKeyOverride;

/// The extractor an engine plugin writes: its own name, its own mapping.
struct TextDb(FleetLease<TextEngine>);

impl FromRequestParts<AppState> for TextDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, AutumnError> {
        resolve::<TextEngine>(parts, state)
            .await
            .map(Self)
            .map_err(AutumnError::from)
    }
}

fn config(root: &Path, max_open: usize) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open,
        max_resident_bytes: 0,
        min_residency: Duration::ZERO,
        pool_size: 1,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: Duration::ZERO,
        restore_check_sample: 1,
        replication_workers: 1,
    }
}

fn fleet(root: &Path, max_open: usize) -> DatabaseFleet<TextEngine> {
    DatabaseFleet::with_engine(&config(root, max_open), TextEngine::new(100)).unwrap()
}

/// App state with the fleet and a default config (no `[tenancy]`).
fn state(fleet: &DatabaseFleet<TextEngine>) -> AppState {
    let state = AppState::for_test().with_extension(AutumnConfig::default());
    state.insert_extension(fleet.clone());
    state
}

/// Installs ownership of `range` for `host-a`, at epoch 1.
async fn own(fleet: &DatabaseFleet<TextEngine>, range: SlotRange) -> Arc<FleetOwnership> {
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = host(&store, "host-a", range);
    let report = fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
    assert_eq!(report.claimed.len(), 1, "{report:?}");
    ownership
}

/// The ownership of `owner` for `range` on `store`. Nothing is claimed yet.
fn host(store: &Arc<MemoryOwnership>, owner: &str, range: SlotRange) -> Arc<FleetOwnership> {
    Arc::new(
        FleetOwnership::new(
            Arc::clone(store) as _,
            OwnerId::new(owner).unwrap(),
            vec![range],
            Duration::from_secs(30),
        )
        .unwrap(),
    )
}

fn request(path: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .extension(ShardKeyOverride(tenant.to_owned()))
        .body(Body::empty())
        .unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Waits until `cond` holds, for at most 5 s.
async fn eventually(mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cond(), "the condition did not hold in time");
}

/// A tenant id whose slot is in (`inside`) or out of `range`.
fn tenant_in(range: SlotRange, inside: bool) -> String {
    (0..10_000)
        .map(|n| format!("t{n}"))
        .find(|id| range.contains(crate::sharding::slot_for_key(id.as_str().into()).0) == inside)
        .expect("16384 slots spread 10k ids")
}

fn write_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/write",
            get(|db: TextDb| async move {
                TextEngine::write(db.0.handle(), "row")
                    .map_err(AutumnError::internal_server_error_msg)?;
                let values =
                    TextEngine::read(&db.0).map_err(AutumnError::internal_server_error_msg)?;
                Ok::<_, AutumnError>(format!("{} {}", db.0.key(), values.len()))
            }),
        )
        .with_state(state)
}

#[tokio::test]
async fn a_handwritten_extractor_serves_the_tenant_database() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let app = write_router(state(&fleet));
    let response = app
        .clone()
        .oneshot(request("/write", "acme"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant:acme 1");
    let response = app.oneshot(request("/write", "acme")).await.unwrap();
    assert_eq!(body_text(response).await, "tenant:acme 2");
    assert!(fleet.exists(&fleet.key_for("acme").unwrap()));
}

#[tokio::test]
async fn a_host_that_does_not_own_the_slot_answers_421_and_opens_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let half = SlotRange::new(0, 8191).unwrap();
    let _ownership = own(&fleet, half).await;
    let app = write_router(state(&fleet));

    let owned = tenant_in(half, true);
    let response = app
        .clone()
        .oneshot(request("/write", &owned))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let opens = fleet
        .engine()
        .opens
        .load(std::sync::atomic::Ordering::SeqCst);
    let foreign = tenant_in(half, false);
    let response = app.oneshot(request("/write", &foreign)).await.unwrap();
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    let key = fleet.key_for(&foreign).unwrap();
    assert!(
        !fleet.exists(&key),
        "a non-owner must not create the database"
    );
    assert!(!fleet.open_keys().contains(&key));
    assert_eq!(
        fleet
            .engine()
            .opens
            .load(std::sync::atomic::Ordering::SeqCst),
        opens,
        "the engine opened nothing"
    );
}

#[tokio::test]
async fn a_slot_another_host_holds_answers_421() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let (low, high) = (
        SlotRange::new(0, 8191).unwrap(),
        SlotRange::new(8192, 16383).unwrap(),
    );
    // Host b holds the upper half. Host a asks for every slot.
    let b = host(&store, "host-b", high);
    assert_eq!(b.refresh().await.claimed.len(), 1);
    let a = host(&store, "host-a", SlotRange::ALL);
    let report = fleet.start_ownership(Arc::clone(&a)).await.unwrap();
    assert_eq!(
        report.waiting,
        [SlotRange::ALL],
        "a live lease of b blocks a"
    );
    let app = write_router(state(&fleet));
    for tenant in [tenant_in(low, true), tenant_in(high, true)] {
        let response = app
            .clone()
            .oneshot(request("/write", &tenant))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::MISDIRECTED_REQUEST,
            "{tenant}"
        );
        assert!(body_text(response).await.contains("does not own slot"));
    }
    assert_eq!(
        fleet
            .engine()
            .opens
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn the_lease_holds_while_the_handler_runs_and_eviction_follows() {
    let tmp = tempfile::tempdir().unwrap();
    // One open database: any second open asks to close the first.
    let fleet = fleet(tmp.path(), 1);
    let state = state(&fleet);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<TextHandle>();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let app = {
        let release = Arc::clone(&release);
        Router::new()
            .route(
                "/hold",
                get(move |db: TextDb| async move {
                    let tx = entered.lock().unwrap().take();
                    if let Some(tx) = tx {
                        let _ = tx.send(db.0.handle().clone());
                    }
                    release.notified().await;
                    // The handler still uses the database here.
                    TextEngine::write(&db.0, "late").map_err(AutumnError::internal_server_error_msg)
                }),
            )
            .with_state(state.clone())
    };
    let call = tokio::spawn(app.oneshot(request("/hold", "a")));
    let held = entered_rx.await.unwrap();
    let a = fleet.key_for("a").unwrap();

    // A job opens another database while the handler runs.
    let b = resolve_key::<TextEngine>(&state, "b").await.unwrap();
    drop(b);
    assert!(
        fleet.open_keys().contains(&a),
        "the handler's lease keeps `a`"
    );
    assert!(!held.is_closed());

    release.notify_one();
    let response = call.await.unwrap().unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        body_text(response).await
    );

    // The lease is gone: the next open evicts `a`.
    drop(resolve_key::<TextEngine>(&state, "c").await.unwrap());
    eventually(|| held.is_closed()).await;
    assert!(!fleet.open_keys().contains(&a));
}

#[tokio::test]
async fn resolve_without_a_fleet_of_the_engine_is_503() {
    let state = AppState::for_test().with_extension(AutumnConfig::default());
    let mut parts = request("/", "acme").into_parts().0;
    let error = resolve::<TextEngine>(&mut parts, &state).await.unwrap_err();
    assert!(matches!(error, FleetError::NoFleet { .. }), "{error}");
    assert_eq!(error.http_status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = resolve_key::<TextEngine>(&state, "acme").await.unwrap_err();
    assert!(matches!(error, FleetError::NoFleet { .. }), "{error}");
}

#[tokio::test]
async fn resolve_without_a_routing_key_is_400() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let state = state(&fleet);
    let mut parts = Request::builder().uri("/").body(()).unwrap().into_parts().0;
    let error = resolve::<TextEngine>(&mut parts, &state).await.unwrap_err();
    assert!(matches!(error, FleetError::NoRoutingKey { .. }), "{error}");
    assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
    assert!(!error.to_string().contains("ShardedDb"), "{error}");
}

#[tokio::test]
async fn resolve_key_takes_a_tenant_id_and_refuses_a_bad_one() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let state = state(&fleet);
    let lease = resolve_key::<TextEngine>(&state, "acme").await.unwrap();
    assert_eq!(lease.key(), &fleet.key_for("acme").unwrap());
    assert_eq!(lease.epoch(), None, "no ownership, no epoch");
    let error = resolve_key::<TextEngine>(&state, "../etc")
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), StatusCode::BAD_REQUEST, "{error}");
    assert!(fleet_of::<TextEngine>(&state).is_some());
}

#[tokio::test]
async fn resolve_key_on_a_non_owner_is_421() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let half = SlotRange::new(0, 8191).unwrap();
    let _ownership = own(&fleet, half).await;
    let state = state(&fleet);
    let owned = resolve_key::<TextEngine>(&state, tenant_in(half, true).as_str())
        .await
        .unwrap();
    assert_eq!(owned.epoch().map(crate::lock::FencingToken::get), Some(1));
    let error = resolve_key::<TextEngine>(&state, tenant_in(half, false).as_str())
        .await
        .unwrap_err();
    assert!(matches!(error, FleetError::NotOwner { .. }), "{error}");
}

/// Records the epoch each hook sees.
#[derive(Default)]
struct EpochHook {
    seen: Mutex<Vec<(&'static str, Option<u64>)>>,
}

impl FleetLifecycle<TextEngine> for EpochHook {
    fn on_open(&self, db: &FleetDatabase<TextEngine>) -> Result<(), String> {
        let epoch = db.epoch().map(crate::lock::FencingToken::get);
        self.seen.lock().unwrap().push(("open", epoch));
        Ok(())
    }

    fn on_close(&self, db: &FleetDatabase<TextEngine>) {
        let epoch = db.epoch().map(crate::lock::FencingToken::get);
        self.seen.lock().unwrap().push(("close", epoch));
    }
}

#[tokio::test]
async fn lifecycle_hooks_see_the_epoch_the_database_opened_under() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let hook = Arc::new(EpochHook::default());
    fleet.add_lifecycle(Arc::clone(&hook) as Arc<dyn FleetLifecycle<TextEngine>>);
    let clock = crate::time::TickingClock::starting_at(
        chrono::DateTime::from_timestamp_millis(1_000_000).unwrap(),
    );
    let store = Arc::new(MemoryOwnership::new(Arc::new(clock.clone())));
    let a = host(&store, "host-a", SlotRange::ALL);
    assert_eq!(
        fleet
            .start_ownership(Arc::clone(&a))
            .await
            .unwrap()
            .claimed
            .len(),
        1
    );
    let key: FleetDbKey = fleet.key_for("acme").unwrap();
    drop(fleet.open(&key).await.unwrap());

    // Host a stops renewing. Host b takes the range at epoch 2.
    clock.advance(Duration::from_secs(31));
    let b = host(&store, "host-b", SlotRange::ALL);
    assert_eq!(b.refresh().await.claimed.len(), 1);
    assert_eq!(a.refresh().await.lost, [SlotRange::ALL]);
    assert_eq!(fleet.fence_unowned(), 1);
    eventually(|| hook.seen.lock().unwrap().len() == 2).await;

    // Host b lets go. Host a claims again, at epoch 3.
    b.release_all().await;
    assert_eq!(a.refresh().await.claimed.len(), 1);
    eventually(|| fleet.stats().draining == 0).await;
    drop(fleet.open(&key).await.unwrap());
    assert_eq!(
        *hook.seen.lock().unwrap(),
        [("open", Some(1)), ("close", Some(1)), ("open", Some(3))],
        "the close of a fenced database sees the old epoch"
    );
}

#[tokio::test]
async fn resolve_without_a_config_is_503() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8);
    let state = AppState::for_test();
    state.insert_extension(fleet.clone());
    let mut parts = Request::builder().uri("/").body(()).unwrap().into_parts().0;
    let error = resolve::<TextEngine>(&mut parts, &state).await.unwrap_err();
    assert_eq!(
        error.http_status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{error}"
    );
}

#[tokio::test]
async fn ownership_store_is_the_store_of_the_builder() {
    let state = AppState::for_test();
    assert!(ownership_store(&state).is_none(), "no store, no pool");
    let builder = crate::app::app()
        .fleet_ownership_store(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    crate::app::run_state_initializers(builder.state_initializers, &state);
    let found = ownership_store(&state).expect("installed");
    found.ensure_schema().await.unwrap();
    let grant = found
        .claim(
            SlotRange::ALL,
            &OwnerId::new("host-a").unwrap(),
            Duration::from_secs(30),
            0,
        )
        .await
        .unwrap();
    assert_eq!(grant.epoch.get(), 1);
    assert_eq!(found.rows().await.unwrap().len(), 1);
}
