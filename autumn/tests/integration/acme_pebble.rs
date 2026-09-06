//! Real-ACME end-to-end coverage against [Pebble](https://github.com/letsencrypt/pebble)
//! (issue #1863, deferred from #1608's [`acme_end_to_end`](super::acme_end_to_end)).
//!
//! Every other ACME end-to-end test in the tree drives a real `instant-acme`
//! client against [`acme_fake_ca`](super::acme_fake_ca) — an in-process stand-in
//! explicitly built because Pebble needs a container plus container→host
//! networking for the HTTP-01 fetch. That substitute is deliberately strict
//! (it checks the key-authorization body, the CSR's SAN set and public key,
//! …), but it is still autumn's own test double: a bug in how `instant-acme`
//! itself is driven — challenge ordering, the finalize payload shape, nonce/
//! retry handling, polling — could pass against the stand-in and still fail
//! against a real, independently-implemented ACME server. This test closes
//! that gap by driving the *same* [`AcmeRenewalTask`] against a real Pebble
//! container.
//!
//! # Networking
//!
//! Pebble's HTTP-01 validation request originates *inside* the Pebble
//! container, so it cannot reach a `127.0.0.1`-bound listener on this host the
//! way the in-process fake CA's tests can. `testcontainers`' `host-port-exposure`
//! feature solves exactly this: [`ImageExt::with_exposed_host_port`] opens an
//! SSH tunnel and injects a `host.testcontainers.internal` DNS alias into the
//! container, resolving to that tunnel. Ordering the certificate for the
//! identifier `host.testcontainers.internal` itself — rather than `localhost`
//! or a made-up name — means Pebble's normal DNS resolution of the identifier
//! (it does no special-casing; "use the system DNS resolver" is its only
//! documented mode) lands exactly on our HTTP-01 responder, with no custom
//! DNS server needed the way [`acme_dns01`](super::acme_dns01)'s `FakeZone`
//! provides for the wildcard/DNS-01 suite.
//!
//! Pebble's own HTTP-01 validation port is fixed at `5002` by its default
//! bundled config (`test/config/pebble-config.json`, loaded automatically —
//! it is the `-config` flag's own default) — not configurable per run without
//! shipping a custom config into the container — so this test binds its
//! challenge listener there directly instead of an ephemeral port the way
//! [`acme_end_to_end`](super::acme_end_to_end) does.
//!
//! # Trust
//!
//! Pebble signs its own HTTPS directory/management API with a **fixed**
//! test root baked into the image at `/test/certs/pebble.minica.pem` (this is
//! a different keypair than the *issuance* root Pebble generates fresh on
//! every boot to sign certificates it issues — the two are not
//! interchangeable). This test copies that file out of the running container
//! at setup time (rather than vendoring a copy) so it always matches whatever
//! Pebble image is pulled, and hands it to `instant-acme` via
//! `[server.tls.acme] ca_root_path`, exactly as a real deployment would point
//! at a private CA.
//!
//! # What this proves
//!
//! [`AcmeRenewalTask::run`] driven to completion against Pebble: order →
//! HTTP-01 authorization → finalize → certificate download, ending with a
//! **real**, parseable, not-yet-expired certificate persisted to the store and
//! hot-swapped into the live [`ReloadableCertResolver`] — the exact assertion
//! #1863 asks for ("assert a usable certificate is obtained").

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use autumn_web::acme::challenge::{Http01Tokens, challenge_router};
use autumn_web::acme::renewal::{AcmeRenewalTask, AcmeStatus, ReporterFn, self_signed_placeholder};
use autumn_web::acme::store::{AcmeStore, CertId, FsAcmeStore};
use autumn_web::config::{AcmeConfig, AcmeDirectory};
use autumn_web::scheduler::{InProcessSchedulerCoordinator, SchedulerCoordinator};
use autumn_web::tls::{
    ReloadableCertResolver, certified_key_from_pem, crypto_provider, leaf_not_after_from_pem,
};
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio_util::sync::CancellationToken;

/// The DNS alias `testcontainers`' `host-port-exposure` feature injects into
/// the container, resolving to a tunnel back to this host.
const HOST_ALIAS: &str = "host.testcontainers.internal";

