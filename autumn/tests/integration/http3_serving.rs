//! HTTP/3 over QUIC: the `[server.http3]` listener.
//!
//! Each test serves a real Autumn router over the real
//! [`autumn_web::http3::Http3Listener`], with the serve-boundary layers that
//! `app.rs` puts around it, and talks to it with an in-process `h3-quinn`
//! client. No container and no `curl --http3` is necessary.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::{AutumnConfig, ClientAuthMode};
use autumn_web::extract::ClientAddr;
use autumn_web::http3::{AltSvcLayer, Http3Listener, Http3Settings};
use autumn_web::test::TestApp;
use autumn_web::tls::ServerTlsParts;
use autumn_web::tls::client_auth::OptionalClientCert;
use autumn_web::{get, post, put, routes};
use bytes::{Buf as _, Bytes};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
use tokio_util::sync::CancellationToken;

use super::mtls_support::{CA_PEM, CLIENT_CERT_PEM, CLIENT_KEY_PEM, TrustFixture};
use super::tls_support::{
    CertFixture, RENEWED_CERT_PEM, RENEWED_KEY_PEM, RecordingVerifier, now_unix,
};

const DEADLINE: Duration = Duration::from_secs(20);

// ── routes ───────────────────────────────────────────────────────────────

#[get("/hello")]
async fn hello() -> &'static str {
    "hello over h3"
}

#[post("/echo")]
async fn echo(body: Bytes) -> Bytes {
    body
}

#[put("/item")]
async fn put_item() -> &'static str {
    "put"
}

#[post("/item")]
async fn post_item() -> &'static str {
    "post"
}

#[get("/whoami")]
async fn whoami(ClientAddr(addr): ClientAddr) -> String {
    addr.ip().to_string()
}

#[get("/host")]
async fn host(headers: axum::http::HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("none")
        .to_owned()
}

#[get("/slow")]
async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(600)).await;
    "drained"
}

#[get("/cert")]
async fn cert(OptionalClientCert(id): OptionalClientCert) -> String {
    id.map_or_else(|| "anonymous".to_owned(), |id| id.subject.clone())
}

// ── server ───────────────────────────────────────────────────────────────

struct H3Server {
    addr: SocketAddr,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<std::io::Result<()>>,
    resolver: Arc<autumn_web::tls::ReloadableCertResolver>,
    _cert: CertFixture,
}

impl H3Server {
    async fn shutdown(self) {
        self.shutdown.cancel();
        tokio::time::timeout(DEADLINE, self.handle)
            .await
            .expect("h3 serve stops after shutdown")
            .expect("h3 serve does not panic")
            .expect("h3 serve returns Ok");
    }
}

fn test_config() -> AutumnConfig {
    AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    }
}

/// Serve `config` + the routes over h3, with the serve-boundary layers of
/// `app.rs`: `MethodOverrideLayer` inside `TrustedProxiesLayer`.
async fn serve_h3(
    config: AutumnConfig,
    settings: Http3Settings,
    client_verifier: Option<Arc<dyn rustls::server::danger::ClientCertVerifier>>,
) -> H3Server {
    let fixture = CertFixture::write();
    let provider = autumn_web::tls::crypto_provider();
    let (resolver, _reloader) = autumn_web::tls::CertReloader::load(
        fixture.cert.clone(),
        fixture.key.clone(),
        Arc::clone(&provider),
        now_unix(),
        Duration::from_secs(60),
    )
    .expect("load cert/key");
    let parts = ServerTlsParts {
        provider,
        resolver: Arc::clone(&resolver) as Arc<dyn rustls::server::ResolvesServerCert>,
        client_verifier,
    };

    let trusted =
        autumn_web::security::TrustedProxiesLayer::from_config(&config.security.trusted_proxies);
    let router = TestApp::new()
        .config(config)
        .routes(routes![
            hello, echo, put_item, post_item, whoami, host, slow, cert
        ])
        .build()
        .into_router();
    let service = tower::Layer::layer(
        &trusted,
        tower::Layer::layer(&autumn_web::middleware::MethodOverrideLayer::new(), router),
    );

    let listener = Http3Listener::bind(
        "127.0.0.1:0".parse().unwrap(),
        parts.quic_config().expect("QUIC config"),
        settings,
    )
    .expect("bind UDP");
    let addr = listener.local_addr().expect("local addr");
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(listener.serve(service, shutdown.clone()));
    H3Server {
        addr,
        shutdown,
        handle,
        resolver,
        _cert: fixture,
    }
}

// ── client ───────────────────────────────────────────────────────────────

struct H3Client {
    send: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    verifier: Arc<RecordingVerifier>,
    _endpoint: quinn::Endpoint,
    _driver: tokio::task::JoinHandle<()>,
}

