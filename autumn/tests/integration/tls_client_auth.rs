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
//!
//! The rejection COUNTERS live in the sibling `tls_client_auth_metrics` binary:
//! the metric registry is process-global and bounded, so a suite asserting on
//! it cannot share a process with ~1700 other tests.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::ClientAuthMode;
use autumn_web::tls::TlsConnectInfo;
use autumn_web::tls::client_auth::{ClientIdentity, ClientIdentityLayer};
use axum::Router;
use axum::routing::get;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject as _};
use tokio_util::sync::CancellationToken;

use super::mtls_support::{
    CA_PEM, CLIENT_CERT_PEM, CLIENT_KEY_PEM, CRL_EMPTY_PEM, CRL_PEM, MtlsServer, REVOKED_CERT_PEM,
    REVOKED_KEY_PEM, ROTATED_CA_PEM, ROTATED_CLIENT_CERT_PEM, ROTATED_CLIENT_KEY_PEM, TrustFixture,
    UNTRUSTED_CERT_PEM, UNTRUSTED_KEY_PEM, eventually, keep_alive_request, mtls_get,
    mtls_get_with_headers, serve_mtls,
};
use super::tls_support::{CertFixture, RecordingVerifier, now_unix};

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

    let sibling = mtls_get(server.addr, "/internal-tools", None)
        .await
        .expect("connection succeeds");
    assert_eq!(
        sibling.status, 200,
        "`/internal` must not capture `/internal-tools`"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_trailing_slash_prefix_still_covers_the_bare_route() {
    // `required_paths = ["/internal/"]` with a handler at exactly `/internal`.
    // Matching it the way a CSRF *exemption* matches would leave that route
    // open — an exemption failing narrow still validates, a requirement failing
    // narrow does not.
    let trust = TrustFixture::write(CA_PEM, None);
    let server = serve_mtls(
        &trust,
        ClientAuthMode::Optional,
        vec!["/internal/".to_owned()],
    )
    .await;

    let denied = mtls_get(server.addr, "/internal", None)
        .await
        .expect("connection succeeds");
    assert_eq!(
        denied.status, 403,
        "the index route under a required prefix must demand a certificate too"
    );

    let allowed = mtls_get(
        server.addr,
        "/internal",
        Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM)),
    )
    .await
    .expect("a verified client is accepted");
    assert_eq!(allowed.status, 200);

    let sibling = mtls_get(server.addr, "/internal-tools", None)
        .await
        .expect("connection succeeds");
    assert_eq!(sibling.status, 200, "the sibling stays public");

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

// ── composition with session auth ───────────────────────────────────────────

#[tokio::test]
async fn machine_identity_and_session_auth_guard_the_same_route_independently() {
    // AC: mTLS composes with `Auth<T>`/`RequireAuth` on one router without
    // conflict. They answer different questions — `RequireAuth` reads the
    // request's session, `RequireClientCert` reads the connection — so a route
    // carrying both must demand BOTH, and neither may swallow the other's
    // rejection.
    use std::collections::HashMap;

    use autumn_web::auth::RequireAuth;
    use autumn_web::session::Session;

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
        // `optional`, so the route-level guards — not the handshake — decide.
        ClientAuthMode::Optional,
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

    // A session is stamped only when the request asks for one, standing in for
    // the session layer without booting an `AppState`.
    let router = Router::new()
        .route("/internal/both", get(|| async { "both" }))
        .layer(RequireAuth::new("user_id"))
        .layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                if req.headers().contains_key("x-test-login") {
                    let mut data = HashMap::new();
                    data.insert("user_id".to_owned(), "42".to_owned());
                    let mut req = req;
                    req.extensions_mut()
                        .insert(Session::new_for_test(String::new(), data));
                    return next.run(req).await;
                }
                next.run(req).await
            },
        ));
    let service = tower::Layer::layer(
        &autumn_web::tls::client_auth::RequireClientCertLayer::for_paths(vec![
            "/internal/".to_owned(),
        ]),
        router,
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

    let cert = Some((CLIENT_CERT_PEM, CLIENT_KEY_PEM));
    // Both guards satisfied.
    let ok = mtls_get_with_headers(addr, "/internal/both", cert, "x-test-login: 1\r\n")
        .await
        .expect("request");
    assert_eq!(ok.status, 200, "both guards satisfied should serve");
    assert_eq!(ok.body, "both");

    // Machine identity without a session: `RequireAuth` rejects, and its
    // rejection is not masked by the mTLS layer.
    let no_session = mtls_get_with_headers(addr, "/internal/both", cert, "")
        .await
        .expect("request");
    assert_eq!(
        no_session.status, 401,
        "a verified machine is still not a logged-in user"
    );

    // A session without a certificate: the mTLS layer rejects, and its
    // rejection is not masked by `RequireAuth`.
    let anonymous: Option<(&str, &str)> = None;
    let no_cert = mtls_get_with_headers(addr, "/internal/both", anonymous, "x-test-login: 1\r\n")
        .await
        .expect("request");
    assert_eq!(
        no_cert.status, 403,
        "a logged-in user still needs the client certificate"
    );

    // Neither: the outer guard answers first, and it is the mTLS one.
    let neither = mtls_get_with_headers(addr, "/internal/both", anonymous, "")
        .await
        .expect("request");
    assert_eq!(neither.status, 403);

    MtlsServer {
        addr,
        shutdown,
        handle,
        _server_cert: server_cert,
    }
    .shutdown()
    .await;
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
