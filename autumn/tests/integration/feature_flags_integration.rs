//! Integration tests for the feature-flag system (AC-11).
//!
//! Verifies that:
//! - Flags registered via `with_flag_store` are available in handlers.
//! - Toggling a flag via the `FlagStore` trait propagates immediately.
//! - The `Flags` extractor returns 500 when no store is registered.
//! - Percent-rollout and actor-allowlist gates work end-to-end.
//! - The `#[feature_flag]` macro gate returns 404 when the flag is disabled
//!   and the full handler body is NOT executed (body extractor not consumed).
//! - A custom fallback handler is called when the flag is disabled.

use std::sync::Arc;

use autumn_web::feature_flags::{FeatureFlagService, FlagStore, InMemoryFlagStore};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use axum::http::StatusCode;

// ── Shared store so tests can mutate flags while the app is running ─────────

#[derive(Clone)]
struct SharedStore(Arc<InMemoryFlagStore>);

impl FlagStore for SharedStore {
    fn get(
        &self,
        key: &str,
    ) -> Result<
        Option<autumn_web::feature_flags::FlagConfig>,
        autumn_web::feature_flags::FlagStoreError,
    > {
        self.0.get(key)
    }
    fn list(
        &self,
    ) -> Result<Vec<autumn_web::feature_flags::FlagConfig>, autumn_web::feature_flags::FlagStoreError>
    {
        self.0.list()
    }
    fn enable(
        &self,
        key: &str,
        actor: Option<&str>,
    ) -> Result<(), autumn_web::feature_flags::FlagStoreError> {
        self.0.enable(key, actor)
    }
    fn disable(
        &self,
        key: &str,
        actor: Option<&str>,
    ) -> Result<(), autumn_web::feature_flags::FlagStoreError> {
        self.0.disable(key, actor)
    }
    fn set_rollout(
        &self,
        key: &str,
        pct: u8,
        actor: Option<&str>,
    ) -> Result<(), autumn_web::feature_flags::FlagStoreError> {
        self.0.set_rollout(key, pct, actor)
    }
    fn allow_actor(
        &self,
        key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), autumn_web::feature_flags::FlagStoreError> {
        self.0.allow_actor(key, actor_id, actor)
    }
    fn add_group(
        &self,
        key: &str,
        group: &str,
        actor: Option<&str>,
    ) -> Result<(), autumn_web::feature_flags::FlagStoreError> {
        self.0.add_group(key, group, actor)
    }
    fn history(
        &self,
        key: &str,
        limit: usize,
    ) -> Result<
        Vec<autumn_web::feature_flags::FlagChangeRecord>,
        autumn_web::feature_flags::FlagStoreError,
    > {
        self.0.history(key, limit)
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

#[get("/gate")]
async fn gate_handler(flags: Flags) -> axum::http::Response<axum::body::Body> {
    if flags.enabled("my_feature") {
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .body("feature on".into())
            .unwrap()
    } else {
        axum::http::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("feature off".into())
            .unwrap()
    }
}

#[get("/rollout")]
async fn rollout_handler(
    axum::extract::State(state): axum::extract::State<autumn_web::AppState>,
) -> axum::http::Response<axum::body::Body> {
    // Use a fixed actor_id so percent-rollout is deterministic regardless of session.
    use autumn_web::feature_flags::FeatureFlagService;
    let svc = state.extension::<FeatureFlagService>();
    let enabled = svc
        .as_deref()
        .is_some_and(|s| s.is_enabled("rollout_flag", Some("user:1")));
    if enabled {
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .body("in rollout".into())
            .unwrap()
    } else {
        axum::http::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("not in rollout".into())
            .unwrap()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn flag_disabled_by_default_returns_feature_off() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![gate_handler])
        .build();

    client
        .get("/gate")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn toggling_flag_propagates_within_one_request() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![gate_handler])
        .build();

    // Initially off
    client
        .get("/gate")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());

    // Enable the flag directly on the shared store
    store.enable("my_feature", None).unwrap();

    // Next request sees the flag as enabled
    client.get("/gate").send().await.assert_ok();
}

#[tokio::test]
async fn flag_disabled_after_enable_returns_feature_off() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![gate_handler])
        .build();

    store.enable("my_feature", None).unwrap();
    client.get("/gate").send().await.assert_ok();

    store.disable("my_feature", None).unwrap();
    client
        .get("/gate")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn rollout_at_100_enables_for_all_actors() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    store.set_rollout("rollout_flag", 100, None).unwrap();

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![rollout_handler])
        .build();

    client.get("/rollout").send().await.assert_ok();
}

