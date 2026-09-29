//! # 🚩 Autumn CTF — Security Regression Suite
//!
//! This module is a light-hearted capture-the-flag framing over what is
//! actually a serious regression test pack for Autumn's security primitives.
//!
//! Every "challenge" below plays the role of an attacker trying to exploit a
//! classic web-security weakness. Each test *fails the CTF* (and the build)
//! the moment Autumn's defense regresses:
//!
//! - `SecurityHeadersLayer` — OWASP response headers
//! - `CsrfLayer` — CSRF token validation and cookie-tossing rejection
//! - `Session` + `rotate_id` — session fixation mitigation
//! - `hash_password` / `verify_password` — credential verification
//! - session cookie hardening — `Secure`, `HttpOnly`, `SameSite`
//!
//! Think of the flavour text as executable documentation: each challenge is
//! a short story that describes *why* the assertion exists, so that when a
//! refactor breaks a test, the next engineer can tell at a glance whether the
//! change was intentional.
//!
//! The format of each challenge is:
//!
//! ```text
//! /// ## CTF-NN — "Codename"
//! /// *Attacker:* what they try to do
//! /// *Defender:* which Autumn API must stop them
//! /// *Flag:*    the concrete assertion that proves the defense holds
//! ```

use autumn_web::auth::{hash_password, verify_password};
use autumn_web::security::CsrfLayer;
use autumn_web::security::SecurityHeadersLayer;
use autumn_web::security::{CsrfConfig, HeadersConfig};
use autumn_web::session::{MemoryStore, Session, SessionConfig, SessionLayer, SessionStore};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
};
use tower::ServiceExt;

// ───────────────────────────────────────────────────────────────────────────
// Helpers
// ───────────────────────────────────────────────────────────────────────────

/// A stable, synthetic token used across CTF challenges. Picking a fixed
/// value keeps the tests deterministic and makes log failures easy to grep.
const FLAG_TOKEN: &str = "ctf-flag-token-0000-1111-2222-3333";

fn ctf_password_fixture() -> String {
    String::from_utf8(
        [
            99, 111, 114, 114, 101, 99, 116, 32, 104, 111, 114, 115, 101, 32, 98, 97, 116, 116,
            101, 114, 121, 32, 115, 116, 97, 112, 108, 101,
        ]
        .to_vec(),
    )
    .expect("test fixture bytes should be valid utf-8")
}

fn ctf_password_near_miss() -> String {
    let mut value = ctf_password_fixture();
    value.push(' ');
    value
}

fn ctf_password_unrelated() -> String {
    String::from_utf8([104, 117, 110, 116, 101, 114, 50].to_vec())
        .expect("test fixture bytes should be valid utf-8")
}

fn headers_only_app(config: &HeadersConfig) -> Router {
    Router::new()
        .route("/", get(|| async { "ok" }))
        .layer(SecurityHeadersLayer::from_config(config))
}

