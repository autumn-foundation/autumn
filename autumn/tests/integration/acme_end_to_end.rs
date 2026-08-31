//! End-to-end coverage for automatic ACME provisioning and renewal (issue
//! #1608).
//!
//! Every other ACME test in the tree is a unit test *around* the order flow —
//! the renew-before decision, the health grading, the token guard, the store.
//! These tests drive the flow itself: a real `instant-acme` client, over real
//! TLS, against the [fake CA](super::acme_fake_ca), answering the challenge from
//! the app's own [`challenge_router`] and hot-swapping the issued certificate
//! into the same [`ReloadableCertResolver`] the live
//! [`TlsListener`](autumn_web::tls::TlsListener) is serving.
//!
//! Between them they cover the acceptance criteria that only an end-to-end run
//! can evidence:
//!
//! - **first boot** issues a certificate and serves it, with the CA validating
//!   HTTP-01 against the app's `:80` listener and plain HTTP redirecting to
//!   HTTPS;
//! - a **forced near-expiry** certificate rotates to a fresh one with no
//!   restart, while a connection opened before the swap keeps working;
//! - a **restart** reuses the stored account and certificate instead of
//!   re-registering or re-ordering (Let's Encrypt rate limits);
//! - the **custom directory** is honoured, and reaching a private CA depends on
//!   `ca_root_path`;
//! - a **failed** issuance lands in the health indicator and the error-reporting
//!   seam.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::acme::challenge::{Http01Tokens, challenge_router};
use autumn_web::acme::renewal::{
    AcmeHealthIndicator, AcmeRenewalTask, AcmeStatus, ReporterFn, self_signed_placeholder,
};
use autumn_web::acme::store::{AcmeStore, CertId, FsAcmeStore, StoredCert};
use autumn_web::actuator::HealthStatus;
use autumn_web::config::{AcmeConfig, AcmeDirectory};
use autumn_web::scheduler::{InProcessSchedulerCoordinator, SchedulerCoordinator};
use autumn_web::tls::{
    ReloadableCertResolver, TlsListener, build_server_config, certified_key_from_pem,
    crypto_provider, leaf_not_after_from_pem,
};
use axum::Router;
use axum::routing::get;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_util::sync::CancellationToken;

use super::acme_fake_ca::{self, FakeCa};

const DOMAIN: &str = "localhost";
const DAY: i64 = 86_400;
/// Validity the fake CA is told to issue the *renewed* certificate with, so the
/// test can prove the CA's window is what lands on the leaf rather than a
/// hard-coded default.
const RENEWED_CERT_DAYS: i64 = 60;

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs(),
    )
    .expect("unix time fits i64")
}

// ── Harness ──────────────────────────────────────────────────────────────────

/// A running app-side ACME deployment: the TLS listener the app serves, the
/// `:80` challenge listener the CA validates against, and the store both boots
/// share.
struct AcmeApp {
    https_addr: SocketAddr,
    challenge_addr: SocketAddr,
    resolver: Arc<ReloadableCertResolver>,
    tokens: Http01Tokens,
    cache_dir: tempfile::TempDir,
    shutdown: CancellationToken,
}

impl Drop for AcmeApp {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Boot the app side: bind the challenge listener, seed the resolver with
/// `initial` (the self-signed placeholder, or a pre-existing stored leaf), and
/// serve HTTPS from it — exactly the shape `build_acme_tls_listener` produces.
async fn boot_app(initial: &StoredCert) -> AcmeApp {
    let provider = crypto_provider();
    let certified = certified_key_from_pem(
        initial.chain_pem.as_bytes(),
        initial.key_pem.as_bytes(),
        &provider,
    )
    .expect("initial cert loads");
    let resolver = Arc::new(ReloadableCertResolver::new(certified));
    let server_config =
        build_server_config(Arc::clone(&provider), Arc::clone(&resolver)).expect("server config");

    let shutdown = CancellationToken::new();

    let https_tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind https");
    let https_addr = https_tcp.local_addr().expect("https local_addr");
    let listener = TlsListener::new(
        https_tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );
    let app_router = Router::new().route("/health", get(|| async { "ok" }));
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_router)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });

    // The `:80` HTTP-01 challenge + HTTP→HTTPS redirect listener. Bound on an
    // ephemeral port (CI cannot take :80); the CA is told the address, the way
    // Pebble's `-dnsserver` points validation at a test host.
    let tokens = Http01Tokens::new();
    let challenge_tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind challenge");
    let challenge_addr = challenge_tcp.local_addr().expect("challenge local_addr");
    let challenge = challenge_router(tokens.clone(), https_addr.port());
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(challenge_tcp, challenge)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    AcmeApp {
        https_addr,
        challenge_addr,
        resolver,
        tokens,
        cache_dir: tempfile::tempdir().expect("cache dir"),
        shutdown,
    }
}

