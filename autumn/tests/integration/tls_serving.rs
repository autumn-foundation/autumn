//! End-to-end coverage for the direct-HTTPS (native TLS) listener (issue
//! #1603).
//!
//! These tests exercise the REAL serving path: the public
//! [`autumn_web::tls::TlsListener`] (the same `axum::serve::Listener` the app
//! binds when `[server.tls]` is set) served through `axum::serve` with the
//! same connect-info and graceful-shutdown wiring `app.rs` uses. A rustls
//! client that trusts the test certificate then drives it over TLS.
//!
//! No tokio-rustls is needed on the client side: like `pg_tls.rs`, the client
//! speaks rustls synchronously over a `std::net::TcpStream` on a blocking
//! thread, so the only test dependencies are `rustls` + `rustls-pki-types`.
//!
//! The certificate fixture is a self-signed `CN=localhost` (SAN `localhost` +
//! `127.0.0.1`) valid until 2126 — shared with the `tls` module's unit tests.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::tls::{
    ReloadableCertResolver, TlsListener, build_server_config, crypto_provider, load_certified_key,
};
use axum::Router;
use axum::routing::get;
use axum::serve::ListenerExt as _;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_util::sync::CancellationToken;

/// A client-side verifier that accepts any server certificate. The fixture is
/// a self-signed CA cert (so rustls' path validation would reject it as a leaf,
/// `CaUsedAsEndEntity`); these tests exercise the TLS *transport*, not identity
/// verification, so danger-accepting the cert is the documented approach.
#[derive(Debug)]
struct AcceptAnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

const CERT_PEM: &str = include_str!("../fixtures/tls/localhost.cert.pem");
const KEY_PEM: &str = include_str!("../fixtures/tls/localhost.key.pem");

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Write the cert + key fixtures to a tempdir and return their paths (kept
/// alive by the returned `TempDir`).
fn write_fixtures() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, CERT_PEM).unwrap();
    std::fs::write(&key, KEY_PEM).unwrap();
    (dir, cert, key)
}

/// A running HTTPS test server plus the knobs to talk to and stop it.
struct TestServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<std::io::Result<()>>,
    _dir: tempfile::TempDir,
}

/// Boot the real `TlsListener` on an ephemeral port, serving a minimal router
/// with the same connect-info + graceful-shutdown wiring as `app.rs`. Uses a
/// generous handshake timeout that never fires in the well-behaved-client tests.
async fn serve_tls() -> TestServer {
    serve_tls_with_handshake_timeout(Duration::from_secs(10)).await
}

/// Like [`serve_tls`] but with a caller-chosen per-handshake timeout, so a test
/// can prove a stalled handshake is shed rather than parking the accept loop.
async fn serve_tls_with_handshake_timeout(handshake_timeout: Duration) -> TestServer {
    let (dir, cert, key) = write_fixtures();
    let provider = crypto_provider();
    let certified = load_certified_key(&cert, &key, &provider, now_unix()).expect("load cert/key");
    let resolver = Arc::new(ReloadableCertResolver::new(certified));
    let server_config =
        build_server_config(Arc::clone(&provider), resolver).expect("build server config");

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = tcp.local_addr().expect("local_addr");
    let listener = TlsListener::new(tcp, server_config, handshake_timeout);

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/whoami",
            // Prove the peer SocketAddr connect-info still flows through the
            // TLS listener exactly as on the plain-TCP path.
            get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>| async move {
                    peer.ip().to_string()
                },
            ),
        );

    // Mirror the app.rs HTTPS serve arm: no-op tap_io wrapper so axum supplies
    // ConnectInfo<SocketAddr>, plus into_make_service_with_connect_info.
    let listener = listener.tap_io(|_io| {});
    let make_service =
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<SocketAddr>(
            router,
        );

    let shutdown = CancellationToken::new();
    let shutdown_wait = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, make_service)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await
    });

    // Give the listener a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    TestServer {
        addr,
        shutdown,
        handle,
        _dir: dir,
    }
}

