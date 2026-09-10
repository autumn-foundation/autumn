//! End-to-end coverage for mutual TLS on the native listener (issue #1640).
//!
//! These tests drive the REAL serving path: `TlsListener` with a rustls client
//! verifier built from a PEM CA bundle, served through `axum::serve` with the
//! same `TlsConnectInfo` connect-info, `ClientIdentityLayer` and
//! `RequireClientCertLayer` wiring `app.rs` uses. A rustls client then presents
//! (or withholds) a client certificate over a real handshake.
//!
//! The matrix the deployment guide promises:
//! valid cert → `200`; no cert, untrusted CA, revoked cert → rejected.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::ClientAuthMode;
use autumn_web::tls::TlsConnectInfo;
use autumn_web::tls::client_auth::{
    ClientCert, ClientIdentity, ClientIdentityLayer, OptionalClientCert, RequireClientCertLayer,
};
use axum::Router;
use axum::routing::get;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use super::tls_support::{CertFixture, HttpResponse, RecordingVerifier, now_unix, parse_response};

const CA_PEM: &str = include_str!("../fixtures/tls/client/ca.cert.pem");
const ROTATED_CA_PEM: &str = include_str!("../fixtures/tls/client/rotated-ca.cert.pem");
const CLIENT_CERT_PEM: &str = include_str!("../fixtures/tls/client/client.cert.pem");
const CLIENT_KEY_PEM: &str = include_str!("../fixtures/tls/client/client.key.pem");
const ROTATED_CLIENT_CERT_PEM: &str =
    include_str!("../fixtures/tls/client/rotated-client.cert.pem");
const ROTATED_CLIENT_KEY_PEM: &str = include_str!("../fixtures/tls/client/rotated-client.key.pem");
const UNTRUSTED_CERT_PEM: &str = include_str!("../fixtures/tls/client/untrusted-client.cert.pem");
const UNTRUSTED_KEY_PEM: &str = include_str!("../fixtures/tls/client/untrusted-client.key.pem");
const REVOKED_CERT_PEM: &str = include_str!("../fixtures/tls/client/revoked.cert.pem");
const REVOKED_KEY_PEM: &str = include_str!("../fixtures/tls/client/revoked.key.pem");
const CRL_PEM: &str = include_str!("../fixtures/tls/client/crl.pem");
const CRL_EMPTY_PEM: &str = include_str!("../fixtures/tls/client/crl-empty.pem");

/// How long a request may take before it counts as a wedged listener.
const DEADLINE: Duration = Duration::from_secs(30);

// ── fixtures ────────────────────────────────────────────────────────────────

/// A client-CA bundle (and optional CRL) on disk, rewritable to simulate a
/// rotation.
struct TrustFixture {
    _dir: tempfile::TempDir,
    bundle: PathBuf,
    crl: Option<PathBuf>,
    /// Bumped per rewrite so each rotation stamps a strictly later mtime than
    /// the last; two writes inside one filesystem tick would otherwise be
    /// invisible to the mtime poller.
    rotations: std::sync::atomic::AtomicI64,
}

