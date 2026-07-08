//! Integration tests for first-class inbound email handling (issue #822).
//!
//! These tests follow TDD: they drive the public API contract for
//! `autumn_web::inbound_mail`.

#![cfg(feature = "inbound-mail")]

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::inbound_mail::{
    Attachment, InboundEmail, InboundMailEndpointConfig, InboundMailHandlerInfo,
    InboundMailProvider, InboundMailRouter, ProcessingMode, RecipientPattern,
    compute_mailgun_signature,
};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use bytes::Bytes;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Encode a slice of key-value pairs as `application/x-www-form-urlencoded`.
/// Test data does not contain characters that need percent-encoding, so a
/// simple join is sufficient.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build a valid Mailgun form body with the correct signature.
fn mailgun_form(key: &str, ts: &str, token: &str, extra: &[(&str, &str)]) -> String {
    let sig = compute_mailgun_signature(ts, token, key);
    let mut pairs: Vec<(&str, &str)> = vec![
        ("timestamp", ts),
        ("token", token),
        ("signature", sig.as_str()),
    ];
    pairs.extend_from_slice(extra);
    // We need the sig to be in the pairs — clone it first
    let sig_owned = sig;
    let _ = sig_owned; // already captured via sig.as_str() above... but that borrow doesn't live long enough
    // Build manually to avoid lifetime issues
    let base = format!("timestamp={ts}&token={token}&signature={sig_owned}");
    let extras = extra
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    if extras.is_empty() {
        base
    } else {
        format!("{base}&{extras}")
    }
}

/// Dummy route so TestApp is happy (it requires at least one route).
#[get("/_ping")]
async fn ping() -> &'static str {
    "pong"
}

// ── AC1: InboundEmail struct ──────────────────────────────────────────────────

#[test]
fn inbound_email_has_all_required_fields() {
    let email = InboundEmail {
        from: "sender@example.com".to_string(),
        to: vec!["support@company.com".to_string()],
        cc: vec!["cc@company.com".to_string()],
        subject: "Test subject".to_string(),
        text_body: Some("Hello world".to_string()),
        html_body: Some("<p>Hello</p>".to_string()),
        headers: [("x-mailer".to_string(), "Test/1.0".to_string())]
            .into_iter()
            .collect(),
        attachments: vec![],
        spam_report: None,
        raw: Bytes::from_static(b""),
        plus_token: None,
        is_bounce: false,
    };
    assert_eq!(email.from, "sender@example.com");
    assert_eq!(email.to, vec!["support@company.com"]);
    assert_eq!(email.cc, vec!["cc@company.com"]);
    assert_eq!(email.subject, "Test subject");
    assert!(email.text_body.is_some());
    assert!(email.html_body.is_some());
    assert!(email.headers.contains_key("x-mailer"));
}

#[test]
fn attachment_has_required_fields() {
    let a = Attachment {
        filename: Some("report.pdf".to_string()),
        content_type: "application/pdf".to_string(),
        data: Bytes::from_static(b"%PDF"),
    };
    assert_eq!(a.filename.as_deref(), Some("report.pdf"));
    assert_eq!(a.content_type, "application/pdf");
    assert!(!a.data.is_empty());
}

#[test]
fn inbound_email_plus_token_accessor() {
    let email = InboundEmail {
        from: "user@example.com".to_string(),
        to: vec!["replies+abc123@app.example".to_string()],
        cc: vec![],
        subject: "Re: ticket".to_string(),
        text_body: None,
        html_body: None,
        headers: Default::default(),
        attachments: vec![],
        spam_report: None,
        raw: Bytes::new(),
        plus_token: Some("abc123".to_string()),
        is_bounce: false,
    };
    assert_eq!(email.plus_token(), Some("abc123"));
}

// ── AC2: Provider enum ────────────────────────────────────────────────────────

#[test]
fn provider_enum_has_all_variants() {
    let _m = InboundMailProvider::Mailgun;
    let _s = InboundMailProvider::Ses;
    let _g = InboundMailProvider::Generic;
}

