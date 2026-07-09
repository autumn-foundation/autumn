//! Integration tests for the typed [`Download`](autumn_web::download::Download)
//! `IntoResponse` (issue #1141).
//!
//! Covers every acceptance criterion: the three constructors, the
//! `Content-Disposition`/`Content-Type`/`Content-Length` header contract,
//! RFC 5987 filename encoding, header-injection sanitization, the `inline`
//! variant, and the blob-backed streaming path against a `LocalBlobStore`.

use autumn_web::download::Download;
use axum::body::to_bytes;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};

/// Read a response body fully into `Bytes` for assertions.
async fn body_bytes(resp: axum::response::Response) -> Bytes {
    to_bytes(resp.into_body(), usize::MAX).await.unwrap()
}

#[tokio::test]
async fn from_bytes_sets_content_length_and_round_trips() {
    let payload = b"hello download world";
    let resp = Download::from_bytes(Bytes::from_static(payload)).into_response();

    assert_eq!(
        resp.headers().get(CONTENT_LENGTH).unwrap(),
        payload.len().to_string().as_str(),
        "Content-Length must reflect the byte length (AC #4)"
    );

    let body = body_bytes(resp).await;
    assert_eq!(&body[..], payload, "body must round-trip (AC #1)");
}

#[tokio::test]
async fn ascii_filename_uses_plain_form_and_infers_pdf() {
    let resp = Download::from_bytes(Bytes::from_static(b"%PDF-1.4"))
        .filename("report.pdf")
        .into_response();

    let disp = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        disp, "attachment; filename=\"report.pdf\"",
        "ASCII names use the plain filename= form (AC #2)"
    );

    let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
    assert_eq!(
        ct, "application/pdf",
        "content-type inferred from .pdf extension (AC #3)"
    );
}

#[tokio::test]
async fn non_ascii_filename_uses_rfc5987_extended_form() {
    let resp = Download::from_bytes(Bytes::from_static(b"data"))
        .filename("naïve 文件.txt")
        .into_response();

    let disp = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disp.contains("filename*=UTF-8''"),
        "non-ASCII names emit the RFC 5987 extended form (AC #2): {disp}"
    );
    assert!(
        disp.starts_with("attachment;"),
        "still an attachment disposition: {disp}"
    );
    // The extended value must be percent-encoded (no raw non-ASCII bytes).
    assert!(disp.is_ascii(), "header value must be pure ASCII: {disp}");
}

#[tokio::test]
async fn explicit_content_type_overrides_inference() {
    let resp = Download::from_bytes(Bytes::from_static(b"x"))
        .filename("report.pdf")
        .content_type("application/x-custom")
        .into_response();

    let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
    assert_eq!(
        ct, "application/x-custom",
        "explicit .content_type() wins over extension inference (AC #3)"
    );
}

#[tokio::test]
async fn unknown_type_falls_back_to_octet_stream() {
    let resp = Download::from_bytes(Bytes::from_static(b"x")).into_response();

    let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
    assert_eq!(
        ct, "application/octet-stream",
        "no filename + no explicit type falls back (AC #3)"
    );
}

#[tokio::test]
async fn inline_variant_sets_inline_disposition() {
    let resp = Download::from_bytes(Bytes::from_static(b"x"))
        .filename("preview.pdf")
        .inline()
        .into_response();

    let disp = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disp.starts_with("inline"),
        ".inline() emits an inline disposition (AC #5): {disp}"
    );
}

#[tokio::test]
async fn crlf_in_filename_is_sanitized_no_header_injection() {
    let resp = Download::from_bytes(Bytes::from_static(b"x"))
        .filename("a\r\nSet-Cookie: x=1")
        .into_response();

    // No injected header from the payload.
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "CRLF injection must not create a Set-Cookie header (AC #8)"
    );

    // Exactly one Content-Disposition header, CR/LF stripped.
    let all: Vec<_> = resp.headers().get_all(CONTENT_DISPOSITION).iter().collect();
    assert_eq!(all.len(), 1, "single Content-Disposition line (AC #8)");
    let disp = all[0].to_str().unwrap();
    assert!(!disp.contains('\r') && !disp.contains('\n'));
    assert!(
        !disp.contains("Set-Cookie:") || disp.contains("\"aSet-Cookie: x=1\""),
        "any residual text stays inside the quoted filename value: {disp}"
    );
}

