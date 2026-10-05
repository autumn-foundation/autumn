//! The edge node: capsule first, remote origin over HTTP on every fallthrough.
//!
//! Guests are hand-written WAT, so no `wasm32-wasip1` toolchain is needed.
//! Every test uses real TCP sockets on `127.0.0.1`.
#![cfg(feature = "node")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_edge::EdgeResponse;
use autumn_edge::gateway::EdgeGateway;
use autumn_edge::host::EdgeArtifact;
use autumn_edge::node::ttfb::{Probe, Report, Summary, measure};
use autumn_edge::node::{EdgeNode, HttpOrigin, NodeError, origin_static_headers, serve};
use autumn_edge::wire::{FallthroughReason, GuestFrame, to_line};
use axum::body::Body;
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

/// The body both lanes serve in the TTFB tests.
const EDGE_BODY: &str = "from the edge";

fn serving_guest() -> Arc<EdgeArtifact> {
    guest(&GuestFrame::Response(EdgeResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: EDGE_BODY.as_bytes().to_vec(),
    }))
}

fn declining_guest() -> Arc<EdgeArtifact> {
    guest(&GuestFrame::Fallthrough {
        reason: FallthroughReason::UnknownRoute,
        detail: "secret detail for logs only".into(),
    })
}

/// One request as the origin saw it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// How the test origin answers.
#[derive(Clone)]
struct Answer {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl Answer {
    fn created() -> Self {
        Self {
            status: StatusCode::CREATED,
            headers: vec![("x-origin", "yes".into())],
            body: "origin".into(),
            delay: Duration::ZERO,
        }
    }
}

/// Start an origin on `127.0.0.1` that records each request. `answer` runs
/// once per request, so a test can vary the answer.
async fn origin_with(answer: impl Fn() -> Answer + Send + Sync + 'static) -> (String, Log) {
    let log = Log::default();
    let recorded = Arc::clone(&log);
    let answer = Arc::new(answer);
    let router = axum::Router::new().fallback(move |request: Request<Body>| {
        let recorded = Arc::clone(&recorded);
        let answer = Arc::clone(&answer);
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX)
                .await
                .expect("body reads");
            recorded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("?").to_owned()))
                        .collect(),
                    body: body.to_vec(),
                });
            let answer = answer();
            tokio::time::sleep(answer.delay).await;
            let mut response = Response::builder().status(answer.status);
            for (name, value) in &answer.headers {
                response = response.header(*name, value.as_str());
            }
            response
                .body(Body::from(answer.body))
                .expect("valid response")
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("origin serves");
    });
    (format!("http://{address}"), log)
}

/// Start an edge node with `artifact` in front of `origin`.
async fn node(artifact: Arc<EdgeArtifact>, origin: &str) -> String {
    let gateway = EdgeGateway::new(artifact, HttpOrigin::new(origin).expect("valid origin"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address: SocketAddr = listener.local_addr().expect("address");
    tokio::spawn(serve(
        listener,
        EdgeNode::new(gateway),
        std::future::pending(),
    ));
    format!("http://{address}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

fn seen(log: &Log) -> Vec<Seen> {
    log.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

#[tokio::test]
async fn a_served_request_never_reaches_the_origin() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(serving_guest(), &origin).await;

    let response = client()
        .get(format!("{edge}/greet"))
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.text().await.expect("body"), EDGE_BODY);
    assert!(seen(&log).is_empty(), "the origin must not be asked");
}

#[tokio::test]
async fn a_declined_request_reaches_the_origin_unchanged() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &origin).await;

    let response = client()
        .get(format!("{edge}/nope?a=1&a=2"))
        .header("cookie", "session=s1")
        .header("authorization", "Bearer t")
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-origin"], "yes");
    assert_eq!(response.text().await.expect("body"), "origin");
    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.method, "GET");
    assert_eq!(request.uri, "/nope?a=1&a=2");
    assert_eq!(request.header("cookie"), Some("session=s1"));
    assert_eq!(request.header("authorization"), Some("Bearer t"));
}

