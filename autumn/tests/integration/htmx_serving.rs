#![cfg(feature = "htmx")]
//! Tests for htmx embedding and serving.

#[test]
fn htmx_version_is_accessible() {
    let version = autumn_web::HTMX_VERSION;
    assert!(!version.is_empty());
    assert!(
        version.starts_with("2."),
        "Expected htmx 2.x, got {version}"
    );
}

#[test]
fn idiomorph_asset_bytes_are_non_empty() {
    let bytes = autumn_web::IDIOMORPH_JS;
    assert!(!bytes.is_empty(), "IDIOMORPH_JS must be non-empty");
    assert_eq!(
        autumn_web::IDIOMORPH_JS_PATH,
        "/static/js/idiomorph.min.js",
        "IDIOMORPH_JS_PATH must be served at /static/js/idiomorph.min.js"
    );
}
