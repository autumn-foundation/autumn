//! Integration tests for HTTP `Range` / `206 Partial Content` support
//! (issue #1352).
//!
//! Exercises the acceptance criteria end to end: ranged and non-ranged
//! [`Download`](autumn_web::download::Download) responses, `416` on an
//! unsatisfiable range, multi-range single-range collapse, `If-Range`
//! validation, the embedded static-asset path, and the blob path fetching only
//! the requested slice from the store.

use autumn_web::download::Download;
use autumn_web::etag::ETag;
use axum::body::to_bytes;
use bytes::Bytes;
use http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, IF_RANGE, RANGE};
use http::{HeaderMap, HeaderValue, StatusCode};

/// Read a response body fully into `Bytes`.
async fn body_bytes(resp: axum::response::Response) -> Bytes {
    to_bytes(resp.into_body(), usize::MAX).await.unwrap()
}

fn range_headers(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(RANGE, HeaderValue::from_str(value).unwrap());
    h
}

// ── AC #2: satisfiable ranged request → 206 ─────────────────────────────────

#[tokio::test]
async fn download_bytes_range_returns_206_with_content_range() {
    let payload = Bytes::from_static(b"0123456789");
    let headers = range_headers("bytes=0-3");
    let resp = Download::from_bytes(payload.clone())
        .into_response_ranged(&headers)
        .await;

    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT, "AC #2: 206");
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 0-3/10",
        "AC #2: Content-Range start-end/total"
    );
    assert_eq!(
        resp.headers().get(CONTENT_LENGTH).unwrap(),
        "4",
        "AC #2: Content-Length is the slice length"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..], b"0123", "AC #2: body is the requested slice");
}

// ── AC #3: non-ranged request → 200 + Accept-Ranges ─────────────────────────

#[tokio::test]
async fn download_bytes_without_range_is_200_and_advertises_accept_ranges() {
    let payload = Bytes::from_static(b"0123456789");
    let resp = Download::from_bytes(payload.clone())
        .into_response_ranged(&HeaderMap::new())
        .await;

    assert_eq!(resp.status(), StatusCode::OK, "AC #3: 200");
    assert_eq!(
        resp.headers().get(ACCEPT_RANGES).unwrap(),
        "bytes",
        "AC #3: Accept-Ranges: bytes advertised"
    );
    assert_eq!(
        resp.headers().get(CONTENT_LENGTH).unwrap(),
        "10",
        "AC #3: full Content-Length"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..], &payload[..]);
}

/// The plain `IntoResponse` conversion also advertises range support so a
/// client learns it may issue `Range` requests.
#[tokio::test]
async fn download_bytes_plain_into_response_advertises_accept_ranges() {
    use axum::response::IntoResponse as _;
    let resp = Download::from_bytes(Bytes::from_static(b"abc")).into_response();
    assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
}

// ── AC #4: unsatisfiable range → 416 ────────────────────────────────────────

#[tokio::test]
async fn download_bytes_range_beyond_eof_is_416() {
    let payload = Bytes::from_static(b"0123456789");
    let headers = range_headers("bytes=999-");
    let resp = Download::from_bytes(payload)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::RANGE_NOT_SATISFIABLE,
        "AC #4: 416"
    );
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap(),
        "bytes */10",
        "AC #4: Content-Range bytes */total"
    );
}

/// AC #4: a `416` is a fully-formed response — empty body, `Content-Length: 0`,
/// and it still advertises `Accept-Ranges: bytes` and `Content-Range: */total`.
#[tokio::test]
async fn download_bytes_416_has_empty_body_and_complete_headers() {
    let payload = Bytes::from_static(b"0123456789");
    let headers = range_headers("bytes=999-");
    let resp = Download::from_bytes(payload)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        resp.headers().get(CONTENT_LENGTH).unwrap(),
        "0",
        "AC #4: 416 carries Content-Length: 0"
    );
    assert_eq!(
        resp.headers().get(ACCEPT_RANGES).unwrap(),
        "bytes",
        "AC #4: 416 still advertises Accept-Ranges: bytes"
    );
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap(),
        "bytes */10",
        "AC #4: 416 carries Content-Range: bytes */total"
    );
    let body = body_bytes(resp).await;
    assert!(body.is_empty(), "AC #4: 416 body is empty");
}

