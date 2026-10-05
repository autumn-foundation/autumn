//! The reference gateway: capsule first, origin on every fallthrough (AC-3).
//!
//! Guests are hand-written WAT, so no `wasm32-wasip1` toolchain is needed.
#![cfg(feature = "host")]

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use autumn_edge::gateway::{EdgeGateway, Lane};
use autumn_edge::host::EdgeArtifact;
use autumn_edge::wire::{FallthroughReason, GuestFrame, to_line};
use autumn_edge::{EdgeResponse, InMemoryEdgeKv};
use axum::body::Body;
use futures::executor::block_on;
use http::{Request, Response, StatusCode};

/// A guest that writes `frame` to stdout and exits.
fn guest(frame: &GuestFrame) -> Arc<EdgeArtifact> {
    let line = to_line(frame).expect("frame serializes");
    let escaped = line
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    let wat = format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 64) "{escaped}")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const {len}))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 32)))))
"#,
        len = line.len()
    );
    let wasm = wat::parse_str(&wat).expect("valid WAT");
    Arc::new(EdgeArtifact::from_bytes(&wasm).expect("valid module"))
}

/// A guest that serves a fixed response.
fn serving_guest() -> Arc<EdgeArtifact> {
    guest(&GuestFrame::Response(EdgeResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: b"from the edge".to_vec(),
    }))
}

/// A guest that declines every request.
fn declining_guest(reason: FallthroughReason) -> Arc<EdgeArtifact> {
    guest(&GuestFrame::Fallthrough {
        reason,
        detail: "secret detail for logs only".into(),
    })
}

/// What the origin saw: method, URI, headers and body of each request.
type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, Vec<u8>)>>>;

/// An origin that records each request and answers `201 origin`.
fn origin(
    seen: &Seen,
) -> impl tower::Service<
    Request<Body>,
    Response = Response<Body>,
    Error = Infallible,
    Future = impl Send,
> + Clone
+ Send
+ 'static {
    let seen = Arc::clone(seen);
    tower::service_fn(move |request: Request<Body>| {
        let seen = Arc::clone(&seen);
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .expect("body reads");
            let headers = parts
                .headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("?").to_owned()))
                .collect();
            seen.lock().unwrap_or_else(PoisonError::into_inner).push((
                parts.method.to_string(),
                parts.uri.to_string(),
                headers,
                bytes.to_vec(),
            ));
            Ok::<_, Infallible>(
                Response::builder()
                    .status(StatusCode::CREATED)
                    .header("x-origin", "yes")
                    .body(Body::from("origin"))
                    .expect("valid response"),
            )
        }
    })
}

fn body_text(response: Response<Body>) -> String {
    let bytes = block_on(axum::body::to_bytes(response.into_body(), usize::MAX)).expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn lane(response: &Response<Body>) -> Lane {
    *response
        .extensions()
        .get::<Lane>()
        .expect("the gateway records the lane")
}

#[test]
fn a_served_request_is_answered_by_the_edge_and_never_reaches_the_origin() {
    let seen = Seen::default();
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen));

    let response = block_on(gateway.handle(Request::get("/greet").body(Body::empty()).unwrap()));

    assert_eq!(lane(&response), Lane::Edge);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(body_text(response), "from the edge");
    assert!(
        seen.lock().unwrap().is_empty(),
        "the origin must not be asked"
    );
}

#[test]
fn a_declined_request_is_forwarded_unchanged_and_the_origin_answer_is_returned() {
    let seen = Seen::default();
    let gateway = EdgeGateway::new(
        declining_guest(FallthroughReason::UnknownRoute),
        origin(&seen),
    );

    let request = Request::get("/nope?a=1&a=2")
        .header("cookie", "session=s1")
        .header("authorization", "Bearer t1")
        .header("accept", "text/html")
        .body(Body::empty())
        .unwrap();
    let response = block_on(gateway.handle(request));

    assert_eq!(
        lane(&response),
        Lane::Fallthrough(FallthroughReason::UnknownRoute)
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-origin"], "yes");
    let text = body_text(response);
    assert_eq!(text, "origin");
    assert!(!text.contains("secret detail"), "detail must not leak");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (method, uri, headers, _) = &seen[0];
    assert_eq!(method, "GET");
    assert_eq!(uri, "/nope?a=1&a=2");
    // The origin gets the credentials. The capsule does not see them.
    for (name, value) in [
        ("cookie", "session=s1"),
        ("authorization", "Bearer t1"),
        ("accept", "text/html"),
    ] {
        assert!(
            headers.contains(&(name.to_owned(), value.to_owned())),
            "{name} missing at the origin: {headers:?}"
        );
    }
}

#[test]
fn a_write_goes_to_the_origin_with_its_body_and_skips_the_capsule() {
    let seen = Seen::default();
    // A serving guest: if the gateway asked it, the edge would answer.
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen));

    let request = Request::post("/feedback")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ok":true}"#))
        .unwrap();
    let response = block_on(gateway.handle(request));

    assert_eq!(
        lane(&response),
        Lane::Fallthrough(FallthroughReason::MethodNotEdgeEligible)
    );
    assert_eq!(body_text(response), "origin");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[0].3, br#"{"ok":true}"#);
}

