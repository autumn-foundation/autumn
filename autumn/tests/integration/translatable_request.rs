//! Request-path proof for `#[translatable]` fields (issue #1384): the same
//! stored record renders different content under different `Accept-Language`
//! headers, with **zero locale arguments in the handler**.

#![cfg(feature = "i18n")]

use std::collections::HashMap;
use std::sync::Arc;

use autumn_web::i18n::{AmbientLocaleLayer, Bundle, I18nConfig, Translated, ambient_locale};
use axum::Extension;
use axum::body::Body;
use axum::http::{Request, header};
use axum::routing::get;
use tower::ServiceExt;

fn config() -> I18nConfig {
    I18nConfig {
        default_locale: "en".to_owned(),
        supported_locales: vec!["en".to_owned(), "es".to_owned(), "fr".to_owned()],
        fallback_chain: vec![],
        dir: "i18n".to_owned(),
        locale_prefix_enabled: false,
        locale_prefix_exclude: vec![],
        locale_prefix_exclude_exact: vec![],
    }
}

fn bundle() -> Arc<Bundle> {
    Arc::new(Bundle::from_messages(HashMap::new(), &config()))
}

/// Stands in for a row loaded by a repository: one record, translations in
/// `en` and `es`, nothing in `fr`.
fn seeded_post() -> Translated {
    let mut title = Translated::new();
    title.set("en", "Hello world");
    title.set("es", "Hola mundo");
    title
}

/// The whole point: **no `Locale` parameter, no locale argument**. The field
/// resolves itself.
async fn show_title() -> String {
    seeded_post().to_string()
}

/// Same, through the explicit `Option` surface.
async fn show_title_opt() -> String {
    seeded_post()
        .resolve()
        .map_or_else(|| "<untranslated>".to_owned(), str::to_owned)
}

async fn show_ambient() -> String {
    ambient_locale().unwrap_or_else(|| "<none>".to_owned())
}

fn router() -> axum::Router {
    axum::Router::new()
        .route("/title", get(show_title))
        .route("/title-opt", get(show_title_opt))
        .route("/ambient", get(show_ambient))
        .layer(AmbientLocaleLayer::new(&bundle()))
        .layer(Extension(bundle()))
}

async fn get_with(path: &str, accept_language: Option<&str>) -> String {
    let mut req = Request::builder().uri(path);
    if let Some(al) = accept_language {
        req = req.header(header::ACCEPT_LANGUAGE, al);
    }
    let resp = router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// AC2 / success metric: `Accept-Language: es` serves the Spanish value.
#[tokio::test]
async fn spanish_accept_language_renders_the_spanish_translation() {
    assert_eq!(get_with("/title", Some("es")).await, "Hola mundo");
}

/// AC3 / success metric: `fr` is untranslated, so resolution walks the
/// fallback chain down to `en` — no 500, no panic.
#[tokio::test]
async fn untranslated_locale_falls_back_through_the_chain() {
    assert_eq!(get_with("/title", Some("fr")).await, "Hello world");
    assert_eq!(get_with("/title-opt", Some("fr")).await, "Hello world");
}

#[tokio::test]
async fn default_locale_applies_without_an_accept_language_header() {
    assert_eq!(get_with("/title", None).await, "Hello world");
}

/// The layer establishes the ambient locale for the whole handler, which is
/// what removes the locale argument from the signature.
#[tokio::test]
async fn the_layer_publishes_the_negotiated_locale_as_the_ambient_one() {
    assert_eq!(get_with("/ambient", Some("es")).await, "es");
    assert_eq!(get_with("/ambient", Some("fr")).await, "fr");
    // Unsupported locale negotiates down to the configured default.
    assert_eq!(get_with("/ambient", Some("de")).await, "en");
}

/// The `?locale=` override the `Locale` extractor honours must move the
/// ambient locale too — the two must never disagree.
#[tokio::test]
async fn the_layer_honours_the_same_resolution_order_as_the_extractor() {
    assert_eq!(get_with("/title?locale=es", None).await, "Hola mundo");
}

// ── Locale-prefix routing (#1251) composition ────────────────────────────────

/// A `/{locale}/…` URL must move the ambient locale too. The prefix is only
/// visible *inside* the router's per-locale nest, so this is the case an
/// app-wide layer alone would silently get wrong.
#[tokio::test]
async fn locale_prefixed_urls_set_the_ambient_locale() {
    use autumn_web::i18n::UriPrefixedLocale;

    // Mirrors the router's nest: the prefix extension is installed *outside*
    // the ambient layer so the layer can see it.
    let inner = axum::Router::new()
        .route("/title", get(show_title))
        .route("/ambient", get(show_ambient))
        .layer(AmbientLocaleLayer::new(&bundle()))
        .layer(Extension(UriPrefixedLocale("es".to_owned())));
    let app = axum::Router::new()
        .nest("/es", inner)
        .layer(Extension(bundle()));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/es/title")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "Hola mundo");
}

// ── Streaming responses (#1384) ──────────────────────────────────────────────

/// A deferred/streaming body is polled AFTER the handler future resolves, so
/// without the layer's body wrapper the task-local scope would already be gone
/// and every frame would render the default locale.
#[tokio::test]
async fn a_streaming_body_still_resolves_to_the_request_locale() {
    use futures::stream;

    async fn stream_titles() -> axum::response::Response {
        let post = seeded_post();
        // Each frame renders lazily, one poll at a time, after this function
        // has already returned.
        let body = axum::body::Body::from_stream(stream::iter(
            (0..3).map(move |i| Ok::<_, std::io::Error>(format!("{i}:{post}\n"))),
        ));
        axum::response::Response::new(body)
    }

    let app = axum::Router::new()
        .route("/stream", get(stream_titles))
        .layer(AmbientLocaleLayer::new(&bundle()))
        .layer(Extension(bundle()));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/stream")
                .header(header::ACCEPT_LANGUAGE, "es")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text, "0:Hola mundo\n1:Hola mundo\n2:Hola mundo\n", "{text}");
}
