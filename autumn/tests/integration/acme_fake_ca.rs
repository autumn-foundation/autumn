//! An in-process, HTTPS-terminating **fake ACME CA** for the #1608 end-to-end
//! tests.
//!
//! Autumn's ACME order flow (`autumn_web::acme::renewal`) is driven by
//! `instant-acme` over a real HTTPS transport, so the only way to exercise it in
//! CI without hitting Let's Encrypt is to stand up an ACME server the client can
//! actually reach. Pebble (Let's Encrypt's own test server) would need a
//! container plus container→host networking for the HTTP-01 fetch; this module
//! is the same idea with no Docker: an axum router speaking the subset of RFC
//! 8555 the client uses, served over TLS by the very
//! [`TlsListener`](autumn_web::tls::TlsListener) the app itself binds, with a
//! self-signed root the test hands to the client through
//! `[server.tls.acme] ca_root_path`.
//!
//! It is deliberately **not** a lenient stub. It performs the checks a real CA
//! performs and the app must survive:
//!
//! - it fetches the HTTP-01 key authorization over plain HTTP from the app's own
//!   challenge listener and requires a `"{token}.{thumbprint}"`-shaped body, so
//!   a token that is never published (or removed too early) fails validation
//!   exactly as it would in production;
//! - for **DNS-01** (#1620) it derives the account key's RFC 7638 thumbprint
//!   from the `jwk` in the client's own `newAccount` JWS, computes the exact
//!   `base64url(sha256("{token}.{thumbprint}"))` RFC 8555 §8.4 requires, and
//!   demands that value be **visible** in the test zone. A record that is
//!   published late (or never), or whose value is wrong, fails validation
//!   exactly as Let's Encrypt would — which is what makes the propagation wait
//!   testable rather than assumed;
//! - it parses the finalize CSR and requires its SAN set to equal the order's
//!   identifiers, and issues against the CSR's own public key — so a malformed
//!   CSR, a mismatched SAN set, or a CSR whose key does not pair with the key
//!   `generate_csr` returned is caught here rather than by Let's Encrypt (the
//!   last of those surfaces as `certified_key_from_pem` rejecting the issued
//!   chain). The CSR *signature* is not checked: `x509-parser`'s `verify`
//!   feature is not part of the workspace's dependency set, and re-deriving it
//!   would test `rcgen`, not autumn;
//! - it issues from a real CA keypair with a caller-chosen validity window, so
//!   near-expiry rotation can be forced deterministically.
//!
//! JWS signatures are *not* verified: the account key is the client's own, and
//! re-implementing JOSE verification would test `instant-acme` rather than
//! autumn. Everything autumn is responsible for is checked.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Validity of the CA's own HTTPS server certificate. Never a factor in a test
/// run; it only has to be comfortably in the future.
const CA_SERVER_CERT_DAYS: i64 = 365;

/// Default validity applied to certificates the CA issues.
pub const DEFAULT_ISSUED_CERT_DAYS: i64 = 90;

/// How long the CA waits for the app's `:80` listener to serve a challenge.
const HTTP01_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A running fake ACME CA. Dropping it stops the listener.
pub struct FakeCa {
    /// The ACME directory URL, to configure as `AcmeDirectory::Custom`.
    pub directory_url: String,
    /// PEM of the CA root, for the test to write out as `ca_root_path`.
    pub root_pem: String,
    /// Observable counters and knobs.
    pub state: Arc<CaState>,
    shutdown: CancellationToken,
}

impl Drop for FakeCa {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// CA state: issued material plus the counters and knobs a test reads and drives.
pub struct CaState {
    base: String,
    ca_params: rcgen::CertificateParams,
    ca_key: rcgen::KeyPair,
    ca_pem: String,
    /// Where the CA fetches HTTP-01 challenges from. A real CA resolves the
    /// order's domain; a test CA is told the address, exactly as Pebble's
    /// `-dnsserver` flag arranges.
    challenge_addr: SocketAddr,
    orders: Mutex<HashMap<String, OrderRec>>,
    certs: Mutex<HashMap<String, String>>,
    fetched: Mutex<Vec<String>>,
    next_id: AtomicUsize,
    nonces: AtomicUsize,