impl TrustFixture {
    fn write(bundle_pem: &str, crl_pem: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("client-ca.pem");
        std::fs::write(&bundle, bundle_pem).expect("write bundle");
        let crl = crl_pem.map(|pem| {
            let path = dir.path().join("client-ca.crl.pem");
            std::fs::write(&path, pem).expect("write crl");
            path
        });
        Self {
            _dir: dir,
            bundle,
            crl,
            rotations: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Overwrite the bundle in place, exactly as an operator ships a rotation.
    fn rotate_bundle(&self, pem: &str) {
        std::fs::write(&self.bundle, pem).expect("rotate bundle");
        self.bump(&self.bundle);
    }

    /// Publish a new revocation list in place.
    fn publish_crl(&self, pem: &str) {
        let path = self.crl.as_ref().expect("fixture has a CRL");
        std::fs::write(path, pem).expect("publish crl");
        self.bump(path);
    }

    fn bump(&self, path: &std::path::Path) {
        let nth = self
            .rotations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let later = filetime::FileTime::from_unix_time(now_unix() + nth, 0);
        filetime::set_file_mtime(path, later).expect("set mtime");
    }
}

/// A running mTLS test server.
struct MtlsServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<std::io::Result<()>>,
    _server_cert: CertFixture,
}

impl MtlsServer {
    async fn shutdown(self) {
        self.shutdown.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("the serve task should stop within 10s of the shutdown signal")
            .expect("the serve task should not panic");
        joined.expect("graceful shutdown should return Ok");
    }
}

/// The router every test in this file serves: one open route, one route that
/// echoes the verified identity, one that requires a certificate through the
/// extractor, and one under a `required_paths` prefix.
fn test_router() -> Router {
    Router::new()
        .route("/open", get(|| async { "open" }))
        .route(
            "/whoami",
            get(|OptionalClientCert(id): OptionalClientCert| async move {
                id.map_or_else(|| "anonymous".to_owned(), |id| id.subject.clone())
            }),
        )
        .route(
            "/fingerprint",
            get(|ClientCert(id): ClientCert| async move { id.fingerprint.clone() }),
        )
        .route(
            "/sans",
            get(|ClientCert(id): ClientCert| async move { id.sans.join(" ") }),
        )
        .route("/internal/keys", get(|| async { "rotated" }))
}

/// Boot the router over a real mTLS listener, wired exactly as `app.rs` wires
/// the HTTPS arm.
async fn serve_mtls(
    trust: &TrustFixture,
    mode: ClientAuthMode,
    required_paths: Vec<String>,
) -> MtlsServer {
    let server_cert = CertFixture::write();
    let provider = autumn_web::tls::crypto_provider();
    let (resolver, _cert_reloader) = autumn_web::tls::CertReloader::load(
        server_cert.cert.clone(),
        server_cert.key.clone(),
        Arc::clone(&provider),
        now_unix(),
        Duration::from_secs(60),
    )
    .expect("load server cert");

    let (verifier, reloader) = autumn_web::tls::client_auth::ClientTrustReloader::load(
        trust.bundle.clone(),
        trust.crl.clone(),
        mode,
        Arc::clone(&provider),
        // Short enough to keep a rotation test quick, long enough not to spin.
        Duration::from_millis(25),
    )
    .expect("load client trust store");

    let server_config = autumn_web::tls::build_server_config_with_client_auth(
        Arc::clone(&provider),
        resolver as Arc<dyn rustls::server::ResolvesServerCert>,
        Some(verifier as Arc<dyn rustls::server::danger::ClientCertVerifier>),
    )
    .expect("build server config");

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = tcp.local_addr().expect("local_addr");
    let shutdown = CancellationToken::new();
    let listener = autumn_web::tls::TlsListener::new(
        tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );

    tokio::spawn(reloader.run(shutdown.child_token()));

    let service = tower::Layer::layer(
        &RequireClientCertLayer::for_paths(required_paths),
        test_router(),
    );
    let service = tower::Layer::layer(&ClientIdentityLayer, service);
    let make_service =
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
            TlsConnectInfo,
        >(service);

    let shutdown_wait = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, make_service)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await
    });

    MtlsServer {
        addr,
        shutdown,
        handle,
        _server_cert: server_cert,
    }
}

// ── client ──────────────────────────────────────────────────────────────────

/// GET `path`, optionally presenting `(cert_pem, key_pem)` as a client
/// certificate. `Err` means the handshake itself failed — which is the
/// observable for a rejected client.
async fn mtls_get(
    addr: SocketAddr,
    path: &str,
    client: Option<(&str, &str)>,
) -> std::io::Result<HttpResponse> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RecordingVerifier::default()));
    let config = match client {
        Some((cert_pem, key_pem)) => {
            let chain: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                    .collect::<Result<_, _>>()
                    .expect("parse client certificate");
            let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("parse client key");
            builder
                .with_client_auth_cert(chain, key)
                .expect("client auth cert")
        }
        None => builder.with_no_client_auth(),
    };

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from("localhost").expect("server name");

    let work = async move {
        let tcp = tokio::net::TcpStream::connect(addr).await?;
        let mut stream = connector.connect(server_name, tcp).await?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        let mut raw = Vec::new();
        if let Err(e) = stream.read_to_end(&mut raw).await
            && raw.is_empty()
        {
            return Err(e);
        }
        if raw.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed the connection without answering",
            ));
        }
        Ok(parse_response(&String::from_utf8_lossy(&raw)))
    };

    tokio::time::timeout(DEADLINE, work)
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("GET {path} did not complete within {DEADLINE:?}"),
            ))
        })
}

