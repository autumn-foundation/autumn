//! Native (origin-side) proof of the edge seam (issue #1790).
//!
//! The edge capsule's promise is that *one* handler source serves identically
//! from the origin binary and from the wasm artifact. The wasm half of that
//! claim is proven by the conformance suite, which needs the `wasm32-wasip1`
//! target; this module proves the half that needs no toolchain at all:
//!
//! - a `#[edge]` handler is still an ordinary Autumn route at the origin;
//! - [`CacheEdgeKv`] projects the app's own `Cache` onto the `EdgeKv` seam, so
//!   what the origin wrote with `insert_cached` is what an edge handler reads;
//! - `AppBuilder::with_edge_kv` installs that seam app-wide;
//! - without it, the extractor declines with the fallthrough sentinel instead
//!   of rendering a page built on a seam that is not there;
//! - the origin router and `build_edge_router` return the same bytes for the
//!   same request — the native-to-native half of the parity contract.

use std::sync::Arc;

use autumn_web::cache::{Cache, MokaCache, insert_cached};
use autumn_web::edge::{
    EdgeCache, EdgeKv, FALLTHROUGH_SENTINEL, InMemoryEdgeKv, build_edge_router,
};
use autumn_web::edge_support::CacheEdgeKv;
use autumn_web::test::TestApp;
use autumn_web::{edge, edge_routes, get, routes};
use axum::extract::Path;
use tower::ServiceExt as _;

// ── The app under test ───────────────────────────────────────────────────
//
// Two handlers, marked exactly as an app author would mark them. Both are
// mounted in the origin router by `routes![]` and in the edge lane by
// `edge_routes![]` — the same functions, registered twice.

#[get("/edge/greet/{name}")]
#[edge]
async fn greet(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

#[get("/edge/note/{key}")]
#[edge(needs(kv))]
async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
    cache
        .get_string(&key)
        .unwrap_or_else(|| "not cached here".to_owned())
}

/// A handler that would betray a lane divergence if credentials were visible:
/// the wire strips them before the capsule, and the macro's native mount must
/// strip the same set.
#[get("/edge/credential-blind")]
#[edge]
async fn credential_blind(headers: axum::http::HeaderMap) -> String {
    let saw: Vec<&str> = ["cookie", "authorization", "proxy-authorization"]
        .into_iter()
        .filter(|name| headers.contains_key(*name))
        .collect();
    if saw.is_empty() {
        "no credentials visible".to_owned()
    } else {
        format!("SAW: {}", saw.join(","))
    }
}

/// A cache pre-filled the way an origin app fills it: through the normal
/// serde-aware `insert_cached` path, with no knowledge of the edge lane.
fn seeded_cache() -> Arc<dyn Cache> {
    let cache = MokaCache::new(16, None);
    insert_cached(&cache, "welcome", b"from the origin".to_vec(), None);
    Arc::new(cache)
}

fn seeded_kv() -> Arc<dyn EdgeKv> {
    Arc::new(CacheEdgeKv::new(seeded_cache()))
}

// ── 1. The adapter reads what the origin wrote ───────────────────────────

#[test]
fn cache_edge_kv_reads_what_insert_cached_wrote() {
    let kv = seeded_kv();

    assert_eq!(kv.get("welcome"), Some(b"from the origin".to_vec()));
    assert_eq!(kv.get("never-written"), None, "a miss is a legal answer");
}

#[test]
fn cache_edge_kv_misses_rather_than_panicking_on_a_foreign_value() {
    // A key holding some *other* type is not an edge value. The seam reports a
    // miss (the one legal degraded answer) rather than erroring the request.
    let cache = MokaCache::new(16, None);
    insert_cached(&cache, "count", 42_u32, None);
    let kv = CacheEdgeKv::new(Arc::new(cache));

    assert_eq!(kv.get("count"), None);
}

// ── 2. `with_edge_kv` installs the seam app-wide ─────────────────────────

#[test]
fn with_edge_kv_registers_the_edge_cache_extension_layer() {
    let builder = autumn_web::app().routes(routes![greet, note]);
    assert!(
        !builder.has_layer::<axum::Extension<EdgeCache>>(),
        "no seam is installed until the author asks for one"
    );

    let builder = builder.with_edge_kv(seeded_kv());
    assert!(
        builder.has_layer::<axum::Extension<EdgeCache>>(),
        "with_edge_kv must install the EdgeCache extension as an app-wide layer"
    );
}