    /// `newAccount` registrations served. A restart that re-registers instead of
    /// reusing stored credentials shows up here.
    pub accounts_registered: AtomicUsize,
    /// `newOrder` requests served. A boot that re-orders an already-valid stored
    /// certificate shows up here.
    pub orders_created: AtomicUsize,
    /// HTTP-01 challenges the CA validated successfully.
    pub validations_ok: AtomicUsize,
    /// HTTP-01 challenges the CA rejected.
    pub validations_failed: AtomicUsize,
    /// Validity window applied to the next issued certificate, so a test can
    /// force a near-expiry leaf and demonstrate rotation.
    pub cert_lifetime_days: AtomicI64,
    /// When set, `newOrder` answers with an RFC 8555 problem document, so the
    /// failure path (health + error reporter) can be exercised.
    pub reject_orders: AtomicBool,

    /// The account key's RFC 7638 thumbprint, learned from the `jwk` in the
    /// client's `newAccount` JWS. Needed to derive the exact DNS-01 TXT value.
    thumbprint: Mutex<Option<String>>,
    /// Where the CA reads TXT records for DNS-01 validation — the test's zone,
    /// which only exposes records that have actually "propagated".
    dns_zone: Mutex<Option<Arc<dyn TxtZoneView>>>,
    /// DNS-01 challenges the CA validated successfully.
    pub dns_validations_ok: AtomicUsize,
    /// DNS-01 challenges the CA rejected (no record, or the wrong value).
    pub dns_validations_failed: AtomicUsize,
    /// The `(fqdn, expected value)` pairs the CA looked for, in order.
    dns_lookups: Mutex<Vec<(String, String)>>,
    /// When set, `set_ready` only QUEUES validation; it runs on the next poll of
    /// the order — which is how a real CA behaves (RFC 8555 §7.5.1: the server
    /// begins validation *after* the request, and the client polls). Inline
    /// validation would let a client that tore its challenge response down the
    /// moment `set_ready` returned still pass.
    pub deferred_validation: AtomicBool,
}

/// What the CA can see in the test's DNS zone.
///
/// Implemented by the test's fake zone, which returns a record only once it has
/// "propagated" — so an app that signals a challenge ready before waiting fails
/// validation here exactly as it would against a real CA.
pub trait TxtZoneView: Send + Sync {
    /// The TXT values currently VISIBLE at `fqdn`.
    fn visible_txt(&self, fqdn: &str) -> Vec<String>;
}

impl CaState {
    /// Key authorizations the CA actually fetched from the challenge listener,
    /// in order.
    pub fn fetched_key_authorizations(&self) -> Vec<String> {
        lock(&self.fetched).clone()
    }

    /// Point DNS-01 validation at the test's zone. Without this the CA offers
    /// only HTTP-01, exactly like a deployment with no DNS provider configured.
    pub fn set_dns_zone(&self, zone: Arc<dyn TxtZoneView>) {
        *lock(&self.dns_zone) = Some(zone);
    }

    /// Whether DNS-01 is on offer (i.e. a zone was installed).
    fn dns_enabled(&self) -> bool {
        lock(&self.dns_zone).is_some()
    }