/// Poll `check` until it holds, or fail after `DEADLINE`.
///
/// Used only where the thing under test is a background poller: a fixed sleep
/// would either be flaky or slow, and the reloader's own interval is the only
/// timing this depends on.
async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{what} did not happen within {DEADLINE:?}");
}

/// Send one keep-alive GET on an already-open stream and read exactly its
/// response, leaving the connection usable for the next one.
///
/// `Connection: close` plus `read_to_end` cannot be reused, and this suite
/// needs two requests on ONE connection to prove a rotation does not drop it.
async fn keep_alive_request<S>(stream: &mut S, path: &str) -> HttpResponse
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write on the held connection");
    stream.flush().await.expect("flush");

    let mut raw: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let read_exactly_one_response = async {
        loop {
            let n = stream
                .read(&mut byte)
                .await
                .expect("read on the held connection");
            assert!(n != 0, "the connection closed mid-response: {raw:?}");
            raw.push(byte[0]);
            // Stop as soon as the headers plus a full `Content-Length` body are
            // in hand — reading one more byte would block until the next
            // response.
            let text = String::from_utf8_lossy(&raw);
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .expect("the test handlers all send a content-length");
                if body.len() >= length {
                    return text.into_owned();
                }
            }
        }
    };
    let text = tokio::time::timeout(DEADLINE, read_exactly_one_response)
        .await
        .unwrap_or_else(|_| panic!("GET {path} on the held connection did not answer"));
    parse_response(&text)
}

// ── the matrix ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_valid_client_certificate_is_accepted_and_its_identity_reaches_the_handler() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    let response = mtls_get(
        server.addr,
        "/whoami",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("a certificate from the trusted CA should complete the handshake");
    assert_eq!(response.status, 200);
    assert!(
        response.body.contains("CN=svc-orders"),
        "the handler should see the verified subject DN, got {:?}",
        response.body
    );

    server.shutdown().await;
}

#[tokio::test]
async fn no_certificate_is_rejected_on_a_required_listener() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    let outcome = mtls_get(server.addr, "/open", None).await;
    assert!(
        outcome.is_err(),
        "a required listener must reject a client that presents no certificate, got {outcome:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_certificate_from_an_untrusted_ca_is_rejected() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    let outcome = mtls_get(
        server.addr,
        "/open",
        Some((UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM)),
    )
    .await;
    assert!(
        outcome.is_err(),
        "a certificate that chains to no configured CA must be rejected, got {outcome:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_revoked_certificate_is_rejected() {
    let trust = TrustFixture::write(CA_PEM, Some(CRL_PEM));
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    // The same CA signed both, so this proves the CRL — not the trust store —
    // is what rejects it.
    let accepted = mtls_get(
        server.addr,
        "/open",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await;
    assert!(
        accepted.is_ok(),
        "the un-revoked sibling certificate must still be accepted, got {accepted:?}"
    );

    let outcome = mtls_get(
        server.addr,
        "/open",
        Some((REVOKED_CERT_PEM, REVOKED_KEY_PEM)),
    )
    .await;
    assert!(
        outcome.is_err(),
        "a certificate listed in the CRL must be rejected, got {outcome:?}"
    );

    server.shutdown().await;
}

// ── modes ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn optional_mode_serves_clients_with_and_without_a_certificate() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Optional, Vec::new()).await;

    let anonymous = mtls_get(server.addr, "/whoami", None)
        .await
        .expect("optional mode must still serve a client with no certificate");
    assert_eq!(anonymous.status, 200);
    assert_eq!(anonymous.body, "anonymous");

    let identified = mtls_get(
        server.addr,
        "/whoami",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("optional mode must accept a valid certificate");
    assert!(identified.body.contains("CN=svc-orders"));

    server.shutdown().await;
}

#[tokio::test]
async fn optional_mode_still_rejects_a_certificate_from_an_untrusted_ca() {
    // "Optional" is about *presenting* one, not about *verifying* it: a client
    // that offers a certificate must still pass the trust store.
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Optional, Vec::new()).await;

    let outcome = mtls_get(
        server.addr,
        "/open",
        Some((UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM)),
    )
    .await;
    assert!(
        outcome.is_err(),
        "an untrusted certificate must be rejected even in optional mode, got {outcome:?}"
    );

    server.shutdown().await;
}