/// A single-byte range (`bytes=0-0`) yields a one-byte `206` slice — the RFC
/// 7233 minimal partial request media players use to probe range support.
#[tokio::test]
async fn download_bytes_single_byte_range_returns_206() {
    let payload = Bytes::from_static(b"0123456789");
    let headers = range_headers("bytes=0-0");
    let resp = Download::from_bytes(payload.clone())
        .into_response_ranged(&headers)
        .await;

    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 0-0/10",
        "single-byte range Content-Range"
    );
    assert_eq!(
        resp.headers().get(CONTENT_LENGTH).unwrap(),
        "1",
        "single-byte range Content-Length is 1"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..], b"0", "body is the first byte");
}

// ── AC #6: multi-range collapses to the first range ─────────────────────────

#[tokio::test]
async fn download_bytes_multi_range_collapses_to_first() {
    let payload = Bytes::from_static(b"0123456789");
    let headers = range_headers("bytes=0-3,6-8");
    let resp = Download::from_bytes(payload)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap(),
        "bytes 0-3/10",
        "AC #6: multi-range collapses to the first sub-range"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..], b"0123");
}

// ── AC #7: If-Range validation ──────────────────────────────────────────────

#[tokio::test]
async fn download_if_range_matching_etag_serves_206() {
    let etag = ETag::strong("v1");
    let mut headers = range_headers("bytes=0-3");
    headers.insert(IF_RANGE, etag.header_value());

    let resp = Download::from_bytes(Bytes::from_static(b"0123456789"))
        .etag(etag)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::PARTIAL_CONTENT,
        "AC #7: matching If-Range validator serves the partial slice"
    );
}

#[tokio::test]
async fn download_if_range_stale_etag_serves_full_200() {
    // Resource's current tag is v2; client still holds v1.
    let current = ETag::strong("v2");
    let mut headers = range_headers("bytes=0-3");
    headers.insert(IF_RANGE, HeaderValue::from_static("\"v1\""));

    let resp = Download::from_bytes(Bytes::from_static(b"0123456789"))
        .etag(current)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "AC #7: stale If-Range validator falls back to full 200"
    );
    let body = body_bytes(resp).await;
    assert_eq!(
        &body[..],
        b"0123456789",
        "full body served on stale If-Range"
    );
}

#[tokio::test]
async fn download_if_range_matching_last_modified_serves_206() {
    let last_modified = "Wed, 21 Oct 2015 07:28:00 GMT";
    let mut headers = range_headers("bytes=0-3");
    headers.insert(IF_RANGE, HeaderValue::from_static(last_modified));

    let resp = Download::from_bytes(Bytes::from_static(b"0123456789"))
        .last_modified(last_modified)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::PARTIAL_CONTENT,
        "AC #7: matching If-Range Last-Modified date serves the partial slice"
    );
    assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes 0-3/10");
}

#[tokio::test]
async fn download_if_range_stale_last_modified_serves_full_200() {
    // Resource was modified Oct 21; the client still holds the older Oct 20 date.
    let current = "Wed, 21 Oct 2015 07:28:00 GMT";
    let mut headers = range_headers("bytes=0-3");
    headers.insert(
        IF_RANGE,
        HeaderValue::from_static("Tue, 20 Oct 2015 00:00:00 GMT"),
    );

    let resp = Download::from_bytes(Bytes::from_static(b"0123456789"))
        .last_modified(current)
        .into_response_ranged(&headers)
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "AC #7: stale If-Range Last-Modified date falls back to full 200"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..], b"0123456789", "full body served on stale date");
}

// ── AC #5 (static): embedded asset served through the range helper ──────────

#[cfg(feature = "embed-assets")]
mod embedded {
    use super::{body_bytes, range_headers};
    use autumn_web::include_dir::{Dir, include_dir};
    use axum::body::Body;
    use http::header::{ACCEPT_RANGES, CONTENT_RANGE};
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;

    // Same fixture tree used by `embed_assets_integration`; registration is a
    // process-wide OnceLock (first-wins), and both tests register byte-identical
    // contents, so the two are order-independent.
    static STATIC: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/fixtures/embed/static");