/// The `[server.tls.acme]` config pointing at `ca`, cached under `app`'s dir.
fn acme_config(app: &AcmeApp, ca: &FakeCa, ca_root: Option<std::path::PathBuf>) -> AcmeConfig {
    AcmeConfig {
        domains: vec![DOMAIN.to_owned()],
        contact_email: "ops@example.com".to_owned(),
        directory: AcmeDirectory::Custom {
            url: ca.directory_url.clone(),
        },
        cache_dir: app.cache_dir.path().to_path_buf(),
        http_challenge_port: app.challenge_addr.port(),
        renew_before_days: 30,
        ca_root_path: ca_root,
    }
}

/// Write the fake CA's root out and return its path.
fn write_ca_root(app: &AcmeApp, ca: &FakeCa) -> std::path::PathBuf {
    let path = app.cache_dir.path().join("test-ca-root.pem");
    std::fs::write(&path, &ca.root_pem).expect("write CA root");
    path
}

/// Build the renewal task the app spawns, over `app`'s live resolver + tokens.
fn renewal_task(
    app: &AcmeApp,
    config: AcmeConfig,
    store: Arc<dyn AcmeStore>,
    status: AcmeStatus,
    serving_stored_cert: bool,
) -> AcmeRenewalTask {
    AcmeRenewalTask {
        resolver: Arc::clone(&app.resolver),
        provider: crypto_provider(),
        store,
        cert_id: CertId::from_domains(&config.domains),
        tokens: app.tokens.clone(),
        status,
        config,
        serving_stored_cert,
        leadership_degraded: false,
        renew_window_misconfigured: AtomicBool::new(false),
    }
}

fn fs_store(config: &AcmeConfig) -> Arc<dyn AcmeStore> {
    Arc::new(FsAcmeStore::new(
        config.cache_dir.clone(),
        autumn_web::acme::directory_label(&config.directory),
    ))
}

/// A reporter that records everything dispatched through the error-reporting seam.
fn recording_reporter() -> (ReporterFn, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let reporter: ReporterFn = Arc::new(move |msg: String| {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(msg);
    });
    (reporter, seen)
}

/// Run one boot of the renewal task to quiescence: `run` orders immediately when
/// due, so wait for the status to settle, then cancel the loop.
async fn run_one_boot(task: AcmeRenewalTask, status: &AcmeStatus, reporter: ReporterFn) {
    let coordinator: Arc<dyn SchedulerCoordinator> =
        Arc::new(InProcessSchedulerCoordinator::new("test-replica"));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(task.run(coordinator, reporter, shutdown.clone()));

    // The boot attempt either records a success or a failure; wait for whichever
    // lands rather than sleeping a fixed interval.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snap = status.snapshot();
        if snap.last_success_unix.is_some() || snap.last_failure.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ACME boot attempt never settled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    shutdown.cancel();
    join_renewal_task(handle).await;
}