struct H3Response {
    status: u16,
    version: axum::http::Version,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn connect(addr: SocketAddr, client_cert: Option<(&str, &str)>) -> Result<H3Client, String> {
    let verifier = Arc::new(RecordingVerifier::default());
    let builder = rustls::ClientConfig::builder_with_provider(autumn_web::tls::crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&verifier) as _);
    let mut tls = match client_cert {
        Some((cert_pem, key_pem)) => {
            let chain: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                    .collect::<Result<_, _>>()
                    .unwrap();
            let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    let conn = tokio::time::timeout(
        DEADLINE,
        endpoint
            .connect(addr, "localhost")
            .map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|_| "connect timed out".to_owned())?
    .map_err(|e| e.to_string())?;
    let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .map_err(|e| e.to_string())?;
    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    Ok(H3Client {
        send,
        verifier,
        _endpoint: endpoint,
        _driver: driver,
    })
}

impl H3Client {
    async fn request(
        &mut self,
        req: axum::http::Request<()>,
        body: Option<Bytes>,
    ) -> Result<H3Response, String> {
        let fut = async {
            let mut stream = self
                .send
                .send_request(req)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(body) = body {
                stream.send_data(body).await.map_err(|e| e.to_string())?;
            }
            stream.finish().await.map_err(|e| e.to_string())?;
            let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
            let mut body = Vec::new();
            while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
                let n = chunk.remaining();
                body.extend_from_slice(&chunk.copy_to_bytes(n));
            }
            Ok(H3Response {
                status: resp.status().as_u16(),
                version: resp.version(),
                headers: resp.headers().clone(),
                body: String::from_utf8_lossy(&body).into_owned(),
            })
        };
        tokio::time::timeout(DEADLINE, fut)
            .await
            .map_err(|_| "request timed out".to_owned())?
    }

    async fn get(&mut self, path: &str) -> Result<H3Response, String> {
        self.get_with(path, &[]).await
    }

    async fn get_with(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<H3Response, String> {
        let mut req = axum::http::Request::get(format!("https://localhost{path}"));
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        self.request(req.body(()).unwrap(), None).await
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn h3_serves_a_get_request() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let resp = client.get("/hello").await.expect("GET over h3");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.version, axum::http::Version::HTTP_3);
    assert_eq!(resp.body, "hello over h3");
    server.shutdown().await;
}

#[tokio::test]
async fn h3_round_trips_a_large_request_body() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let payload: String = "autumn-".repeat(20_000);
    let req = axum::http::Request::post("https://localhost/echo")
        .header("content-type", "application/octet-stream")
        .body(())
        .unwrap();
    let resp = client
        .request(req, Some(Bytes::from(payload.clone())))
        .await
        .expect("POST over h3");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), payload.len());
    assert_eq!(resp.body, payload);
    server.shutdown().await;
}

#[tokio::test]
async fn h3_method_override_reaches_the_put_handler() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let req = axum::http::Request::post("https://localhost/item")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(())
        .unwrap();
    let resp = client
        .request(req, Some(Bytes::from_static(b"_method=PUT")))
        .await
        .expect("POST over h3");
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.body, "put", "the override must run before routing");
    server.shutdown().await;
}