// ── per-route requirement ───────────────────────────────────────────────────

#[tokio::test]
async fn a_required_path_rejects_an_anonymous_request_with_a_json_envelope() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(
        &trust,
        ClientAuthMode::Optional,
        vec!["/internal/".to_owned()],
    )
    .await;

    // A public route on the same process is untouched.
    let open = mtls_get(server.addr, "/open", None)
        .await
        .expect("the public route must still serve an anonymous client");
    assert_eq!(open.status, 200);

    let denied = mtls_get(server.addr, "/internal/keys", None)
        .await
        .expect("the connection itself succeeds; the route rejects");
    assert_eq!(
        denied.status, 403,
        "an mTLS-only route must reject with the documented status"
    );
    let body: serde_json::Value =
        serde_json::from_str(&denied.body).expect("the rejection must carry a JSON error envelope");
    assert_eq!(body["status"], 403, "envelope was {body}");
    assert_eq!(body["code"], "autumn.forbidden", "envelope was {body}");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|m| m.contains("client certificate")),
        "envelope should name the missing credential: {body}"
    );

    let allowed = mtls_get(
        server.addr,
        "/internal/keys",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("a verified client reaches the mTLS-only route");
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.body, "rotated");

    server.shutdown().await;
}

#[tokio::test]
async fn a_required_prefix_does_not_leak_onto_a_sibling_path() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(
        &trust,
        ClientAuthMode::Optional,
        // No trailing slash: `/internal` must not capture `/internal-tools`.
        vec!["/internal".to_owned()],
    )
    .await;

    let denied = mtls_get(server.addr, "/internal/keys", None)
        .await
        .expect("connection succeeds");
    assert_eq!(denied.status, 403);

    // A dot-segment cannot escape the requirement: the layer matches the
    // normalized path, so this still resolves to `/internal/keys`.
    let sneaky = mtls_get(server.addr, "/open/../internal/keys", None)
        .await
        .expect("connection succeeds");
    assert_eq!(
        sneaky.status, 403,
        "a dot-segment path must not slip past the mTLS requirement"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn the_client_cert_extractor_rejects_an_anonymous_request() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Optional, Vec::new()).await;

    let denied = mtls_get(server.addr, "/fingerprint", None)
        .await
        .expect("connection succeeds");
    assert_eq!(denied.status, 403);

    let allowed = mtls_get(
        server.addr,
        "/fingerprint",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("a verified client is accepted");
    assert_eq!(allowed.status, 200);
    assert!(
        allowed.body.starts_with("sha256:"),
        "the extractor should yield the certificate fingerprint, got {:?}",
        allowed.body
    );

    server.shutdown().await;
}

#[tokio::test]
async fn the_identity_carries_the_sans_a_policy_would_key_on() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    let response = mtls_get(
        server.addr,
        "/sans",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("a verified client is accepted");
    assert!(
        response
            .body
            .contains("URI:spiffe://autumn.test/svc/orders"),
        "the SPIFFE-style URI SAN should reach the handler, got {:?}",
        response.body
    );
    assert!(response.body.contains("DNS:svc-orders.internal"));

    server.shutdown().await;
}