// ── 3. An `#[edge]` route is an ordinary origin route ────────────────────

#[tokio::test]
async fn an_edge_route_serves_natively_through_the_origin_router() {
    let client = TestApp::new().routes(routes![greet, note]).build();

    client
        .get("/edge/greet/ada")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("Hello, ada!");
}

#[tokio::test]
async fn the_origin_mount_strips_credentials_exactly_like_the_wire_does() {
    let client = TestApp::new()
        .routes(routes![greet, note, credential_blind])
        .build();

    client
        .get("/edge/credential-blind")
        .header("cookie", "session=secret")
        .header("authorization", "Bearer secret")
        .header("proxy-authorization", "Basic secret")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("no credentials visible");
}

#[tokio::test]
async fn the_injected_seam_is_visible_to_a_native_handler() {
    let client = TestApp::new()
        .routes(routes![greet, note])
        .layer(EdgeCache::layer(seeded_kv()))
        .build();

    client
        .get("/edge/note/welcome")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("from the origin");

    client
        .get("/edge/note/absent")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("not cached here");
}

// ── 4. Without the seam: a sentinel-carrying decline, not a broken page ──

#[tokio::test]
async fn an_uninjected_seam_declines_with_the_fallthrough_sentinel() {
    let client = TestApp::new().routes(routes![greet, note]).build();

    let response = client.get("/edge/note/welcome").send().await;
    response.assert_status(500);

    assert_eq!(
        response.header(FALLTHROUGH_SENTINEL),
        Some("missing_capability"),
        "the sentinel is what turns this into a fallthrough at the edge"
    );
    assert!(
        response.text().contains("with_edge_kv"),
        "the origin case must name the fix: {}",
        response.text()
    );
}

// ── 5. Native-to-native parity ───────────────────────────────────────────

/// Drive the edge lane router the capsule would build, with the seam injected
/// the way the capsule runtime injects it (a request extension).
async fn edge_lane_response(uri: &str, kv: Option<Arc<dyn EdgeKv>>) -> (u16, String) {
    let mut request = http::Request::builder().uri(uri);
    if let Some(kv) = kv {
        request = request.extension(EdgeCache::new(kv));
    }
    let request = request
        .body(axum::body::Body::empty())
        .expect("well-formed request");

    let response = build_edge_router(edge_routes![greet, note])
        .oneshot(request)
        .await
        .expect("edge dispatch is infallible");
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("edge body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn origin_and_edge_lanes_agree_byte_for_byte() {
    let client = TestApp::new()
        .routes(routes![greet, note])
        .layer(EdgeCache::layer(seeded_kv()))
        .build();

    for uri in ["/edge/greet/ada", "/edge/note/welcome", "/edge/note/absent"] {
        let origin = client.get(uri).send().await;
        let (edge_status, edge_body) = edge_lane_response(uri, Some(seeded_kv())).await;

        assert_eq!(
            origin.status.as_u16(),
            edge_status,
            "status differs for {uri}"
        );
        assert_eq!(origin.text(), edge_body, "body differs for {uri}");
    }
}

#[tokio::test]
async fn the_edge_lane_declines_a_path_it_does_not_serve() {
    let (status, body) = edge_lane_response("/edge/unknown", None).await;

    assert_eq!(status, 404);
    assert!(body.contains("/edge/unknown"), "{body}");
}

#[tokio::test]
async fn an_in_memory_kv_is_interchangeable_with_the_cache_backed_one() {
    // The seam is a trait, not a cache: a handler cannot tell which store is
    // behind it. This is what lets the capsule bind the same handler to a
    // dialogue-backed reader at the edge.
    let kv: Arc<dyn EdgeKv> = Arc::new(InMemoryEdgeKv::new().with("welcome", "from the origin"));
    let (status, body) = edge_lane_response("/edge/note/welcome", Some(kv)).await;

    assert_eq!(status, 200);
    assert_eq!(body, "from the origin");
}