#[tokio::test]
async fn h3_ignores_forwarded_for_from_an_untrusted_peer() {
    let mut config = test_config();
    config.security.trusted_proxies.trust_forwarded_headers = true;
    // The loopback peer is not in this range, so it is not a trusted proxy.
    config.security.trusted_proxies.ranges = vec!["10.0.0.0/8".to_owned()];
    let server = serve_h3(config, Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let resp = client
        .get_with("/whoami", &[("x-forwarded-for", "203.0.113.7")])
        .await
        .expect("GET over h3");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body, "127.0.0.1",
        "an untrusted peer cannot spoof its address"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn h3_honors_forwarded_for_from_a_trusted_peer() {
    let mut config = test_config();
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_proxies.ranges = vec!["127.0.0.0/8".to_owned()];
    let server = serve_h3(config, Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let resp = client
        .get_with("/whoami", &[("x-forwarded-for", "203.0.113.7")])
        .await
        .expect("GET over h3");
    assert_eq!(
        resp.body, "203.0.113.7",
        "the peer address reached the resolver"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn h3_request_carries_a_host_header_from_the_authority() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let resp = client.get("/host").await.expect("GET over h3");
    assert_eq!(resp.body, "localhost");
    server.shutdown().await;
}

#[tokio::test]
async fn h3_serves_a_renewed_certificate_on_the_next_connection() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let first = connect(server.addr, None).await.expect("h3 connects");
    let original = first.verifier.last_leaf().expect("leaf recorded");

    let renewed = autumn_web::tls::certified_key_from_pem(
        RENEWED_CERT_PEM.as_bytes(),
        RENEWED_KEY_PEM.as_bytes(),
        &autumn_web::tls::crypto_provider(),
    )
    .expect("renewed pair loads");
    let renewed_leaf = renewed.cert[0].clone();
    server.resolver.store(renewed);

    let mut second = connect(server.addr, None).await.expect("h3 reconnects");
    assert_eq!(second.get("/hello").await.expect("GET").status, 200);
    let served = second.verifier.last_leaf().expect("leaf recorded");
    assert_ne!(served, original);
    assert_eq!(
        served, renewed_leaf,
        "the endpoint serves the renewed certificate"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn h3_drains_an_in_flight_request_on_shutdown() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    let mut client = connect(server.addr, None).await.expect("h3 connects");
    let shutdown = server.shutdown.clone();
    let request = tokio::spawn(async move { client.get("/slow").await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();
    let resp = request
        .await
        .expect("client task")
        .expect("the in-flight request completes");
    assert_eq!(resp.body, "drained");
    server.shutdown().await;
}

#[tokio::test]
async fn h3_refuses_new_requests_after_shutdown() {
    let server = serve_h3(test_config(), Http3Settings::default(), None).await;
    server.shutdown.cancel();
    tokio::time::timeout(DEADLINE, server.handle)
        .await
        .expect("serve returns")
        .expect("no panic")
        .expect("Ok");
    let result = async {
        let mut client = connect(server.addr, None).await?;
        client.get("/hello").await
    };
    let result = tokio::time::timeout(Duration::from_secs(5), result).await;
    assert!(
        !matches!(result, Ok(Ok(_))),
        "a stopped endpoint must not serve requests"
    );
}

#[tokio::test]
async fn h3_caps_open_connections() {
    let mut settings = Http3Settings::default();
    settings.max_connections = Some(1);
    let server = serve_h3(test_config(), settings, None).await;
    let mut first = connect(server.addr, None).await.expect("first connects");
    assert_eq!(first.get("/hello").await.expect("GET").status, 200);
    let second = async {
        let mut client = connect(server.addr, None).await?;
        client.get("/hello").await
    };
    let second = tokio::time::timeout(Duration::from_secs(5), second).await;
    assert!(
        !matches!(second, Ok(Ok(_))),
        "a second connection over the cap must not be served"
    );
    drop(first);
    server.shutdown().await;
}

fn required_verifier(trust: &TrustFixture) -> Arc<dyn rustls::server::danger::ClientCertVerifier> {
    let (verifier, _reloader) = autumn_web::tls::client_auth::ClientTrustReloader::load(
        trust.bundle.clone(),
        None,
        ClientAuthMode::Required,
        autumn_web::tls::crypto_provider(),
        Duration::from_secs(60),
    )
    .expect("load client trust store");
    verifier
}

#[tokio::test]
async fn h3_mtls_rejects_a_client_without_a_certificate() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_h3(
        test_config(),
        Http3Settings::default(),
        Some(required_verifier(&trust)),
    )
    .await;
    let result = async {
        let mut client = connect(server.addr, None).await?;
        client.get("/cert").await
    };
    let result = tokio::time::timeout(Duration::from_secs(10), result).await;
    assert!(
        !matches!(result, Ok(Ok(ref r)) if r.status == 200),
        "UDP must not bypass required client auth"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn h3_mtls_exposes_the_verified_client_identity() {
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_h3(
        test_config(),
        Http3Settings::default(),
        Some(required_verifier(&trust)),
    )
    .await;
    let mut client = connect(server.addr, Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)))
        .await
        .expect("h3 connects with a client certificate");
    let resp = client.get("/cert").await.expect("GET over h3");
    assert_eq!(resp.status, 200);
    assert_ne!(resp.body, "anonymous", "the identity reached the handler");
    server.shutdown().await;
}

#[tokio::test]
async fn alt_svc_layer_adds_the_header() {
    use tower::ServiceExt as _;
    let router = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
    let service = tower::Layer::layer(
        &AltSvcLayer::new(axum::http::HeaderValue::from_static("h3=\":443\"; ma=60")),
        router,
    );
    let resp = service
        .oneshot(axum::http::Request::new(axum::body::Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("alt-svc").unwrap(), "h3=\":443\"; ma=60");
}

#[tokio::test]
async fn alt_svc_layer_keeps_a_header_the_app_set() {
    use tower::ServiceExt as _;
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(|| async { ([("alt-svc", "clear")], "ok") }),
    );
    let service = tower::Layer::layer(
        &AltSvcLayer::new(axum::http::HeaderValue::from_static("h3=\":443\"; ma=60")),
        router,
    );
    let resp = service
        .oneshot(axum::http::Request::new(axum::body::Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("alt-svc").unwrap(), "clear");
}

#[test]
fn settings_follow_the_config_sections() {
    let mut config = AutumnConfig::default();
    config.server.http3.max_idle_timeout_secs = 12;
    config.server.http3.max_concurrent_bidi_streams = 9;
    config.server.http.max_header_bytes = Some(32 * 1024);
    config.server.http.max_connections = Some(50);
    let limits = autumn_web::http_server::HttpLimits::from(&config.server.http);
    let settings = Http3Settings::from_config(&config.server.http3, &limits);
    assert_eq!(settings.max_idle_timeout, Duration::from_secs(12));
    assert_eq!(settings.max_concurrent_bidi_streams, 9);
    assert_eq!(settings.max_field_section_size, Some(32 * 1024));
    assert_eq!(settings.max_connections, Some(50));
}