#[tokio::test]
async fn a_write_sends_its_body_to_the_origin() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(serving_guest(), &origin).await;

    let response = client()
        .post(format!("{edge}/feedback"))
        .body("payload")
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::CREATED);
    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, b"payload");
}

#[tokio::test]
async fn the_origin_sees_the_client_and_the_original_host() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &origin).await;

    // A client cannot set the forwarded headers: the node is the first proxy.
    client()
        .get(format!("{edge}/who"))
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "for=203.0.113.9;host=evil.example")
        .send()
        .await
        .expect("node answers");

    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.header("x-forwarded-for"), Some("127.0.0.1"));
    let edge_host = edge.trim_start_matches("http://");
    assert_eq!(request.header("x-forwarded-host"), Some(edge_host));
    assert_eq!(request.header("x-forwarded-proto"), Some("http"));
    assert_eq!(request.header("forwarded"), None);
    let origin_host = origin.trim_start_matches("http://");
    assert_eq!(request.header("host"), Some(origin_host));
}

#[tokio::test]
async fn hop_by_hop_headers_do_not_cross_the_node() {
    let (origin, log) = origin_with(|| Answer {
        headers: vec![
            ("keep-alive", "timeout=5".into()),
            ("x-origin", "yes".into()),
        ],
        ..Answer::created()
    })
    .await;
    let edge = node(declining_guest(), &origin).await;

    let response = client()
        .get(format!("{edge}/hop"))
        .header("proxy-authorization", "Basic c2VjcmV0")
        .header("te", "trailers")
        .send()
        .await
        .expect("node answers");

    assert!(response.headers().get("keep-alive").is_none());
    assert_eq!(response.headers()["x-origin"], "yes");
    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.header("proxy-authorization"), None);
    assert_eq!(request.header("te"), None);
}

#[tokio::test]
async fn the_node_does_not_follow_an_origin_redirect() {
    let (origin, log) = origin_with(|| Answer {
        status: StatusCode::FOUND,
        headers: vec![("location", "/elsewhere".into())],
        body: String::new(),
        delay: Duration::ZERO,
    })
    .await;
    let edge = node(declining_guest(), &origin).await;

    let response = client()
        .get(format!("{edge}/old"))
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "/elsewhere");
    assert_eq!(seen(&log).len(), 1);
}

#[tokio::test]
async fn an_unreachable_origin_is_a_502() {
    // Bind, then drop: nothing listens on this port.
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        listener.local_addr().expect("address")
    };
    let edge = node(declining_guest(), &format!("http://{closed}")).await;

    let response = client()
        .get(format!("{edge}/x"))
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_base_path_prefixes_every_forwarded_path() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &format!("{origin}/app/")).await;

    client()
        .get(format!("{edge}/x?y=1"))
        .send()
        .await
        .expect("node answers");

    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.uri, "/app/x?y=1");
}

#[test]
fn an_origin_url_must_be_http_without_a_query() {
    assert!(HttpOrigin::new("http://origin.example").is_ok());
    assert!(HttpOrigin::new("https://origin.example:8443/app").is_ok());
    assert!(HttpOrigin::new("ftp://origin.example").is_err());
    assert!(HttpOrigin::new("http://origin.example/?a=1").is_err());
    assert!(HttpOrigin::new("http://origin.example/#top").is_err());
    assert!(HttpOrigin::new("origin.example").is_err());
    assert!(
        HttpOrigin::new("https://user:secret@origin.example").is_err(),
        "credentials in the URL would go to the origin on each request"
    );
}

#[tokio::test]
async fn security_headers_are_copied_from_the_origin() {
    let (origin, _) = origin_with(|| Answer {
        headers: vec![
            ("x-frame-options", "DENY".into()),
            ("referrer-policy", "no-referrer".into()),
            ("content-security-policy", "default-src 'self'".into()),
            ("x-other", "not copied".into()),
        ],
        ..Answer::created()
    })
    .await;

    let headers = origin_static_headers(&origin, "/").await.expect("probe");

    let mut names: Vec<_> = headers.iter().map(|(n, _)| n.as_str().to_owned()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "content-security-policy",
            "referrer-policy",
            "x-frame-options"
        ]
    );
}

