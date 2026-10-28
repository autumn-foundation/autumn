//! #2439 — the NUL field error is localized from the request locale.
//!
//! `ChangesetForm` and `NestedChangesetForm` record the message before any
//! handler holds a `Locale`. They resolve it from the `Arc<Bundle>` in request
//! extensions, using the catalog key `common.error.nul_character`.

use std::collections::HashMap;
use std::sync::Arc;

use autumn_web::form::{ChangesetForm, NUL_CHARACTER_FIELD_ERROR, NUL_CHARACTER_MESSAGE_KEY};
use autumn_web::i18n::{Bundle, I18nConfig};
use autumn_web::nested_form::{NestedChangesetForm, NestedChild};
use autumn_web::reexports::axum::body::Body;
use autumn_web::reexports::axum::http::Request;
use autumn_web::reexports::axum::response::IntoResponse;
use autumn_web::reexports::axum::routing::post;
use autumn_web::reexports::axum::{Extension, Router, response::Response};
use tower::ServiceExt as _;

#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
struct PostForm {
    #[serde(default)]
    body: String,
}

#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
struct Order {
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
struct Line {
    #[serde(default)]
    sku: String,
}

impl NestedChild for Line {
    const COLLECTION: &'static str = "items";
}

async fn flat(form: ChangesetForm<PostForm>) -> Response {
    form.errors_for("body").join("|").into_response()
}

async fn nested(form: NestedChangesetForm<Order, Line>) -> Response {
    format!(
        "{}#{}",
        form.errors_for("name").join("|"),
        form.rows()[0].errors_for("sku").join("|")
    )
    .into_response()
}

fn bundle(catalog: &[(&str, &[(&str, &str)])]) -> Arc<Bundle> {
    let config = I18nConfig {
        default_locale: "en".to_owned(),
        supported_locales: vec!["en".to_owned(), "es".to_owned()],
        ..I18nConfig::default()
    };
    let messages: HashMap<String, HashMap<String, String>> = catalog
        .iter()
        .map(|(locale, pairs)| {
            (
                (*locale).to_owned(),
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        })
        .collect();
    Arc::new(Bundle::from_messages(messages, &config))
}

fn full_catalog() -> Arc<Bundle> {
    bundle(&[
        ("en", &[(NUL_CHARACTER_MESSAGE_KEY, "English catalog text")]),
        (
            "es",
            &[(NUL_CHARACTER_MESSAGE_KEY, "No puede contener NUL")],
        ),
    ])
}

async fn call(router: Router, uri: &str, body: &'static str) -> String {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("request");
    let response = router.oneshot(request).await.expect("response");
    let bytes = autumn_web::reexports::axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn flat_router(bundle: Arc<Bundle>) -> Router {
    Router::new()
        .route("/submit", post(flat))
        .layer(Extension(bundle))
}

fn nested_router(bundle: Arc<Bundle>) -> Router {
    Router::new()
        .route("/order", post(nested))
        .layer(Extension(bundle))
}

#[test]
fn the_catalog_key_is_stable() {
    assert_eq!(NUL_CHARACTER_MESSAGE_KEY, "common.error.nul_character");
}

#[tokio::test]
async fn flat_form_uses_the_request_locale() {
    let text = call(
        flat_router(full_catalog()),
        "/submit?locale=es",
        "body=a%00b",
    )
    .await;
    assert_eq!(text, "No puede contener NUL");
}

#[tokio::test]
async fn flat_form_uses_the_default_locale_catalog_text() {
    let text = call(flat_router(full_catalog()), "/submit", "body=a%00b").await;
    assert_eq!(text, "English catalog text");
}

#[tokio::test]
async fn a_missing_key_falls_back_to_the_english_constant() {
    let empty = bundle(&[("en", &[]), ("es", &[])]);
    let router = flat_router(Arc::clone(&empty));
    let text = call(router, "/submit?locale=es", "body=a%00b").await;
    assert_eq!(text, NUL_CHARACTER_FIELD_ERROR);
    assert_eq!(empty.miss_count(), 0, "a fallback is not a missing key");
}

#[tokio::test]
async fn without_a_bundle_the_english_constant_is_used() {
    let router = Router::new().route("/submit", post(flat));
    let text = call(router, "/submit?locale=es", "body=a%00b").await;
    assert_eq!(text, NUL_CHARACTER_FIELD_ERROR);
}

#[tokio::test]
async fn a_clean_submission_records_no_error() {
    let text = call(flat_router(full_catalog()), "/submit?locale=es", "body=ab").await;
    assert_eq!(text, "");
}

#[tokio::test]
async fn nested_form_uses_the_request_locale_for_parent_and_row() {
    let text = call(
        nested_router(full_catalog()),
        "/order?locale=es",
        "name=a%00b&items%5B0%5D%5Bsku%5D=c%00d",
    )
    .await;
    assert_eq!(text, "No puede contener NUL#No puede contener NUL");
}

#[tokio::test]
async fn nested_form_without_a_bundle_uses_the_english_constant() {
    let router = Router::new().route("/order", post(nested));
    let text = call(router, "/order", "name=a%00b&items%5B0%5D%5Bsku%5D=ok").await;
    assert_eq!(text, format!("{NUL_CHARACTER_FIELD_ERROR}#"));
}
