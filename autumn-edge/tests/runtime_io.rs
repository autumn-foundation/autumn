//! The guest dialogue loop, driven end to end over in-memory pipes.
//!
//! `serve_io` is the whole capsule: everything `serve` adds is the two calls
//! that hand it real stdin and stdout. Driving it through a `Cursor` and a
//! shared `Vec<u8>` makes every wire-level behaviour — the KV round trip, each
//! fallthrough reason, the version gate, the sentinel — testable natively, with
//! no wasm target and no host in the loop.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex, PoisonError};

use autumn_edge::prelude::*;
use autumn_edge::route::EdgeState;
use autumn_edge::wire::{
    EdgeRequest, EdgeResponse, FALLTHROUGH_SENTINEL, FallthroughReason, GuestFrame, HostFrame,
    WIRE_VERSION,
};
use autumn_edge::{edge_get, serve_io};

// ── fixture routes ───────────────────────────────────────────────────

async fn greet(
    Path(name): Path<String>,
) -> (http::StatusCode, [(&'static str, &'static str); 1], String) {
    (
        http::StatusCode::OK,
        [("x-edge-lane", "edge")],
        format!("hello {name}"),
    )
}

async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
    cache
        .get_string(&key)
        .unwrap_or_else(|| "not cached here".to_owned())
}

/// A handler that declines explicitly by setting the sentinel itself.
async fn optout() -> (
    http::StatusCode,
    [(&'static str, &'static str); 1],
    &'static str,
) {
    (
        http::StatusCode::OK,
        [(FALLTHROUGH_SENTINEL, "missing_capability")],
        "this body must never reach the wire",
    )
}

/// A handler that sets the sentinel with a value no version knows.
async fn garbled_optout() -> (
    http::StatusCode,
    [(&'static str, &'static str); 1],
    &'static str,
) {
    (
        http::StatusCode::OK,
        [(FALLTHROUGH_SENTINEL, "not-a-known-reason")],
        "nor this one",
    )
}

/// A handler whose response header value is valid HTTP but not valid UTF-8 —
/// it cannot cross the JSON wire byte-faithfully, so the runtime must decline
/// rather than serve a lossily rewritten value the origin would never send.
async fn latin1_header() -> (http::HeaderMap, &'static str) {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-latin1",
        http::HeaderValue::from_bytes(&[0xE9, 0xE8]).expect("opaque bytes are a valid value"),
    );
    (headers, "body")
}

fn routes() -> Vec<EdgeRoute> {
    vec![
        EdgeRoute {
            method: http::Method::GET,
            path: "/greet/{name}",
            handler: edge_get(greet),
            name: "greet",
            needs: &[],
        },
        EdgeRoute {
            method: http::Method::GET,
            path: "/latin1",
            handler: edge_get(latin1_header),
            name: "latin1_header",
            needs: &[],
        },
        EdgeRoute {
            method: http::Method::GET,
            path: "/note/{key}",
            handler: edge_get(note),
            name: "note",
            needs: &[EdgeCapability::Kv],
        },
        EdgeRoute {
            method: http::Method::GET,
            path: "/optout",
            handler: edge_get(optout),
            name: "optout",
            needs: &[],
        },
        EdgeRoute {
            method: http::Method::GET,
            path: "/garbled",
            handler: edge_get(garbled_optout),
            name: "garbled_optout",
            needs: &[],
        },
    ]
}

// ── in-memory transport ──────────────────────────────────────────────

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run the capsule against a scripted stdin, returning every frame it emitted.
fn drive(input: &[String]) -> Vec<GuestFrame> {
    let script = input.concat();
    let writer = SharedWriter::default();
    serve_io(routes(), Cursor::new(script.into_bytes()), writer.clone())
        .expect("the in-memory transport never fails");

    let raw = writer.take();
    let text = String::from_utf8(raw).expect("capsule emits UTF-8 NDJSON");
    text.lines()
        .map(|line| serde_json::from_str::<GuestFrame>(line).expect("a well-formed guest frame"))
        .collect()
}

fn request_line(request: EdgeRequest, capabilities: &[EdgeCapability]) -> String {
    let frame = request.into_host_frame(capabilities);
    format!("{}\n", serde_json::to_string(&frame).expect("serializes"))
}

fn kv_value_line(value: Option<&[u8]>) -> String {
    let frame = HostFrame::KvValue {
        value: value.map(<[u8]>::to_vec),
    };
    format!("{}\n", serde_json::to_string(&frame).expect("serializes"))
}