#[tokio::test]
async fn from_stream_reassembles_multi_chunk_body() {
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from_static(b"chunk-one-")),
        Ok(Bytes::from_static(b"chunk-two-")),
        Ok(Bytes::from_static(b"chunk-three")),
    ];
    let resp = Download::from_stream(stream::iter(chunks))
        .filename("data.bin")
        .into_response();

    // Length unknown for a stream.
    assert!(
        resp.headers().get(CONTENT_LENGTH).is_none(),
        "stream downloads have no Content-Length (AC #4)"
    );

    let body = body_bytes(resp).await;
    assert_eq!(&body[..], b"chunk-one-chunk-two-chunk-three");
}

#[tokio::test]
async fn from_async_read_streams_reader_bytes() {
    let data = b"async-read-payload".to_vec();
    let reader = std::io::Cursor::new(data.clone());
    let resp = Download::from_async_read(reader)
        .filename("x.bin")
        .into_response();

    let body = body_bytes(resp).await;
    assert_eq!(&body[..], &data[..], "AsyncRead body round-trips (AC #1)");
}

#[cfg(feature = "storage")]
mod blob {
    use super::{Download, body_bytes};
    use axum::response::IntoResponse;
    use bytes::Bytes;
    use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
    use std::sync::Arc;
    use std::time::Duration;

    use autumn_web::storage::{LocalBlobStore, SharedBlobStore, local::SigningKey};

    fn make_store(dir: &std::path::Path) -> SharedBlobStore {
        Arc::new(
            LocalBlobStore::new(
                "default",
                dir.to_path_buf(),
                "/_blobs",
                Duration::from_secs(300),
                SigningKey::new(b"download-test-key".to_vec()),
                vec![],
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn from_blob_streams_bytes_length_and_type_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        let payload = Bytes::from_static(b"%PDF-1.7 stored blob bytes");
        store
            .put("reports/q3.pdf", "application/pdf", payload.clone())
            .await
            .unwrap();

        let resp = Download::from_blob(&store, "reports/q3.pdf")
            .await
            .unwrap()
            .filename("report.pdf")
            .into_response();

        assert_eq!(
            resp.headers().get(CONTENT_LENGTH).unwrap(),
            payload.len().to_string().as_str(),
            "Content-Length from blob metadata (AC #4)"
        );
        // filename is report.pdf → still application/pdf, and blob meta agrees.
        let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert_eq!(ct, "application/pdf", "content-type available (AC #3/#4)");

        let body = body_bytes(resp).await;
        assert_eq!(&body[..], &payload[..], "blob body round-trips (AC #1)");
    }

    #[tokio::test]
    async fn from_blob_content_type_defaults_to_blob_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        // No filename set → content-type should come from blob metadata.
        store
            .put(
                "k/data",
                "application/x-blobtype",
                Bytes::from_static(b"zz"),
            )
            .await
            .unwrap();

        let resp = Download::from_blob(&store, "k/data")
            .await
            .unwrap()
            .into_response();

        let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert_eq!(
            ct, "application/x-blobtype",
            "blob metadata content-type used when no filename/explicit type (AC #3)"
        );
    }

    /// AC #6: the blob download is a plain `IntoResponse`, so it composes
    /// inside any handler — including a policy-protected one. This exercises
    /// the value returned from a handler-shaped async fn.
    #[tokio::test]
    async fn from_blob_composes_in_a_handler_expression() {
        let dir = tempfile::tempdir().unwrap();
        let store = make_store(dir.path());
        store
            .put(
                "private/doc.txt",
                "text/plain",
                Bytes::from_static(b"secret"),
            )
            .await
            .unwrap();

        let resp = handler(store).await;
        let body = body_bytes(resp).await;
        assert_eq!(&body[..], b"secret");
    }

    /// Shape of a `#[secured]` handler body: build the download in one
    /// expression from a resolved store + key.
    async fn handler(store: SharedBlobStore) -> axum::response::Response {
        Download::from_blob(&store, "private/doc.txt")
            .await
            .unwrap()
            .filename("doc.txt")
            .into_response()
    }
}
