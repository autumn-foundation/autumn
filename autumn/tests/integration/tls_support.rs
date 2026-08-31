//! Shared client/server helpers for the direct-HTTPS (`[server.tls]`) suites
//! (issue #1603).
//!
//! Both TLS suites drive the REAL serving path — the public
//! [`autumn_web::tls::TlsListener`], the same `axum::serve::Listener` the app
//! binds when `[server.tls]` is set — with the same connect-info and
//! graceful-shutdown wiring `app.rs` uses. This module owns the pieces they
//! share: the certificate fixtures, a rustls client that trusts them, and the
//! two spawn helpers (TLS and plain TCP) used for like-for-like parity checks.
//!
//! The certificate fixture is a self-signed `CN=localhost` (SAN `localhost` +
//! `127.0.0.1`) valid until 2126, shared with the `tls` module's unit tests.
//! `RENEWED_CERT_PEM`/`RENEWED_KEY_PEM` is a second, distinct pair with the
//! same subject — the stand-in for what `certbot` writes over the live files
//! at renewal time.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::serve::ListenerExt as _;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

/// A handshake timeout generous enough never to fire in the well-behaved-client
/// suites (the shed-a-stalled-handshake path has its own test in
/// `tls_serving.rs`).
pub const RECORDING_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub const CERT_PEM: &str = include_str!("../fixtures/tls/localhost.cert.pem");
pub const KEY_PEM: &str = include_str!("../fixtures/tls/localhost.key.pem");
pub const RENEWED_CERT_PEM: &str = include_str!("../fixtures/tls/localhost-renewed.cert.pem");
pub const RENEWED_KEY_PEM: &str = include_str!("../fixtures/tls/localhost-renewed.key.pem");

/// Wall-clock seconds since the epoch, the shape `tls::load_certified_key`
/// takes for its validity-window check.
pub fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// A client-side verifier that accepts any server certificate and records the
/// leaf it was offered.
///
/// The fixture is a self-signed CA cert (so rustls' path validation would
/// reject it as a leaf, `CaUsedAsEndEntity`); these suites exercise the TLS
/// *transport*, not identity verification, so danger-accepting the cert is the
/// documented approach. Recording the leaf lets a test assert *which*
/// certificate the server served — the observable a hot reload has to change.
#[derive(Debug, Default)]
pub struct RecordingVerifier {
    seen: Mutex<Option<CertificateDer<'static>>>,
}

impl RecordingVerifier {
    /// The leaf certificate the server presented on the most recent handshake.
    pub fn last_leaf(&self) -> Option<CertificateDer<'static>> {
        self.seen.lock().unwrap().clone()
    }
}

impl rustls::client::danger::ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(end_entity.clone().into_owned());
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

/// A cert/key pair on disk, rewritable in place to simulate a renewal.
pub struct CertFixture {
    /// Kept alive so the directory (and the PEM files in it) outlive the
    /// fixture; never read directly.
    _dir: tempfile::TempDir,
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
    /// Number of renewals so far, so each one stamps a strictly later mtime
    /// than the last (two renewals inside one clock second would otherwise be
    /// indistinguishable to an mtime poller).
    renewals: std::sync::atomic::AtomicI64,
}

impl CertFixture {
    /// Write the long-lived `localhost` fixture pair into a fresh tempdir.
    pub fn write() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        Self {
            _dir: dir,
            cert,
            key,
            renewals: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Overwrite the pair in place, exactly as `certbot` rewrites
    /// `fullchain.pem`/`privkey.pem` at renewal.
    ///
    /// The mtimes are bumped explicitly: a renewal that lands inside the same
    /// filesystem timestamp granularity as the original write would otherwise
    /// be invisible to an mtime-polling reloader on a coarse-resolution
    /// filesystem, making the test flaky rather than the reload wrong.
    pub fn renew(&self, cert_pem: &str, key_pem: &str) {
        std::fs::write(&self.cert, cert_pem).unwrap();
        std::fs::write(&self.key, key_pem).unwrap();
        let nth = self
            .renewals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let later = filetime::FileTime::from_unix_time(now_unix() + nth, 0);
        filetime::set_file_mtime(&self.cert, later).unwrap();
        filetime::set_file_mtime(&self.key, later).unwrap();
    }

    /// Overwrite the certificate with bytes that are not a certificate at all —
    /// the "operator (or renewal tool) wrote garbage" case a reload must
    /// survive without dropping the site.
    pub fn corrupt_cert(&self) {
        self.renew("-----BEGIN CERTIFICATE-----\nnot a certificate\n", KEY_PEM);
    }
}

/// A running test server plus the knobs to talk to and stop it.
pub struct TestServer {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub handle: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    /// Cancel the graceful-shutdown token and require `axum::serve` to return
    /// `Ok(())` promptly.
    ///
    /// The result is asserted, not discarded: a serve task that panicked, errored
    /// or refused to drain is a failure of the thing these suites exist to prove,
    /// and swallowing it would turn that into a silently slower test.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("the serve task should stop within 10s of the shutdown signal")
            .expect("the serve task should not panic");
        joined.expect("graceful shutdown should return Ok");
    }
}