    /// The `(fqdn, expected TXT value)` pairs the CA looked for, in order.
    pub fn dns_lookups(&self) -> Vec<(String, String)> {
        lock(&self.dns_lookups).clone()
    }
}

struct OrderRec {
    domains: Vec<String>,
    status: &'static str,
    authzs: Vec<AuthzRec>,
    cert_id: Option<String>,
}

struct AuthzRec {
    /// The BASE domain — an authorization for `*.myapp.com` is for `myapp.com`
    /// with `wildcard: true` (RFC 8555 §7.1.4).
    domain: String,
    wildcard: bool,
    token: String,
    valid: bool,
    /// Set when `deferred_validation` is on and the client has signalled the
    /// challenge ready: validation has been QUEUED but not run. It runs on the
    /// next poll of the order, the way a real CA works.
    pending: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Start a fake CA whose HTTP-01 validator fetches from `challenge_addr`.
///
/// Returns once the listener is accepting.
pub async fn start(challenge_addr: SocketAddr) -> FakeCa {
    // A self-signed root that signs both the CA's own HTTPS certificate and
    // every certificate it issues over ACME.
    let ca_key = rcgen::KeyPair::generate().expect("generate CA key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let mut ca_dn = rcgen::DistinguishedName::new();
    ca_dn.push(rcgen::DnType::CommonName, "Autumn Test ACME Root");
    ca_params.distinguished_name = ca_dn;
    ca_params.not_before = days_from_now(-1);
    ca_params.not_after = days_from_now(CA_SERVER_CERT_DAYS);
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA");
    let ca_pem = ca_cert.pem();

    // The CA's own HTTPS server certificate, signed by that root.
    let server_key = rcgen::KeyPair::generate().expect("generate CA server key");
    let mut server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("CA server params");
    server_params.not_before = days_from_now(-1);
    server_params.not_after = days_from_now(CA_SERVER_CERT_DAYS);
    server_params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::from([
            127, 0, 0, 1,
        ])));
    let server_cert = {
        let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
        server_params
            .signed_by(&server_key, &issuer)
            .expect("sign CA server cert")
    };
    let server_chain_pem = format!("{}{}", server_cert.pem(), ca_pem);

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake CA");
    let addr = tcp.local_addr().expect("fake CA local_addr");
    // The literal the listener binds, not `localhost`: an IPv6-preferring
    // resolver would try `[::1]` first and fail. The CA's own certificate
    // carries a `127.0.0.1` IP SAN for exactly this.
    let base = format!("https://127.0.0.1:{}", addr.port());

    let state = Arc::new(CaState {
        base: base.clone(),
        ca_params,
        ca_key,
        ca_pem,
        challenge_addr,
        orders: Mutex::new(HashMap::new()),
        certs: Mutex::new(HashMap::new()),
        fetched: Mutex::new(Vec::new()),
        next_id: AtomicUsize::new(1),
        nonces: AtomicUsize::new(1),
        accounts_registered: AtomicUsize::new(0),
        orders_created: AtomicUsize::new(0),
        validations_ok: AtomicUsize::new(0),
        validations_failed: AtomicUsize::new(0),
        cert_lifetime_days: AtomicI64::new(DEFAULT_ISSUED_CERT_DAYS),
        reject_orders: AtomicBool::new(false),
        thumbprint: Mutex::new(None),
        dns_zone: Mutex::new(None),
        dns_validations_ok: AtomicUsize::new(0),
        dns_validations_failed: AtomicUsize::new(0),
        dns_lookups: Mutex::new(Vec::new()),
        deferred_validation: AtomicBool::new(false),
    });

    let provider = autumn_web::tls::crypto_provider();
    let certified = autumn_web::tls::certified_key_from_pem(
        server_chain_pem.as_bytes(),
        server_key.serialize_pem().as_bytes(),
        &provider,
    )
    .expect("CA server key pair");
    let resolver = Arc::new(autumn_web::tls::ReloadableCertResolver::new(certified));
    let server_config = autumn_web::tls::build_server_config(Arc::clone(&provider), resolver)
        .expect("CA server config");

    let shutdown = CancellationToken::new();
    let listener = autumn_web::tls::TlsListener::new(
        tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );

    let router = router(Arc::clone(&state));
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });

    // Let the acceptor spin up before the first client request.
    tokio::time::sleep(Duration::from_millis(50)).await;

    FakeCa {
        directory_url: format!("{base}/dir"),
        root_pem: state.ca_pem.clone(),
        state,
        shutdown,
    }
}

fn router(state: Arc<CaState>) -> Router {
    Router::new()
        .route("/dir", get(directory))
        .route("/new-nonce", get(new_nonce))
        .route("/new-account", post(new_account))
        .route("/new-order", post(new_order))
        .route("/authz/{id}", post(authz))
        .route("/chall/{id}", post(challenge))
        .route("/order/{id}", post(order))
        .route("/order/{id}/finalize", post(finalize))
        .route("/cert/{id}", post(certificate))
        .with_state(state)
}

// ── Transport helpers ────────────────────────────────────────────────────────

/// Every ACME response carries a fresh `Replay-Nonce` (RFC 8555 §6.5).
fn with_nonce(state: &CaState, status: StatusCode, body: Value) -> Response {
    let nonce = state.nonces.fetch_add(1, Ordering::SeqCst);
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        "replay-nonce",
        HeaderValue::from_str(&format!("nonce{nonce:08}")).expect("ascii nonce"),
    );
    response
}

