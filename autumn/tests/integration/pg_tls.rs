//! Postgres `sslmode` TLS behavior of the connection pool (issue #1507).
//!
//! No real Postgres (or docker) is required: a stub server speaks just enough
//! of the Postgres wire protocol to answer the client's `SSLRequest`, complete
//! (or refuse) a rustls handshake with a self-signed certificate, and record
//! what it received. That is exactly the layer these tests pin:
//!
//! - `sslmode=require` must actually negotiate TLS (before TLS support, every
//!   such connection failed with "no TLS implementation configured") and must
//!   deliver the Postgres startup message over the *encrypted* stream;
//! - `sslmode=require` against a server that refuses TLS must fail with a
//!   clear "server does not support TLS" error;
//! - an URL without `sslmode` must keep the historical `NoTls` path: the
//!   client sends a plaintext startup message and never an `SSLRequest`.
//!
//! Certificate-identity behavior is deliberately relaxed for `require`
//! (`libpq` parity — the stub's self-signed cert is accepted), which these
//! tests exercise implicitly: a verifying client would abort the handshake.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::create_pool;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// Self-signed certificate for `CN=localhost`, valid until 2126. Test fixture
/// only — the key pair is intentionally public.
const TEST_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBfjCCASWgAwIBAgIUfXFks57omSDyLocoh9/qZRw5l6kwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MDcwNzIxMjcxN1oYDzIxMjYwNjEz
MjEyNzE3WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAQlLDZGMtq9VrMRWNNegQo7vPyD0qdi34gb4geyKyRp0OJx1welA8cI
dg+FlgfjTmV4XYDOZ4eBzqfMNuYeWM05o1MwUTAdBgNVHQ4EFgQUH6WQkQzygN1l
Ltnn3ejCLNXu0P0wHwYDVR0jBBgwFoAUH6WQkQzygN1lLtnn3ejCLNXu0P0wDwYD
VR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBOpd93XbNAJb0o5w1wosTD
T/NiGagcq4r74Rxz7TkK+wIgY9jZSKSamks42gN/BbEaAkTjeXuKL1rBq4MfAgGr
Fxg=
-----END CERTIFICATE-----
";

const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgwtGbuQCRFq3WGqQD
H/I/i2zioTOGwepZpYzIMetrHcWhRANCAAQlLDZGMtq9VrMRWNNegQo7vPyD0qdi
34gb4geyKyRp0OJx1welA8cIdg+FlgfjTmV4XYDOZ4eBzqfMNuYeWM05
-----END PRIVATE KEY-----
";

/// The Postgres `SSLRequest` code (1234 << 16 | 5679).
const SSL_REQUEST_CODE: u32 = 80_877_103;
/// The Postgres protocol version in a startup message (3.0).
const PROTOCOL_VERSION: u32 = 196_608;

fn rustls_server_config() -> Arc<rustls::ServerConfig> {
    let certs = vec![CertificateDer::from_pem_slice(TEST_CERT_PEM.as_bytes()).unwrap()];
    let key = PrivateKeyDer::from_pem_slice(TEST_KEY_PEM.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    Arc::new(config)
}

/// What the stub observed from one client connection.
#[derive(Debug)]
enum Observed {
    /// TLS was negotiated; carries the (decrypted) startup-message bytes.
    TlsStartup(Vec<u8>),
    /// The client sent a plaintext startup message (no `SSLRequest`).
    PlaintextStartup(Vec<u8>),
    /// The client sent an `SSLRequest` (reported by the refusing stub).
    SslRequest,
}

/// Start a one-connection stub Postgres server. `offer_tls` controls the
/// answer to an `SSLRequest`: `true` answers `S` and runs a rustls handshake,
/// `false` answers `N` (TLS refused). Returns the bound port and a channel
/// yielding what the stub observed.
fn spawn_stub(offer_tls: bool) -> (u16, mpsc::Receiver<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut first = [0u8; 8];
        stream.read_exact(&mut first).unwrap();
        let code = u32::from_be_bytes(first[4..8].try_into().unwrap());
        if code != SSL_REQUEST_CODE {
            // Plaintext startup message: `first` holds length + protocol
            // version; drain whatever else was sent and report it.
            let mut rest = vec![0u8; 512];
            let n = stream.read(&mut rest).unwrap_or(0);
            let mut startup = first.to_vec();
            startup.extend_from_slice(&rest[..n]);
            let _ = tx.send(Observed::PlaintextStartup(startup));
            return;
        }
        if !offer_tls {
            let _ = tx.send(Observed::SslRequest);
            let _ = stream.write_all(b"N");
            // Keep the socket open briefly so the client reads the refusal.
            std::thread::sleep(std::time::Duration::from_millis(200));
            return;
        }
        stream.write_all(b"S").unwrap();
        let conn = rustls::ServerConnection::new(rustls_server_config()).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, stream);
        // Reading drives the handshake to completion and then yields the
        // decrypted startup message.
        let mut startup = vec![0u8; 512];
        let n = tls.read(&mut startup).unwrap_or(0);
        startup.truncate(n);
        let _ = tx.send(Observed::TlsStartup(startup));
    });
    (port, rx)
}