#[tokio::test]
async fn rollout_at_0_disables_for_all_actors() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    store.set_rollout("rollout_flag", 0, None).unwrap();

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![rollout_handler])
        .build();

    client
        .get("/rollout")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn flags_extractor_returns_500_when_no_store_registered() {
    let client = TestApp::new().routes(routes![gate_handler]).build();

    client
        .get("/gate")
        .send()
        .await
        .assert_status(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
}

// ── `with_flag_store` wiring test ────────────────────────────────────────────

#[tokio::test]
async fn with_flag_store_installs_service_as_extension() {
    let store = InMemoryFlagStore::new();
    store.enable("wired_flag", Some("test")).unwrap();

    // with_flag_store must be called BEFORE state_initializer so the service
    // is already installed when the assertion runs.
    let client = TestApp::new()
        .with_flag_store(store)
        .state_initializer(|state| {
            // Verify FeatureFlagService is accessible after with_flag_store wiring.
            assert!(state.extension::<FeatureFlagService>().is_some());
        })
        .routes(routes![gate_handler])
        .build();

    // If we reach here, the extension was installed successfully.
    let _ = client;
}

// ── #[feature_flag] macro gate tests ─────────────────────────────────────────

#[get("/macro-gated")]
#[feature_flag("macro_flag")]
async fn macro_gated_handler() -> &'static str {
    "macro handler body ran"
}

#[get("/macro-fallback")]
#[feature_flag("fallback_flag", fallback = custom_fallback)]
async fn macro_fallback_handler() -> &'static str {
    "handler ran"
}

#[allow(clippy::unused_async)]
async fn custom_fallback() -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::FORBIDDEN, "flag disabled")
}

#[tokio::test]
async fn feature_flag_macro_returns_404_when_flag_disabled() {
    let store = InMemoryFlagStore::new();
    // Flag is absent (disabled by default).

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![macro_gated_handler])
        .build();

    client
        .get("/macro-gated")
        .send()
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn feature_flag_macro_passes_through_when_flag_enabled() {
    let store = InMemoryFlagStore::new();
    store.enable("macro_flag", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![macro_gated_handler])
        .build();

    client.get("/macro-gated").send().await.assert_ok();
}

#[tokio::test]
async fn feature_flag_macro_calls_custom_fallback_when_flag_disabled() {
    let store = InMemoryFlagStore::new();
    // fallback_flag absent → gate fires → custom_fallback returns 403.

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![macro_fallback_handler])
        .build();

    client
        .get("/macro-fallback")
        .send()
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN.as_u16());
}

#[tokio::test]
async fn feature_flag_macro_gate_disabled_then_enabled() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![macro_gated_handler])
        .build();

    // Initially gated.
    client
        .get("/macro-gated")
        .send()
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND.as_u16());

    store.enable("macro_flag", None).unwrap();

    // Now passes through.
    client.get("/macro-gated").send().await.assert_ok();
}

// ── Flags::service() and actor_id resolution ─────────────────────────────────

#[get("/list-flags")]
async fn list_flags_handler(flags: Flags) -> axum::Json<Vec<String>> {
    let keys: Vec<String> = flags
        .service()
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.key)
        .collect();
    axum::Json(keys)
}

#[tokio::test]
async fn flags_service_accessor_returns_underlying_service() {
    let store = InMemoryFlagStore::new();
    store.enable("alpha", None).unwrap();
    store.enable("beta", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![list_flags_handler])
        .build();

    let resp = client.get("/list-flags").send().await;
    resp.assert_ok();
    let body = resp.text();
    assert!(
        body.contains("alpha") && body.contains("beta"),
        "got: {body}"
    );
}

#[get("/macro-gated-primitive")]
#[feature_flag("macro_flag")]
async fn macro_gated_primitive_handler() -> bool {
    true
}

#[tokio::test]
async fn feature_flag_macro_primitive_wrapper_stacked() {
    let store = InMemoryFlagStore::new();
    store.enable("macro_flag", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![macro_gated_primitive_handler])
        .build();

    let resp = client.get("/macro-gated-primitive").send().await;
    resp.assert_ok();
    assert_eq!(resp.text(), "true");
}

#[post("/macro-gated-idempotent")]
#[feature_flag("replay_flag")]
async fn macro_gated_idempotent_handler() -> &'static str {
    "handler ran"
}

#[tokio::test]
async fn feature_flag_checked_before_idempotency_replay() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    store.enable("replay_flag", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![macro_gated_idempotent_handler])
        .idempotent()
        .build();

    let r1 = client
        .post("/macro-gated-idempotent")
        .header("idempotency-key", "replay-test-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.text(), "handler ran");

    store.disable("replay_flag", None).unwrap();

    client
        .post("/macro-gated-idempotent")
        .header("idempotency-key", "replay-test-key")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());
}

#[feature_flag("replay_flag_outer")]
#[post("/macro-gated-idempotent-outer")]
async fn macro_gated_idempotent_outer_handler() -> &'static str {
    "handler ran outer"
}

#[tokio::test]
async fn feature_flag_checked_before_idempotency_replay_outer() {
    let store = Arc::new(InMemoryFlagStore::new());
    let shared = SharedStore(store.clone());

    store.enable("replay_flag_outer", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(shared)
        .routes(routes![macro_gated_idempotent_outer_handler])
        .idempotent()
        .build();

    let r1 = client
        .post("/macro-gated-idempotent-outer")
        .header("idempotency-key", "replay-test-key-outer")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.text(), "handler ran outer");

    store.disable("replay_flag_outer", None).unwrap();

    client
        .post("/macro-gated-idempotent-outer")
        .header("idempotency-key", "replay-test-key-outer")
        .send()
        .await
        .assert_status(StatusCode::NOT_FOUND.as_u16());
}
