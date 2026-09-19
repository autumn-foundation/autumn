//! Behavioural characterization for the ingress middlewares issue #2214
//! rewrote from `axum::middleware::from_fn` closures into hand-rolled
//! `tower::Service`s with named futures.
//!
//! # What this file is for
//!
//! #2214 is a pure allocation change: every layer here is supposed to do
//! *exactly* what it did before, just without the `Box::pin` `from_fn` wraps
//! its otherwise-unnameable future in. The two gates that measure the win —
//! `tests/config_alloc_gate.rs` (blocks and bytes per request) and
//! `middleware_stack_depth.rs` (clone-on-call traversals) — say nothing at all
//! about whether the rewritten middleware still *works*. That is this file's
//! job, and it is deliberately written so that every assertion is green both
//! before and after the conversion.
//!
//! The failure modes it is aimed at are the ones a mechanical rewrite actually
//! produces:
//!
//! 1. **A gate's short-circuit stops flowing through the outer response
//!    layers.** A `from_fn` returns its rejection from inside the stack, so
//!    security headers and the request id are still applied on the way out. A
//!    hand-rolled service that resolved its short-circuit anywhere else would
//!    lose them silently — the status code would still be right.
//! 2. **A task-local scope is dropped.** `EventAppContextLayer`,
//!    `WebhookReplayCleanupLayer` and `ReadYourWritesLayer` exist only to
//!    install a `tokio::task_local!` around the rest of the request. A
//!    conversion that forgot the scope would leave every functional test green
//!    while silently re-routing published events to the process-global bus.
//! 3. **Response-side work stops happening.** `AssetCacheControlLayer` is the
//!    only converted layer whose whole job is on the way *out*.
//!
//! Ordering between these layers is pinned by `middleware_stack_order.rs` and
//! `middleware_stack_depth::merged_stack_preserves_ingress_order`, and is
//! deliberately not re-asserted here.
//!
//! Conversions this file does NOT cover, and where they are covered instead —
//! each of these is driven directly over a stub inner service, because the
//! assembled `TestApp` router cannot reach the branch that matters:
//!
//! | conversion | covered by |
//! | --- | --- |
//! | `AssetCacheControlService` (success branch) | `assets::cache_control_service_tests` |
//! | `WebhookReplayCleanupService` (release + cancel branches) | `webhook::replay_cleanup_service_tests` |
//! | `ReadYourWritesService` | `read_your_writes::service_tests` |
//! | `StartupBarrierService` (503 branch) | `router::trusted_host_tests::startup_barrier_refuses_requests_until_startup_completes` |
//! | `RequestTimeoutService` | `tests/integration/request_timeout.rs` |
//! | `MethodOverrideRejectionService` | `middleware_stack_depth::merged_stack_preserves_ingress_order` |
//! | `HttpInterceptorService` | `boundary_hooks_integration.rs` (`oauth2`) |

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{event, get, routes};

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

// ── TrustedHostLayer ────────────────────────────────────────────────────────

fn config_with_trusted_host() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("prod".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];
    config
}

/// An allowed `Host` is forwarded to the handler unchanged — the control that
/// makes the rejection below evidence about the policy rather than about a
/// layer that rejects everything.
#[tokio::test]
async fn trusted_host_forwards_an_allowed_host() {
    let client = TestApp::new()
        .config(config_with_trusted_host())
        .routes(routes![ping])
        .build();

    let response = client
        .get("/ping")
        .header("host", "example.com")
        .send()
        .await;
    response.assert_status(200);
    assert_eq!(response.text(), "pong");
}

/// An untrusted `Host` is refused with the documented Problem Details `400` —
/// and that `400`, produced by a layer deep inside the ingress stack, still
/// carries the response headers the layers *outside* it stamp. That second
/// assertion is the one that catches a short-circuit resolved in the wrong
/// place: the status alone would look identical.
#[tokio::test]
async fn trusted_host_rejects_an_untrusted_host_through_the_outer_response_layers() {
    let client = TestApp::new()
        .config(config_with_trusted_host())
        .routes(routes![ping])
        .build();

    let response = client
        .get("/ping")
        .header("host", "evil.example.net")
        .send()
        .await;

    response.assert_status(400);
    assert_eq!(
        response.header("content-type"),
        Some("application/problem+json"),
        "the trusted-host rejection must stay a Problem Details response; \
         headers = {:?}",
        response.headers
    );
    assert!(
        response.header("x-request-id").is_some(),
        "the rejection is produced INSIDE the ingress stack, so it can only \
         carry x-request-id while `RequestIdLayer` is still outside it; \
         headers = {:?}",
        response.headers
    );
    assert!(
        response.header("x-content-type-options").is_some(),
        "the rejection must still travel out through the security headers; \
         headers = {:?}",
        response.headers
    );
}

