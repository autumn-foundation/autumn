//! Shared fixtures, server and client helpers for the mutual-TLS suites
//! (issue #1640).
//!
//! Lives beside `tls_support` rather than inside `tls_client_auth` because two
//! binaries need it: the consolidated suite, and the metrics suite, which
//! asserts on the process-global metric registry and so must run in a process
//! of its own (see CLAUDE.md, "Process-global state needs its own binary").

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::ClientAuthMode;
use autumn_web::tls::TlsConnectInfo;
use autumn_web::tls::client_auth::{
    ClientCert, ClientIdentityLayer, OptionalClientCert, RequireClientCertLayer,
};
use axum::Router;
use axum::routing::get;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use super::tls_support::{CertFixture, HttpResponse, RecordingVerifier, now_unix, parse_response};

pub const CA_PEM: &str = include_str!("../fixtures/tls/client/ca.cert.pem");
pub const ROTATED_CA_PEM: &str = include_str!("../fixtures/tls/client/rotated-ca.cert.pem");
pub const CLIENT_CERT_PEM: &str = include_str!("../fixtures/tls/client/client.cert.pem");
pub const CLIENT_KEY_PEM: &str = include_str!("../fixtures/tls/client/client.key.pem");
pub const ROTATED_CLIENT_CERT_PEM: &str =
    include_str!("../fixtures/tls/client/rotated-client.cert.pem");
pub const ROTATED_CLIENT_KEY_PEM: &str =
    include_str!("../fixtures/tls/client/rotated-client.key.pem");
pub const UNTRUSTED_CERT_PEM: &str =
    include_str!("../fixtures/tls/client/untrusted-client.cert.pem");
pub const UNTRUSTED_KEY_PEM: &str = include_str!("../fixtures/tls/client/untrusted-client.key.pem");
pub const REVOKED_CERT_PEM: &str = include_str!("../fixtures/tls/client/revoked.cert.pem");
pub const REVOKED_KEY_PEM: &str = include_str!("../fixtures/tls/client/revoked.key.pem");
pub const CRL_PEM: &str = include_str!("../fixtures/tls/client/crl.pem");
pub const CRL_EMPTY_PEM: &str = include_str!("../fixtures/tls/client/crl-empty.pem");

/// How long a request may take before it counts as a wedged listener.
pub const DEADLINE: Duration = Duration::from_secs(30);

// ── fixtures ────────────────────────────────────────────────────────────────

/// A client-CA bundle (and optional CRL) on disk, rewritable to simulate a
/// rotation.
pub struct TrustFixture {
    _dir: tempfile::TempDir,
    pub bundle: PathBuf,
    pub crl: Option<PathBuf>,
    /// Bumped per rewrite so each rotation stamps a strictly later mtime than
    /// the last; two writes inside one filesystem tick would otherwise be
    /// invisible to the mtime poller.
    rotations: std::sync::atomic::AtomicI64,
}

impl TrustFixture {
    pub fn write(bundle_pem: &str, crl_pem: Option<&str>) -> Self {
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
    pub fn rotate_bundle(&self, pem: &str) {
        std::fs::write(&self.bundle, pem).expect("rotate bundle");
        self.bump(&self.bundle);
    }

    /// Publish a new revocation list in place.
    pub fn publish_crl(&self, pem: &str) {
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
pub struct MtlsServer {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub handle: tokio::task::JoinHandle<std::io::Result<()>>,
    pub _server_cert: CertFixture,
}

impl MtlsServer {
    pub async fn shutdown(self) {
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
pub fn test_router() -> Router {
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
        // An index handler mounted at exactly the configured prefix. A
        // `required_paths = ["/internal/"]` that left this open was the
        // fail-open the requirement rule was fixed for.
        .route("/internal", get(|| async { "index" }))
        // A sibling that must NOT be captured by the `/internal` prefix.
        .route("/internal-tools", get(|| async { "tools" }))
}

/// Boot the router over a real mTLS listener, wired exactly as `app.rs` wires
/// the HTTPS arm.
pub async fn serve_mtls(
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
pub async fn mtls_get(
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
pub async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
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
pub async fn keep_alive_request<S>(stream: &mut S, path: &str) -> HttpResponse
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