#[test]
fn every_fallthrough_reason_reaches_the_origin() {
    for reason in [
        FallthroughReason::UnknownRoute,
        FallthroughReason::MissingCapability,
        FallthroughReason::CapsuleError,
    ] {
        let seen = Seen::default();
        let gateway = EdgeGateway::new(declining_guest(reason), origin(&seen));
        let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));
        assert_eq!(lane(&response), Lane::Fallthrough(reason));
        assert_eq!(response.status(), StatusCode::CREATED, "{reason}");
        assert_eq!(seen.lock().unwrap().len(), 1, "{reason}");
    }
}

#[test]
fn a_trapping_capsule_falls_through_to_the_origin() {
    let wasm = wat::parse_str(
        r#"(module (memory (export "memory") 1) (func (export "_start") unreachable))"#,
    )
    .expect("valid WAT");
    let artifact = Arc::new(EdgeArtifact::from_bytes(&wasm).expect("valid module"));
    let seen = Seen::default();
    let gateway = EdgeGateway::new(artifact, origin(&seen));

    let response = block_on(gateway.handle(Request::get("/boom").body(Body::empty()).unwrap()));

    assert_eq!(
        lane(&response),
        Lane::Fallthrough(FallthroughReason::CapsuleError)
    );
    assert_eq!(body_text(response), "origin");
}

#[test]
fn a_non_utf8_header_skips_the_capsule_and_reaches_the_origin_intact() {
    let seen = Seen::default();
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen));

    let request = Request::get("/greet")
        .header(
            "x-latin1",
            http::HeaderValue::from_bytes(b"caf\xe9").unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = block_on(gateway.handle(request));

    assert_eq!(lane(&response), Lane::OriginOnly);
    assert_eq!(body_text(response), "origin");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn an_invalid_edge_response_falls_through_instead_of_serving_garbage() {
    let seen = Seen::default();
    let artifact = guest(&GuestFrame::Response(EdgeResponse {
        status: 42,
        headers: Vec::new(),
        body: Vec::new(),
    }));
    let gateway = EdgeGateway::new(artifact, origin(&seen));

    let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));

    assert_eq!(
        lane(&response),
        Lane::Fallthrough(FallthroughReason::CapsuleError)
    );
    assert_eq!(body_text(response), "origin");
}

#[test]
fn the_gateway_is_a_tower_service() {
    use tower::ServiceExt;

    let seen = Seen::default();
    let gateway =
        EdgeGateway::new(serving_guest(), origin(&seen)).with_kv(Arc::new(InMemoryEdgeKv::new()));

    let response = block_on(gateway.oneshot(Request::get("/greet").body(Body::empty()).unwrap()))
        .unwrap_or_else(|never| match never {});

    assert_eq!(lane(&response), Lane::Edge);
    assert_eq!(body_text(response), "from the edge");
}

#[test]
fn response_headers_are_set_on_edge_responses_only() {
    let headers = [(
        http::HeaderName::from_static("x-frame-options"),
        http::HeaderValue::from_static("DENY"),
    )];

    let seen = Seen::default();
    let edge =
        EdgeGateway::new(serving_guest(), origin(&seen)).with_response_headers(headers.clone());
    let response = block_on(edge.handle(Request::get("/greet").body(Body::empty()).unwrap()));
    assert_eq!(lane(&response), Lane::Edge);
    assert_eq!(response.headers()["x-frame-options"], "DENY");

    // The origin sets its own headers; the gateway leaves its response alone.
    let declined = EdgeGateway::new(
        declining_guest(FallthroughReason::UnknownRoute),
        origin(&seen),
    )
    .with_response_headers(headers);
    let response = block_on(declined.handle(Request::get("/x").body(Body::empty()).unwrap()));
    assert!(response.headers().get("x-frame-options").is_none());
}

/// The gateway does not trust the capsule: a response the edge runtime would
/// refuse is a fallthrough, also when a capsule not built by Autumn sends it.
#[test]
fn a_response_the_runtime_would_refuse_falls_through() {
    for (status, header) in [
        (200, Some(("set-cookie", "sid=1"))),
        (
            200,
            Some((autumn_edge::FALLTHROUGH_SENTINEL, "unknown_route")),
        ),
        (101, None),
    ] {
        let artifact = guest(&GuestFrame::Response(EdgeResponse {
            status,
            headers: header
                .map(|(name, value)| vec![(name.to_owned(), value.to_owned())])
                .unwrap_or_default(),
            body: Vec::new(),
        }));
        let seen = Seen::default();
        let gateway = EdgeGateway::new(artifact, origin(&seen));

        let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));

        assert_eq!(
            lane(&response),
            Lane::Fallthrough(FallthroughReason::CapsuleError),
            "{status} {header:?}"
        );
        assert_eq!(body_text(response), "origin");
    }
}