#[tokio::test]
async fn a_nonce_csp_is_not_copied() {
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (origin, _) = origin_with(move || {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Answer {
            headers: vec![
                ("x-frame-options", "DENY".into()),
                ("content-security-policy", format!("script-src 'nonce-{n}'")),
            ],
            ..Answer::created()
        }
    })
    .await;

    let headers = origin_static_headers(&origin, "/").await.expect("probe");

    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].0, "x-frame-options");
}

#[tokio::test]
async fn an_unreachable_origin_fails_the_security_header_probe() {
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        listener.local_addr().expect("address")
    };
    assert!(matches!(
        origin_static_headers(&format!("http://{closed}"), "/").await,
        Err(NodeError::Request(_))
    ));
}

// ── TTFB probe ───────────────────────────────────────────────────────

/// An origin that serves the guest's bytes after `delay`.
async fn slow_origin(delay: Duration, body: &'static str) -> String {
    origin_with(move || Answer {
        status: StatusCode::OK,
        headers: vec![("content-type", "text/plain".into())],
        body: body.into(),
        delay,
    })
    .await
    .0
}

#[tokio::test]
async fn the_probe_shows_the_edge_is_faster_with_zero_divergence() {
    // A large delay: the check must hold on slow CI runners too.
    let origin = slow_origin(Duration::from_millis(300), EDGE_BODY).await;
    let edge = node(serving_guest(), &origin).await;

    let report = measure(&Probe {
        edge,
        origin,
        paths: vec!["/greet".into(), "/greet?x=1".into()],
        rounds: 3,
    })
    .await
    .expect("probe runs");

    assert_eq!(report.edge.samples.len(), 6);
    assert_eq!(report.origin.samples.len(), 6);
    assert!(report.divergences.is_empty(), "{:?}", report.divergences);
    assert!(
        report.reduction_percent() >= 50.0,
        "edge {:?} vs origin {:?}",
        report.edge.median(),
        report.origin.median()
    );
    assert!(report.passes(50.0));
}

#[tokio::test]
async fn the_probe_counts_a_divergence_and_fails() {
    let origin = slow_origin(Duration::ZERO, "a different body").await;
    let edge = node(serving_guest(), &origin).await;

    let report = measure(&Probe {
        edge,
        origin,
        paths: vec!["/greet".into()],
        rounds: 2,
    })
    .await
    .expect("probe runs");

    assert_eq!(report.divergences.len(), 2);
    assert!(report.divergences[0].contains("/greet"));
    assert!(!report.passes(0.0));
}

#[tokio::test]
async fn the_probe_reports_an_unreachable_target() {
    let origin = slow_origin(Duration::ZERO, EDGE_BODY).await;
    let probe = Probe {
        edge: "http://127.0.0.1:1".into(),
        origin,
        paths: vec!["/greet".into()],
        rounds: 1,
    };
    assert!(matches!(measure(&probe).await, Err(NodeError::Request(_))));
}

#[test]
fn median_and_p90_use_the_nearest_rank() {
    let summary = Summary {
        samples: (1..=10).map(Duration::from_millis).collect(),
    };
    assert_eq!(summary.median(), Duration::from_millis(5));
    assert_eq!(summary.p90(), Duration::from_millis(9));
    assert_eq!(Summary::default().median(), Duration::ZERO);
}

#[test]
fn the_reduction_is_relative_to_the_origin_median() {
    let report = Report {
        edge: Summary {
            samples: vec![Duration::from_millis(20)],
        },
        origin: Summary {
            samples: vec![Duration::from_millis(100)],
        },
        divergences: Vec::new(),
    };
    assert!((report.reduction_percent() - 80.0).abs() < 1e-9);
    assert!(report.passes(50.0));
    assert!(!report.passes(90.0));
    assert!((Report::default().reduction_percent()).abs() < 1e-9);
    assert!(!Report::default().passes(0.0), "no samples is not a pass");
}