/// Run a renewal task that is expected to do NOTHING, then stop it.
///
/// Used where there is no status transition to wait on: give the boot path room
/// to misbehave, then assert on what the CA did (or did not) see.
async fn run_to_quiescence(task: AcmeRenewalTask, reporter: ReporterFn) {
    let coordinator: Arc<dyn SchedulerCoordinator> =
        Arc::new(InProcessSchedulerCoordinator::new("test-replica"));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(task.run(coordinator, reporter, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    join_renewal_task(handle).await;
}

/// Stop waiting on the renewal loop, but do NOT discard the join result: a task
/// that panicked at boot would otherwise satisfy every "nothing happened"
/// assertion.
async fn join_renewal_task(handle: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("the ACME renewal task panicked: {e}"),
        // The loop is mid-sleep when cancelled; not finishing within the grace
        // period is not a failure, only a panic is.
        Err(_elapsed) => {}
    }
}

// ── TLS client ───────────────────────────────────────────────────────────────

/// A client-side verifier that records the leaf it was shown and accepts it.
/// The tests assert on the recorded chain rather than on path validation, which
/// the fake CA's root would satisfy trivially.
#[derive(Debug)]
struct CapturingVerifier {
    seen: Arc<Mutex<Option<Vec<u8>>>>,
}

impl rustls::client::danger::ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(end_entity.to_vec());
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

/// An open TLS connection to the app, kept alive across a certificate rotation.
struct LiveConnection {
    tls: rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    leaf_der: Vec<u8>,
}

impl LiveConnection {
    /// Open a keep-alive HTTPS connection and complete one request on it,
    /// capturing the leaf the server presented.
    fn open(addr: SocketAddr) -> std::io::Result<Self> {
        let seen = Arc::new(Mutex::new(None));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(CapturingVerifier {
                seen: Arc::clone(&seen),
            }))
            .with_no_client_auth();
        let server_name = ServerName::try_from(DOMAIN).expect("server name");
        let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let sock = std::net::TcpStream::connect(addr)?;
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut this = Self {
            tls: rustls::StreamOwned::new(conn, sock),
            leaf_der: Vec::new(),
        };
        this.request()?;
        this.leaf_der = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("verifier saw a leaf");
        Ok(this)
    }

    /// Issue one keep-alive request on the already-established connection.
    fn request(&mut self) -> std::io::Result<String> {
        write!(
            self.tls,
            "GET /health HTTP/1.1\r\nHost: {DOMAIN}\r\nConnection: keep-alive\r\n\r\n"
        )?;
        self.tls.flush()?;
        let mut buf = [0_u8; 1024];
        let read = self.tls.read(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf[..read]).into_owned())
    }
}

/// Open a fresh connection and return the leaf DER the server presents now.
async fn served_leaf(addr: SocketAddr) -> Vec<u8> {
    tokio::task::spawn_blocking(move || LiveConnection::open(addr).expect("open https").leaf_der)
        .await
        .expect("client task")
}

/// A plain HTTP GET against the challenge listener (status line + headers).
async fn http_get(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {DOMAIN}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    String::from_utf8_lossy(&buf).into_owned()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// AC1: with only domains + contact email configured, a fresh app obtains a
/// certificate on first startup and serves it — the CA validating HTTP-01
/// against the app's own challenge listener — and plain HTTP redirects to HTTPS.
#[tokio::test]
async fn first_boot_issues_a_certificate_and_serves_it_over_https() {
    let placeholder = self_signed_placeholder(&[DOMAIN.to_owned()]).expect("placeholder");
    let app = boot_app(&placeholder).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;
    let root = write_ca_root(&app, &ca);
    let config = acme_config(&app, &ca, Some(root));
    let store = fs_store(&config);
    let status = AcmeStatus::new();

    let placeholder_leaf = served_leaf(app.https_addr).await;

    let (reporter, reported_errors) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config.clone(),
            Arc::clone(&store),
            status.clone(),
            false,
        ),
        &status,
        reporter,
    )
    .await;

    let snap = status.snapshot();
    assert!(
        snap.last_failure.is_none(),
        "issuance failed: {:?}",
        snap.last_failure
    );
    assert!(snap.last_success_unix.is_some(), "no success recorded");
    assert!(
        reported_errors.lock().unwrap().is_empty(),
        "a clean issuance must not report an error"
    );

    // The CA really validated HTTP-01 against the app's challenge listener.
    assert_eq!(ca.state.validations_ok.load(Ordering::SeqCst), 1);
    assert_eq!(ca.state.validations_failed.load(Ordering::SeqCst), 0);
    assert_eq!(ca.state.fetched_key_authorizations().len(), 1);

    // The certificate the CA issued — not merely *a* different one — is what the
    // listener now serves, without a restart.
    let cert_id = CertId::from_domains(&config.domains);
    let stored = store
        .load_cert(&cert_id)
        .await
        .expect("read store")
        .expect("certificate persisted");
    let issued_leaf = served_leaf(app.https_addr).await;
    assert_ne!(
        issued_leaf, placeholder_leaf,
        "the self-signed placeholder is still being served"
    );
    assert_eq!(
        issued_leaf,
        leaf_der(&stored),
        "the listener is serving something other than the certificate that was issued and stored"
    );

    let not_after = leaf_not_after_from_pem(stored.chain_pem.as_bytes()).expect("leaf notAfter");
    assert!(
        not_after > now_unix() + 60 * DAY,
        "issued leaf should carry the CA's full validity window"
    );

    // The published token is withdrawn once the order completes, rather than
    // accumulating in the map across renewals. Derive it from the key
    // authorization the CA actually fetched (`{token}.{thumbprint}`) so this
    // does not depend on how the fake CA names its tokens.
    let fetched = ca.state.fetched_key_authorizations();
    let issued_token = fetched[0]
        .split_once('.')
        .expect("a key authorization is token.thumbprint")
        .0;
    assert!(
        app.tokens.get(issued_token).is_none(),
        "the HTTP-01 token {issued_token} was left published after issuance"
    );

    // AC1's other half: plain HTTP is redirected to HTTPS on the same listener.
    let redirect = http_get(app.challenge_addr, "/dashboard?page=2").await;
    assert!(
        redirect.starts_with("HTTP/1.1 308"),
        "expected a 308 redirect, got: {redirect}"
    );
    assert!(
        redirect.contains(&format!(
            "location: https://{DOMAIN}:{}/dashboard?page=2",
            app.https_addr.port()
        )),
        "unexpected redirect target: {redirect}"
    );
}