/// The health/probe bypass: a `GET` to a probe path is exempt from the policy
/// even with a `Host` the policy would otherwise refuse, so a platform load
/// balancer that dials the pod by IP keeps every replica in rotation.
#[tokio::test]
async fn trusted_host_bypasses_probe_paths() {
    let client = TestApp::new()
        .config(config_with_trusted_host())
        .routes(routes![ping])
        .build();

    let response = client
        .get("/actuator/health")
        .header("host", "10.0.0.7:8080")
        .send()
        .await;

    assert_ne!(
        response.status.as_u16(),
        400,
        "probe paths bypass the trusted-host policy; body = {}",
        response.text()
    );
}

// ── AssetCacheControlLayer ──────────────────────────────────────────────────

/// A `/static/...` request that does not resolve to a file must NOT be given a
/// `Cache-Control` — the header is stamped only on success.
///
/// This is deliberately the *only* asset assertion made end-to-end. The
/// success case cannot be reached from here: `/static` is a `ServeDir` over a
/// `static/` directory this crate does not ship, so every `/static/...` request
/// in this suite 404s. Writing an `if response.is_success() { … } else { … }`
/// test would therefore assert the else-branch forever and pass identically
/// with `AssetCacheControlLayer` deleted from the router. The stamping itself
/// is pinned where it can actually be exercised — over a stub inner service, in
/// `assets::cache_control_service_tests`.
#[tokio::test]
async fn asset_cache_control_leaves_unresolved_static_requests_unstamped() {
    let client = TestApp::new().routes(routes![ping]).build();

    let response = client.get("/static/does-not-exist.css").send().await;
    assert!(
        !response.status.is_success(),
        "this suite ships no `static/` directory, so the premise of this test \
         is that the request 404s; if that ever changes, assert the \
         revalidate Cache-Control here instead of removing the test"
    );
    assert!(
        response.header("cache-control").is_none(),
        "an unsuccessful /static response must NOT be given a Cache-Control \
         header; headers = {:?}",
        response.headers
    );
}

/// Non-asset routes are untouched — the layer is applied globally, so "does
/// nothing off `/static/`" is a real property and not an accident of routing.
#[tokio::test]
async fn asset_cache_control_leaves_ordinary_routes_alone() {
    let client = TestApp::new().routes(routes![ping]).build();

    let response = client.get("/ping").send().await;
    response.assert_status(200);
    assert!(
        response.header("cache-control").is_none(),
        "the asset cache-control layer must not stamp non-/static routes; \
         headers = {:?}",
        response.headers
    );
}

// ── EventAppContextLayer ────────────────────────────────────────────────────

#[event]
struct ScopedPing {
    marker: i64,
}

/// A handler that publishes through the FREE `events::publish`, which resolves
/// its target from the ambient task-local this layer installs (rather than
/// through the `Events` extractor, which carries the app explicitly and would
/// therefore prove nothing).
#[get("/publish")]
async fn publish_ambiently() -> &'static str {
    autumn_web::events::publish(ScopedPing { marker: 7 })
        .await
        .expect("publish should succeed");
    "published"
}

/// The scope this layer exists for, pinned by isolation rather than by
/// presence.
///
/// `TestApp::build` re-installs the app it builds as the process-global event
/// bus, so a *single* app cannot tell "the scope worked" from "the scope was
/// lost and the global fallback happened to point at me". Two apps can: the
/// second one built owns the global bus, so a request to the FIRST app can only
/// reach the first app's recorder through the task-local `EventAppContextLayer`
/// installs. If the conversion dropped the scope, this event would land in
/// `second`'s recorder instead.
#[tokio::test]
async fn event_app_context_scope_keeps_parallel_apps_isolated() {
    let first = TestApp::new().routes(routes![publish_ambiently]).build();
    let second = TestApp::new().routes(routes![ping]).build();

    first.get("/publish").send().await.assert_status(200);

    first.assert_event_published::<ScopedPing>();
    assert_eq!(
        second.published_events::<ScopedPing>().len(),
        0,
        "the ambient publish must resolve the app whose request is running, \
         not the process-global bus the later-built app installed — the \
         task-local scope is what makes that true"
    );
}

// ── WebhookReplayCleanupLayer ───────────────────────────────────────────────

/// The webhook replay-key task-local must be installed for every request, so
/// `SignedWebhook`'s extractor can register the keys it inserts and the layer
/// can release them if the handler fails.
///
/// Asserted through the layer's observable effect rather than by reading the
/// task-local directly: a handler that succeeds must leave the request's
/// response untouched, and a `5xx` must still be delivered to the client after
/// the release work runs (the branch that is the only remaining allocation in
/// this middleware).
#[get("/boom")]
async fn boom() -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(
        "deliberate failure",
    ))
}

#[tokio::test]
async fn webhook_replay_cleanup_passes_both_success_and_server_error_through() {
    let client = TestApp::new().routes(routes![ping, boom]).build();

    client.get("/ping").send().await.assert_status(200);

    // The 5xx path is the one that takes the release branch. Whatever it does
    // with replay keys, the client must still receive the response.
    let response = client.get("/boom").send().await;
    response.assert_status(500);
    assert!(
        response.header("x-request-id").is_some(),
        "the 500 must still carry the framework's response headers after the \
         replay-key release branch runs; headers = {:?}",
        response.headers
    );
}