#[test]
fn mailgun_endpoint_config_constructor() {
    let c = InboundMailEndpointConfig::mailgun("/inbound/mailgun", "my-key");
    assert_eq!(c.provider, InboundMailProvider::Mailgun);
    assert_eq!(c.path, "/inbound/mailgun");
    assert_eq!(c.signing_key.as_deref(), Some("my-key"));
}

#[test]
fn ses_endpoint_config_constructor() {
    let c = InboundMailEndpointConfig::ses("/inbound/ses");
    assert_eq!(c.provider, InboundMailProvider::Ses);
    assert_eq!(c.path, "/inbound/ses");
    assert!(c.signing_key.is_none());
}

#[test]
fn generic_endpoint_config_constructor() {
    let c = InboundMailEndpointConfig::generic("/inbound/raw");
    assert_eq!(c.provider, InboundMailProvider::Generic);
    assert!(c.signing_key.is_none());
}

// ── AC3: compute_mailgun_signature ────────────────────────────────────────────

#[test]
fn compute_signature_returns_64_char_hex() {
    let sig = compute_mailgun_signature("1234567890", "abcdeftoken", "my-signing-key");
    assert_eq!(sig.len(), 64);
    assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn compute_signature_is_deterministic() {
    let s1 = compute_mailgun_signature("ts", "tok", "key");
    let s2 = compute_mailgun_signature("ts", "tok", "key");
    assert_eq!(s1, s2);
}

#[test]
fn compute_signature_differs_with_different_inputs() {
    let s1 = compute_mailgun_signature("ts1", "tok", "key");
    let s2 = compute_mailgun_signature("ts2", "tok", "key");
    assert_ne!(s1, s2);
}

// ── AC3: Endpoint rejects invalid signature ───────────────────────────────────

static REJECT_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn noop_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    REJECT_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn mailgun_invalid_signature_returns_401() {
    REJECT_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun(
            "/inbound/mailgun",
            "correct-key",
        ))
        .handler(InboundMailHandlerInfo {
            name: "noop",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: noop_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = encode_form(&[
        ("from", "sender@example.com"),
        ("to", "support@company.com"),
        ("subject", "Test"),
        ("body-plain", "Hello"),
        ("timestamp", "1234567890"),
        ("token", "some-token"),
        (
            "signature",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        ),
    ]);

    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(401);

    assert_eq!(
        REJECT_HANDLER_CALLS.load(Ordering::SeqCst),
        0,
        "handler must not be invoked on invalid signature"
    );
}

// ── AC3: Valid signature reaches handler ──────────────────────────────────────

static VALID_SIG_CALLS: AtomicUsize = AtomicUsize::new(0);

fn valid_sig_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    VALID_SIG_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn mailgun_valid_signature_reaches_handler() {
    VALID_SIG_CALLS.store(0, Ordering::SeqCst);

    let key = "test-signing-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "unique-token-abc";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "valid_sig",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: valid_sig_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "sender@example.com"),
            ("to", "support@company.com"),
            ("subject", "Integration-test"),
            ("body-plain", "Hello"),
        ],
    );

    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(VALID_SIG_CALLS.load(Ordering::SeqCst), 1);
}

// ── AC4: RecipientPattern — unit ──────────────────────────────────────────────

#[test]
fn pattern_exact_case_insensitive() {
    let p = RecipientPattern::Exact("support@company.com".to_string());
    assert!(p.matches("support@company.com"));
    assert!(p.matches("SUPPORT@COMPANY.COM"));
    assert!(!p.matches("other@company.com"));
}

#[test]
fn pattern_local_prefix_matches_prefix_and_plus() {
    let p = RecipientPattern::LocalPrefix("ticket".to_string());
    assert!(p.matches("ticket@example.com"));
    assert!(p.matches("ticket+123@example.com"));
    assert!(!p.matches("myticket@example.com"));
}