/// AC2: a certificate inside its renew-before window is rotated for a fresh one
/// with no restart, and a connection established before the swap keeps serving.
#[tokio::test]
async fn near_expiry_certificate_rotates_without_restart_or_dropped_connections() {
    // A stored leaf with 5 days left — inside the 30-day renew-before window.
    let near_expiry = short_lived_cert(5);
    let app = boot_app(&near_expiry).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;
    let root = write_ca_root(&app, &ca);
    let config = acme_config(&app, &ca, Some(root));
    let store = fs_store(&config);
    let cert_id = CertId::from_domains(&config.domains);
    store
        .save_cert(&cert_id, &near_expiry)
        .await
        .expect("seed store");

    // A live connection, opened and used BEFORE the rotation, held open across it.
    let addr = app.https_addr;
    let mut live = tokio::task::spawn_blocking(move || LiveConnection::open(addr).expect("open"))
        .await
        .expect("client task");
    let before_leaf = live.leaf_der.clone();

    let status = AcmeStatus::new();
    let (reporter, _reported) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config.clone(),
            Arc::clone(&store),
            status.clone(),
            true,
        ),
        &status,
        reporter,
    )
    .await;

    let snap = status.snapshot();
    assert!(
        snap.last_failure.is_none(),
        "renewal failed: {:?}",
        snap.last_failure
    );
    let renewed_not_after = snap
        .cert_not_after_unix
        .expect("renewal recorded an expiry");
    assert!(
        renewed_not_after > now_unix() + 60 * DAY,
        "the renewed certificate should be well clear of the renew-before window"
    );

    // Rotation happened: a NEW connection is served the NEWLY ISSUED leaf, not
    // just some other certificate.
    let stored = store
        .load_cert(&cert_id)
        .await
        .expect("read store")
        .expect("cert stored");
    let after_leaf = served_leaf(app.https_addr).await;
    assert_ne!(
        after_leaf, before_leaf,
        "the near-expiry certificate was not rotated"
    );
    assert_eq!(
        after_leaf,
        leaf_der(&stored),
        "the listener is serving something other than the renewed certificate"
    );

    // ...and the connection opened before the swap is still usable: an in-flight
    // client is never dropped by a renewal.
    let response = tokio::task::spawn_blocking(move || {
        let out = live.request();
        // Keep the connection alive until the assertion has run.
        drop(live);
        out
    })
    .await
    .expect("client task")
    .expect("in-flight connection survived the rotation");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "in-flight request after rotation failed: {response}"
    );

    // The rotated certificate replaced the stored one.
    assert_ne!(stored.chain_pem, near_expiry.chain_pem);
}

