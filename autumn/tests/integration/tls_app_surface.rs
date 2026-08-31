//! Direct-HTTPS parity for the rest of the app surface (issue #1603, AC4/AC2).
//!
//! `tls_serving.rs` proves the listener itself works. This suite proves the
//! *app* behaves identically once TLS is in front of it: the framework probes
//! (`/health`, `/live`, `/ready`), the inbound request timeout, SSE streaming,
//! `wss://` sockets, and graceful shutdown of an in-flight HTTPS request —
//! plus the renewal story, that a certificate rewritten on disk is served
//! without a restart.
//!
//! Every test drives a REAL Autumn router (`TestApp::…::into_router`, the same
//! router `app.rs` serves) over the REAL [`autumn_web::tls::TlsListener`], so a
//! regression in either the middleware stack or the TLS transport fails here.
//! The parity tests serve the *same* router configuration twice — once over
//! TLS, once over plain TCP — and compare the responses, so "identical under
//! TLS" is asserted against today's HTTP behavior rather than a hand-written
//! expectation that could drift.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use autumn_web::config::AutumnConfig;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use futures::stream::Stream;
use rustls_pki_types::pem::PemObject as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::tls_support::{
    CertFixture, RECORDING_HANDSHAKE_TIMEOUT, RENEWED_CERT_PEM, RENEWED_KEY_PEM, RecordingVerifier,
    http_get, https_get, serve_plain_router, serve_tls_router, tls_connect,
};

// ── Routes under test ────────────────────────────────────────────────────

/// The inbound deadline the timeout tests configure. Comfortably longer than
/// any real work here, so only a genuinely over-deadline handler trips it: a
/// tighter bound would turn ordinary CI scheduler jitter on `/fast` into a
/// spurious 503.
const REQUEST_DEADLINE_MS: u64 = 400;

/// Sleeps well past [`REQUEST_DEADLINE_MS`], so the deadline — not the sleep —
/// is what ends the request.
#[get("/slow")]
async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(3)).await;
    "done"
}

#[get("/fast")]
async fn fast() -> &'static str {
    "quick"
}

/// Set the moment a request enters the drain handler, so the shutdown test can
/// wait for "the request is really in flight" instead of guessing with a sleep.
static DRAIN_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The route the graceful-shutdown test keeps in flight across the shutdown
/// signal. Separate from `/slow` so no other test's traffic can flip
/// [`DRAIN_ENTERED`].
#[get("/drain")]
async fn drain() -> &'static str {
    DRAIN_ENTERED.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    "drained"
}

/// The gap between the two SSE events. Wide enough that the first event can be
/// late by an ordinary CI scheduling hiccup and still arrive long before the
/// second one is produced — the ordering that proves the body is streamed.
const SSE_EVENT_GAP: Duration = Duration::from_millis(1500);

/// Streams `tick-0` immediately and `tick-1` a [`SSE_EVENT_GAP`] later — well
/// past the inbound deadline — so a single response proves both that SSE
/// streams incrementally over TLS and that a stream is exempt from the
/// deadline.
#[get("/stream")]
async fn stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = futures::stream::unfold(0u8, |i| async move {
        if i >= 2 {
            return None;
        }
        if i > 0 {
            tokio::time::sleep(SSE_EVENT_GAP).await;
        }
        Some((
            Ok::<_, Infallible>(Event::default().data(format!("tick-{i}"))),
            i + 1,
        ))
    });
    Sse::new(events)
}

/// The router every parity test serves — built twice so the TLS and plain arms
/// are configured identically.
fn app_router(config: AutumnConfig) -> axum::Router {
    TestApp::new()
        .routes(routes![slow, fast, drain, stream])
        .config(config)
        .build()
        .into_router()
}

/// A config with the inbound request deadline enabled.
fn with_request_timeout(ms: u64) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.server.timeouts.request_timeout_ms = Some(ms);
    config
}