/// Pebble's fixed HTTP-01 validation port (its default bundled config's
/// `httpPort`, loaded automatically with no `-config` flag).
const PEBBLE_HTTP01_PORT: u16 = 5002;

/// Pebble's fixed ACME directory port (its default bundled config's
/// `listenAddress`).
const PEBBLE_ACME_PORT: u16 = 14000;

/// The image is pulled by floating `latest` tag (Pebble ships no versioned
/// tags on `ghcr.io/letsencrypt/pebble` as of writing — only `latest`, built
/// from `main`) rather than a pinned digest: Pebble's ACME surface is a
/// stable RFC 8555 subset that has not broken this test across releases, and
/// pinning a digest would silently stop picking up Pebble's own fixes.
const PEBBLE_IMAGE: &str = "ghcr.io/letsencrypt/pebble";
const PEBBLE_TAG: &str = "latest";

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs(),
    )
    .expect("unix time fits i64")
}

/// A running Pebble container plus the pieces the ACME client needs to reach
/// it: the directory URL and the root that signs Pebble's own HTTPS API.
struct Pebble {
    // Never read: kept alive purely so the container isn't dropped (and torn
    // down) while `directory_url`/`root_pem` are still in use.
    #[allow(dead_code)]
    container: ContainerAsync<GenericImage>,
    directory_url: String,
    root_pem: Vec<u8>,
}

/// Start a Pebble container whose HTTP-01 validation reaches `challenge_port`
/// on this host via the `host-port-exposure` tunnel (see the module doc).
///
/// `#[allow(future_not_send)]`: `ContainerAsync::copy_file_from`'s returned
/// future is not `Send` (testcontainers' internal archive-reading path), and
/// this single-threaded `#[tokio::test]` never needs to move it across
/// threads.
#[allow(clippy::future_not_send)]
async fn start_pebble(challenge_port: u16) -> Pebble {
    let container = GenericImage::new(PEBBLE_IMAGE, PEBBLE_TAG)
        .with_exposed_port(ContainerPort::Tcp(PEBBLE_ACME_PORT))
        .with_wait_for(WaitFor::message_on_stdout("ACME directory available at:"))
        // Skip Pebble's artificial validation delay (default: sleep up to 15s
        // before validating) so this test runs at CI-friendly speed.
        .with_env_var("PEBBLE_VA_NOSLEEP", "1")
        // Pebble randomly rejects 5% of otherwise-good nonces by default, to
        // exercise a client's badNonce retry. That is `instant-acme`'s
        // contract to uphold, not autumn's order-flow code under test here —
        // disable it so this test is not flaky on the CA's own fault
        // injection.
        .with_env_var("PEBBLE_WFE_NONCEREJECT", "0")
        .with_exposed_host_port(challenge_port)
        .start()
        .await
        .expect("start the Pebble container");

    let acme_port = container
        .get_host_port_ipv4(PEBBLE_ACME_PORT)
        .await
        .expect("Pebble's mapped ACME directory port");
    let directory_url = format!("https://127.0.0.1:{acme_port}/dir");

    // Pebble's own HTTPS listener (the ACME directory/account/order API) is
    // signed by a FIXED root baked into the image — not the issuance root it
    // generates fresh on every boot (see the module doc). Pull it from the
    // running container rather than vendoring a copy, so it always matches
    // whatever image was pulled.
    let root_pem: Vec<u8> = container
        .copy_file_from("/test/certs/pebble.minica.pem", Vec::new())
        .await
        .expect("read Pebble's minica root certificate out of the container");
    assert!(
        !root_pem.is_empty(),
        "Pebble's minica root certificate must not be empty"
    );

    Pebble {
        container,
        directory_url,
        root_pem,
    }
}

/// Bind the ACME HTTP-01 challenge listener on Pebble's fixed validation
/// port. Returns the shared token map and a guard that tears the listener
/// down when cancelled.
fn spawn_challenge_listener(
    challenge_tcp: tokio::net::TcpListener,
) -> (Http01Tokens, CancellationToken, tokio::task::JoinHandle<()>) {
    let tokens = Http01Tokens::new();
    let shutdown = CancellationToken::new();
    let router = challenge_router(tokens.clone(), 443);
    let serve_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(challenge_tcp, router)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });
    (tokens, shutdown, handle)
}