/// Serve `router` over the real [`TlsListener`](autumn_web::tls::TlsListener),
/// wired exactly as `app.rs` wires the HTTPS arm.
///
/// Returns the bound address, the shutdown token, the serve task, and the
/// resolver, so a caller can drive a certificate swap.
///
/// No settle delay is needed before connecting: the socket is listening from
/// the moment it is bound (below, before the serve task is spawned), so a
/// client that connects first simply waits in the kernel backlog until the
/// task is polled. A sleep here would only add latency — and a probe
/// connection would leave a stray half-open handshake behind.
pub async fn serve_tls_router(
    router: Router,
    fixture: &CertFixture,
    handshake_timeout: Duration,
) -> (TestServer, Arc<autumn_web::tls::ReloadableCertResolver>) {
    let provider = autumn_web::tls::crypto_provider();
    let certified =
        autumn_web::tls::load_certified_key(&fixture.cert, &fixture.key, &provider, now_unix())
            .expect("load cert/key");
    let resolver = Arc::new(autumn_web::tls::ReloadableCertResolver::new(certified));
    let server_config =
        autumn_web::tls::build_server_config(Arc::clone(&provider), Arc::clone(&resolver))
            .expect("build server config");

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = tcp.local_addr().expect("local_addr");
    let shutdown = CancellationToken::new();
    let listener = autumn_web::tls::TlsListener::new(
        tcp,
        server_config,
        handshake_timeout,
        shutdown.child_token(),
    );

    // Mirror the app.rs HTTPS serve arm: no-op tap_io wrapper so axum supplies
    // ConnectInfo<SocketAddr>, plus into_make_service_with_connect_info.
    let listener = listener.tap_io(|_io| {});
    let make_service =
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<SocketAddr>(
            router,
        );

    let shutdown_wait = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, make_service)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await
    });

    (
        TestServer {
            addr,
            shutdown,
            handle,
        },
        resolver,
    )
}

/// Serve `router` over a plain TCP listener with the same connect-info and
/// graceful-shutdown wiring — the control arm for HTTPS/HTTP parity checks.
pub async fn serve_plain_router(router: Router) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
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

    TestServer {
        addr,
        shutdown,
        handle,
    }
}

/// Open a TLS connection to `addr`, trusting the fixture certificate and
/// recording the leaf the server offered.
pub async fn tls_connect(
    addr: SocketAddr,
    verifier: Arc<RecordingVerifier>,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    // SNI uses the certificate's `localhost` SAN even though we dial 127.0.0.1.
    let server_name = ServerName::try_from("localhost").unwrap();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    connector.connect(server_name, tcp).await
}

/// A parsed HTTP/1.1 response: status code, headers (lowercased names), and the
/// de-chunked body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// How long a single request helper may take before it is treated as a failure.
///
/// Every request in these suites is a loopback call to a handler that returns
/// within a second, so anything near this bound is a wedged listener — the exact
/// bug class `tls_serving.rs` exists to catch. Without it, `cargo test` has no
/// per-test timeout and such a bug hangs the CI job instead of failing it.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

