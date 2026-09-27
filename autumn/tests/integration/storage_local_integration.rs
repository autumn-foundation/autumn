//! Framework-level integration tests for the `Local` blob store.
//!
//! `survives_process_restart` is the headline test the storage spec
//! ([issue #494](https://github.com/autumn-foundation/autumn/issues/494)) cares
//! about: write a blob through one [`LocalBlobStore`], drop it, point a
//! fresh store at the same root + same signing key, and confirm the
//! bytes are still there. This proves the on-disk format outlives a
//! process restart on the dev / single-replica path.
//!
//! These tests deliberately don't depend on a database — they exercise
//! the framework primitive in isolation. The `examples/reddit-clone`
//! crate carries the database-and-UI demo for a `Blob` column on a
//! `#[model]`.

#![cfg(feature = "storage")]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::storage::{
    BlobStore, BlobStoreState, LocalBlobStore, SharedBlobStore, local::SigningKey,
};
use bytes::Bytes;

#[tokio::test]
async fn survives_process_restart() {
    let dir = tempfile::tempdir().unwrap();
    let key = SigningKey::new(b"persistent-key".to_vec());

    // First "process": upload through the local store.
    {
        let store = LocalBlobStore::new(
            "default",
            dir.path().to_path_buf(),
            "/_blobs",
            Duration::from_secs(300),
            key.clone(),
            vec![],
        )
        .unwrap();
        let blob = store
            .put(
                "avatars/me.png",
                "image/png",
                Bytes::from_static(b"\x89PNG\r\n\x1a\nfake"),
            )
            .await
            .unwrap();
        assert_eq!(blob.byte_size, 12);
        assert_eq!(blob.provider_id, "default");
    }

    // Second "process": fresh store, same root, same key. Bytes still there.
    {
        let store = LocalBlobStore::new(
            "default",
            dir.path().to_path_buf(),
            "/_blobs",
            Duration::from_secs(300),
            key,
            vec![],
        )
        .unwrap();
        let bytes = store.get("avatars/me.png").await.unwrap();
        assert_eq!(&bytes[..], b"\x89PNG\r\n\x1a\nfake");
    }
}

#[tokio::test]
async fn presigned_url_round_trip_via_serving_route() {
    use autumn_web::reexports::axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    let dir = tempfile::tempdir().unwrap();
    let key = SigningKey::new(b"serving-key".to_vec());
    let store = LocalBlobStore::new(
        "default",
        dir.path().to_path_buf(),
        "/_blobs",
        Duration::from_secs(60),
        key,
        vec![],
    )
    .unwrap();
    let blob = store
        .put("hello.txt", "text/plain", Bytes::from_static(b"hello"))
        .await
        .unwrap();

    let url = store
        .presigned_url(&blob.key, Duration::from_secs(120))
        .await
        .unwrap();

    let arc: SharedBlobStore = Arc::new(store.clone());
    let state = autumn_web::AppState::for_test().with_extension(BlobStoreState::new(arc));
    let router = autumn_web::storage::local::serve_router(&store).with_state(state);

    let request = Request::builder().uri(&url).body(Body::empty()).unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The serving route reads the persisted sidecar metadata so the
    // response carries the original content_type, not the
    // `application/octet-stream` default of `Bytes::into_response`.
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"hello");
}