// ── AC4: probes behave identically under TLS ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn framework_probes_are_identical_over_tls_and_plain_http() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    let (tls, _resolver) = serve_tls_router(
        app_router(AutumnConfig::default()),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;
    let plain = serve_plain_router(app_router(AutumnConfig::default())).await;

    for path in ["/health", "/live", "/ready", "/startup", "/actuator/health"] {
        let over_tls = https_get(tls.addr, path, Arc::clone(&verifier))
            .await
            .unwrap_or_else(|e| panic!("HTTPS GET {path} failed: {e}"));
        let over_http = http_get(plain.addr, path)
            .await
            .unwrap_or_else(|e| panic!("HTTP GET {path} failed: {e}"));

        assert_eq!(
            over_tls.status, over_http.status,
            "{path}: status differs between TLS ({}) and plain HTTP ({})",
            over_tls.status, over_http.status
        );
        assert_eq!(
            over_tls.status, 200,
            "{path} must be 200 under TLS, got {}: {}",
            over_tls.status, over_tls.body
        );
        assert_eq!(
            over_tls.body, over_http.body,
            "{path}: body differs between TLS and plain HTTP"
        );
        assert_eq!(
            over_tls.header("content-type"),
            over_http.header("content-type"),
            "{path}: content-type differs between TLS and plain HTTP"
        );
        // Guard against the comparison above passing on two `None`s.
        assert!(
            over_tls.header("content-type").is_some(),
            "{path}: expected a content-type header over TLS"
        );
    }

    tls.shutdown().await;
    plain.shutdown().await;
}

// ── AC4: the inbound request timeout still fires under TLS ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn inbound_request_timeout_behaves_identically_under_tls() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    let (tls, _resolver) = serve_tls_router(
        app_router(with_request_timeout(REQUEST_DEADLINE_MS)),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;
    let plain = serve_plain_router(app_router(with_request_timeout(REQUEST_DEADLINE_MS))).await;

    let slow_tls = https_get(tls.addr, "/slow", Arc::clone(&verifier))
        .await
        .expect("HTTPS GET /slow");
    let slow_http = http_get(plain.addr, "/slow").await.expect("HTTP GET /slow");
    assert_eq!(
        slow_tls.status, 503,
        "a handler past the deadline must time out under TLS too, got {}: {}",
        slow_tls.status, slow_tls.body
    );
    assert_eq!(
        slow_tls.status, slow_http.status,
        "timeout status differs between TLS and plain HTTP"
    );

    // A fast route is not swept up by the deadline just because TLS is on.
    let fast_tls = https_get(tls.addr, "/fast", Arc::clone(&verifier))
        .await
        .expect("HTTPS GET /fast");
    assert_eq!(
        fast_tls.status, 200,
        "fast route must not time out under TLS"
    );
    assert_eq!(fast_tls.body, "quick");

    tls.shutdown().await;
    plain.shutdown().await;
}

// ── AC4: SSE streams incrementally over TLS ──────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn sse_streams_incrementally_over_tls() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    // The deadline is far shorter than the gap between the two events: an SSE
    // response must be exempt from it under TLS exactly as it is over HTTP.
    let (tls, _resolver) = serve_tls_router(
        app_router(with_request_timeout(REQUEST_DEADLINE_MS)),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;

    let mut stream = tls_connect(tls.addr, Arc::clone(&verifier))
        .await
        .expect("TLS connect");
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
        .await
        .expect("write SSE request");
    stream.flush().await.expect("flush SSE request");

    let started = Instant::now();
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    // The first event must arrive long before the second one is produced —
    // that gap is what proves the body is streamed, not buffered to completion.
    let first_at = loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no `tick-0` within 10s over TLS; saw: {seen:?}"
        );
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .expect("read timed out waiting for the first SSE event")
            .expect("read failed");
        assert!(n > 0, "server closed the stream before `tick-0`: {seen:?}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        if seen.contains("tick-0") {
            break started.elapsed();
        }
    };
    // The load-bearing assertion: the first event is in hand while the second
    // one is still being produced. A buffered-to-completion body could only
    // ever deliver both at once, so this fails on buffering without depending
    // on how fast the runner is.
    assert!(
        !seen.contains("tick-1"),
        "the second event arrived with the first; the response was buffered: {seen:?}"
    );
    // A generous liveness bound on top: the event is produced immediately, so
    // anything approaching the inter-event gap means it was not forwarded when
    // it was produced.
    assert!(
        first_at < SSE_EVENT_GAP,
        "the first SSE event took {first_at:?} over TLS, longer than the {SSE_EVENT_GAP:?} \
         gap before the second one — it was not streamed as it was produced"
    );
    assert!(
        seen.starts_with("HTTP/1.1 200"),
        "SSE response must be a 200 under TLS, got: {seen:?}"
    );

    // …and the second event still arrives, well past the request deadline.
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no `tick-1` within 20s over TLS; saw: {seen:?}"
        );
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .expect("read timed out waiting for the second SSE event")
            .expect("read failed");
        if n == 0 {
            break;
        }
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        if seen.contains("tick-1") {
            break;
        }
    }
    assert!(
        seen.contains("tick-1"),
        "an SSE stream must outlive the inbound deadline under TLS; saw: {seen:?}"
    );

    tls.shutdown().await;
}

