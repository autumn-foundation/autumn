//! End-to-end integration tests for the i18n module: extractor + bundle +
//! Axum request flow. These exercise the same surface a real handler hits.

#![cfg(feature = "i18n")]

use std::collections::HashMap;
use std::sync::Arc;

use autumn_web::i18n::{Bundle, I18nConfig, Locale};
use axum::Extension;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tower::ServiceExt;

fn config() -> I18nConfig {
    I18nConfig {
        default_locale: "en".to_owned(),
        supported_locales: vec!["en".to_owned(), "es".to_owned()],
        fallback_chain: vec![],
        dir: "i18n".to_owned(),
    }
}

fn bundle() -> Arc<Bundle> {
    let mut messages = HashMap::new();
    let mut en = HashMap::new();
    en.insert("greeting".to_owned(), "Hello, { $name }!".to_owned());
    en.insert("only_en".to_owned(), "english only".to_owned());
    messages.insert("en".to_owned(), en);
    let mut es = HashMap::new();
    es.insert("greeting".to_owned(), "¡Hola, { $name }!".to_owned());
    messages.insert("es".to_owned(), es);
    Arc::new(Bundle::from_messages(messages, &config()))
}

async fn greet_handler(locale: Locale) -> impl IntoResponse {
    autumn_web::t!(locale, "greeting", name = "Ada")
}

async fn fallback_handler(locale: Locale) -> impl IntoResponse {
    autumn_web::t!(locale, "only_en")
}

fn router() -> axum::Router {
    axum::Router::new()
        .route("/greet", get(greet_handler))
        .route("/fallback", get(fallback_handler))
        .layer(Extension(bundle()))
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn english_default_when_no_headers() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/greet")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, "Hello, Ada!");
}

#[tokio::test]
async fn spanish_via_accept_language() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/greet")
                .header(header::ACCEPT_LANGUAGE, "es-MX,es;q=0.9,en;q=0.5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "¡Hola, Ada!");
}

#[tokio::test]
async fn query_override_wins_over_accept_language() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/greet?locale=en")
                .header(header::ACCEPT_LANGUAGE, "es")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "Hello, Ada!");
}

#[tokio::test]
async fn cookie_wins_over_accept_language() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/greet")
                .header(header::COOKIE, "autumn_locale=es")
                .header(header::ACCEPT_LANGUAGE, "en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "¡Hola, Ada!");
}

#[tokio::test]
async fn missing_key_falls_back_to_default_locale() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/fallback?locale=es")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // `only_en` is missing in `es` — should fall back to `en`.
    assert_eq!(body_string(resp).await, "english only");
}

#[tokio::test]
async fn unsupported_locale_falls_through_to_default() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/greet?locale=ja")
                .header(header::ACCEPT_LANGUAGE, "ja-JP,ja;q=0.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "Hello, Ada!");
}

// ── AutumnConfig deserialization ──────────────────────────────

#[test]
fn autumn_toml_deserializes_i18n_block() {
    let toml_str = r#"
        [i18n]
        default_locale = "es"
        supported_locales = ["es", "en"]
        fallback_chain = ["es", "en"]
        dir = "translations"
    "#;
    let cfg: autumn_web::config::AutumnConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.i18n.default_locale, "es");
    assert_eq!(
        cfg.i18n.supported_locales,
        vec!["es".to_owned(), "en".to_owned()]
    );
    assert_eq!(cfg.i18n.dir, "translations");
}

#[test]
fn autumn_toml_uses_defaults_when_block_absent() {
    let cfg: autumn_web::config::AutumnConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.i18n.default_locale, "en");
    assert_eq!(cfg.i18n.supported_locales, vec!["en".to_owned()]);
    assert_eq!(cfg.i18n.dir, "i18n");
}