#[tokio::test]
async fn the_access_log_records_the_lane_of_each_request() {
    use autumn_edge::gateway::Lane;
    use autumn_edge::node::AccessEntry;

    let (origin, _) = origin_with(Answer::created).await;
    let entries: Arc<Mutex<Vec<AccessEntry>>> = Arc::default();
    let sink = Arc::clone(&entries);
    let gateway = EdgeGateway::new(serving_guest(), HttpOrigin::new(&origin).expect("origin"));
    let node = EdgeNode::new(gateway).with_access_log(move |entry: &AccessEntry| {
        sink.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry.clone());
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let edge = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(serve(listener, node, std::future::pending()));

    client()
        .get(format!("{edge}/greet?a=1"))
        .send()
        .await
        .expect("get");
    client()
        .post(format!("{edge}/feedback"))
        .send()
        .await
        .expect("post");

    let entries = entries
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[0].path, "/greet", "the log must not keep the query");
    assert_eq!(entries[0].status, 200);
    assert_eq!(entries[0].lane, Some(Lane::Edge));
    assert_eq!(entries[1].method, "POST");
    assert_eq!(entries[1].status, 201);
    assert_eq!(
        entries[1].lane,
        Some(Lane::Fallthrough(FallthroughReason::MethodNotEdgeEligible))
    );
}

#[tokio::test]
async fn a_static_cors_header_is_copied_and_a_changing_one_is_not() {
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (origin, _) = origin_with(move || {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Answer {
            headers: vec![
                ("access-control-allow-origin", "*".into()),
                ("access-control-expose-headers", format!("x-n-{n}")),
                // Not a middleware header: never copied.
                ("cache-control", "no-store".into()),
            ],
            ..Answer::created()
        }
    })
    .await;

    let headers = origin_static_headers(&origin, "/").await.expect("probe");

    let names: Vec<_> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["access-control-allow-origin"]);
}

#[tokio::test]
async fn a_repeated_security_header_keeps_every_value() {
    let (origin, _) = origin_with(|| Answer {
        headers: vec![
            ("permissions-policy", "camera=()".into()),
            ("permissions-policy", "geolocation=()".into()),
        ],
        ..Answer::created()
    })
    .await;

    let headers = origin_static_headers(&origin, "/").await.expect("probe");

    let values: Vec<_> = headers.iter().map(|(_, v)| v.to_str().unwrap()).collect();
    assert_eq!(values, ["camera=()", "geolocation=()"]);
}

#[tokio::test]
async fn a_large_write_body_reaches_the_origin_intact() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(serving_guest(), &origin).await;
    let payload: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();

    let response = client()
        .put(format!("{edge}/upload"))
        .body(payload.clone())
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::CREATED);
    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.header("content-length"), Some("2000000"));
    assert!(request.body == payload, "the body changed on the way");
}

/// Send `target` as written, over raw TCP. An HTTP client would resolve the
/// dot segments first. Returns the status code.
async fn raw_status(edge: &str, target: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let address = edge.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let request = format!("GET {target} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.expect("read");
    answer
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {answer}"))
}

#[tokio::test]
async fn a_dot_segment_or_backslash_path_is_refused() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &format!("{origin}/app")).await;

    for target in [
        "/../secret",
        "/a/%2e%2E/b",
        "/a/./b",
        "/a\\..\\b",
        "http://other.example/../x",
    ] {
        assert_eq!(raw_status(&edge, target).await, 400, "{target}");
    }
    assert!(seen(&log).is_empty(), "the origin must not be asked");

    // A safe path still reaches the origin, under the base path.
    assert_eq!(raw_status(&edge, "/a/b..c/d?x=../y").await, 201);
    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.uri, "/app/a/b..c/d?x=../y");
}

#[tokio::test]
async fn a_header_named_in_connection_does_not_cross_the_node() {
    let (origin, log) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &origin).await;

    client()
        .get(format!("{edge}/c"))
        .header("connection", "x-hop")
        .header("x-hop", "1")
        .header("x-kept", "1")
        .send()
        .await
        .expect("node answers");

    let [request] = seen(&log).try_into().expect("the origin is asked once");
    assert_eq!(request.header("x-hop"), None);
    assert_eq!(request.header("x-kept"), Some("1"));
}