    #[tokio::test]
    async fn embedded_asset_range_returns_206() {
        autumn_web::assets::register_embedded_static(autumn_web::assets::EmbeddedStaticDir(
            &STATIC,
        ));
        let app = autumn_web::assets::embedded_static_router();

        // `css/app.css` fixture is `body{color:#0a0}\n`.
        let req = Request::builder()
            .uri("/static/css/app.css")
            .header(http::header::RANGE, "bytes=0-3")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::PARTIAL_CONTENT,
            "AC #5 static: embedded asset serves 206 for a Range request"
        );
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
        let content_range = resp
            .headers()
            .get(CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            content_range.starts_with("bytes 0-3/"),
            "AC #5 static: Content-Range present: {content_range}"
        );
        let body = body_bytes(resp).await;
        assert_eq!(&body[..], b"body", "AC #5 static: sliced embedded bytes");
    }

    #[tokio::test]
    async fn embedded_asset_without_range_is_200_with_accept_ranges() {
        let _ = range_headers; // shared helper, unused in this case
        autumn_web::assets::register_embedded_static(autumn_web::assets::EmbeddedStaticDir(
            &STATIC,
        ));
        let app = autumn_web::assets::embedded_static_router();

        let req = Request::builder()
            .uri("/static/css/app.css")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    }
}

// ── AC #5 (blob): ranged read fetches only the requested slice ──────────────

#[cfg(feature = "storage")]
mod blob {
    use super::{body_bytes, range_headers};
    use autumn_web::download::Download;
    use autumn_web::storage::{LocalBlobStore, SharedBlobStore, local::SigningKey};
    use bytes::Bytes;
    use http::StatusCode;
    use http::header::{CONTENT_LENGTH, CONTENT_RANGE};
    use std::sync::Arc;
    use std::time::Duration;

    fn make_store(dir: &std::path::Path) -> SharedBlobStore {
        Arc::new(
            LocalBlobStore::new(
                "default",
                dir.to_path_buf(),
                "/_blobs",
                Duration::from_secs(300),
                SigningKey::new(b"range-test-key".to_vec()),
                vec![],
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn blob_range_returns_206_with_correct_slice() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        let payload = Bytes::from_static(b"0123456789abcdef");
        store
            .put(
                "media/clip.bin",
                "application/octet-stream",
                payload.clone(),
            )
            .await
            .unwrap();

        let headers = range_headers("bytes=4-9");
        let resp = Download::from_blob(&store, "media/clip.bin")
            .await
            .unwrap()
            .into_response_ranged(&headers)
            .await;

        assert_eq!(
            resp.status(),
            StatusCode::PARTIAL_CONTENT,
            "AC #5 blob: 206"
        );
        assert_eq!(
            resp.headers().get(CONTENT_RANGE).unwrap(),
            "bytes 4-9/16",
            "AC #5 blob: Content-Range over the object size"
        );
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "6");
        let body = body_bytes(resp).await;
        assert_eq!(&body[..], b"456789", "AC #5 blob: exact slice bytes");
    }

    /// AC #5: the ranged read asks the store for **only** the requested slice
    /// rather than buffering the whole object. Exercise `get_range` directly and
    /// assert it yields just the slice.
    #[tokio::test]
    async fn store_get_range_yields_only_the_slice() {
        use futures::StreamExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        let payload = Bytes::from_static(b"0123456789abcdef");
        store
            .put("media/clip.bin", "application/octet-stream", payload)
            .await
            .unwrap();

        let mut stream = store.get_range("media/clip.bin", 4, 9).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            collected, b"456789",
            "AC #5 blob: get_range returns only [start, end], not the whole object"
        );
    }

    #[tokio::test]
    async fn blob_without_range_streams_full_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        let payload = Bytes::from_static(b"0123456789abcdef");
        store
            .put(
                "media/clip.bin",
                "application/octet-stream",
                payload.clone(),
            )
            .await
            .unwrap();

        let resp = Download::from_blob(&store, "media/clip.bin")
            .await
            .unwrap()
            .into_response_ranged(&http::HeaderMap::new())
            .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(http::header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let body = body_bytes(resp).await;
        assert_eq!(&body[..], &payload[..]);
    }

    #[tokio::test]
    async fn blob_range_beyond_eof_is_416() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        store
            .put(
                "media/clip.bin",
                "application/octet-stream",
                Bytes::from_static(b"short"),
            )
            .await
            .unwrap();

        let headers = range_headers("bytes=999-");
        let resp = Download::from_blob(&store, "media/clip.bin")
            .await
            .unwrap()
            .into_response_ranged(&headers)
            .await;

        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes */5");
    }
}
