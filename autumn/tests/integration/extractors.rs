//!
//! Integration tests for extractors.
//!
#[cfg(feature = "multipart")]
use autumn_web::extract::Multipart;
use autumn_web::extract::{Form, Json, Path, Query};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

// ── Json tests ───────────────────────────────────────────────

#[derive(Deserialize)]
struct JsonInput {
    value: i32,
}

#[derive(Serialize)]
struct JsonOutput {
    doubled: i32,
}

#[derive(Debug, PartialEq, Eq)]
struct WrapperCompatInput {
    title: String,
}

fn title_from_ref(input: &WrapperCompatInput) -> &str {
    &input.title
}

const fn str_len(input: &str) -> usize {
    input.len()
}

#[test]
fn extractor_wrappers_preserve_axum_style_deref_coercions() {
    let form = Form(WrapperCompatInput {
        title: "form".to_owned(),
    });
    assert_eq!(title_from_ref(&form), "form");

    let json = Json(WrapperCompatInput {
        title: "json".to_owned(),
    });
    assert_eq!(title_from_ref(&json), "json");

    let query = Query(WrapperCompatInput {
        title: "query".to_owned(),
    });
    assert_eq!(title_from_ref(&query), "query");

    let id = Path(42_i64);
    assert_eq!(*id, 42);

    let slug = Path("problem-details".to_owned());
    assert_eq!(str_len(&slug), 15);
}

#[tokio::test]
async fn json_extractor_and_response() {
    async fn handler(Json(input): Json<JsonInput>) -> Json<JsonOutput> {
        Json(JsonOutput {
            doubled: input.value * 2,
        })
    }

    let app = Router::new().route("/", axum::routing::post(handler));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"value": 21}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("application/json"));

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(output["doubled"], 42);
}

#[tokio::test]
async fn invalid_json_returns_error() {
    async fn handler(Json(_): Json<serde_json::Value>) -> &'static str {
        "ok"
    }

    let app = Router::new().route("/", axum::routing::post(handler));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── Form tests ───────────────────────────────────────────────

#[derive(Deserialize)]
struct FormInput {
    name: String,
    age: u32,
}