#[tokio::test]
async fn a_head_without_content_length_does_not_get_a_false_zero() {
    let (origin, _) = origin_with(Answer::created).await;
    let edge = node(declining_guest(), &origin).await;

    // The origin sets content-length on HEAD too; the node keeps it.
    let response = client()
        .head(format!("{edge}/h"))
        .send()
        .await
        .expect("node answers");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["content-length"], "6");
}

#[tokio::test]
async fn a_head_of_a_chunked_resource_does_not_get_a_false_zero() {
    // An origin with a body of unknown length: no content-length.
    let router = axum::Router::new().fallback(|| async {
        let chunks = futures::stream::iter([Ok::<_, std::io::Error>("chunked body")]);
        Body::from_stream(chunks)
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let edge = node(declining_guest(), &origin).await;

    let direct = client()
        .head(format!("{origin}/x"))
        .send()
        .await
        .expect("origin");
    let through = client()
        .head(format!("{edge}/x"))
        .send()
        .await
        .expect("node");

    assert_eq!(
        through.headers().get("content-length"),
        direct.headers().get("content-length"),
        "the node must not invent a length"
    );
}

#[tokio::test]
async fn the_probe_ignores_hop_by_hop_headers() {
    let origin = origin_with(|| Answer {
        status: StatusCode::OK,
        headers: vec![
            ("content-type", "text/plain".into()),
            ("keep-alive", "timeout=5".into()),
        ],
        body: EDGE_BODY.into(),
        delay: Duration::ZERO,
    })
    .await
    .0;
    let edge = node(declining_guest(), &origin).await;

    let report = measure(&Probe {
        edge,
        origin,
        paths: vec!["/greet".into()],
        rounds: 1,
    })
    .await
    .expect("probe runs");

    assert!(report.divergences.is_empty(), "{:?}", report.divergences);
}

#[tokio::test]
async fn the_probe_refuses_a_bad_configuration() {
    let probe = |paths: Vec<String>, rounds| Probe {
        edge: "http://127.0.0.1:9".into(),
        origin: "http://127.0.0.1:9".into(),
        paths,
        rounds,
    };
    for bad in [
        probe(vec![], 1),
        probe(vec!["/a".into()], 0),
        probe(vec!["a".into()], 1),
    ] {
        assert!(matches!(measure(&bad).await, Err(NodeError::Config(_))));
    }
    assert!(matches!(
        origin_static_headers("http://127.0.0.1:9", "no-slash").await,
        Err(NodeError::Config(_))
    ));
}

#[tokio::test]
async fn an_upgrade_is_tunnelled_to_the_origin() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // An origin that accepts `upgrade: echo` and echoes the raw bytes.
    let router = axum::Router::new().fallback(|mut request: Request<Body>| async move {
        let Some(upgrade) = request
            .extensions_mut()
            .remove::<hyper::upgrade::OnUpgrade>()
        else {
            return Response::builder().status(400).body(Body::empty()).unwrap();
        };
        tokio::spawn(async move {
            let upgraded = upgrade.await.expect("the client upgrades");
            let mut io = hyper_util::rt::TokioIo::new(upgraded);
            let mut buffer = [0u8; 4];
            io.read_exact(&mut buffer).await.expect("read");
            io.write_all(&buffer).await.expect("echo");
        });
        Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("connection", "upgrade")
            .header("upgrade", "echo")
            .body(Body::empty())
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let edge = node(declining_guest(), &origin).await;

    let address = edge.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    stream
        .write_all(
            format!("GET /ws HTTP/1.1\r\nhost: {address}\r\nconnection: upgrade\r\nupgrade: echo\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.expect("read head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(head.contains("upgrade: echo"), "{head}");

    stream.write_all(b"ping").await.expect("write");
    let mut echo = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut echo))
        .await
        .expect("the tunnel answers in time")
        .expect("read");
    assert_eq!(&echo, b"ping");
}

/// The headers of each request an origin got.
type HeaderLog = Arc<Mutex<Vec<Vec<(String, String)>>>>;

/// A node in front of an in-process origin that records the headers the
/// gateway gives it. The gateway gives the capsule the same request.
async fn recording_node(trusted: &[&str]) -> (String, HeaderLog) {
    use autumn_edge::node::TrustedProxy;

    let seen = HeaderLog::default();
    let sink = Arc::clone(&seen);
    let origin = tower::service_fn(move |request: Request<Body>| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap_or_else(PoisonError::into_inner).push(
                request
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("?").to_owned()))
                    .collect(),
            );
            Ok::<_, std::convert::Infallible>(Response::new(Body::from("origin")))
        }
    });
    let trusted = trusted
        .iter()
        .map(|raw| TrustedProxy::parse(raw).expect("valid proxy"))
        .collect();
    let node =
        EdgeNode::new(EdgeGateway::new(declining_guest(), origin)).with_trusted_proxies(trusted);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let edge = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(serve(listener, node, std::future::pending()));
    (edge, seen)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn both_lanes_get_the_replaced_forwarded_headers() {
    let (edge, seen) = recording_node(&[]).await;

    client()
        .get(format!("{edge}/x"))
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "for=203.0.113.9")
        .send()
        .await
        .expect("node answers");

    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let [headers] = seen.as_slice() else {
        panic!("the origin is asked once: {seen:?}");
    };
    assert_eq!(header(headers, "x-forwarded-for"), Some("127.0.0.1"));
    assert_eq!(
        header(headers, "x-forwarded-host"),
        Some(edge.trim_start_matches("http://"))
    );
    assert_eq!(header(headers, "x-forwarded-proto"), Some("http"));
    assert_eq!(header(headers, "forwarded"), None);
}