fn pool_config(url: String) -> DatabaseConfig {
    DatabaseConfig {
        url: Some(url),
        pool_size: 1,
        connect_timeout_secs: 5,
        ..DatabaseConfig::default()
    }
}

async fn checkout_error(url: String) -> String {
    let pool = create_pool(&pool_config(url))
        .expect("pool construction must succeed")
        .expect("a URL was configured");
    match pool.get().await {
        Ok(_) => panic!("the stub is not a real Postgres — checkout must fail"),
        Err(e) => format!("{e}"),
    }
}

#[tokio::test]
async fn sslmode_require_negotiates_tls_and_sends_startup_encrypted() {
    let (port, rx) = spawn_stub(true);
    let error = checkout_error(format!(
        "postgres://app_user:secret@127.0.0.1:{port}/app?sslmode=require"
    ))
    .await;

    // Before TLS support this failed with diesel-async's NoTls error.
    assert!(
        !error.contains("no TLS implementation configured"),
        "sslmode=require must not hit the NoTls path, got: {error}"
    );

    // The stub completed a real rustls handshake and decrypted the Postgres
    // startup message: protocol 3.0 and the user parameter we supplied.
    let observed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stub must observe a connection");
    let Observed::TlsStartup(startup) = observed else {
        panic!("expected a TLS startup message, got {observed:?}");
    };
    assert!(startup.len() > 8, "startup message too short: {startup:?}");
    let protocol = u32::from_be_bytes(startup[4..8].try_into().unwrap());
    assert_eq!(protocol, PROTOCOL_VERSION);
    let haystack = String::from_utf8_lossy(&startup);
    assert!(
        haystack.contains("app_user"),
        "startup message must carry the user over TLS, got: {haystack}"
    );
}

/// The startup migration / wait-check path must use the SAME rustls
/// connector as the pool (issue #1585 review): the bundled libpq has no SSL
/// support, so a TLS-only server that the async pool reaches fine would
/// still fail at startup when `run_startup_migrations` (or `autumn migrate
/// --wait`) connects synchronously. The stub cannot run real migrations,
/// but a completed TLS handshake carrying the startup message proves the
/// wait path went through rustls rather than libpq.
///
/// A plain `#[test]` on purpose: the migration path is synchronous and
/// `block_on`s internally, so it must run without an ambient tokio runtime
/// (exactly like the CLI) — a `#[tokio::test]` worker thread would be the
/// one context it forbids.
#[test]
fn wait_for_database_negotiates_tls_for_sslmode_require() {
    let (port, rx) = spawn_stub(true);
    let url = format!("postgres://app_user:secret@127.0.0.1:{port}/app?sslmode=require");

    // The stub is not a real Postgres, so the wait can never succeed —
    // what matters is what the stub OBSERVES from the first attempt.
    let _ = autumn_web::migrate::wait_for_database(
        &url,
        std::time::Duration::from_secs(3),
        |_attempt, _delay| {},
    );

    let observed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stub must observe a connection from the wait path");
    let Observed::TlsStartup(startup) = observed else {
        panic!("the migration/wait path must negotiate TLS, got {observed:?}");
    };
    let haystack = String::from_utf8_lossy(&startup);
    assert!(
        haystack.contains("app_user"),
        "startup message must arrive over the encrypted stream, got: {haystack}"
    );
}