#[tokio::test]
async fn form_extraction_works() {
    async fn handler(Form(data): Form<FormInput>) -> String {
        format!("{} is {}", data.name, data.age)
    }

    let app = Router::new().route("/", axum::routing::post(handler));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("name=Alice&age=30"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"Alice is 30");
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_extraction_works() {
    async fn handler(mut multipart: Multipart) -> autumn_web::AutumnResult<String> {
        let field = multipart
            .next_field()
            .await?
            .expect("expected one multipart field");
        let name = field.file_name().unwrap_or("missing").to_string();
        let body = field.bytes_limited().await?;
        Ok(format!("{name}:{}", body.len()))
    }

    let boundary = "X-BOUNDARY";
    let payload = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nhello world\r\n--{boundary}--\r\n"
    );

    let app = Router::new().route("/", axum::routing::post(handler));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"hello.txt:11");
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_mime_allow_list_skips_non_file_fields() {
    async fn handler(mut multipart: Multipart) -> autumn_web::AutumnResult<String> {
        let mut text_seen = false;
        let mut file_seen = false;

        while let Some(field) = multipart.next_field().await? {
            if field.file_name().is_some() {
                file_seen = true;
            } else {
                text_seen = true;
            }
            let _ = field.bytes_limited().await?;
        }

        Ok(format!("text={text_seen},file={file_seen}"))
    }

    // The allow-list is enforced by CONTENT (magic bytes), not the declared
    // header, and only for file parts. The non-file `title` field must bypass
    // it entirely, while the genuine PNG file part is accepted by its sniffed
    // type even though its declared header says octet-stream.
    let boundary = "X-BOUNDARY";
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nreport\r\n")
            .as_bytes(),
    );
    payload.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"real.png\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    payload.extend_from_slice(&[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52,
    ]);
    payload.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let app = Router::new()
        .route("/", axum::routing::post(handler))
        .layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(autumn_web::security::UploadConfig {
                        allowed_mime_types: vec!["image/png".to_owned()],
                        ..autumn_web::security::UploadConfig::default()
                    });
                next.run(req).await
            },
        ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"text=true,file=true");
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_file_size_limit_returns_413() {
    async fn handler(mut multipart: Multipart) -> autumn_web::AutumnResult<&'static str> {
        let field = multipart.next_field().await?.expect("expected field");
        let _ = field.bytes_limited().await?;
        Ok("ok")
    }

    let boundary = "X-BOUNDARY";
    let payload = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nhello world\r\n--{boundary}--\r\n"
    );

    let app = Router::new()
        .route("/", axum::routing::post(handler))
        .layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(autumn_web::security::UploadConfig {
                        max_file_size_bytes: 4,
                        ..autumn_web::security::UploadConfig::default()
                    });
                next.run(req).await
            },
        ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_request_size_limit_returns_413() {
    async fn handler(mut multipart: Multipart) -> autumn_web::AutumnResult<&'static str> {
        let _ = multipart.next_field().await?;
        Ok("ok")
    }

    let boundary = "X-BOUNDARY";
    let payload = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nhello world\r\n--{boundary}--\r\n"
    );

    let app = Router::new()
        .route("/", axum::routing::post(handler))
        .layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(autumn_web::security::UploadConfig {
                        max_request_size_bytes: 8,
                        ..autumn_web::security::UploadConfig::default()
                    });
                next.run(req).await
            },
        ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ── Content-sniffing (magic-byte) MIME validation (issue #1354) ──────

/// Build a multipart body for a single file part as raw bytes, since binary
/// fixtures (PNG/JPEG) are not valid UTF-8.
#[cfg(feature = "multipart")]
fn single_file_multipart_body(
    boundary: &str,
    field_name: &str,
    file_name: &str,
    declared_content_type: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_name}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {declared_content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Same as `single_file_multipart_body`, but emits NO `Content-Type` header
/// for the file part (used to prove strict mismatch mode can't be bypassed by
/// omitting the declared type).
#[cfg(feature = "multipart")]
fn single_file_multipart_body_no_content_type(
    boundary: &str,
    field_name: &str,
    file_name: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[cfg(feature = "multipart")]
async fn sniff_handler(mut multipart: Multipart) -> autumn_web::AutumnResult<String> {
    let field = multipart
        .next_field()
        .await?
        .expect("expected one multipart field");
    let sniffed = field.sniffed_content_type().unwrap_or("none").to_string();
    let body = field.bytes_limited().await?;
    Ok(format!("{sniffed}:{}", body.len()))
}

#[cfg(feature = "multipart")]
async fn run_sniff_upload(
    config: autumn_web::security::UploadConfig,
    body: Vec<u8>,
    boundary: &str,
) -> axum::http::Response<Body> {
    let app = Router::new()
        .route("/", axum::routing::post(sniff_handler))
        .layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let config = config.clone();
                async move {
                    req.extensions_mut().insert(config);
                    next.run(req).await
                }
            },
        ));

    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[cfg(feature = "multipart")]
fn png_fixture() -> Vec<u8> {
    // PNG magic bytes + a bit more so `infer` recognizes it.
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52,
    ]
}