// ── rotation ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_ca_rotation_lands_without_a_restart_and_without_dropping_connections() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    // Before: only the original CA is trusted.
    assert!(
        mtls_get(
            server.addr,
            "/open",
            Some((ROTATED_CLIENT_CERT_PEM, ROTATED_CLIENT_KEY_PEM))
        )
        .await
        .is_err(),
        "the new CA is not trusted yet"
    );

    // Step 1 of a rotation: ship old + new in one bundle.
    trust.rotate_bundle(&format!("{CA_PEM}{ROTATED_CA_PEM}"));
    eventually("the new CA becomes trusted", async || {
        mtls_get(
            server.addr,
            "/open",
            Some((ROTATED_CLIENT_CERT_PEM, ROTATED_CLIENT_KEY_PEM)),
        )
        .await
        .is_ok()
    })
    .await;

    // The old CA still works during the overlap — that is what makes the
    // rotation non-disruptive.
    assert!(
        mtls_get(
            server.addr,
            "/open",
            Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM))
        )
        .await
        .is_ok(),
        "the outgoing CA must keep working through the overlap window"
    );

    // Step 2: drop the old CA.
    trust.rotate_bundle(ROTATED_CA_PEM);
    eventually("the old CA stops being trusted", async || {
        mtls_get(
            server.addr,
            "/open",
            Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
        )
        .await
        .is_err()
    })
    .await;
    assert!(
        mtls_get(
            server.addr,
            "/open",
            Some((ROTATED_CLIENT_CERT_PEM, ROTATED_CLIENT_KEY_PEM))
        )
        .await
        .is_ok(),
        "the incoming CA keeps working after the old one is dropped"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_established_connection_survives_a_rotation_that_would_reject_it() {
    // The point of swapping the verifier rather than rebuilding the listener:
    // rustls verifies once, at handshake time, so a connection opened under the
    // old trust store keeps serving after the rotation removes its CA.
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(CLIENT_CERT_PEM.as_bytes())
            .collect::<Result<_, _>>()
            .expect("parse client certificate");
    let key = PrivateKeyDer::from_pem_slice(CLIENT_KEY_PEM.as_bytes()).expect("parse client key");
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RecordingVerifier::default()))
        .with_client_auth_cert(chain, key)
        .expect("client auth cert");
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(server.addr)
        .await
        .expect("connect");
    let mut held = connector
        .connect(ServerName::try_from("localhost").expect("server name"), tcp)
        .await
        .expect("handshake under the original trust store");

    // Drive one keep-alive request through BEFORE rotating. Under TLS 1.3 the
    // client's `connect` returns once it has sent its own Finished — the server
    // verifies the certificate afterwards — so without a completed round-trip
    // the rotation could land mid-handshake and be judged against the NEW store.
    // A served response proves the server finished verifying under the old one.
    let first = keep_alive_request(&mut held, "/open").await;
    assert_eq!(first.status, 200);

    // Rotate the old CA out entirely while the connection is open.
    trust.rotate_bundle(ROTATED_CA_PEM);
    eventually(
        "the rotation takes effect for new connections",
        async || {
            mtls_get(
                server.addr,
                "/open",
                Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
            )
            .await
            .is_err()
        },
    )
    .await;

    // The already-established connection still serves.
    let after = keep_alive_request(&mut held, "/open").await;
    assert_eq!(
        after.status, 200,
        "a rotation must not drop an established connection"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn publishing_a_revocation_takes_effect_without_a_restart() {
    let trust = TrustFixture::write(CA_PEM, Some(CRL_EMPTY_PEM));
    let server = serve_mtls(&trust, ClientAuthMode::Required, Vec::new()).await;

    assert!(
        mtls_get(
            server.addr,
            "/open",
            Some((REVOKED_CERT_PEM, REVOKED_KEY_PEM))
        )
        .await
        .is_ok(),
        "nothing is revoked yet"
    );

    trust.publish_crl(CRL_PEM);
    eventually("the newly published revocation takes effect", async || {
        mtls_get(
            server.addr,
            "/open",
            Some((REVOKED_CERT_PEM, REVOKED_KEY_PEM)),
        )
        .await
        .is_err()
    })
    .await;

    assert!(
        mtls_get(
            server.addr,
            "/open",
            Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM))
        )
        .await
        .is_ok(),
        "revoking one certificate must not revoke its siblings"
    );

    server.shutdown().await;
}

// ── parity with the server-only path ────────────────────────────────────────