// ── AC4: `wss://` WebSockets work over the TLS listener ──────────────────

#[cfg(feature = "ws")]
mod websocket {
    use super::{Arc, CertFixture, Duration, RECORDING_HANDSHAKE_TIMEOUT, RecordingVerifier};
    use super::{serve_tls_router, tls_connect};
    use autumn_web::test::TestApp;
    use autumn_web::ws::{Message, WebSocket, WsHandler};
    use autumn_web::{routes, ws};
    use futures::{SinkExt as _, StreamExt as _};

    #[ws("/echo")]
    async fn echo() -> impl WsHandler {
        |mut socket: WebSocket| async move {
            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_upgrade_and_echo_round_trip_over_wss() {
        let fixture = CertFixture::write();
        let verifier = Arc::new(RecordingVerifier::default());
        let router = TestApp::new().routes(routes![echo]).build().into_router();
        let (tls, _resolver) =
            serve_tls_router(router, &fixture, RECORDING_HANDSHAKE_TIMEOUT).await;

        let stream = tls_connect(tls.addr, Arc::clone(&verifier))
            .await
            .expect("TLS connect");
        let url = format!("wss://localhost:{}/echo", tls.addr.port());
        let (mut socket, response) = tokio_tungstenite::client_async(url, stream)
            .await
            .expect("wss:// upgrade");
        assert_eq!(
            response.status().as_u16(),
            101,
            "wss:// must complete the WebSocket upgrade over TLS"
        );

        socket
            .send(tokio_tungstenite::tungstenite::Message::Text("ping".into()))
            .await
            .expect("send over wss://");
        let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("wss:// echo timed out")
            .expect("stream ended before the echo")
            .expect("wss:// read failed");
        assert_eq!(
            echoed.into_text().expect("text frame").as_str(),
            "ping",
            "the echo handler must round-trip a frame over wss://"
        );

        socket.close(None).await.ok();
        tls.shutdown().await;
    }
}

// ── AC4: graceful shutdown drains an in-flight HTTPS request ─────────────

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_drains_an_in_flight_https_request() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    let (tls, _resolver) = serve_tls_router(
        app_router(AutumnConfig::default()),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;
    let addr = tls.addr;

    // Start a request that is still running when shutdown is signalled.
    let inflight = tokio::spawn({
        let verifier = Arc::clone(&verifier);
        async move { https_get(addr, "/drain", verifier).await }
    });

    // Wait for the handler to be ENTERED rather than sleeping a guessed
    // interval: cancelling while the request is still in the handshake tests
    // nothing about draining, and on a loaded runner that is exactly what a
    // fixed sleep would do.
    let entered = Instant::now();
    while !DRAIN_ENTERED.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            entered.elapsed() < Duration::from_secs(20),
            "the request never reached the handler, so nothing was in flight to drain"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    tls.shutdown.cancel();

    let response = tokio::time::timeout(Duration::from_secs(20), inflight)
        .await
        .expect("in-flight HTTPS request never completed after shutdown")
        .expect("in-flight task panicked")
        .expect("in-flight HTTPS request failed");
    assert_eq!(
        response.status, 200,
        "graceful shutdown must drain the in-flight HTTPS request, got {}: {}",
        response.status, response.body
    );
    assert_eq!(response.body, "drained");

    // And the serve task itself has to finish cleanly, not merely stop.
    let joined = tokio::time::timeout(Duration::from_secs(20), tls.handle)
        .await
        .expect("the serve task should stop after draining")
        .expect("the serve task should not panic");
    assert!(
        joined.is_ok(),
        "graceful shutdown under TLS should return Ok, got: {joined:?}"
    );
}

/// Spawn the real reload task at a test-friendly poll interval, returning its
/// shutdown token and join handle.
fn spawn_reloader(
    resolver: &Arc<autumn_web::tls::ReloadableCertResolver>,
    fixture: &CertFixture,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let reloader = autumn_web::tls::CertReloader::new(
        Arc::clone(resolver),
        autumn_web::tls::crypto_provider(),
        fixture.cert.clone(),
        fixture.key.clone(),
        Duration::from_millis(50),
    );
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { reloader.run(shutdown).await }
    });
    (shutdown, task)
}

