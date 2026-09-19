//! Integration tests for first-class response compression (#974).
//!
//! Covers:
//! - Compressible dynamic response with `Accept-Encoding: gzip` → `Content-Encoding: gzip`
//! - `Vary: Accept-Encoding` present on all compressible responses
//! - Non-compressible content types are never compressed
//! - No double-compression when `Content-Encoding` is already set
//! - Compression is off by default (opt-in)

use autumn_web::config::{AutumnConfig, CompressionConfig};
use autumn_web::error::AutumnError;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::http::header;
use axum::response::{IntoResponse, Response};

// ── Handlers ─────────────────────────────────────────────────────────────────

const HTML_BODY: &str = concat!(
    "<!DOCTYPE html><html><body>",
    // Pad to ensure the body is large enough that gzip savings are positive.
    "Hello from Autumn! This is a compressible HTML response used to test ",
    "that the framework correctly applies gzip compression when the client ",
    "advertises Accept-Encoding: gzip. Lorem ipsum dolor sit amet. ",
    "Lorem ipsum dolor sit amet. Lorem ipsum dolor sit amet. ",
    "Lorem ipsum dolor sit amet. Lorem ipsum dolor sit amet. ",
    "</body></html>"
);

#[get("/html")]
async fn html_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        HTML_BODY,
    )
}

#[get("/json")]
async fn json_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"message":"hello","data":"lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod"}"#,
    )
}

#[get("/png")]
async fn png_handler() -> impl IntoResponse {
    // Simulate a binary image response (non-compressible content type).
    (
        [(header::CONTENT_TYPE, "image/png")],
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00".as_slice(),
    )
}

#[get("/zip")]
async fn zip_handler() -> impl IntoResponse {
    // Simulate a dynamic zip archive download (already compressed — should not be re-compressed).
    (
        [(header::CONTENT_TYPE, "application/zip")],
        b"PK\x03\x04fake-zip-content-for-testing-purposes-only-padded".as_slice(),
    )
}

/// A large-enough-to-compress error response, rebuilt by `ProblemDetailsFilter`
/// (`ExceptionFilterLayer`) rather than returned as-is by the handler.
#[get("/error")]
async fn error_handler() -> Result<String, AutumnError> {
    Err(AutumnError::not_found_msg(
        "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod \
         tempor incididunt ut labore et dolore magna aliqua — this resource \
         does not exist",
    ))
}

#[get("/pre-encoded")]
async fn pre_encoded_handler() -> Response {
    // Simulate a response that already has Content-Encoding set (e.g. pre-compressed asset).
    let mut resp = Response::new(axum::body::Body::from(vec![0u8; 32]));
    resp.headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    resp
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn compression_enabled_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.compression.enabled = true;
    config
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Compression is **off by default** — no Content-Encoding even with Accept-Encoding.
#[tokio::test]
async fn compression_disabled_by_default() {
    let app = TestApp::new().routes(routes![html_handler]).build();

    let resp = app
        .get("/html")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        None,
        "compression must be opt-in; default config must not compress"
    );
}