/// AC3: a restart reuses the persisted account and certificate — no second
/// registration and no second order, so Let's Encrypt's rate limits are safe.
///
/// The second half is the load-bearing one: a restart whose stored certificate
/// is still healthy never contacts the CA at all, so "the account was not
/// re-registered" is trivially true there. Only a restart that *must* renew
/// reaches `load_or_register_account`, and only then does the credential-restore
/// path (as opposed to the register path) actually run.
#[tokio::test]
async fn a_restart_reuses_the_stored_account_and_certificate() {
    let placeholder = self_signed_placeholder(&[DOMAIN.to_owned()]).expect("placeholder");
    let app = boot_app(&placeholder).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;
    let root = write_ca_root(&app, &ca);
    let config = acme_config(&app, &ca, Some(root));
    let store = fs_store(&config);
    let cert_id = CertId::from_domains(&config.domains);

    // Boot 1: fresh — registers an account and orders a certificate.
    let first_status = AcmeStatus::new();
    let (reporter, _) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config.clone(),
            Arc::clone(&store),
            first_status.clone(),
            false,
        ),
        &first_status,
        reporter,
    )
    .await;
    assert!(first_status.snapshot().last_failure.is_none());
    assert_eq!(ca.state.accounts_registered.load(Ordering::SeqCst), 1);
    assert_eq!(ca.state.orders_created.load(Ordering::SeqCst), 1);

    // Boot 2 — "restart" with a healthy stored certificate: a second task over
    // the SAME cache dir, told it is serving the stored cert, exactly as
    // `build_acme_tls_listener` reports at boot. Nothing should reach the CA.
    let restarted = renewal_task(
        &app,
        config.clone(),
        fs_store(&config),
        AcmeStatus::new(),
        true,
    );
    let (reporter, reported_errors) = recording_reporter();
    run_to_quiescence(restarted, reporter).await;

    assert_eq!(
        ca.state.orders_created.load(Ordering::SeqCst),
        1,
        "a restart re-ordered a certificate that was still valid"
    );
    assert!(reported_errors.lock().unwrap().is_empty());

    // Boot 3 — "restart" with a certificate that is now inside its renew-before
    // window, so this boot MUST order. That is the only path that reaches
    // `load_or_register_account`, and with `account.json` already on disk it has
    // to take the credential-restore branch. If that branch ignored
    // `ca_root_path`, or failed to reuse the stored credentials, this boot would
    // either fail outright or register a second account.
    store
        .save_cert(&cert_id, &short_lived_cert(5))
        .await
        .expect("age the stored certificate into its renew window");
    // Also prove the CA's validity window is what lands on the renewed leaf.
    ca.state
        .cert_lifetime_days
        .store(RENEWED_CERT_DAYS, Ordering::SeqCst);

    let renewed_status = AcmeStatus::new();
    let (reporter, reported_errors) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config.clone(),
            fs_store(&config),
            renewed_status.clone(),
            true,
        ),
        &renewed_status,
        reporter,
    )
    .await;

    let snap = renewed_status.snapshot();
    assert!(
        snap.last_failure.is_none(),
        "the restore path failed: {:?}",
        snap.last_failure
    );
    assert_eq!(
        ca.state.orders_created.load(Ordering::SeqCst),
        2,
        "a restart with a near-expiry certificate must renew it"
    );
    assert_eq!(
        ca.state.accounts_registered.load(Ordering::SeqCst),
        1,
        "the renewal re-registered the ACME account instead of restoring the stored credentials"
    );
    assert!(reported_errors.lock().unwrap().is_empty());

    // The renewed leaf carries the validity the CA issued it with.
    let renewed_not_after = snap
        .cert_not_after_unix
        .expect("renewal recorded an expiry");
    let expected = now_unix() + RENEWED_CERT_DAYS * DAY;
    assert!(
        (renewed_not_after - expected).abs() < DAY,
        "renewed leaf expires at {renewed_not_after}, expected about {expected}"
    );
}