fn post(uri: &str) -> EdgeRequest {
    EdgeRequest {
        method: "POST".to_owned(),
        uri: uri.to_owned(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

#[track_caller]
fn expect_response(frame: &GuestFrame) -> &EdgeResponse {
    match frame {
        GuestFrame::Response(response) => response,
        other => panic!("expected a response frame, got {other:?}"),
    }
}

#[track_caller]
fn expect_fallthrough(frame: &GuestFrame) -> (FallthroughReason, &str) {
    match frame {
        GuestFrame::Fallthrough { reason, detail } => (*reason, detail.as_str()),
        other => panic!("expected a fallthrough frame, got {other:?}"),
    }
}

fn header<'a>(response: &'a EdgeResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| value.as_str())
}

// ── the dialogue ─────────────────────────────────────────────────────

#[test]
fn a_matched_get_is_served() {
    let frames = drive(&[request_line(EdgeRequest::get("/greet/ada"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let response = expect_response(&frames[0]);
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello ada");
    assert_eq!(header(response, "x-edge-lane"), Some("edge"));
}

#[test]
fn head_is_served_by_the_get_router_with_an_empty_body_and_gets_content_length() {
    let get_frames = drive(&[request_line(EdgeRequest::get("/greet/ada"), &[])]);
    let mut head_request = EdgeRequest::get("/greet/ada");
    head_request.method = "HEAD".to_owned();
    let head_frames = drive(&[request_line(head_request, &[])]);

    let get_response = expect_response(&get_frames[0]);
    let head_response = expect_response(&head_frames[0]);
    assert_eq!(head_response.status, 200);
    assert!(
        head_response.body.is_empty(),
        "HEAD must carry no body: {head_response:?}"
    );
    // axum answers HEAD from the GET handler, advertising the GET body's
    // length — the same on every target, since it is the same axum.
    assert_eq!(
        header(head_response, "content-length"),
        header(get_response, "content-length"),
        "HEAD must advertise the GET body's content-length"
    );
    assert_eq!(header(head_response, "x-edge-lane"), Some("edge"));
}

#[test]
fn a_non_utf8_response_header_value_is_a_fallthrough_not_a_lossy_rewrite() {
    let frames = drive(&[request_line(EdgeRequest::get("/latin1"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::CapsuleError);
    assert!(
        detail.contains("x-latin1") && detail.contains("non-UTF-8"),
        "the decline must name the offending header: {detail}"
    );
}

#[test]
fn response_headers_cross_the_wire_lowercased_and_sorted() {
    let frames = drive(&[request_line(EdgeRequest::get("/greet/ada"), &[])]);
    let response = expect_response(&frames[0]);

    let names: Vec<&str> = response
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "{names:?}");
    assert!(
        names.iter().all(|name| *name == name.to_ascii_lowercase()),
        "{names:?}"
    );
}

#[test]
fn the_loop_serves_every_request_until_eof() {
    let frames = drive(&[
        request_line(EdgeRequest::get("/greet/ada"), &[]),
        request_line(EdgeRequest::get("/greet/grace"), &[]),
    ]);

    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(expect_response(&frames[0]).body, b"hello ada");
    assert_eq!(expect_response(&frames[1]).body, b"hello grace");
}

#[test]
fn a_kv_hit_round_trips_through_the_host() {
    let frames = drive(&[
        request_line(EdgeRequest::get("/note/greeting"), &[EdgeCapability::Kv]),
        kv_value_line(Some(b"hello from the host")),
    ]);

    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(
        frames[0],
        GuestFrame::KvGet {
            key: "greeting".to_owned()
        }
    );
    assert_eq!(expect_response(&frames[1]).body, b"hello from the host");
}

#[test]
fn a_kv_miss_renders_the_handler_fallback() {
    let frames = drive(&[
        request_line(EdgeRequest::get("/note/absent"), &[EdgeCapability::Kv]),
        kv_value_line(None),
    ]);

    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(
        frames[0],
        GuestFrame::KvGet {
            key: "absent".to_owned()
        }
    );
    assert_eq!(expect_response(&frames[1]).body, b"not cached here");
}

// ── the four ways to decline ─────────────────────────────────────────

#[test]
fn a_write_method_falls_through() {
    let frames = drive(&[request_line(post("/greet/ada"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::MethodNotEdgeEligible);
    assert!(detail.contains("POST"), "{detail}");
}

#[test]
fn an_unknown_route_falls_through() {
    let frames = drive(&[request_line(EdgeRequest::get("/nope"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::UnknownRoute);
    assert!(detail.contains("/nope"), "{detail}");
}

#[test]
fn a_route_whose_capability_is_not_provided_falls_through_before_dispatch() {
    // No `kv_value` line is scripted: proving the handler never ran, because
    // if it had, the KV read would have blocked on an empty stdin.
    let frames = drive(&[request_line(EdgeRequest::get("/note/greeting"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::MissingCapability);
    assert!(detail.contains("kv"), "{detail}");
}

#[test]
fn a_wire_version_mismatch_falls_through() {
    let mut frame = EdgeRequest::get("/greet/ada").into_host_frame(&[]);
    if let HostFrame::Request { wire_version, .. } = &mut frame {
        *wire_version = WIRE_VERSION + 1;
    }
    let line = format!("{}\n", serde_json::to_string(&frame).expect("serializes"));

    let frames = drive(&[line]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::CapsuleError);
    assert!(detail.contains("wire version"), "{detail}");
}

#[test]
fn a_malformed_frame_falls_through_and_the_loop_survives_it() {
    let frames = drive(&[
        "{ this is not json\n".to_owned(),
        request_line(EdgeRequest::get("/greet/ada"), &[]),
    ]);

    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(
        expect_fallthrough(&frames[0]).0,
        FallthroughReason::CapsuleError
    );
    assert_eq!(expect_response(&frames[1]).body, b"hello ada");
}

// ── the sentinel never leaks ─────────────────────────────────────────

#[test]
fn a_handler_set_sentinel_becomes_a_fallthrough() {
    let frames = drive(&[request_line(EdgeRequest::get("/optout"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, _) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::MissingCapability);
}

#[test]
fn an_unparseable_sentinel_value_becomes_a_capsule_error() {
    let frames = drive(&[request_line(EdgeRequest::get("/garbled"), &[])]);

    assert_eq!(frames.len(), 1, "{frames:?}");
    let (reason, detail) = expect_fallthrough(&frames[0]);
    assert_eq!(reason, FallthroughReason::CapsuleError);
    assert!(detail.contains("not-a-known-reason"), "{detail}");
}

#[test]
fn no_served_response_ever_carries_the_sentinel() {
    let frames = drive(&[
        request_line(EdgeRequest::get("/greet/ada"), &[]),
        request_line(EdgeRequest::get("/optout"), &[]),
        request_line(EdgeRequest::get("/garbled"), &[]),
        request_line(EdgeRequest::get("/nope"), &[]),
    ]);

    for frame in &frames {
        if let GuestFrame::Response(response) = frame {
            assert_eq!(header(response, FALLTHROUGH_SENTINEL), None, "{response:?}");
        }
    }
}

#[test]
fn a_declined_response_body_never_reaches_the_wire() {
    let frames = drive(&[request_line(EdgeRequest::get("/optout"), &[])]);
    let rendered = serde_json::to_string(&frames).expect("serializes");
    assert!(
        !rendered.contains("this body must never reach the wire"),
        "{rendered}"
    );
}

// ── credential hygiene ───────────────────────────────────────────────

#[test]
fn the_guest_strips_credentials_the_host_forgot_to() {
    // A hostile or buggy host that skips its own strip must not be able to
    // hand a capsule a session cookie.
    let frame = HostFrame::Request {
        wire_version: WIRE_VERSION,
        provided_capabilities: Vec::new(),
        request: EdgeRequest::get("/echo-headers")
            .with_header("cookie", "sid=secret")
            .with_header("accept", "text/html"),
    };
    let line = format!("{}\n", serde_json::to_string(&frame).expect("serializes"));

    let frames = drive(&[line]);
    let rendered = serde_json::to_string(&frames).expect("serializes");
    assert!(!rendered.contains("sid=secret"), "{rendered}");
}

// ── the extractor is state-agnostic ──────────────────────────────────

#[derive(Clone)]
struct AppState {
    label: &'static str,
}

async fn stateless(cache: EdgeCache) -> String {
    cache.get_string("k").unwrap_or_else(|| "miss".to_owned())
}

async fn stateful(
    axum::extract::State(state): axum::extract::State<AppState>,
    cache: EdgeCache,
) -> String {
    format!(
        "{}:{}",
        state.label,
        cache.get_string("k").unwrap_or_else(|| "miss".to_owned())
    )
}

async fn edge_stated(cache: EdgeCache) -> String {
    cache.get_string("k").unwrap_or_else(|| "miss".to_owned())
}

fn call(router: axum::Router<()>, uri: &str, cache: Option<EdgeCache>) -> (u16, String) {
    use tower::ServiceExt as _;

    let mut request = http::Request::builder()
        .uri(uri)
        .body(axum::body::Body::empty())
        .expect("request");
    if let Some(cache) = cache {
        request.extensions_mut().insert(cache);
    }

    futures::executor::block_on(async {
        let response = router.oneshot(request).await.expect("infallible");
        let status = response.status().as_u16();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    })
}

#[test]
fn edge_cache_extracts_from_any_router_state() {
    let cache = EdgeCache::new(Arc::new(autumn_edge::InMemoryEdgeKv::new().with("k", "v")));

    let unit: axum::Router<()> = axum::Router::new().route("/a", axum::routing::get(stateless));
    assert_eq!(call(unit, "/a", Some(cache.clone())), (200, "v".to_owned()));

    let stated: axum::Router<()> = axum::Router::new()
        .route("/b", axum::routing::get(stateful))
        .with_state(AppState { label: "origin" });
    assert_eq!(
        call(stated, "/b", Some(cache.clone())),
        (200, "origin:v".to_owned())
    );

    let edge: axum::Router<()> = axum::Router::new()
        .route("/c", axum::routing::get(edge_stated))
        .with_state(EdgeState);
    assert_eq!(call(edge, "/c", Some(cache)), (200, "v".to_owned()));
}

#[test]
fn edge_cache_without_an_installed_store_is_an_actionable_rejection() {
    let unit: axum::Router<()> = axum::Router::new().route("/a", axum::routing::get(stateless));
    let (status, body) = call(unit, "/a", None);

    assert_eq!(status, 500);
    assert!(body.contains("with_edge_kv"), "{body}");
}