fn located(state: &CaState, status: StatusCode, location: &str, body: Value) -> Response {
    let mut response = with_nonce(state, status, body);
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).expect("ascii location"),
    );
    response
}

fn problem(state: &CaState, status: StatusCode, kind: &str, detail: &str) -> Response {
    with_nonce(
        state,
        status,
        json!({
            "type": format!("urn:ietf:params:acme:error:{kind}"),
            "detail": detail,
            "status": status.as_u16(),
        }),
    )
}

/// Extract the JWS payload of a POST body. An empty payload (`""`) is a
/// POST-as-GET (RFC 8555 §6.3) and yields `None`.
fn jws_payload(body: &[u8]) -> Option<Value> {
    let envelope: Value = serde_json::from_slice(body).ok()?;
    let encoded = envelope.get("payload")?.as_str()?;
    if encoded.is_empty() {
        return None;
    }
    let raw = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&raw).ok()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn directory(State(state): State<Arc<CaState>>) -> Response {
    let base = &state.base;
    with_nonce(
        &state,
        StatusCode::OK,
        json!({
            "newNonce": format!("{base}/new-nonce"),
            "newAccount": format!("{base}/new-account"),
            "newOrder": format!("{base}/new-order"),
            "revokeCert": format!("{base}/revoke-cert"),
            "keyChange": format!("{base}/key-change"),
        }),
    )
}

async fn new_nonce(State(state): State<Arc<CaState>>) -> Response {
    with_nonce(&state, StatusCode::OK, json!({}))
}

async fn new_account(State(state): State<Arc<CaState>>, body: axum::body::Bytes) -> Response {
    // RFC 8555 §7.3: `newAccount` carries the account public key inline as the
    // `jwk` of its JWS protected header. Deriving its RFC 7638 thumbprint here
    // is what lets DNS-01 validation check the EXACT value the client should
    // have published, rather than accepting any non-empty TXT record.
    if let Some(thumbprint) = jws_key_thumbprint(&body) {
        *lock(&state.thumbprint) = Some(thumbprint);
    }
    let id = state.accounts_registered.fetch_add(1, Ordering::SeqCst) + 1;
    let location = format!("{}/acct/{id}", state.base);
    located(
        &state,
        StatusCode::CREATED,
        &location,
        json!({ "status": "valid" }),
    )
}

async fn new_order(State(state): State<Arc<CaState>>, body: axum::body::Bytes) -> Response {
    if state.reject_orders.load(Ordering::SeqCst) {
        return problem(
            &state,
            StatusCode::FORBIDDEN,
            "rateLimited",
            "too many certificates already issued for this exact set of identifiers",
        );
    }

    let Some(payload) = jws_payload(&body) else {
        return problem(
            &state,
            StatusCode::BAD_REQUEST,
            "malformed",
            "empty payload",
        );
    };
    let domains: Vec<String> = payload
        .get("identifiers")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.get("value").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if domains.is_empty() {
        return problem(
            &state,
            StatusCode::BAD_REQUEST,
            "malformed",
            "order carried no dns identifiers",
        );
    }

    state.orders_created.fetch_add(1, Ordering::SeqCst);
    let id = state.next_id.fetch_add(1, Ordering::SeqCst).to_string();
    let authzs: Vec<AuthzRec> = domains
        .iter()
        .enumerate()
        .map(|(index, domain)| AuthzRec {
            domain: domain.strip_prefix("*.").unwrap_or(domain).to_owned(),
            wildcard: domain.starts_with("*."),
            token: format!("tok-{id}-{index}"),
            valid: false,
            pending: false,
        })
        .collect();
    lock(&state.orders).insert(
        id.clone(),
        OrderRec {
            domains,
            status: "pending",
            authzs,
            cert_id: None,
        },
    );

    let location = format!("{}/order/{id}", state.base);
    located(
        &state,
        StatusCode::CREATED,
        &location,
        order_json(&state, &id).expect("order just inserted"),
    )
}

