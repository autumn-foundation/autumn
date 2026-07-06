//! Single-binary deploy proof for the `embed-assets` feature (issue #1004).
//!
//! Embeds a fixture `static/` tree (with its fingerprint manifest) and an
//! `i18n/` bundle into this test binary, then drives a router **from an empty
//! working directory** — proving that copying only the binary serves styled,
//! localized pages with zero sidecar files.

#![cfg(all(feature = "embed-assets", feature = "i18n"))]

use std::sync::Arc;

use autumn_web::i18n::{Bundle, I18nConfig, Locale};
use autumn_web::include_dir::{Dir, include_dir};
use axum::Extension;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Html;
use axum::routing::get;
use tower::ServiceExt;

static STATIC: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/fixtures/embed/static");
static LOCALES: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/fixtures/embed/i18n");

/// Fingerprint of `body{color:#0a0}\n` — the first 8 hex of its SHA-256, exactly
/// as `autumn build --embed` would compute. Kept in sync with the committed
/// fixture `static/css/app.<hash>.css` and `.autumn-manifest.json`.
const FINGERPRINTED: &str = "/static/css/app.79edc02e.css";

fn config() -> I18nConfig {
    I18nConfig {
        default_locale: "en".to_owned(),
        supported_locales: vec!["en".to_owned(), "es".to_owned()],
        fallback_chain: vec![],
        dir: "i18n".to_owned(),
    }
}

async fn home(locale: Locale) -> Html<String> {
    // `asset_url` must resolve against the embedded manifest (no disk read).
    let css = autumn_web::assets::asset_url("css/app.css");
    let greet = autumn_web::t!(locale, "greet");
    Html(format!(
        "<link rel=\"stylesheet\" href=\"{css}\"><p>{greet}</p>"
    ))
}

fn app() -> axum::Router {
    let bundle =
        Arc::new(Bundle::load_from_embedded(&LOCALES, &config()).expect("embedded bundle"));
    axum::Router::new()
        .route("/", get(home))
        .merge(autumn_web::assets::embedded_static_router())
        .layer(Extension(bundle))
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn get_path(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn single_binary_serves_styled_localized_pages_from_empty_dir() {
    // Register the embedded static tree as the process-wide asset source.
    autumn_web::assets::register_embedded_static(autumn_web::assets::EmbeddedStaticDir(&STATIC));

    // Run from an empty directory: zero sidecar files present. Everything served
    // below must come from the binary.
    let empty = tempfile::tempdir().unwrap();
    std::env::set_current_dir(empty.path()).unwrap();
    assert_eq!(
        std::fs::read_dir(empty.path()).unwrap().count(),
        0,
        "working directory must be empty — no static/ or i18n/ sidecars"
    );

    let app = app();

    // 1. Home page renders the fingerprinted asset URL from the embedded manifest.
    let resp = get_path(&app, "/").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_string(resp).await;
    assert!(
        html.contains(FINGERPRINTED),
        "home page must reference the embedded fingerprinted asset; got: {html}"
    );
    assert!(html.contains("Hello"), "default locale must render: {html}");

    // 2. The fingerprinted asset itself serves 200 with the right content-type
    //    and a year-long immutable cache lifetime (manifest membership).
    let resp = get_path(&app, FINGERPRINTED).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fingerprinted asset must 200"
    );
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/css; charset=utf-8"
    );
    assert!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable"),
        "fingerprinted asset must be immutable"
    );
    let expected_css = concat!("body{color:#0a0}", "\n");
    assert_eq!(body_string(resp).await, expected_css);

    // 3. The logical (non-fingerprinted) path also serves, but must-revalidate.
    let resp = get_path(&app, "/static/css/app.css").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("must-revalidate"),
        "non-fingerprinted asset must revalidate"
    );

    // 4. The embedded manifest itself must never be served to clients.
    let resp = get_path(&app, "/static/.autumn-manifest.json").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 5. A missing asset 404s rather than panicking.
    let resp = get_path(&app, "/static/css/missing.css").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 6. The non-default locale renders from the embedded bundle.
    let resp = get_path(&app, "/?locale=es").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_string(resp).await.contains("Hola"),
        "non-default locale must render from the embedded bundle"
    );
}