#[tokio::test]
async fn tampered_signature_is_rejected() {
    use autumn_web::reexports::axum::body::Body;
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::new(
        "default",
        dir.path().to_path_buf(),
        "/_blobs",
        Duration::from_secs(60),
        SigningKey::new(b"the-key".to_vec()),
        vec![],
    )
    .unwrap();
    store
        .put("a.txt", "text/plain", Bytes::from_static(b"a"))
        .await
        .unwrap();

    let url = store
        .presigned_url("a.txt", Duration::from_secs(120))
        .await
        .unwrap();
    // Flip a hex digit in the signature so verification fails.
    let len = url.len();
    let tampered = if url.ends_with('0') {
        format!("{}1", &url[..len - 1])
    } else {
        format!("{}0", &url[..len - 1])
    };

    let arc: SharedBlobStore = Arc::new(store.clone());
    let state = autumn_web::AppState::for_test().with_extension(BlobStoreState::new(arc));
    let router = autumn_web::storage::local::serve_router(&store).with_state(state);

    let request = Request::builder()
        .uri(&tampered)
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ── sign_upload_legacy signature malleability ──────────────────────
//
// `sign_upload` was fixed (docs/plans/2026-06-05-feedback-bugfixes.md,
// "Length-Delimit Upload Signature Fields") to length-prefix `blob_key` and
// `content_type` specifically because concatenating them with a bare `:`
// delimiter lets two different (key, content_type) pairs hash identically
// when one field contains a `:`. `sign_upload_legacy` is the pre-fix
// algorithm, kept — and still checked by `verify_upload_rotation_with_now`
// against the *current* signing key, not just keys already known to be
// retired — purely so upload tokens minted before that fix don't break
// mid-flight. It reintroduces the exact ambiguity the fix eliminated.
//
// See docs/security/2026-09-12-legacy-upload-signature-malleability/ for the
// full writeup of why this is a negative result rather than a fix PR:
// `storage::validate_key` happens to reject every key this ambiguity can
// produce (it forbids `:` for unrelated, Windows-portability reasons), so no
// working exploit exists against either shipped `BlobStore` backend today.
// Both tests below are load-bearing tripwires, not just documentation.

#[test]
fn legacy_upload_signature_collides_across_the_key_content_type_boundary() {
    use autumn_web::storage::local::{sign_upload, sign_upload_legacy};

    let key = b"shared-signing-key";
    let exp_at: u64 = 4_102_444_800; // 2100-01-01 — far enough out to never expire in CI

    // Two different (blob_key, content_type) pairs that concatenate to the
    // identical byte string under the legacy "key:content_type:exp" scheme.
    let legacy_a = sign_upload_legacy(key, "reports/mine.txt", "text/plain:evil", exp_at);
    let legacy_b = sign_upload_legacy(key, "reports/mine.txt:text/plain", "evil", exp_at);
    assert_eq!(
        legacy_a, legacy_b,
        "sign_upload_legacy must not collide across a re-sliced key/content-type \
         boundary — if this starts failing, the legacy algorithm has been fixed \
         and the compensating-control test below can be revisited"
    );

    // The current, length-prefixed scheme does not have this ambiguity.
    let fixed_a = sign_upload(key, "reports/mine.txt", "text/plain:evil", exp_at);
    let fixed_b = sign_upload(key, "reports/mine.txt:text/plain", "evil", exp_at);
    assert_ne!(
        fixed_a, fixed_b,
        "sign_upload (length-prefixed) must not reproduce the legacy collision"
    );
}

#[tokio::test]
async fn legacy_signature_replay_cannot_retarget_an_upload() {
    use autumn_web::reexports::axum::body::Body;
    use autumn_web::storage::local::sign_upload_legacy;
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let dir = tempfile::tempdir().unwrap();
    let signing_key = SigningKey::new(b"the-key".to_vec());
    let store = LocalBlobStore::new(
        "default",
        dir.path().to_path_buf(),
        "/_blobs",
        Duration::from_secs(300),
        signing_key.clone(),
        vec![],
    )
    .unwrap();

    let exp_at = (std::time::SystemTime::now() + Duration::from_secs(300))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Stand-in for a legacy-format token minted for the attacker's own
    // object ("reports/mine.txt", content_type "text/plain:evil" — an
    // unusual but unvalidated content-type string, since `presign_put`
    // never restricts its format) that is still unexpired.
    let sig = sign_upload_legacy(
        signing_key.as_bytes(),
        "reports/mine.txt",
        "text/plain:evil",
        exp_at,
    );

    let arc: SharedBlobStore = Arc::new(store.clone());
    let state = autumn_web::AppState::for_test().with_extension(BlobStoreState::new(arc));
    let router = autumn_web::storage::local::serve_router(&store).with_state(state);

    // Re-sliced replay: same signature, but the request now claims key
    // "reports/mine.txt:text/plain" with content_type "evil" — a different
    // pair that hashes identically under the legacy scheme (proven above).
    let uri =
        format!("/_blobs/reports/mine.txt:text/plain?upload=1&ct=evil&exp={exp_at}&sig={sig}");
    let request = Request::builder()
        .method("PUT")
        .uri(&uri)
        .body(Body::from("attacker-controlled-bytes"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    // The signature check alone accepts this re-sliced pair — that's the
    // malleability bug. `validate_key` rejecting the embedded `:` is the
    // only thing stopping the write; pin that it still does.
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "expected validate_key to reject the re-sliced key even though the \
         legacy signature check accepted it — if this starts returning 200, \
         the colon-rejection compensating control has been relaxed and the \
         legacy fallback is now a live cross-object-write bypass"
    );

    // No blob exists under either interpretation of the ambiguous pair.
    assert!(store.get("reports/mine.txt:text/plain").await.is_err());
    assert!(store.get("reports/mine.txt").await.is_err());
}