async fn authz(State(state): State<Arc<CaState>>, Path(id): Path<String>) -> Response {
    let Some((order_id, index)) = split_authz_id(&id) else {
        return problem(&state, StatusCode::NOT_FOUND, "malformed", "bad authz id");
    };
    let orders = lock(&state.orders);
    let Some(rec) = orders.get(&order_id).and_then(|o| o.authzs.get(index)) else {
        return problem(&state, StatusCode::NOT_FOUND, "malformed", "no such authz");
    };
    let status = if rec.valid { "valid" } else { "pending" };
    let mut challenges = vec![json!({
        "type": "http-01",
        "url": format!("{}/chall/{id}-http", state.base),
        "token": rec.token,
        "status": status,
    })];
    if state.dns_enabled() {
        challenges.push(json!({
            "type": "dns-01",
            "url": format!("{}/chall/{id}-dns", state.base),
            "token": rec.token,
            "status": status,
        }));
    }
    // A wildcard authorization is flagged, and its identifier is the BASE
    // domain — the shape RFC 8555 §7.1.4 defines and the one autumn must derive
    // the `_acme-challenge` record name from.
    let body = json!({
        "status": status,
        "identifier": { "type": "dns", "value": rec.domain },
        "wildcard": rec.wildcard,
        "challenges": challenges,
    });
    drop(orders);
    with_nonce(&state, StatusCode::OK, body)
}

/// The client signalling "the challenge response is published". A real CA queues
/// validation; we run it inline so the subsequent `poll_ready` sees a settled
/// order.
async fn challenge(State(state): State<Arc<CaState>>, Path(id): Path<String>) -> Response {
    let Some((order_id, index, kind)) = split_challenge_id(&id) else {
        return problem(
            &state,
            StatusCode::NOT_FOUND,
            "malformed",
            "bad challenge id",
        );
    };
    let found = {
        let orders = lock(&state.orders);
        let rec = orders
            .get(&order_id)
            .and_then(|order| order.authzs.get(index))
            .map(|rec| (rec.token.clone(), rec.domain.clone()));
        drop(orders);
        rec
    };
    let Some((token, domain)) = found else {
        return problem(
            &state,
            StatusCode::NOT_FOUND,
            "malformed",
            "no such challenge",
        );
    };

    // A real CA queues validation and answers `processing`; the client then
    // polls the order. With `deferred_validation` set we do the same, so a
    // client that removed its challenge response the instant `set_ready`
    // returned would fail — which is exactly the bug this models.
    if state.deferred_validation.load(Ordering::SeqCst) {
        let mut orders = lock(&state.orders);
        if let Some(rec) = orders
            .get_mut(&order_id)
            .and_then(|order| order.authzs.get_mut(index))
        {
            rec.pending = true;
        }
        drop(orders);
        return with_nonce(
            &state,
            StatusCode::OK,
            json!({
                "type": kind.as_str(),
                "url": format!("{}/chall/{id}", state.base),
                "token": token,
                "status": "processing",
            }),
        );
    }

    let (validated, detail) = match kind {
        ChallengeKind::Http01 => (
            validate_http01(&state, &token).await,
            "no valid key authorization was served on port 80 for this token",
        ),
        ChallengeKind::Dns01 => (
            validate_dns01(&state, &domain, &token),
            "the _acme-challenge TXT record for this identifier is missing or carries the wrong \
             key authorization digest",
        ),
    };
    if !validated {
        match kind {
            ChallengeKind::Http01 => &state.validations_failed,
            ChallengeKind::Dns01 => &state.dns_validations_failed,
        }
        .fetch_add(1, Ordering::SeqCst);
        // RFC 8555 §6.7: a rejection is a 4xx carrying the problem document.
        // `instant-acme` treats only non-2xx/3xx as problems, so answering 200
        // here would make the client try to deserialize the problem as a
        // `Challenge` and fail with an unrelated serde error instead.
        return problem(&state, StatusCode::FORBIDDEN, "unauthorized", detail);
    }
    match kind {
        ChallengeKind::Http01 => &state.validations_ok,
        ChallengeKind::Dns01 => &state.dns_validations_ok,
    }
    .fetch_add(1, Ordering::SeqCst);

    {
        let mut orders = lock(&state.orders);
        if let Some(order) = orders.get_mut(&order_id) {
            if let Some(rec) = order.authzs.get_mut(index) {
                rec.valid = true;
            }
            if order.authzs.iter().all(|a| a.valid) {
                order.status = "ready";
            }
        }
    }

    with_nonce(
        &state,
        StatusCode::OK,
        json!({
            "type": kind.as_str(),
            "url": format!("{}/chall/{id}", state.base),
            "token": token,
            "status": "valid",
        }),
    )
}