/// What's left of the harness once its [`AcmeRenewalTask`] has been handed
/// off to run — everything [`assert_certificate_issued`] inspects afterward.
struct Harness {
    resolver: Arc<ReloadableCertResolver>,
    store: Arc<dyn AcmeStore>,
    cert_id: CertId,
}

/// Build the renewal task the way the app would at boot, pointed at
/// `pebble`'s directory and trusting its root.
fn build_harness(
    pebble: &Pebble,
    tokens: Http01Tokens,
    cache_dir: &std::path::Path,
) -> (AcmeRenewalTask, Harness) {
    let root_path = cache_dir.join("pebble-minica-root.pem");
    std::fs::write(&root_path, &pebble.root_pem).expect("write Pebble's minica root to disk");

    let domains = vec![HOST_ALIAS.to_owned()];
    let config = AcmeConfig {
        domains: domains.clone(),
        contact_email: "acme-pebble-test@example.com".to_owned(),
        directory: AcmeDirectory::Custom {
            url: pebble.directory_url.clone(),
        },
        cache_dir: cache_dir.to_path_buf(),
        http_challenge_port: PEBBLE_HTTP01_PORT,
        // Pebble's bundled config declares a `default` (90-day) AND a
        // `shortlived` (6-day) profile, and which one it hands back to an
        // order that does not request one by name is Pebble's choice, not
        // ours — observed empirically to sometimes be the 6-day profile. `1`
        // stays safely under either, so this assertion is about autumn's own
        // renew-before-expiry decision, not a bet on which profile Pebble picks.
        renew_before_days: 1,
        ca_root_path: Some(root_path),
        dns: None,
    };
    config
        .validate()
        .expect("the test's own ACME config must be valid");

    let placeholder = self_signed_placeholder(&domains).expect("self-signed placeholder builds");
    let provider = crypto_provider();
    let certified = certified_key_from_pem(
        placeholder.chain_pem.as_bytes(),
        placeholder.key_pem.as_bytes(),
        &provider,
    )
    .expect("placeholder cert loads");
    let resolver = Arc::new(ReloadableCertResolver::new(certified));

    let store: Arc<dyn AcmeStore> = Arc::new(FsAcmeStore::new(
        config.cache_dir.clone(),
        autumn_web::acme::directory_label(&config.directory),
    ));
    let cert_id = CertId::from_domains(&domains);
    let task = AcmeRenewalTask {
        resolver: Arc::clone(&resolver),
        provider: crypto_provider(),
        store: Arc::clone(&store),
        cert_id: cert_id.clone(),
        tokens,
        status: AcmeStatus::new(),
        config,
        serving_stored_cert: false,
        leadership_degraded: false,
        renew_window_misconfigured: AtomicBool::new(false),
        dns: None,
        recovery: None,
    };
    (
        task,
        Harness {
            resolver,
            store,
            cert_id,
        },
    )
}

/// Wait until `status` records either a success or a failure, or panic if the
/// renewal task ends first (which means it panicked — see
/// [`acme_end_to_end::run_one_boot`](super::acme_end_to_end)'s identical
/// reasoning) or the deadline passes.
async fn await_outcome(status: &AcmeStatus, handle: &tokio::task::JoinHandle<()>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let snap = status.snapshot();
        if snap.last_success_unix.is_some() || snap.last_failure.is_some() {
            return;
        }
        assert!(
            !handle.is_finished(),
            "the ACME renewal task ended before recording any outcome (it likely panicked)"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "ACME order against Pebble never settled within 60s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run `task` to a settled outcome (success or failure recorded), then stop
/// it. Returns whatever the reporter captured, for a clear failure message.
async fn run_to_outcome(task: AcmeRenewalTask, status: &AcmeStatus) -> Vec<String> {
    let coordinator: Arc<dyn SchedulerCoordinator> =
        Arc::new(InProcessSchedulerCoordinator::new("acme-pebble-test"));
    let failures: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&failures);
    let reporter: ReporterFn = Arc::new(move |msg| {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(msg);
    });
    let shutdown = CancellationToken::new();
    let run_shutdown = shutdown.clone();
    let handle = tokio::spawn(task.run(coordinator, reporter, run_shutdown));

    await_outcome(status, &handle).await;
    shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("the ACME renewal task panicked: {e}"),
        Err(_elapsed) => {}
    }

    Arc::try_unwrap(failures)
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .unwrap_or_default()
}