/// When enabled and client sends `Accept-Encoding: gzip`, response is gzip-compressed.
#[tokio::test]
async fn gzip_compression_applied_when_enabled_and_accepted() {
    let app = TestApp::new()
        .routes(routes![html_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/html")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        Some("gzip"),
        "should have Content-Encoding: gzip when compression is enabled and client accepts gzip"
    );
    // Compressed body should not equal the raw body (it's a different byte sequence).
    assert_ne!(
        resp.body,
        HTML_BODY.as_bytes(),
        "compressed body must differ from raw body"
    );
}

/// JSON responses are also compressed.
#[tokio::test]
async fn json_response_compressed_when_enabled() {
    let app = TestApp::new()
        .routes(routes![json_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/json")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        Some("gzip"),
        "JSON should be compressed"
    );
}

/// `Vary: Accept-Encoding` must be present on compressible responses when compression is enabled.
#[tokio::test]
async fn vary_accept_encoding_set_on_compressible_response() {
    let app = TestApp::new()
        .routes(routes![html_handler])
        .config(compression_enabled_config())
        .build();

    // Even without Accept-Encoding the Vary header should tell caches the
    // response could differ by encoding.
    let resp = app.get("/html").send().await;

    resp.assert_ok();
    let vary = resp.header("vary").unwrap_or("");
    assert!(
        vary.to_lowercase().contains("accept-encoding"),
        "Vary: Accept-Encoding must be set on compressible responses; got Vary: {vary:?}"
    );
}

/// Binary / already-non-compressible content types (image/png) must not be compressed.
#[tokio::test]
async fn binary_content_type_not_compressed() {
    let app = TestApp::new()
        .routes(routes![png_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/png")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        None,
        "binary content types must not be compressed"
    );
}

/// Responses that already carry `Content-Encoding` must not be double-compressed.
#[tokio::test]
async fn no_double_compression_when_already_encoded() {
    let app = TestApp::new()
        .routes(routes![pre_encoded_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/pre-encoded")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    // Content-Encoding should still be exactly "gzip" (the original), not "gzip, gzip".
    let ce = resp.header("content-encoding").unwrap_or("");
    assert_eq!(
        ce, "gzip",
        "double-compression must not occur; got Content-Encoding: {ce:?}"
    );
}

/// Without `Accept-Encoding` header, no compression is applied even when enabled.
#[tokio::test]
async fn no_compression_when_client_does_not_accept() {
    let app = TestApp::new()
        .routes(routes![html_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app.get("/html").send().await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        None,
        "no Accept-Encoding → no Content-Encoding"
    );
}

/// Archive content types (application/zip etc.) must not be compressed — they're
/// already compressed and re-compressing wastes CPU and increases transfer size.
#[tokio::test]
async fn archive_content_type_not_compressed() {
    let app = TestApp::new()
        .routes(routes![zip_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/zip")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    resp.assert_ok();
    assert_eq!(
        resp.header("content-encoding"),
        None,
        "application/zip must not be compressed by the framework"
    );
}

/// Regression test for issue #2371: response compression must stay OUTER to
/// the exception-filter chain (`ExceptionFilterLayer` / `ProblemDetailsFilter`
/// inside `apply_middleware`'s `outer_stack`), so a filter-rebuilt error body
/// (here, a `404` normalized to a JSON Problem Details document) is
/// compressed like any other response rather than inheriting a stale
/// `Content-Encoding` from a compression pass that already ran on the
/// handler's PRE-rebuild body. If compression were ever moved inner to the
/// exception filter, this response would carry `Content-Encoding: gzip` on a
/// body the filter rebuilt AFTER compression ran — plain bytes under a gzip
/// label — and a client gunzipping it would get garbage or a decode error,
/// which is exactly what this test's gzip round trip would catch.
#[tokio::test]
async fn compressed_error_response_gzip_decodes_to_the_rebuilt_body() {
    let app = TestApp::new()
        .routes(routes![error_handler])
        .config(compression_enabled_config())
        .build();

    let resp = app
        .get("/error")
        .header("accept-encoding", "gzip")
        .send()
        .await;

    assert_eq!(resp.status.as_u16(), 404);
    assert_eq!(
        resp.header("content-encoding"),
        Some("gzip"),
        "the exception-filter-rebuilt error body must still be compressed \
         like any other response"
    );

    let decoded = {
        use std::io::Read as _;
        let mut gz = flate2::read::GzDecoder::new(resp.body.as_slice());
        let mut out = String::new();
        gz.read_to_string(&mut out).expect(
            "Content-Encoding: gzip must decode as valid gzip -- a body that \
             is actually plain, uncompressed bytes under a stale gzip header \
             means compression ran BEFORE the exception filter rebuilt the \
             response, not after",
        );
        out
    };
    assert!(
        decoded.contains("does not exist"),
        "decoded body should be the exception filter's rebuilt JSON Problem \
         Details document, got: {decoded}"
    );
}

// ── Unit tests for CompressionConfig ─────────────────────────────────────────

#[test]
fn compression_config_disabled_by_default() {
    let config = CompressionConfig::default();
    assert!(!config.enabled, "compression must be off by default");
}

#[test]
fn compression_config_toml_round_trips() {
    let toml_str = r"
[compression]
enabled = true
";
    let config: AutumnConfig = toml::from_str(toml_str).unwrap();
    assert!(config.compression.enabled);
}

#[test]
fn compression_config_env_override() {
    use autumn_web::config::MockEnv;
    let env = MockEnv::new().with("AUTUMN_COMPRESSION__ENABLED", "true");
    let mut config = AutumnConfig::default();
    config.apply_env_overrides_with_env(&env);
    assert!(config.compression.enabled);
}