/// Which challenge a `/chall/{id}` URL addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChallengeKind {
    Http01,
    Dns01,
}

impl ChallengeKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http01 => "http-01",
            Self::Dns01 => "dns-01",
        }
    }
}

async fn order(State(state): State<Arc<CaState>>, Path(id): Path<String>) -> Response {
    settle_pending_validations(&state, &id);
    order_json(&state, &id).map_or_else(
        || problem(&state, StatusCode::NOT_FOUND, "malformed", "no such order"),
        |body| with_nonce(&state, StatusCode::OK, body),
    )
}

async fn finalize(
    State(state): State<Arc<CaState>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let Some(payload) = jws_payload(&body) else {
        return problem(
            &state,
            StatusCode::BAD_REQUEST,
            "malformed",
            "empty payload",
        );
    };
    let Some(csr_der) = payload
        .get("csr")
        .and_then(Value::as_str)
        .and_then(|b64| URL_SAFE_NO_PAD.decode(b64).ok())
    else {
        return problem(
            &state,
            StatusCode::BAD_REQUEST,
            "badCSR",
            "finalize carried no decodable csr",
        );
    };

    let ready = {
        let orders = lock(&state.orders);
        let state_of = orders
            .get(&id)
            .map(|order| (order.status, order.domains.clone()));
        drop(orders);
        state_of
    };
    let domains = match ready {
        None => return problem(&state, StatusCode::NOT_FOUND, "malformed", "no such order"),
        Some((status, _)) if status != "ready" => {
            return problem(
                &state,
                StatusCode::FORBIDDEN,
                "orderNotReady",
                "order is not ready for finalization",
            );
        }
        Some((_, domains)) => domains,
    };

    let chain_pem = match issue_from_csr(&state, &csr_der, &domains) {
        Ok(pem) => pem,
        Err(detail) => return problem(&state, StatusCode::BAD_REQUEST, "badCSR", &detail),
    };

    let cert_id = format!("cert-{}", state.next_id.fetch_add(1, Ordering::SeqCst));
    lock(&state.certs).insert(cert_id.clone(), chain_pem);
    {
        let mut orders = lock(&state.orders);
        if let Some(order) = orders.get_mut(&id) {
            order.status = "valid";
            order.cert_id = Some(cert_id);
        }
    }

    with_nonce(
        &state,
        StatusCode::OK,
        order_json(&state, &id).expect("order exists"),
    )
}

async fn certificate(State(state): State<Arc<CaState>>, Path(id): Path<String>) -> Response {
    let Some(pem) = lock(&state.certs).get(&id).cloned() else {
        return problem(&state, StatusCode::NOT_FOUND, "malformed", "no such cert");
    };
    let nonce = state.nonces.fetch_add(1, Ordering::SeqCst);
    let mut response = (StatusCode::OK, pem).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pem-certificate-chain"),
    );
    headers.insert(
        "replay-nonce",
        HeaderValue::from_str(&format!("nonce{nonce:08}")).expect("ascii nonce"),
    );
    response
}

// ── CA internals ─────────────────────────────────────────────────────────────

/// Run any validation queued by `deferred_validation` for order `id`.
///
/// Called when the client polls the order, so the challenge response must still
/// be published at THAT point — not merely at the moment `set_ready` returned.
fn settle_pending_validations(state: &CaState, id: &str) {
    let pending: Vec<(usize, String, String)> = {
        let orders = lock(&state.orders);
        let Some(order) = orders.get(id) else {
            return;
        };
        let pending = order
            .authzs
            .iter()
            .enumerate()
            .filter(|(_, rec)| rec.pending && !rec.valid)
            .map(|(index, rec)| (index, rec.domain.clone(), rec.token.clone()))
            .collect();
        drop(orders);
        pending
    };
    for (index, domain, token) in pending {
        let ok = validate_dns01(state, &domain, &token);
        if ok {
            state.dns_validations_ok.fetch_add(1, Ordering::SeqCst);
        } else {
            state.dns_validations_failed.fetch_add(1, Ordering::SeqCst);
        }
        let mut orders = lock(&state.orders);
        if let Some(order) = orders.get_mut(id) {
            if let Some(rec) = order.authzs.get_mut(index) {
                rec.pending = false;
                rec.valid = ok;
            }
            if order.authzs.iter().all(|a| a.valid) {
                order.status = "ready";
            } else if !ok {
                order.status = "invalid";
            }
        }
    }
}