#[test]
fn a_write_with_a_bad_header_is_still_a_method_fallthrough() {
    let seen = Seen::default();
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen));
    let request = Request::post("/feedback")
        .header(
            "x-latin1",
            http::HeaderValue::from_bytes(b"caf\xe9").unwrap(),
        )
        .body(Body::empty())
        .unwrap();

    let response = block_on(gateway.handle(request));

    assert_eq!(
        lane(&response),
        Lane::Fallthrough(FallthroughReason::MethodNotEdgeEligible)
    );
}

#[test]
fn a_utf8_header_value_and_a_bad_credential_still_reach_the_capsule() {
    let seen = Seen::default();
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen));
    // `é` is UTF-8 but not visible ASCII. The cookie never reaches the
    // capsule, so its bytes do not matter.
    let request = Request::get("/greet")
        .header(
            "x-name",
            http::HeaderValue::from_bytes("José".as_bytes()).unwrap(),
        )
        .header(
            "cookie",
            http::HeaderValue::from_bytes(b"sid=\xff").unwrap(),
        )
        .body(Body::empty())
        .unwrap();

    let response = block_on(gateway.handle(request));

    assert_eq!(lane(&response), Lane::Edge);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn response_headers_keep_every_value_of_a_repeated_name() {
    let name = http::HeaderName::from_static("x-policy");
    let seen = Seen::default();
    let gateway = EdgeGateway::new(serving_guest(), origin(&seen)).with_response_headers([
        (name.clone(), http::HeaderValue::from_static("a")),
        (name.clone(), http::HeaderValue::from_static("b")),
    ]);

    let response = block_on(gateway.handle(Request::get("/greet").body(Body::empty()).unwrap()));

    let values: Vec<_> = response.headers().get_all(&name).iter().collect();
    assert_eq!(values, ["a", "b"]);
}

/// A framing header that does not match the body would break the response on
/// the wire. The gateway falls through instead.
#[test]
fn a_framing_header_that_does_not_match_the_body_falls_through() {
    for (name, value) in [
        ("content-length", "0"),
        ("content-length", "999"),
        ("transfer-encoding", "chunked"),
        ("connection", "close"),
        ("te", "trailers"),
        ("trailer", "x-checksum"),
        ("proxy-authenticate", "Basic"),
        ("proxy-connection", "keep-alive"),
    ] {
        let artifact = guest(&GuestFrame::Response(EdgeResponse {
            status: 200,
            headers: vec![(name.to_owned(), value.to_owned())],
            body: b"from the edge".to_vec(),
        }));
        let seen = Seen::default();
        let gateway = EdgeGateway::new(artifact, origin(&seen));

        let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));

        assert_eq!(
            lane(&response),
            Lane::Fallthrough(FallthroughReason::CapsuleError),
            "{name}: {value}"
        );
    }
}

#[test]
fn a_correct_content_length_is_served_and_head_keeps_the_get_length() {
    let artifact = guest(&GuestFrame::Response(EdgeResponse {
        status: 200,
        headers: vec![("content-length".into(), "13".into())],
        body: b"from the edge".to_vec(),
    }));
    let seen = Seen::default();
    let gateway = EdgeGateway::new(artifact, origin(&seen));
    let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));
    assert_eq!(lane(&response), Lane::Edge);
    assert_eq!(body_text(response), "from the edge");

    // HEAD: no body, and the length of the GET body.
    let artifact = guest(&GuestFrame::Response(EdgeResponse {
        status: 200,
        headers: vec![("content-length".into(), "13".into())],
        body: Vec::new(),
    }));
    let gateway = EdgeGateway::new(artifact, origin(&seen));
    let response = block_on(gateway.handle(Request::head("/x").body(Body::empty()).unwrap()));
    assert_eq!(lane(&response), Lane::Edge);
    assert_eq!(response.headers()["content-length"], "13");
}

/// 204, 205 and 304 have no body in HTTP. A capsule that sends one falls
/// through instead.
#[test]
fn a_body_on_a_no_content_status_falls_through() {
    for status in [204, 205, 304] {
        let artifact = guest(&GuestFrame::Response(EdgeResponse {
            status,
            headers: Vec::new(),
            body: b"not allowed".to_vec(),
        }));
        let seen = Seen::default();
        let gateway = EdgeGateway::new(artifact, origin(&seen));

        let response = block_on(gateway.handle(Request::get("/x").body(Body::empty()).unwrap()));

        assert_eq!(
            lane(&response),
            Lane::Fallthrough(FallthroughReason::CapsuleError),
            "{status}"
        );
    }
}