// ── AC2: a renewed certificate is served without a restart ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn renewed_certificate_is_served_without_a_restart() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    let (tls, resolver) = serve_tls_router(
        app_router(AutumnConfig::default()),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;

    // The same reload task `app.rs` spawns, at a test-friendly poll interval.
    let (reload_shutdown, reload_task) = spawn_reloader(&resolver, &fixture);

    let before = https_get(tls.addr, "/health", Arc::clone(&verifier))
        .await
        .expect("HTTPS GET /health before renewal");
    assert_eq!(before.status, 200);
    let original_leaf = verifier.last_leaf().expect("a leaf was recorded");

    // `certbot` rewrites the live cert/key files in place.
    fixture.renew(RENEWED_CERT_PEM, RENEWED_KEY_PEM);

    // The site keeps serving throughout: every probe below is a real request,
    // and the served leaf flips to the renewed certificate with no restart.
    let deadline = Instant::now() + Duration::from_secs(10);
    let renewed_leaf = loop {
        assert!(
            Instant::now() < deadline,
            "the renewed certificate was never served within 10s"
        );
        let response = https_get(tls.addr, "/health", Arc::clone(&verifier))
            .await
            .expect("HTTPS GET /health during renewal");
        assert_eq!(
            response.status, 200,
            "the site must keep answering while the certificate is reloaded"
        );
        let leaf = verifier.last_leaf().expect("a leaf was recorded");
        if leaf != original_leaf {
            break leaf;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let expected = rustls_pki_types::CertificateDer::from_pem_slice(RENEWED_CERT_PEM.as_bytes())
        .expect("parse the renewed fixture");
    assert_eq!(
        renewed_leaf, expected,
        "the listener must serve the certificate that replaced the original on disk"
    );
    reload_shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), reload_task).await;
    tls.shutdown().await;
}

// ── AC2: a BROKEN renewal never breaks the listener ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_certificate_on_disk_keeps_the_previous_one_serving() {
    let fixture = CertFixture::write();
    let verifier = Arc::new(RecordingVerifier::default());
    let (tls, resolver) = serve_tls_router(
        app_router(AutumnConfig::default()),
        &fixture,
        RECORDING_HANDSHAKE_TIMEOUT,
    )
    .await;
    let (reload_shutdown, reload_task) = spawn_reloader(&resolver, &fixture);

    let before = https_get(tls.addr, "/health", Arc::clone(&verifier))
        .await
        .expect("HTTPS GET /health before the bad renewal");
    assert_eq!(before.status, 200);
    let original_leaf = verifier.last_leaf().expect("a leaf was recorded");

    // A renewal tool (or an operator) writes something that is not a
    // certificate over the live file.
    fixture.corrupt_cert();

    // Give the poller several intervals to see it, then prove the site is
    // untouched: still 200, still serving the certificate it loaded at boot.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = https_get(tls.addr, "/health", Arc::clone(&verifier))
        .await
        .expect("a failed reload must not break the listener");
    assert_eq!(
        after.status, 200,
        "a corrupt certificate on disk must not take the site down"
    );
    assert_eq!(
        verifier.last_leaf().expect("a leaf was recorded"),
        original_leaf,
        "a failed reload must keep serving the previously loaded certificate"
    );

    // …and once a VALID certificate replaces the corrupt one, the reloader
    // still picks it up: the failed attempt must not have advanced its
    // baseline past the change.
    fixture.renew(RENEWED_CERT_PEM, RENEWED_KEY_PEM);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the reloader never recovered after a failed reload"
        );
        let response = https_get(tls.addr, "/health", Arc::clone(&verifier))
            .await
            .expect("HTTPS GET /health while recovering");
        assert_eq!(response.status, 200);
        if verifier.last_leaf().expect("a leaf was recorded") != original_leaf {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    reload_shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), reload_task).await;
    tls.shutdown().await;
}