#[tokio::test]
async fn the_peer_address_still_reaches_connect_info_under_mtls() {
    // The HTTPS arm swapped `ConnectInfo<SocketAddr>` for `TlsConnectInfo`;
    // `ClientIdentityLayer` re-stamps the plain peer address so trusted-proxy
    // resolution, `ClientAddr` and rate limiting behave as on plain TCP. This
    // is the regression test for that swap.
    let trust = TrustFixture::write(CA_PEM, None);
    let server_cert = CertFixture::write();
    let provider = autumn_web::tls::crypto_provider();
    let (resolver, _reloader) = autumn_web::tls::CertReloader::load(
        server_cert.cert.clone(),
        server_cert.key.clone(),
        Arc::clone(&provider),
        now_unix(),
        Duration::from_secs(60),
    )
    .expect("load server cert");
    let (verifier, _trust_reloader) = autumn_web::tls::client_auth::ClientTrustReloader::load(
        trust.bundle.clone(),
        None,
        ClientAuthMode::Required,
        Arc::clone(&provider),
        Duration::from_secs(60),
    )
    .expect("load trust store");
    let server_config = autumn_web::tls::build_server_config_with_client_auth(
        Arc::clone(&provider),
        resolver as Arc<dyn rustls::server::ResolvesServerCert>,
        Some(verifier as Arc<dyn rustls::server::danger::ClientCertVerifier>),
    )
    .expect("build server config");

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("local_addr");
    let shutdown = CancellationToken::new();
    let listener = autumn_web::tls::TlsListener::new(
        tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );

    let router = Router::new().route(
        "/peer",
        get(
            |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>| async move {
                peer.ip().to_string()
            },
        ),
    );
    let service = tower::Layer::layer(&ClientIdentityLayer, router);
    let make_service =
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
            TlsConnectInfo,
        >(service);
    let shutdown_wait = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, make_service)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await
    });

    let response = mtls_get(addr, "/peer", Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)))
        .await
        .expect("request");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body, "127.0.0.1",
        "ConnectInfo<SocketAddr> must still resolve the real TCP peer"
    );

    MtlsServer {
        addr,
        shutdown,
        handle,
        _server_cert: server_cert,
    }
    .shutdown()
    .await;
}

// ── policy context ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_verified_identity_reaches_a_policy_context_built_inside_the_handler() {
    // `#[authorize]` builds its `PolicyContext` deep inside the handler, so the
    // identity has to be ambient rather than an argument. This asserts the
    // task-local scope `ClientIdentityLayer` establishes reaches that far.
    let trust = TrustFixture::write(CA_PEM, None);
    let server_cert = CertFixture::write();
    let provider = autumn_web::tls::crypto_provider();
    let (resolver, _reloader) = autumn_web::tls::CertReloader::load(
        server_cert.cert.clone(),
        server_cert.key.clone(),
        Arc::clone(&provider),
        now_unix(),
        Duration::from_secs(60),
    )
    .expect("load server cert");
    let (verifier, _trust_reloader) = autumn_web::tls::client_auth::ClientTrustReloader::load(
        trust.bundle.clone(),
        None,
        ClientAuthMode::Required,
        Arc::clone(&provider),
        Duration::from_secs(60),
    )
    .expect("load trust store");
    let server_config = autumn_web::tls::build_server_config_with_client_auth(
        Arc::clone(&provider),
        resolver as Arc<dyn rustls::server::ResolvesServerCert>,
        Some(verifier as Arc<dyn rustls::server::danger::ClientCertVerifier>),
    )
    .expect("build server config");

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("local_addr");
    let shutdown = CancellationToken::new();
    let listener = autumn_web::tls::TlsListener::new(
        tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );

    let router = Router::new().route(
        "/policy",
        get(|| async {
            // Read the ambient identity exactly as `PolicyContext::from_session`
            // does, without needing an `AppState` in this suite.
            let identity: Option<Arc<ClientIdentity>> =
                autumn_web::tls::client_auth::current_client_identity();
            identity.map_or_else(
                || "no-identity".to_owned(),
                |id| id.common_name().unwrap_or("no-cn").to_owned(),
            )
        }),
    );
    let service = tower::Layer::layer(&ClientIdentityLayer, router);
    let make_service =
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
            TlsConnectInfo,
        >(service);
    let shutdown_wait = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, make_service)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await
    });

    let response = mtls_get(addr, "/policy", Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)))
        .await
        .expect("request");
    assert_eq!(response.body, "svc-orders");

    MtlsServer {
        addr,
        shutdown,
        handle,
        _server_cert: server_cert,
    }
    .shutdown()
    .await;
}