/// Write a `Connection: close` GET and read the whole response.
async fn get_over<S>(stream: &mut S, path: &str, extra_headers: &str) -> std::io::Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra_headers}\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut raw = Vec::new();
    // A clean TLS close after `Connection: close` can surface as an
    // "unexpected EOF" from peers that omit close_notify; bytes already read
    // are still the complete response, so only a read that produced NOTHING is
    // a real error.
    if let Err(e) = stream.read_to_end(&mut raw).await
        && raw.is_empty()
    {
        return Err(e);
    }
    if raw.is_empty() {
        // Surface "the server closed without answering" as an error naming that,
        // rather than as a parse panic about a missing header/body separator.
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("server closed the connection without sending a response to GET {path}"),
        ));
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Fail with a timeout error rather than hanging forever.
async fn within_deadline<F>(what: &str, future: F) -> std::io::Result<HttpResponse>
where
    F: Future<Output = std::io::Result<HttpResponse>>,
{
    tokio::time::timeout(REQUEST_DEADLINE, future)
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{what} did not complete within {REQUEST_DEADLINE:?}"),
            ))
        })
}

/// GET `path` over HTTPS and return the parsed response.
pub async fn https_get(
    addr: SocketAddr,
    path: &str,
    verifier: Arc<RecordingVerifier>,
) -> std::io::Result<HttpResponse> {
    https_get_with_headers(addr, path, "", verifier).await
}

/// GET `path` over HTTPS with extra request headers (each `Name: value\r\n`).
pub async fn https_get_with_headers(
    addr: SocketAddr,
    path: &str,
    extra_headers: &str,
    verifier: Arc<RecordingVerifier>,
) -> std::io::Result<HttpResponse> {
    within_deadline(&format!("HTTPS GET {path}"), async move {
        let mut stream = tls_connect(addr, verifier).await?;
        let raw = get_over(&mut stream, path, extra_headers).await?;
        Ok(parse_response(&raw))
    })
    .await
}

/// GET `path` over plain HTTP and return the parsed response.
pub async fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<HttpResponse> {
    http_get_with_headers(addr, path, "").await
}

/// GET `path` over plain HTTP with extra request headers.
pub async fn http_get_with_headers(
    addr: SocketAddr,
    path: &str,
    extra_headers: &str,
) -> std::io::Result<HttpResponse> {
    within_deadline(&format!("HTTP GET {path}"), async move {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        let raw = get_over(&mut stream, path, extra_headers).await?;
        Ok(parse_response(&raw))
    })
    .await
}

/// Parse a raw HTTP/1.1 response into status, headers, and a de-chunked body.
pub fn parse_response(raw: &str) -> HttpResponse {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("response has no header/body separator: {raw:?}"));
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {status_line:?}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    HttpResponse {
        status,
        headers,
        body,
    }
}

/// Strip HTTP/1.1 chunked-transfer framing, so a chunked body compares equal to
/// the same bytes sent with a `Content-Length`.
///
/// Strict on purpose: every malformed-framing case panics with the offending
/// input rather than returning a short body. A silent truncation here would
/// make two differently-broken responses compare *equal* in a parity test —
/// the one failure mode these suites must never have.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size_line, after)) = rest.split_once("\r\n") else {
            panic!("chunked body ended mid-frame (no chunk-size line in {rest:?})");
        };
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .unwrap_or_else(|e| panic!("unparseable chunk size {size_hex:?}: {e}"));
        if size == 0 {
            // The terminating zero-size chunk: trailers (if any) follow, and
            // this suite never sends them.
            break;
        }
        // `get` rather than a slice: the caller lossily decoded bytes to a
        // String, so a size that lands off a char boundary must fail loudly
        // instead of panicking with an opaque slice error.
        let chunk = after
            .get(..size)
            .unwrap_or_else(|| panic!("chunk of {size} bytes does not fit in {after:?}"));
        out.push_str(chunk);
        rest = after[size..]
            .strip_prefix("\r\n")
            .unwrap_or_else(|| panic!("chunk of {size} bytes was not followed by CRLF"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{dechunk, parse_response};

    #[test]
    fn parses_a_content_length_response() {
        let parsed = parse_response(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{\"status\":\"ok\"}",
        );
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, "{\"status\":\"ok\"}");
        assert_eq!(parsed.header("content-type"), Some("application/json"));
    }

    #[test]
    fn de_chunks_a_chunked_response_into_the_same_bytes() {
        let parsed = parse_response(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n7\r\ntick-0\n\r\n7\r\ntick-1\n\r\n0\r\n\r\n",
        );
        assert_eq!(parsed.status, 200);
        assert_eq!(
            parsed.body, "tick-0\ntick-1\n",
            "a chunked body must compare equal to the same bytes sent with a content-length"
        );
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn a_truncated_chunk_panics_instead_of_returning_a_short_body() {
        let _ = dechunk("7\r\ntic");
    }
}