#[test]
fn pattern_plus_address_matches_and_extracts_token() {
    let p = RecipientPattern::PlusAddress {
        local: "replies".to_string(),
        domain: Some("app.example".to_string()),
    };
    assert!(p.matches("replies+abc@app.example"));
    assert!(!p.matches("other+abc@app.example"));
    assert!(!p.matches("replies@app.example"));
    assert_eq!(
        p.extract_token("replies+token42@app.example"),
        Some("token42".to_string())
    );
}

#[test]
fn pattern_plus_without_domain_matches_any_domain() {
    let p = RecipientPattern::PlusAddress {
        local: "replies".to_string(),
        domain: None,
    };
    assert!(p.matches("replies+tok@any.org"));
    assert_eq!(
        p.extract_token("replies+mytoken@anything.com"),
        Some("mytoken".to_string())
    );
}

#[test]
fn pattern_any_matches_everything() {
    let p = RecipientPattern::Any;
    assert!(p.matches("anyone@example.com"));
    assert!(p.matches(""));
}

// ── AC4: Routing dispatches to correct handler ────────────────────────────────

static SUPPORT_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static REPLY_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn support_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    SUPPORT_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

fn reply_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    REPLY_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn routing_dispatches_to_matching_handler() {
    SUPPORT_HANDLER_CALLS.store(0, Ordering::SeqCst);
    REPLY_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let key = "routing-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "routing-token";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "support",
            pattern: RecipientPattern::Exact("support@company.com".to_string()),
            processing: ProcessingMode::Sync,
            handler: support_handler,
        })
        .handler(InboundMailHandlerInfo {
            name: "replies",
            pattern: RecipientPattern::Exact("replies@company.com".to_string()),
            processing: ProcessingMode::Sync,
            handler: reply_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "user@example.com"),
            ("to", "support@company.com"),
            ("subject", "Help"),
            ("body-plain", "I-need-help"),
        ],
    );
    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(SUPPORT_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(REPLY_HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

// ── AC4: Plus-address token extracted ─────────────────────────────────────────

static PLUS_TOKEN_RECEIVED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn plus_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    *PLUS_TOKEN_RECEIVED.lock().unwrap() = email.plus_token().map(str::to_string);
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn routing_plus_address_captures_token() {
    *PLUS_TOKEN_RECEIVED.lock().unwrap() = None;

    let key = "plus-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "plus-token";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "replies",
            pattern: RecipientPattern::PlusAddress {
                local: "replies".to_string(),
                domain: None,
            },
            processing: ProcessingMode::Sync,
            handler: plus_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "user@example.com"),
            ("to", "replies%2Bticket-42@app.example"),
            ("subject", "Re:-ticket"),
            ("body-plain", "Following-up"),
        ],
    );
    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(
        PLUS_TOKEN_RECEIVED.lock().unwrap().as_deref(),
        Some("ticket-42"),
        "handler must receive the extracted plus token"
    );
}

// ── AC5: Background processing ────────────────────────────────────────────────

#[tokio::test]
async fn background_mode_returns_200_before_handler_completes() {
    fn slow_handler(
        email: InboundEmail,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
    > {
        let _ = email;
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            Ok(())
        })
    }

    let key = "bg-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "bg-token";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "slow",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Background,
            handler: slow_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "user@example.com"),
            ("to", "any@company.com"),
            ("subject", "Async-test"),
            ("body-plain", "Test"),
        ],
    );

    let start = std::time::Instant::now();
    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 300,
        "background processing should return before the slow handler finishes; elapsed: {}ms",
        elapsed.as_millis()
    );
}

// ── AC6: Fallback handler ─────────────────────────────────────────────────────

static FALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);

fn fallback_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    FALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

fn specific_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn unmatched_email_goes_to_fallback() {
    FALLBACK_CALLS.store(0, Ordering::SeqCst);

    let key = "fallback-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "fallback-token";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "specific",
            pattern: RecipientPattern::Exact("specific@company.com".to_string()),
            processing: ProcessingMode::Sync,
            handler: specific_fn,
        })
        .fallback(fallback_fn);

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "user@example.com"),
            ("to", "unknown@company.com"),
            ("subject", "Fallback-test"),
            ("body-plain", "No-handler"),
        ],
    );
    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(FALLBACK_CALLS.load(Ordering::SeqCst), 1);
}