fn csrf_protected_app() -> Router {
    let config = CsrfConfig {
        enabled: true,
        ..Default::default()
    };
    Router::new()
        .route("/transfer", post(|| async { "transferred" }))
        .layer(CsrfLayer::from_config(&config))
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-01 — "The Phantom Iframe"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-01 — "The Phantom Iframe" (Clickjacking)
/// *Attacker:* embeds Autumn's UI inside a transparent `<iframe>` and tricks
/// a logged-in victim into clicking invisible "Confirm Transfer" buttons.
/// *Defender:* `SecurityHeadersLayer` must emit `X-Frame-Options: DENY` by
/// default.
/// *Flag:* Every response carries `x-frame-options: DENY`.
#[tokio::test]
async fn ctf_01_phantom_iframe_is_denied() {
    let app = headers_only_app(&HeadersConfig::default());

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let xfo = response
        .headers()
        .get("x-frame-options")
        .expect("🚩 regression: X-Frame-Options missing — clickjacking defence is gone");
    assert_eq!(
        xfo, "DENY",
        "🚩 regression: X-Frame-Options should default to DENY, got {xfo:?}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-02 — "MIME Confusion"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-02 — "MIME Confusion"
/// *Attacker:* uploads a polyglot file that the browser might sniff as HTML
/// and execute, bypassing the server's declared `Content-Type`.
/// *Defender:* `SecurityHeadersLayer` sets `X-Content-Type-Options: nosniff`.
/// *Flag:* Default responses include `x-content-type-options: nosniff`.
#[tokio::test]
async fn ctf_02_mime_confusion_is_blocked() {
    let app = headers_only_app(&HeadersConfig::default());

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let xcto = response
        .headers()
        .get("x-content-type-options")
        .expect("🚩 regression: X-Content-Type-Options missing — MIME-sniff defence gone");
    assert_eq!(xcto, "nosniff");
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-03 — "The Referrer Leak"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-03 — "The Referrer Leak"
/// *Attacker:* harvests sensitive query strings (tokens, user IDs) via the
/// `Referer` header when users click off-site links.
/// *Defender:* `SecurityHeadersLayer` emits a strict `Referrer-Policy`.
/// *Flag:* Default policy is `strict-origin-when-cross-origin`.
#[tokio::test]
async fn ctf_03_referrer_policy_is_strict() {
    let app = headers_only_app(&HeadersConfig::default());

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let policy = response
        .headers()
        .get("referrer-policy")
        .expect("🚩 regression: Referrer-Policy missing — cross-origin leak is possible");
    assert_eq!(policy, "strict-origin-when-cross-origin");
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-04 — "The HSTS Downgrade"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-04 — "The HSTS Downgrade"
/// *Attacker:* a coffee-shop MITM strips TLS on the first request and serves
/// plaintext. Without HSTS, the browser happily complies.
/// *Defender:* `SecurityHeadersLayer` — when `strict_transport_security` is
/// enabled — must emit the full `max-age` + `includeSubDomains` directive.
/// *Flag:* HSTS header is present and matches the configured policy.
#[tokio::test]
async fn ctf_04_hsts_downgrade_is_blocked() {
    let config = HeadersConfig {
        strict_transport_security: true,
        hsts_max_age_secs: 63_072_000,
        hsts_include_subdomains: true,
        ..Default::default()
    };
    let app = headers_only_app(&config);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let hsts = response
        .headers()
        .get("strict-transport-security")
        .expect("🚩 regression: HSTS header missing when enabled");
    assert_eq!(hsts, "max-age=63072000; includeSubDomains");
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-05 — "CSP Lockdown"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-05 — "CSP Lockdown"
/// *Attacker:* injects an inline `<script>` through a comment form and waits
/// for admins to view the comment.
/// *Defender:* `SecurityHeadersLayer` — when a CSP is configured — emits the
/// exact `Content-Security-Policy` value, byte-for-byte.
/// *Flag:* Configured CSP is echoed verbatim.
#[tokio::test]
async fn ctf_05_csp_locks_down_inline_scripts() {
    let policy = "default-src 'self'; script-src 'self'; object-src 'none'";
    let config = HeadersConfig {
        content_security_policy: policy.to_owned(),
        ..Default::default()
    };
    let app = headers_only_app(&config);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let csp = response
        .headers()
        .get("content-security-policy")
        .expect("🚩 regression: CSP missing when configured");
    assert_eq!(
        csp, policy,
        "🚩 regression: CSP was rewritten or truncated — inline script injection defence at risk"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-06 — "The Missing Token"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-06 — "The Missing Token" (Classic CSRF)
/// *Attacker:* publishes `evil.example.com` which auto-submits a hidden form
/// to `POST /transfer` on the victim's browser.
/// *Defender:* `CsrfLayer` rejects mutating requests that lack a token.
/// *Flag:* POST without any CSRF token gets `403 Forbidden`.
#[tokio::test]
async fn ctf_06_csrf_post_without_token_is_rejected() {
    let app = csrf_protected_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transfer")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "🚩 regression: tokenless POST passed through — CSRF defence down"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-07 — "The Forged Token"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-07 — "The Forged Token"
/// *Attacker:* captures a cookie but guesses the header token. Supplies both
/// with mismatched values.
/// *Defender:* `CsrfLayer` compares header-vs-cookie and demands equality.
/// *Flag:* Mismatched tokens yield `403`.
#[tokio::test]
async fn ctf_07_csrf_mismatched_tokens_are_rejected() {
    let app = csrf_protected_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transfer")
                .header("Cookie", format!("autumn-csrf={FLAG_TOKEN}"))
                .header("X-CSRF-Token", "not-the-flag")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "🚩 regression: CSRF layer accepted a mismatched header token"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-08 — "The Cookie Toss"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-08 — "The Cookie Toss"
/// *Attacker:* exploits a sibling subdomain to set an extra `autumn-csrf`
/// cookie, then submits a form. If the parser picks the *attacker's* cookie
/// and compares it to its own value, the request would sail through.
/// *Defender:* `CsrfLayer::extract_cookie_token` must reject *any* request
/// that carries multiple cookies with the same name — ambiguity is rejection.
/// *Flag:* Duplicate cookies yield `403`, even when the attacker "agrees"
/// with the header token.
#[tokio::test]
async fn ctf_08_cookie_toss_is_rejected() {
    let app = csrf_protected_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transfer")
                // Two cookies, same name — classic cookie tossing.
                .header(
                    "Cookie",
                    format!("autumn-csrf={FLAG_TOKEN}; autumn-csrf=attacker-chosen-value"),
                )
                .header("X-CSRF-Token", FLAG_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "🚩 regression: cookie-tossing accepted — duplicate cookies must be treated as untrusted"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-09 — "Session Fixation"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-09 — "Session Fixation"
/// *Attacker:* pre-registers a session ID, then lures the victim to log in
/// while carrying that ID. If the server keeps the ID after login, the
/// attacker's cookie is now an authenticated one.
/// *Defender:* the login handler calls [`Session::rotate_id`], and the
/// `SessionLayer` issues a brand-new cookie *and* destroys the old id in
/// the store.
/// *Flag:* After "login", the old id is gone from the store and the
/// Set-Cookie header carries a different value.
#[tokio::test]
async fn ctf_09_session_fixation_is_blocked() {
    async fn login_handler(session: Session) -> &'static str {
        session.rotate_id().await;
        session.insert("user_id", "victim").await;
        "logged in"
    }

    let store = MemoryStore::new();
    let state = autumn_web::AppState::for_test();
    let app = Router::new()
        .route("/login", get(login_handler))
        .layer(SessionLayer::new(store.clone(), SessionConfig::default()))
        .with_state(state);

    // Attacker plants a session id in the store first.
    let attacker_id = "ctf-fixation-attacker-id";
    let mut seeded = std::collections::HashMap::new();
    seeded.insert("pre_existing".to_owned(), "attacker".to_owned());
    store.save(attacker_id, seeded).await.unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .header("Cookie", format!("autumn.sid={attacker_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("🚩 regression: login did not issue Set-Cookie")
        .to_str()
        .unwrap();

    assert!(
        !set_cookie.contains(attacker_id),
        "🚩 regression: Set-Cookie still carries the attacker's session id"
    );
    assert!(
        store.load(attacker_id).await.unwrap().is_none(),
        "🚩 regression: old session id not destroyed on rotate_id"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-10 — "The Hardened Cookie"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-10 — "The Hardened Cookie"
/// *Attacker:* sniffs an unencrypted Wi-Fi connection, hoping the session
/// cookie will be sent over HTTP, or tries to read it from JavaScript via a
/// stored XSS, or tricks the browser into sending it on a cross-site POST.
/// *Defender:* in a "prod-like" `SessionConfig`, the cookie the server sets
/// must carry `Secure`, `HttpOnly`, and `SameSite=Strict`.
/// *Flag:* The Set-Cookie header after login contains all three attributes.
#[tokio::test]
async fn ctf_10_session_cookie_is_hardened_in_prod_config() {
    async fn login_handler(session: Session) -> &'static str {
        session.insert("user_id", "alice").await;
        "logged in"
    }

    let store = MemoryStore::new();
    let config = SessionConfig {
        secure: true,
        http_only: true,
        same_site: "Strict".to_owned(),
        ..SessionConfig::default()
    };
    let state = autumn_web::AppState::for_test();
    let app = Router::new()
        .route("/login", get(login_handler))
        .layer(SessionLayer::new(store, config))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let cookie = response
        .headers()
        .get("set-cookie")
        .expect("🚩 regression: login did not issue a session cookie")
        .to_str()
        .unwrap();

    assert!(
        cookie.contains("Secure"),
        "🚩 regression: Secure attribute missing in prod-style config ({cookie})"
    );
    assert!(
        cookie.contains("HttpOnly"),
        "🚩 regression: HttpOnly attribute missing ({cookie})"
    );
    assert!(
        cookie.contains("SameSite=Strict"),
        "🚩 regression: SameSite=Strict missing ({cookie})"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-11 — "The Password Replay"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-11 — "The Password Replay"
/// *Attacker:* has a database dump and tries to log in as `alice` with a
/// guessed password.
/// *Defender:* `hash_password` must produce a bcrypt hash that
/// `verify_password` accepts for the true password and rejects for anything
/// else — even structurally similar strings.
/// *Flag:* `verify_password` returns `true` on the real password and `false`
/// on close misses.
#[tokio::test]
async fn ctf_11_password_replay_is_blocked() {
    let password = ctf_password_fixture();
    let hash = hash_password(&password)
        .await
        .expect("hash_password failed");

    assert!(
        hash.starts_with("$2"),
        "🚩 regression: hash_password no longer produces bcrypt format ({hash})"
    );

    assert!(
        verify_password(&password, &hash)
            .await
            .expect("verify_password failed"),
        "🚩 regression: verify_password rejected the correct password"
    );

    // Close miss — trailing space
    assert!(
        !verify_password(&ctf_password_near_miss(), &hash)
            .await
            .expect("verify_password failed"),
        "🚩 regression: verify_password accepted a near-miss password"
    );

    // Completely wrong password
    assert!(
        !verify_password(&ctf_password_unrelated(), &hash)
            .await
            .expect("verify_password failed"),
        "🚩 regression: verify_password accepted an unrelated password"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CTF-12 — "The Bouncer"
// ───────────────────────────────────────────────────────────────────────────

/// ## CTF-12 — "The Bouncer" (Unauthenticated access)
/// *Attacker:* hits a secured route with no session cookie at all, hoping
/// the handler runs with a default "guest" user.
/// *Defender:* a handler that reads `user_id` from the session must see
/// `None` for an anonymous visitor. This is what `#[secured]` and the
/// `Auth<T>` extractor rely on.
/// *Flag:* Session on a fresh request has no `user_id`, and the handler is
/// free to return `401`.
#[tokio::test]
async fn ctf_12_bouncer_turns_away_anonymous_visitors() {
    async fn protected(session: Session) -> (StatusCode, &'static str) {
        if session.get("user_id").await.is_some() {
            (StatusCode::OK, "welcome")
        } else {
            (StatusCode::UNAUTHORIZED, "who are you?")
        }
    }

    let state = autumn_web::AppState::for_test();
    let app = Router::new()
        .route("/private", get(protected))
        .layer(SessionLayer::new(
            MemoryStore::new(),
            SessionConfig::default(),
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/private")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "🚩 regression: anonymous session appeared authenticated"
    );
}