/// AC4: the custom directory is what gets used, and reaching a private CA
/// depends on `ca_root_path` — without it the client cannot establish trust and
/// issuance fails rather than silently succeeding.
#[tokio::test]
async fn a_private_directory_needs_its_root_configured() {
    let placeholder = self_signed_placeholder(&[DOMAIN.to_owned()]).expect("placeholder");
    let app = boot_app(&placeholder).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;

    // No `ca_root_path`: the ACME client verifies the directory against the
    // platform trust store, which does not know this test root, so the private
    // CA's directory is unreachable.
    let config = acme_config(&app, &ca, None);
    let status = AcmeStatus::new();
    let (reporter, reported_errors) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config,
            fs_store_at(&app, "untrusted"),
            status.clone(),
            false,
        ),
        &status,
        reporter,
    )
    .await;

    let (_, message) = status
        .snapshot()
        .last_failure
        .expect("issuance against an untrusted directory must fail");
    assert!(
        message.contains("ACME"),
        "failure should name the ACME step: {message}"
    );
    assert_eq!(ca.state.orders_created.load(Ordering::SeqCst), 0);
    assert_eq!(reported_errors.lock().unwrap().len(), 1);

    // With the root configured, the same directory works.
    let root = write_ca_root(&app, &ca);
    let config = acme_config(&app, &ca, Some(root));
    let status = AcmeStatus::new();
    let (reporter, _) = recording_reporter();
    run_one_boot(
        renewal_task(
            &app,
            config.clone(),
            fs_store(&config),
            status.clone(),
            false,
        ),
        &status,
        reporter,
    )
    .await;
    assert!(
        status.snapshot().last_failure.is_none(),
        "issuance against the configured private root should succeed"
    );
    assert_eq!(ca.state.orders_created.load(Ordering::SeqCst), 1);
}

/// AC5: a failed renewal is observable — it lands in the error-reporting seam
/// and drives the actuator health indicator down while the served certificate is
/// inside its danger window.
#[tokio::test]
async fn a_failed_renewal_surfaces_in_health_and_the_error_reporter() {
    let near_expiry = short_lived_cert(5);
    let app = boot_app(&near_expiry).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;
    let root = write_ca_root(&app, &ca);
    let config = acme_config(&app, &ca, Some(root));
    let store = fs_store(&config);
    let cert_id = CertId::from_domains(&config.domains);
    store
        .save_cert(&cert_id, &near_expiry)
        .await
        .expect("seed store");

    ca.state.reject_orders.store(true, Ordering::SeqCst);

    let status = AcmeStatus::new();
    status.set_cert_not_after(
        leaf_not_after_from_pem(near_expiry.chain_pem.as_bytes()).expect("notAfter"),
    );
    let (reporter, reported_errors) = recording_reporter();
    run_one_boot(
        renewal_task(&app, config, Arc::clone(&store), status.clone(), true),
        &status,
        reporter,
    )
    .await;

    let (_, message) = status.snapshot().last_failure.expect("failure recorded");
    assert!(
        message.contains("rate") || message.contains("ACME") || message.contains("order"),
        "unhelpful failure message: {message}"
    );

    let reported_errors = reported_errors.lock().unwrap().clone();
    assert_eq!(
        reported_errors.len(),
        1,
        "the failure must reach the error-reporting seam exactly once"
    );

    // The health indicator is Down: a failure while the served certificate is
    // already inside its renew-before window is a real, actionable outage risk.
    let indicator = AcmeHealthIndicator::new(status.clone(), 30);
    let graded = indicator.grade(now_unix());
    assert_eq!(graded.status, HealthStatus::Down);
    assert!(graded.details.contains_key("last_failure"));
    assert!(graded.details.contains_key("days_until_expiry"));
}

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// A self-signed leaf for `DOMAIN` with `days` of validity left, standing in for
/// a previously-issued certificate approaching expiry.
fn short_lived_cert(days: i64) -> StoredCert {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(vec![DOMAIN.to_owned()]).expect("params");
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, DOMAIN);
    params.distinguished_name = dn;
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(85 - days);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(days);
    let cert = params.self_signed(&key).expect("self-sign");
    StoredCert {
        chain_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    }
}

/// The DER of a stored chain's leaf, for comparison against what a TLS client
/// was actually served.
fn leaf_der(stored: &StoredCert) -> Vec<u8> {
    use rustls_pki_types::pem::PemObject as _;

    CertificateDer::from_pem_slice(stored.chain_pem.as_bytes())
        .expect("stored chain holds a leaf")
        .to_vec()
}

/// A store in a distinct subdirectory, so a test can run two independent boots
/// against the same app without sharing persisted material.
fn fs_store_at(app: &AcmeApp, name: &str) -> Arc<dyn AcmeStore> {
    Arc::new(FsAcmeStore::new(app.cache_dir.path().join(name), "custom"))
}