#[cfg(feature = "multipart")]
fn jpeg_fixture() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
    ]
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_spoofed_html_declared_png_is_rejected() {
    let boundary = "X-BOUNDARY";
    let html = b"<!DOCTYPE html><script>alert(1)</script>";
    let body = single_file_multipart_body(boundary, "file", "evil.png", "image/png", html);

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_genuine_png_passes() {
    let boundary = "X-BOUNDARY";
    let png = png_fixture();
    // Deliberately declare a NON-png type to prove the client header is ignored.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "real.png",
        "application/octet-stream",
        &png,
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    // Verified type is exposed and full byte count preserved (prefix not lost).
    assert_eq!(&out[..], format!("image/png:{}", png.len()).as_bytes());
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_mismatch_mode_rejects_when_declared_disagrees() {
    let boundary = "X-BOUNDARY";
    let png = png_fixture();
    // Genuine PNG but declared image/jpeg. Both are individually allowed,
    // yet strict mismatch mode must reject because declared != sniffed.
    let body = single_file_multipart_body(boundary, "file", "real.png", "image/jpeg", &png);

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned(), "image/jpeg".to_owned()],
        reject_on_content_type_mismatch: true,
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_short_file_rejected_when_allow_list_set() {
    let boundary = "X-BOUNDARY";
    // 2-byte binary file — too short for `infer` to recognize (sniffed = None)
    // and not markup. Its declared type (octet-stream) is NOT in the allow-list,
    // so the declared-essence fallback also fails → reject. This proves an
    // unverifiable file cannot slip in unless its declared type is allow-listed.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "tiny.bin",
        "application/octet-stream",
        &[0x01, 0x02],
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_empty_allow_list_passes_through() {
    let boundary = "X-BOUNDARY";
    let html = b"<!DOCTYPE html><script>alert(1)</script>";
    // No allow-list, mismatch mode off: additive change must not reject.
    let body = single_file_multipart_body(boundary, "file", "page.png", "image/png", html);

    let config = autumn_web::security::UploadConfig::default();

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    // No sniff happened (empty allow-list, mismatch off) → sniffed = none,
    // and the full body is still readable.
    assert_eq!(&out[..], format!("none:{}", html.len()).as_bytes());
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_jpeg_allow_list_passes() {
    let boundary = "X-BOUNDARY";
    let jpeg = jpeg_fixture();
    let body = single_file_multipart_body(
        boundary,
        "file",
        "photo.bin",
        "application/octet-stream",
        &jpeg,
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/jpeg".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&out[..], format!("image/jpeg:{}", jpeg.len()).as_bytes());
}

// ── FIX 1: signature-less text formats via declared-essence fallback ─

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_genuine_json_passes_via_declared_fallback() {
    let boundary = "X-BOUNDARY";
    // `infer` has no magic bytes for JSON → sniffed None, not markup →
    // declared essence `application/json` (in allow-list) admits it.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "data.json",
        "application/json",
        br#"{"a":1}"#,
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["application/json".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    // Sniffing could not positively verify the type, so it stays "none".
    assert_eq!(&out[..], b"none:7");
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_genuine_csv_passes_via_declared_fallback() {
    let boundary = "X-BOUNDARY";
    let body = single_file_multipart_body(boundary, "file", "data.csv", "text/csv", b"a,b\n1,2");

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["text/csv".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&out[..], b"none:7");
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_svg_rejected_even_when_declared_image() {
    let boundary = "X-BOUNDARY";
    // SVG is markup: the leading `<` must trip the markup guard (or be sniffed
    // as image/svg+xml), so it can never be admitted under an image/png list.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "evil.png",
        "image/png",
        b"<svg onload=alert(1)></svg>",
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_html_markup_guard_blocks_declared_fallback() {
    let boundary = "X-BOUNDARY";
    // HTML declared as text/csv (an allow-listed text type): the markup guard
    // must reject before the declared-essence fallback can admit it.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "page.csv",
        "text/csv",
        b"<!DOCTYPE html><script>alert(1)</script>",
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["text/csv".to_owned()],
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── FIX 2: strict mode can't be bypassed by omitting Content-Type ────

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_strict_mode_rejects_missing_content_type() {
    let boundary = "X-BOUNDARY";
    let png = png_fixture();
    // Genuine PNG but NO declared Content-Type. Strict mode must reject rather
    // than let an attacker skip the mismatch check by omitting the header.
    let body = single_file_multipart_body_no_content_type(boundary, "file", "real.png", &png);

    let config = autumn_web::security::UploadConfig {
        reject_on_content_type_mismatch: true,
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── FIX 3: declared essence ignores parameters ──────────────────────

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_declared_essence_ignores_parameters() {
    let boundary = "X-BOUNDARY";
    let png = png_fixture();
    // `image/png; charset=binary` must compare equal to sniffed `image/png`.
    let body = single_file_multipart_body(
        boundary,
        "file",
        "real.png",
        "image/png; charset=binary",
        &png,
    );

    let config = autumn_web::security::UploadConfig {
        allowed_mime_types: vec!["image/png".to_owned()],
        reject_on_content_type_mismatch: true,
        ..autumn_web::security::UploadConfig::default()
    };

    let response = run_sniff_upload(config, body, boundary).await;
    assert_eq!(response.status(), StatusCode::OK);
    let out = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&out[..], format!("image/png:{}", png.len()).as_bytes());
}