fn order_json(state: &CaState, id: &str) -> Option<Value> {
    let orders = lock(&state.orders);
    let order = orders.get(id)?;
    let mut body = json!({
        "status": order.status,
        "identifiers": order.domains.iter()
            .map(|d| json!({ "type": "dns", "value": d }))
            .collect::<Vec<_>>(),
        "authorizations": (0..order.authzs.len())
            .map(|index| format!("{}/authz/{id}-{index}", state.base))
            .collect::<Vec<_>>(),
        "finalize": format!("{}/order/{id}/finalize", state.base),
    });
    if let Some(cert_id) = &order.cert_id {
        body["certificate"] = json!(format!("{}/cert/{cert_id}", state.base));
    }
    drop(orders);
    Some(body)
}

fn split_authz_id(id: &str) -> Option<(String, usize)> {
    let (order, index) = id.rsplit_once('-')?;
    Some((order.to_owned(), index.parse().ok()?))
}

/// Split a `/chall/{order}-{index}-{kind}` id.
fn split_challenge_id(id: &str) -> Option<(String, usize, ChallengeKind)> {
    let (authz, kind) = id.rsplit_once('-')?;
    let kind = match kind {
        "http" => ChallengeKind::Http01,
        "dns" => ChallengeKind::Dns01,
        _ => return None,
    };
    let (order, index) = split_authz_id(authz)?;
    Some((order, index, kind))
}

/// The RFC 7638 thumbprint of the account key in a `newAccount` JWS.
///
/// The protected header carries the public key inline as `jwk`; the thumbprint
/// is the SHA-256 of its canonical JSON (required members only, lexicographic,
/// no whitespace), base64url-encoded without padding. `instant-acme` uses ES256,
/// so the key is always an EC P-256 key.
fn jws_key_thumbprint(body: &[u8]) -> Option<String> {
    use sha2::{Digest as _, Sha256};

    let envelope: Value = serde_json::from_slice(body).ok()?;
    let protected = envelope.get("protected")?.as_str()?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).ok()?).ok()?;
    let jwk = header.get("jwk")?;
    let canonical = format!(
        r#"{{"crv":"{}","kty":"{}","x":"{}","y":"{}"}}"#,
        jwk.get("crv")?.as_str()?,
        jwk.get("kty")?.as_str()?,
        jwk.get("x")?.as_str()?,
        jwk.get("y")?.as_str()?,
    );
    Some(URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes())))
}

/// Validate a DNS-01 challenge: the zone must VISIBLY carry
/// `base64url(sha256("{token}.{thumbprint}"))` at `_acme-challenge.{domain}`
/// (RFC 8555 §8.4).
///
/// Reads the zone's *visible* view, so a record published but not yet
/// propagated fails exactly as it would against a real CA — which is what makes
/// autumn's propagation wait a tested behaviour rather than an assumption.
fn validate_dns01(state: &CaState, domain: &str, token: &str) -> bool {
    use sha2::{Digest as _, Sha256};

    let Some(zone) = lock(&state.dns_zone).clone() else {
        return false;
    };
    let Some(thumbprint) = lock(&state.thumbprint).clone() else {
        return false;
    };
    let key_authorization = format!("{token}.{thumbprint}");
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(key_authorization.as_bytes()));
    let fqdn = format!("_acme-challenge.{domain}");
    lock(&state.dns_lookups).push((fqdn.clone(), expected.clone()));
    zone.visible_txt(&fqdn).contains(&expected)
}