// ── AC6: Bounce handler ───────────────────────────────────────────────────────

static BOUNCE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn bounce_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    BOUNCE_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn bounce_event_routes_to_bounce_handler() {
    BOUNCE_CALLS.store(0, Ordering::SeqCst);

    let key = "bounce-key";
    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let token = "bounce-token";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .on_bounce(bounce_fn);

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    // Mailgun signals bounces via the X-Mailgun-Bounced-Address form field.
    let body = mailgun_form(
        key,
        ts,
        token,
        &[
            ("from", "MAILER-DAEMON@mailgun.net"),
            ("to", "bounced@company.com"),
            ("subject", "Delivery-failed"),
            ("body-plain", "bounce"),
            ("X-Mailgun-Bounced-Address", "user@bad-domain.com"),
        ],
    );
    client
        .post("/inbound/mailgun")
        .form(&body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(BOUNCE_CALLS.load(Ordering::SeqCst), 1);
}

// ── AC2: Generic RFC 5322 provider ────────────────────────────────────────────

static GENERIC_CALLS: AtomicUsize = AtomicUsize::new(0);
static GENERIC_SUBJECT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn generic_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    GENERIC_CALLS.fetch_add(1, Ordering::SeqCst);
    *GENERIC_SUBJECT.lock().unwrap() = email.subject.clone();
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn generic_provider_parses_rfc5322_body() {
    GENERIC_CALLS.store(0, Ordering::SeqCst);

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::generic("/inbound/raw"))
        .handler(InboundMailHandlerInfo {
            name: "generic",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: generic_fn,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let raw_email = "From: sender@example.com\r\n\
                     To: support@company.com\r\n\
                     Subject: RFC-5322-Test\r\n\
                     MIME-Version: 1.0\r\n\
                     Content-Type: text/plain\r\n\
                     \r\n\
                     Hello from a raw RFC 5322 email!";

    client
        .post("/inbound/raw")
        .header("content-type", "message/rfc822")
        .body(raw_email)
        .send()
        .await
        .assert_status(200);

    assert_eq!(GENERIC_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(*GENERIC_SUBJECT.lock().unwrap(), "RFC-5322-Test");
}

// ── AC3: Signing key from env var ─────────────────────────────────────────────

#[test]
fn endpoint_config_signing_key_from_env() {
    let c = InboundMailEndpointConfig {
        path: "/inbound/mailgun".to_string(),
        provider: InboundMailProvider::Mailgun,
        signing_key: None,
        signing_key_env: Some("MY_MAILGUN_KEY".to_string()),
        processing: ProcessingMode::Background,
        topic_arn: None,
    };
    assert_eq!(c.signing_key_env.as_deref(), Some("MY_MAILGUN_KEY"));
}

// ── AC5: Default processing mode ─────────────────────────────────────────────

#[test]
fn default_processing_mode_is_background() {
    assert_eq!(ProcessingMode::default(), ProcessingMode::Background);
}

// ── AC3: Generic endpoint signing key ─────────────────────────────────────────

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

static GENERIC_SIGNED_CALLS: AtomicUsize = AtomicUsize::new(0);

fn generic_signed_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    GENERIC_SIGNED_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn generic_valid_hmac_signature_accepts_request() {
    GENERIC_SIGNED_CALLS.store(0, Ordering::SeqCst);

    let key = "generic-secret";
    let raw_email = "From: sender@example.com\r\n\
                     To: support@company.com\r\n\
                     Subject: Signed-Generic\r\n\
                     \r\n\
                     Signed body.";
    let sig = hmac_sha256_hex(key.as_bytes(), raw_email.as_bytes());

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig {
            path: "/inbound/generic".to_string(),
            provider: InboundMailProvider::Generic,
            signing_key: Some(key.to_string()),
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        })
        .handler(InboundMailHandlerInfo {
            name: "signed",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: generic_signed_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/generic")
        .header("x-inbound-signature", sig.as_str())
        .header("content-type", "message/rfc822")
        .body(raw_email)
        .send()
        .await
        .assert_status(200);

    assert_eq!(GENERIC_SIGNED_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn generic_invalid_hmac_signature_returns_401() {
    let key = "generic-secret2";
    let raw_email = "From: s@example.com\r\nTo: r@example.com\r\nSubject: Bad\r\n\r\nBody";

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig {
            path: "/inbound/generic".to_string(),
            provider: InboundMailProvider::Generic,
            signing_key: Some(key.to_string()),
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        })
        .handler(InboundMailHandlerInfo {
            name: "signed2",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: generic_signed_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/generic")
        .header("x-inbound-signature", "wrongsignature")
        .header("content-type", "message/rfc822")
        .body(raw_email)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn generic_missing_env_var_returns_500() {
    // Endpoint configured with env var, but the var is not set → fail closed.
    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig {
            path: "/inbound/generic".to_string(),
            provider: InboundMailProvider::Generic,
            signing_key: None,
            signing_key_env: Some("AUTUMN_TEST_MISSING_KEY_XYZ".to_string()),
            processing: ProcessingMode::Background,
            topic_arn: None,
        })
        .handler(InboundMailHandlerInfo {
            name: "failclosed",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: generic_signed_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/generic")
        .header("content-type", "message/rfc822")
        .body("From: a@b.com\r\nTo: c@d.com\r\nSubject: S\r\n\r\nB")
        .send()
        .await
        .assert_status(500);
}

static MULTIPART_SUBJECT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn multipart_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    *MULTIPART_SUBJECT.lock().unwrap() = email.subject.clone();
    Box::pin(async { Ok(()) })
}

#[tokio::test]
async fn generic_multipart_email_parsed() {
    *MULTIPART_SUBJECT.lock().unwrap() = String::new();

    let boundary = "TESTBOUNDARY";
    let raw_email = format!(
        "From: sender@example.com\r\n\
         To: support@company.com\r\n\
         Subject: Multipart-Test\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary={boundary}\r\n\
         \r\n\
         --{boundary}\r\n\
         Content-Type: text/plain\r\n\
         \r\n\
         Plain text part\r\n\
         --{boundary}\r\n\
         Content-Type: text/html\r\n\
         \r\n\
         <p>HTML part</p>\r\n\
         --{boundary}--\r\n"
    );

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::generic("/inbound/raw2"))
        .handler(InboundMailHandlerInfo {
            name: "multipart",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: multipart_fn,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/raw2")
        .header("content-type", "message/rfc822")
        .body(raw_email.into_bytes())
        .send()
        .await
        .assert_status(200);

    assert_eq!(*MULTIPART_SUBJECT.lock().unwrap(), "Multipart-Test");
}

// ── SES / SNS HTTP endpoint ───────────────────────────────────────────────────

#[cfg(feature = "inbound-ses")]
static SES_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Disable SNS signature verification for the remainder of this test process.
///
/// Uses the `SKIP_SNS_VERIFICATION` atomic flag exported by `autumn_web::inbound_mail`
/// rather than `std::env::set_var` (which is `unsafe` in edition 2024) or
/// `temp_env::async_with_vars` (which races when two tests run concurrently).
#[cfg(feature = "inbound-ses")]
fn set_skip_sns_verification() {
    autumn_web::inbound_mail::SKIP_SNS_VERIFICATION
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "inbound-ses")]
fn ses_dispatch_handler(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    SES_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[cfg(feature = "inbound-ses")]
#[tokio::test]
async fn ses_subscription_confirmation_fetch_failure_returns_500() {
    set_skip_sns_verification();

    let router = InboundMailRouter::new()
        .endpoint(
            InboundMailEndpointConfig::ses("/inbound/ses")
                .with_topic_arn("arn:aws:sns:us-east-1:123456789012:test-topic"),
        )
        .handler(InboundMailHandlerInfo {
            name: "ses",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: ses_dispatch_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    let body = serde_json::json!({
        "Type": "SubscriptionConfirmation",
        "SubscribeURL": "https://sns.example.com/confirm?token=abc123"
    })
    .to_string();

    client
        .post("/inbound/ses")
        .header("x-amz-sns-message-type", "SubscriptionConfirmation")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        // The SubscribeURL is unreachable in tests, so the handler returns 500 and SNS
        // will retry the SubscriptionConfirmation delivery.
        .assert_status(500);
}

#[cfg(feature = "inbound-ses")]
#[tokio::test]
async fn ses_notification_dispatches_to_handler() {
    set_skip_sns_verification();
    SES_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let raw = "From: sender@example.com\r\n\
                   To: support@company.com\r\n\
                   Subject: SES-Integration\r\n\
                   \r\n\
                   Hello from SES.";
    let encoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(raw)
    };

    let body = serde_json::json!({
        "Type": "Notification",
        "Message": encoded
    })
    .to_string();

    let router = InboundMailRouter::new()
        .endpoint(
            InboundMailEndpointConfig::ses("/inbound/ses")
                .with_topic_arn("arn:aws:sns:us-east-1:123456789012:test-topic"),
        )
        .handler(InboundMailHandlerInfo {
            name: "ses_notify",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: ses_dispatch_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/ses")
        .header("x-amz-sns-message-type", "Notification")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(SES_HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "inbound-ses")]
#[tokio::test]
async fn ses_bad_json_returns_400() {
    let router = InboundMailRouter::new()
        .endpoint(
            InboundMailEndpointConfig::ses("/inbound/ses")
                .with_topic_arn("arn:aws:sns:us-east-1:123456789012:test-topic"),
        )
        .handler(InboundMailHandlerInfo {
            name: "ses_bad",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: ses_dispatch_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/ses")
        .header("x-amz-sns-message-type", "Notification")
        .header("content-type", "application/json")
        .body("not-json")
        .send()
        .await
        .assert_status(400);
}

// ── Mailgun empty signing key ─────────────────────────────────────────────────

#[tokio::test]
async fn mailgun_empty_signing_key_returns_500() {
    // Mailgun endpoint with no key at all → parse_mailgun rejects with 500.
    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig {
            path: "/inbound/mailgun".to_string(),
            provider: InboundMailProvider::Mailgun,
            signing_key: None,
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        })
        .handler(InboundMailHandlerInfo {
            name: "nokey",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: noop_handler,
        });

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/mailgun")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("from=a%40b.com&to=c%40d.com&timestamp=1&token=t&signature=s")
        .send()
        .await
        .assert_status(500);
}

// ── on_spam API ───────────────────────────────────────────────────────────────

static SPAM_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn spam_fn(
    email: InboundEmail,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
> {
    SPAM_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = email;
    Box::pin(async { Ok(()) })
}

#[test]
fn on_spam_registers_handler() {
    // Verify the builder API compiles and stores the handler without panicking.
    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::generic("/inbound"))
        .on_spam(spam_fn);
    drop(router);
}

#[tokio::test]
async fn mailgun_spam_flagged_dispatched_to_spam_handler() {
    SPAM_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let ts = &std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let key = "spam-key";
    let body = mailgun_form(
        key,
        ts,
        "spam-tok",
        &[
            ("from", "sender@example.com"),
            ("to", "user@example.com"),
            ("X-Mailgun-Sflag", "Yes"),
        ],
    );

    let router = InboundMailRouter::new()
        .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", key))
        .handler(InboundMailHandlerInfo {
            name: "regular",
            pattern: RecipientPattern::Any,
            processing: ProcessingMode::Sync,
            handler: noop_handler,
        })
        .on_spam(spam_fn);

    let client = TestApp::new()
        .inbound_mail_router(router)
        .routes(routes![ping])
        .build();

    client
        .post("/inbound/mailgun")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .assert_status(200);

    assert_eq!(SPAM_HANDLER_CALLS.load(Ordering::SeqCst), 1);
}