#[tokio::test]
async fn a_trusted_proxy_keeps_its_forwarded_headers() {
    let (edge, seen) = recording_node(&["127.0.0.0/8"]).await;

    client()
        .get(format!("{edge}/x"))
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-forwarded-host", "shop.example")
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .expect("node answers");

    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let [headers] = seen.as_slice() else {
        panic!("the origin is asked once: {seen:?}");
    };
    assert_eq!(
        header(headers, "x-forwarded-for"),
        Some("203.0.113.9, 127.0.0.1")
    );
    assert_eq!(header(headers, "x-forwarded-host"), Some("shop.example"));
    assert_eq!(header(headers, "x-forwarded-proto"), Some("https"));
}

#[tokio::test]
async fn a_trusted_proxy_cannot_set_a_bad_scheme() {
    let (edge, seen) = recording_node(&["127.0.0.1"]).await;

    client()
        .get(format!("{edge}/x"))
        .header("x-forwarded-proto", "javascript")
        .send()
        .await
        .expect("node answers");

    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(header(&seen[0], "x-forwarded-proto"), Some("http"));
}

#[test]
fn a_trusted_proxy_is_an_address_or_a_cidr_range() {
    use autumn_edge::node::TrustedProxy;
    use std::net::IpAddr;

    let ip = |raw: &str| raw.parse::<IpAddr>().unwrap();
    let range = TrustedProxy::parse("10.0.0.0/8").unwrap();
    assert!(range.contains(ip("10.1.2.3")));
    assert!(!range.contains(ip("11.0.0.1")));
    assert!(!range.contains(ip("::1")));
    let one = TrustedProxy::parse("::1").unwrap();
    assert!(one.contains(ip("::1")));
    assert!(!one.contains(ip("::2")));
    assert!(
        TrustedProxy::parse("0.0.0.0/0")
            .unwrap()
            .contains(ip("8.8.8.8"))
    );
    assert!(
        TrustedProxy::parse("fd00::/8")
            .unwrap()
            .contains(ip("fd12::1"))
    );
    for bad in ["", "10.0.0.0/33", "::/129", "nope", "10.0.0.0/x"] {
        assert!(TrustedProxy::parse(bad).is_err(), "{bad}");
    }
}