/// Perform a blocking HTTPS GET over rustls, trusting the test certificate, and
/// return the raw response (status line + headers + body). Runs synchronously,
/// so callers wrap it in `spawn_blocking`.
fn https_get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_no_client_auth();

    // SNI/verification uses the certificate's `localhost` SAN even though we
    // dial 127.0.0.1.
    let server_name = ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let sock = std::net::TcpStream::connect(addr)?;
    let mut tls = rustls::StreamOwned::new(conn, sock);

    write!(
        tls,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    tls.flush()?;
    let mut resp = String::new();
    // A clean stream close after `Connection: close` surfaces as EOF; ignore a
    // benign "close_notify not received" which some servers omit.
    match tls.read_to_string(&mut resp) {
        Ok(_) => {}
        Err(e) if !resp.is_empty() => {
            let _ = e;
        }
        Err(e) => return Err(e),
    }
    Ok(resp)
}

#[tokio::test]
async fn serves_https_and_returns_200() {
    let server = serve_tls().await;
    let addr = server.addr;

    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET /health");

    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "expected 200 over TLS, got: {resp:?}"
    );
    assert!(resp.contains("ok"), "expected body over TLS, got: {resp:?}");

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn peer_socketaddr_connect_info_flows_over_tls() {
    let server = serve_tls().await;
    let addr = server.addr;

    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/whoami"))
        .await
        .unwrap()
        .expect("HTTPS GET /whoami");

    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
    // The handler echoed the peer IP resolved from ConnectInfo<SocketAddr>.
    assert!(
        resp.contains("127.0.0.1"),
        "peer SocketAddr should flow through the TLS listener, got: {resp:?}"
    );

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn graceful_shutdown_completes_under_tls() {
    let server = serve_tls().await;
    let addr = server.addr;

    // One successful request first, so the listener is definitely serving.
    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET before shutdown");
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");

    // Trigger graceful shutdown; the serve task must join cleanly and promptly.
    server.shutdown.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(5), server.handle).await;
    let result = joined.expect("serve task should stop within the timeout");
    assert!(
        matches!(result, Ok(Ok(()))),
        "graceful shutdown under TLS should return Ok, got: {result:?}"
    );
}

#[tokio::test]
async fn bad_handshake_does_not_kill_the_listener() {
    let server = serve_tls().await;
    let addr = server.addr;

    // A plaintext client that never speaks TLS: connect, send junk, close. The
    // handshake fails inside the accept loop, which must skip it rather than
    // die.
    tokio::task::spawn_blocking(move || {
        if let Ok(mut sock) = std::net::TcpStream::connect(addr) {
            let _ = sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
            let _ = sock.flush();
        }
    })
    .await
    .unwrap();

    // The listener must still serve a subsequent, well-formed HTTPS request.
    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET after a bad handshake");
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "listener should survive a failed handshake, got: {resp:?}"
    );

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn hung_handshake_does_not_wedge_the_listener() {
    // A short handshake timeout so the stalled connection is shed quickly.
    let server = serve_tls_with_handshake_timeout(Duration::from_secs(1)).await;
    let addr = server.addr;

    // Open a raw TCP connection and send NOTHING — no ClientHello, ever. Before
    // the handshake-timeout fix this parks `acceptor.accept(...)` inside the
    // accept loop forever, denying every other client (single-connection DoS).
    // Hold the socket open for the duration of the good request below.
    let hung = std::net::TcpStream::connect(addr).expect("open raw TCP connection");

    // Immediately, while the hung socket is still connected and silent, a
    // well-behaved client must still get served. Give it a generous but bounded
    // timeout: before the fix this hangs; after, it returns 200 promptly.
    let good = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || https_get(addr, "/health")),
    )
    .await
    .expect("good request must not hang behind the stalled handshake")
    .unwrap()
    .expect("HTTPS GET /health while a handshake is stalled");

    assert!(
        good.starts_with("HTTP/1.1 200"),
        "listener must keep serving while a handshake is stalled, got: {good:?}"
    );

    // Keep the hung socket alive until after the good request completed, so the
    // test truly overlapped a stalled handshake with a real one.
    drop(hung);

    server.shutdown.cancel();
    let _ = server.handle.await;
}

// TODO(#1603 follow-up): wss (WebSocket-over-TLS) integration coverage. The
// listener yields the same `ConnectInfo<SocketAddr>` stream as the plain-TCP
// path, so a `ws` upgrade rides the identical service; wiring a full
// tokio-tungstenite-over-rustls client here is disproportionate for this first
// slice and is tracked as a follow-up.