/// Assert a real, usable certificate was obtained end to end: recorded as a
/// success, persisted to the store with a sane not-yet-expired `notAfter`,
/// and hot-swapped into the live resolver (not left on the placeholder).
async fn assert_certificate_issued(harness: &Harness, status: &AcmeStatus, failures: &[String]) {
    let snap = status.snapshot();
    assert!(
        snap.last_failure.is_none(),
        "expected no ACME failure, got: {:?} (reported failures: {failures:?})",
        snap.last_failure,
    );
    assert!(
        snap.last_success_unix.is_some(),
        "expected the ACME renewal task to record a successful issuance against Pebble"
    );

    let stored = harness
        .store
        .load_cert(&harness.cert_id)
        .await
        .expect("load the persisted certificate")
        .expect("a certificate must have been persisted for this cert id");
    assert!(
        !stored.chain_pem.is_empty() && !stored.key_pem.is_empty(),
        "the persisted certificate chain and key must not be empty"
    );

    let not_after =
        leaf_not_after_from_pem(stored.chain_pem.as_bytes()).expect("parse the leaf's notAfter");
    assert!(
        not_after > now_unix(),
        "the issued certificate must not already be expired (notAfter={not_after})"
    );
    // Pebble's default profile issues 90-day (7,776,000s) certificates; a
    // sanity ceiling well above that catches a badly wrong parse without
    // being sensitive to Pebble's exact configured validity period.
    assert!(
        not_after < now_unix() + 366 * 86_400,
        "the issued certificate's notAfter is implausibly far in the future \
         (notAfter={not_after}); this likely indicates a parsing bug rather than a real cert"
    );

    // The resolver was hot-swapped, not left on the self-signed placeholder:
    // the served certified key's leaf must now be the persisted, CA-issued
    // chain rather than the boot-time placeholder.
    let served = harness.resolver.current();
    let served_leaf = served
        .cert
        .first()
        .expect("served certified key has a leaf certificate");
    let stored_leaf_der = {
        use rustls_pki_types::pem::PemObject as _;
        rustls_pki_types::CertificateDer::pem_slice_iter(stored.chain_pem.as_bytes())
            .next()
            .and_then(Result::ok)
            .expect("parse the first (leaf) certificate out of the stored chain PEM")
    };
    assert_eq!(
        served_leaf.as_ref(),
        stored_leaf_der.as_ref(),
        "the live TLS resolver must serve the freshly-issued Pebble certificate, not the \
         self-signed placeholder or a stale cert"
    );
}

// Regression coverage for issue #1863 (deferred from #1608 / PR #1858): every
// other ACME test drives autumn's order state machine against
// `acme_fake_ca`, an in-process stand-in. This test drives the SAME
// `AcmeRenewalTask` against a real, independently-implemented ACME server
// (Pebble) end to end, so a protocol-level regression the stand-in cannot
// see (challenge ordering, the finalize payload shape, polling) would fail
// here even if every other ACME test stayed green.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn drives_a_real_pebble_order_end_to_end() {
    // Bind the HTTP-01 challenge listener BEFORE starting Pebble: Pebble's
    // validation port is fixed (see `PEBBLE_HTTP01_PORT`), so nothing here
    // depends on Pebble's own startup — but binding first still fails fast
    // and clearly if the port is unexpectedly taken.
    let challenge_tcp = tokio::net::TcpListener::bind(("127.0.0.1", PEBBLE_HTTP01_PORT))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "bind the ACME HTTP-01 challenge listener on 127.0.0.1:{PEBBLE_HTTP01_PORT} \
                 (Pebble's fixed validation port): {e}"
            )
        });
    let (tokens, challenge_shutdown, challenge_handle) = spawn_challenge_listener(challenge_tcp);

    let pebble = start_pebble(PEBBLE_HTTP01_PORT).await;
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let (task, harness) = build_harness(&pebble, tokens, cache_dir.path());

    let status = task.status.clone();
    let failures = run_to_outcome(task, &status).await;

    challenge_shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), challenge_handle).await;

    assert_certificate_issued(&harness, &status, &failures).await;
}