/// Fetch `http://{challenge_addr}/.well-known/acme-challenge/{token}` and check
/// the body is the `"{token}.{thumbprint}"` key authorization RFC 8555 requires.
async fn validate_http01(state: &CaState, token: &str) -> bool {
    let path = format!("/.well-known/acme-challenge/{token}");
    let Ok(response) = http_get(state.challenge_addr, &path).await else {
        return false;
    };
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return false;
    };
    if !head.starts_with("HTTP/1.1 200") {
        return false;
    }
    let body = body.trim();
    // A key authorization is `token || '.' || base64url(thumbprint(accountKey))`.
    if !body.starts_with(&format!("{token}.")) || body.len() <= token.len() + 1 {
        return false;
    }
    lock(&state.fetched).push(body.to_owned());
    true
}

/// Minimal blocking-free HTTP/1.1 GET. Avoids pulling a client crate into the
/// test build just to fetch one plain-text challenge response.
async fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // Bounded: an unresponsive challenge listener must surface as a failed
    // validation, not as a stalled handler that only unwinds when the test's own
    // deadline trips.
    tokio::time::timeout(HTTP01_FETCH_TIMEOUT, async move {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "HTTP-01 fetch timed out"))?
}

/// Verify the CSR and issue a leaf for `domains` signed by the CA root.
fn issue_from_csr(state: &CaState, csr_der: &[u8], domains: &[String]) -> Result<String, String> {
    use x509_parser::prelude::FromDer as _;

    let (rest, csr) =
        x509_parser::certification_request::X509CertificationRequest::from_der(csr_der)
            .map_err(|e| format!("unparseable CSR: {e}"))?;
    if !rest.is_empty() {
        return Err("trailing bytes after CSR".to_owned());
    }
    // A real CA binds the issued certificate to the order's identifiers and
    // rejects a CSR asking for anything else.
    let mut requested: Vec<String> = csr
        .requested_extensions()
        .into_iter()
        .flatten()
        .filter_map(|ext| match ext {
            x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) => Some(san),
            _ => None,
        })
        .flat_map(|san| san.general_names.iter())
        .filter_map(|name| match name {
            x509_parser::extensions::GeneralName::DNSName(dns) => Some((*dns).to_owned()),
            _ => None,
        })
        .collect();
    requested.sort();
    let mut expected: Vec<String> = domains.to_vec();
    expected.sort();
    if requested != expected {
        return Err(format!(
            "CSR SANs {requested:?} do not match the order identifiers {expected:?}"
        ));
    }

    let spki = &csr.certification_request_info.subject_pki;
    if spki.algorithm.algorithm != x509_parser::oid_registry::OID_KEY_TYPE_EC_PUBLIC_KEY {
        return Err("CSR public key is not ECDSA".to_owned());
    }
    let public_key = CsrPublicKey {
        der: spki.subject_public_key.data.to_vec(),
    };

    let lifetime = state.cert_lifetime_days.load(Ordering::SeqCst);
    let mut params =
        rcgen::CertificateParams::new(domains.to_vec()).map_err(|e| format!("leaf params: {e}"))?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, domains[0].clone());
    params.distinguished_name = dn;
    params.not_before = days_from_now(-1);
    params.not_after = days_from_now(lifetime);
    params.use_authority_key_identifier_extension = true;

    let issuer = rcgen::Issuer::from_params(&state.ca_params, &state.ca_key);
    let leaf = params
        .signed_by(&public_key, &issuer)
        .map_err(|e| format!("issuing leaf: {e}"))?;
    Ok(format!("{}{}", leaf.pem(), state.ca_pem))
}

/// The CSR's public key, adapted to rcgen's signing interface.
///
/// `rcgen::SubjectPublicKeyInfo::from_der` would do this, but only behind
/// rcgen's `x509-parser` feature — which the workspace does not enable for the
/// production `acme` build. Implementing the (public) trait over the SPKI bits
/// keeps this cost inside the test.
struct CsrPublicKey {
    der: Vec<u8>,
}

impl rcgen::PublicKeyData for CsrPublicKey {
    fn der_bytes(&self) -> &[u8] {
        &self.der
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        // `AcmeRenewalTask::generate_csr` uses `rcgen::KeyPair::generate()`,
        // whose default is ECDSA P-256; `issue_from_csr` rejects anything else
        // before reaching here.
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

/// `now + days`, as rcgen wants its validity bounds.
fn days_from_now(days: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() + time::Duration::days(days)
}