#[tokio::test]
async fn keyword_value_connection_string_passes_validation_and_negotiates_tls() {
    // The libpq keyword/value form (with quoting and whitespace around `=`)
    // must survive config validation (issue #1585 review: it used to be
    // rejected as "must start with postgres://" before ever reaching the
    // pool) AND drive the same TLS path as the URL form.
    let (port, rx) = spawn_stub(true);
    let url = format!(
        "host=127.0.0.1 port={port} user=app_user password='secret' dbname=app sslmode = require"
    );

    let config = pool_config(url.clone());
    config
        .validate()
        .expect("a keyword/value connection string must pass config validation");

    let error = checkout_error(url).await;
    assert!(
        !error.contains("no TLS implementation configured"),
        "keyword-form sslmode=require must not hit the NoTls path, got: {error}"
    );

    // The stub completed a rustls handshake and decrypted the startup
    // message carrying the user from the keyword string.
    let observed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stub must observe a connection");
    let Observed::TlsStartup(startup) = observed else {
        panic!("expected a TLS startup message, got {observed:?}");
    };
    let haystack = String::from_utf8_lossy(&startup);
    assert!(
        haystack.contains("app_user"),
        "startup message must carry the user over TLS, got: {haystack}"
    );
}

#[tokio::test]
async fn sslmode_require_fails_clearly_when_server_refuses_tls() {
    let (port, rx) = spawn_stub(false);
    let error = checkout_error(format!(
        "postgres://app_user:secret@127.0.0.1:{port}/app?sslmode=require"
    ))
    .await;

    assert!(
        error.contains("server does not support TLS"),
        "a TLS-less server must be reported clearly, got: {error}"
    );
    assert!(matches!(
        rx.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(Observed::SslRequest)
    ));
}

#[tokio::test]
async fn absent_sslmode_keeps_the_plaintext_notls_path() {
    let (port, rx) = spawn_stub(true);
    let _error = checkout_error(format!("postgres://app_user:secret@127.0.0.1:{port}/app")).await;

    // Zero behavior change for existing users: no SSLRequest is sent — the
    // client opens with the historical plaintext startup message.
    let observed = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stub must observe a connection");
    let Observed::PlaintextStartup(startup) = observed else {
        panic!("expected a plaintext startup message, got {observed:?}");
    };
    let protocol = u32::from_be_bytes(startup[4..8].try_into().unwrap());
    assert_eq!(protocol, PROTOCOL_VERSION);
}

#[tokio::test]
async fn sslmode_verify_full_rejects_the_self_signed_stub() {
    // The stricter mode must NOT accept the stub's self-signed certificate —
    // this is the identity check `require` deliberately skips.
    let (port, _rx) = spawn_stub(true);
    let error = checkout_error(format!(
        "postgres://app_user:secret@127.0.0.1:{port}/app?sslmode=verify-full"
    ))
    .await;
    assert!(
        error.to_lowercase().contains("certificate")
            || error.to_lowercase().contains("invalid peer")
            || error.to_lowercase().contains("unknown issuer")
            || error.to_lowercase().contains("handshake"),
        "verify-full must reject an untrusted certificate, got: {error}"
    );
    assert!(!error.contains("no TLS implementation configured"));
}

#[tokio::test]
async fn sslmode_verify_ca_fails_loudly_with_guidance() {
    // No listener needed: the unsupported combination is rejected before any
    // socket is opened.
    let error = checkout_error(
        "postgres://app_user:secret@127.0.0.1:5432/app?sslmode=verify-ca".to_owned(),
    )
    .await;
    assert!(
        error.contains("verify-ca") && error.contains("verify-full"),
        "verify-ca must fail with guidance, got: {error}"
    );
}
