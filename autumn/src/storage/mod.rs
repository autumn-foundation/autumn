//! Pluggable file storage backends for Autumn applications.
//!
//! This module provides a [`BlobStore`] trait abstraction with one
//! built-in backend:
//!
//! - **[`Local`](local::LocalBlobStore)** — writes to a configurable root
//!   directory and serves bytes through an autumn-mounted route at
//!   `[storage.local].mount_path` (default `/_blobs`). URLs are signed
//!   with HMAC-SHA256 and time-bounded.
//!
//! For S3-compatible storage (AWS S3, Cloudflare R2, `MinIO`, `DigitalOcean`
//! Spaces, Wasabi) add the `autumn-storage-s3` crate and call
//! `.with_blob_store(S3BlobStore::from_config(&config.storage.s3).await?)`
//! on your [`AppBuilder`](crate::app::AppBuilder).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use autumn_web::storage::{Blob, BlobStore, BlobStoreError};
//!
//! async fn upload<S: BlobStore + ?Sized>(store: &S, key: &str, bytes: bytes::Bytes)
//!     -> Result<Blob, BlobStoreError>
//! {
//!     store.put(key, "image/png", bytes).await
//! }
//! ```
//!
//! ## Profile-aware defaults
//!
//! | Profile | Default backend | Notes |
//! |---------|-----------------|-------|
//! | `dev`   | `Local` rooted at `target/blobs/` | Always-on with the `storage` feature |
//! | `prod`  | Fail-fast on `local` unless `storage.allow_local_in_production = true` | Force explicit acknowledgement of multi-replica risk |
//!
//! ## Configuration
//!
//! ```toml
//! [storage]
//! backend = "local"        # "local" | "s3" | "disabled"
//! default_provider = "default"
//!
//! [storage.local]
//! root = "target/blobs"
//! mount_path = "/_blobs"
//!
//! [storage.s3]
//! bucket = "my-app-uploads"
//! region = "us-east-1"
//! endpoint = "https://s3.amazonaws.com"
//! access_key_id_env = "AWS_ACCESS_KEY_ID"
//! secret_access_key_env = "AWS_SECRET_ACCESS_KEY"
//! force_path_style = false
//! ```

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use thiserror::Error;

pub mod blob;
pub mod config;
pub mod direct_upload;
pub mod local;
pub mod migrations;

#[cfg(feature = "maud")]
pub mod form_helper;

#[cfg(feature = "variants")]
pub mod variant;

pub use blob::{Blob, BlobMeta};
pub use config::{
    StorageBackend, StorageBackendConfigError, StorageBackendPlan, StorageConfig,
    StorageLocalConfig, StorageS3Config, StorageVariantsConfig,
};
pub use direct_upload::{PresignPutResult, complete_direct_upload};
pub use local::LocalBlobStore;

#[cfg(feature = "variants")]
pub use variant::{Transform, VariantBudget, VariantError, VariantHandle};

/// Boxed future returned by [`BlobStore`] methods.
///
/// Pinning the future as a trait object keeps [`BlobStore`] dyn-safe so
/// applications can hold an `Arc<dyn BlobStore>` and swap backends at
/// runtime.
pub type BlobFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BlobStoreError>> + Send + 'a>>;

/// Stream of byte chunks accepted by [`BlobStore::put_stream`].
///
/// Each item is a `bytes::Bytes` chunk; errors propagate as
/// [`BlobStoreError`]. The lifetime parameter lets callers borrow the
/// chunk source from their own stack (e.g. an in-flight multipart
/// extractor) without forcing a `'static` bound.
pub type ByteStream<'a> = Pin<Box<dyn Stream<Item = Result<Bytes, BlobStoreError>> + Send + 'a>>;

/// Errors returned by [`BlobStore`] operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BlobStoreError {
    /// The requested key was not found.
    #[error("blob not found: {0}")]
    NotFound(String),

    /// Authentication or authorization failed against the backend.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Invalid input — most often a malformed or unsafe key.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Caller exceeded a backend size limit (per-file or per-request).
    /// Maps to HTTP `413 Payload Too Large`.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// I/O failure (filesystem, network, transport).
    #[error("io error: {0}")]
    Io(String),

    /// The configured backend doesn't support this operation.
    #[error("operation not supported: {0}")]
    Unsupported(String),

    /// A signed URL could not be verified or has expired.
    #[error("signature error: {0}")]
    Signature(String),

    /// Backend-specific error reported as a string for portability.
    #[error("backend error: {0}")]
    Backend(String),
}

impl BlobStoreError {
    /// Wrap an `io::Error` for the I/O variant.
    #[must_use]
    pub fn io(err: impl std::fmt::Display) -> Self {
        Self::Io(err.to_string())
    }

    /// Convenience constructor for [`BlobStoreError::Backend`].
    #[must_use]
    pub fn backend(err: impl std::fmt::Display) -> Self {
        Self::Backend(err.to_string())
    }
}

impl BlobStoreError {
    /// HTTP status code that best fits this error variant.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            Self::NotFound(_) => http::StatusCode::NOT_FOUND,
            // A tampered or expired URL is an auth failure: 403, as `local::serve_router` returns.
            Self::PermissionDenied(_) | Self::Signature(_) => http::StatusCode::FORBIDDEN,
            Self::InvalidInput(_) => http::StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge(_) => http::StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unsupported(_) => http::StatusCode::NOT_IMPLEMENTED,
            Self::Io(_) | Self::Backend(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Promote into an [`AutumnError`](crate::AutumnError) carrying the
    /// status from [`BlobStoreError::status`].
    ///
    /// Handlers using `?` get a 500 by default via the blanket
    /// `impl From<E: Error> for AutumnError`; call this instead to
    /// preserve the precise status (404 for missing blobs, 403 for
    /// signature failures, etc.).
    #[must_use]
    pub fn into_autumn_error(self) -> crate::AutumnError {
        let status = self.status();
        crate::AutumnError::internal_server_error(self).with_status(status)
    }
}

/// Pluggable file-storage backend.
///
/// Implement this trait to add new backends. The built-in backend is
/// [`LocalBlobStore`]. S3-compatible storage is provided by the
/// `autumn-storage-s3` crate.
///
/// The trait is **dyn-safe** so apps can hold `Arc<dyn BlobStore>` and
/// swap backends at runtime — for example, choosing local in tests and
/// S3 in production via configuration.
pub trait BlobStore: Send + Sync + 'static {
    /// Stable identifier for the configured provider, recorded on every
    /// [`Blob`] so applications can detect cross-store mismatches.
    fn provider_id(&self) -> &str;

    /// Store `bytes` under `key`, returning a [`Blob`] handle.
    fn put<'a>(&'a self, key: &'a str, content_type: &'a str, bytes: Bytes)
    -> BlobFuture<'a, Blob>;

    /// Store `bytes` under `key` only when `key` is free, in one atomic step.
    ///
    /// Returns `None` and writes nothing when `key` exists. A check and a
    /// write in two steps can replace a blob of another writer, so the
    /// default returns [`BlobStoreError::Unsupported`]. [`LocalBlobStore`]
    /// and the S3 backend override it with a conditional create.
    fn put_if_absent<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        bytes: Bytes,
    ) -> BlobFuture<'a, Option<Blob>> {
        let _ = (key, content_type, bytes);
        Box::pin(async {
            Err(BlobStoreError::Unsupported(
                "this storage backend has no conditional create (put_if_absent)".into(),
            ))
        })
    }

    /// Stream `data` under `key`, returning a [`Blob`] handle.
    ///
    /// Use this for files larger than memory.
    fn put_stream<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        data: ByteStream<'a>,
    ) -> BlobFuture<'a, Blob>;

    /// Read the bytes for `key` into memory.
    fn get<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Bytes>;

    /// Stream an object's bytes without buffering the whole object in memory.
    ///
    /// Use this for serving large objects (see
    /// [`Download::from_blob`](crate::download::Download::from_blob)) so a
    /// 100 MB file does not materialize entirely in RAM.
    ///
    /// The default implementation buffers via [`get`](BlobStore::get) and
    /// yields a single chunk; backends that can truly stream (like the
    /// [`LocalBlobStore`]) override this to read the object incrementally.
    /// The returned stream is `'static` so callers can detach it from the
    /// store's borrow and hand it to a response body.
    ///
    /// The S3 backend (`autumn-storage-s3`, a separate crate) uses the
    /// buffering default; overriding it there is tracked separately.
    fn get_stream<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ByteStream<'static>> {
        Box::pin(async move {
            let bytes = self.get(key).await?;
            let stream: ByteStream<'static> =
                Box::pin(futures::stream::once(async move { Ok(bytes) }));
            Ok(stream)
        })
    }

    /// Stream only the inclusive byte range `[start, end]` of an object.
    ///
    /// Powers HTTP `Range` requests (see
    /// [`Download::into_response_ranged`](crate::download::Download::into_response_ranged)):
    /// a client seeking within a large video asks for a slice, and the store
    /// returns just those bytes.
    ///
    /// `start` and `end` are inclusive and are assumed to have been validated
    /// against the object's size by the caller (via
    /// [`autumn_web::range`](crate::range)).
    ///
    /// The default implementation buffers the whole object via
    /// [`get`](BlobStore::get) and slices it — used by backends that cannot do
    /// a true ranged read (e.g. the S3 backend until it is overridden there).
    /// The [`LocalBlobStore`] overrides this to `seek` + `take` so a slice of a
    /// large file never materializes the whole object in memory.
    fn get_range<'a>(
        &'a self,
        key: &'a str,
        start: u64,
        end: u64,
    ) -> BlobFuture<'a, ByteStream<'static>> {
        Box::pin(async move {
            let bytes = self.get(key).await?;
            let lo = usize::try_from(start)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let hi = usize::try_from(end)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
                .min(bytes.len());
            let slice = bytes.slice(lo..hi.max(lo));
            let stream: ByteStream<'static> =
                Box::pin(futures::stream::once(async move { Ok(slice) }));
            Ok(stream)
        })
    }

    /// Delete the blob at `key`. No-op when the key does not exist.
    fn delete<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ()>;

    /// Return metadata for `key` if it exists.
    fn head<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Option<BlobMeta>>;

    /// Build a time-bounded URL that serves the blob's bytes.
    ///
    /// On the [`LocalBlobStore`] this is an HMAC-signed link to the
    /// mounted serving route. On S3 backends it is a real S3 presigned
    /// URL.
    fn presigned_url<'a>(&'a self, key: &'a str, expires_in: Duration) -> BlobFuture<'a, String>;

    /// Build a time-bounded presigned envelope the browser can use to PUT
    /// bytes directly to the storage backend, bypassing the Autumn app
    /// process.
    ///
    /// Returns a [`PresignPutResult`] with the URL, HTTP method, and any
    /// headers the browser must include in the upload request.
    ///
    /// Backends that do not support direct PUT uploads return
    /// [`BlobStoreError::Unsupported`]. Callers that want a graceful fallback
    /// should check for that variant and fall back to the through-app upload
    /// path via [`BlobStore::put_stream`].
    ///
    /// A signed-URL leak does **not** allow the holder to bind the blob to
    /// any model: the completion step (recording the [`Blob`] in the
    /// database) is always the application's own CSRF- and session-protected
    /// route, not a framework-issued token.
    fn presign_put<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        expires_in: Duration,
    ) -> BlobFuture<'a, PresignPutResult> {
        let _ = (key, content_type, expires_in);
        Box::pin(async {
            Err(BlobStoreError::Unsupported(
                "this storage backend does not support direct browser uploads via \
                 presigned PUT; use the through-app upload path \
                 (MultipartField::save_to_blob_store) instead"
                    .into(),
            ))
        })
    }
}

/// Type alias for a runtime-installed shared [`BlobStore`].
///
/// Applications obtain this from [`AppState`](crate::AppState) via
/// [`BlobStoreState::store`].
pub type SharedBlobStore = Arc<dyn BlobStore>;

/// Wrapper installed on [`AppState`](crate::AppState) so handlers can
/// pull the configured store back out via
/// [`AppState::extension::<BlobStoreState>()`](crate::AppState::extension).
#[derive(Clone)]
pub struct BlobStoreState {
    inner: SharedBlobStore,
}

impl BlobStoreState {
    /// Wrap a runtime-built blob store.
    #[must_use]
    pub fn new(store: SharedBlobStore) -> Self {
        Self { inner: store }
    }

    /// Borrow the underlying [`BlobStore`] handle.
    #[must_use]
    pub fn store(&self) -> &SharedBlobStore {
        &self.inner
    }
}

/// Validate a user-supplied object key.
///
/// Keys must be non-empty, must not contain `..` segments, must not be
/// absolute paths, and must not contain NUL bytes. This protects the
/// [`LocalBlobStore`] from path-traversal and gives S3 backends a
/// consistent input contract.
///
/// # Errors
///
/// Returns [`BlobStoreError::InvalidInput`] when the key is rejected.
pub fn validate_key(key: &str) -> Result<(), BlobStoreError> {
    check_basic_formatting(key)?;
    check_windows_paths(key)?;
    for segment in key.split('/') {
        validate_segment(segment)?;
    }
    check_reserved_suffixes(key)?;
    check_case_folding(key)?;
    Ok(())
}

fn check_basic_formatting(key: &str) -> Result<(), BlobStoreError> {
    if key.is_empty() {
        return Err(BlobStoreError::InvalidInput("blob key is empty".into()));
    }
    if key.contains('\0') {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains NUL byte".into(),
        ));
    }
    if key.starts_with('/') || key.starts_with('\\') {
        return Err(BlobStoreError::InvalidInput(
            "blob key must be relative".into(),
        ));
    }
    // Windows treats `a\b` as `a/b`, so two keys would name one file.
    if key.contains('\\') {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains a backslash; use `/` as the segment separator".into(),
        ));
    }
    Ok(())
}

fn check_windows_paths(key: &str) -> Result<(), BlobStoreError> {
    // `Path::join` on Windows treats drive-letter and UNC forms (`C:\…`, `C:/…`, `\\?\…`,
    // `\\server\share\…`) as absolute, escaping the root. Rejected on every host so keys stay
    // portable.
    let bytes = key.as_bytes();
    let drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if drive_letter {
        return Err(BlobStoreError::InvalidInput(
            "blob key looks like a Windows drive-letter path".into(),
        ));
    }
    if key.starts_with("\\\\") || key.starts_with("//") {
        return Err(BlobStoreError::InvalidInput(
            "blob key looks like a UNC / network path".into(),
        ));
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<(), BlobStoreError> {
    if segment == ".." {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains traversal segment".into(),
        ));
    }
    // `a/./b` and `a//b` resolve to `a/b` on disk, and HTTP clients normalize them away, so
    // the signed key would no longer match the requested path.
    if segment == "." {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains a `.` segment".into(),
        ));
    }
    if segment.is_empty() {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains an empty segment".into(),
        ));
    }
    // Windows strips a trailing `.` or space (`foo.png.` → `foo.png`), which would alias two
    // keys and let `con ` slip past the reserved-name check below.
    if segment.ends_with('.') || segment.ends_with(' ') {
        return Err(BlobStoreError::InvalidInput(format!(
            "blob key segment {segment:?} ends with `.` or space; Windows normalizes \
             these and would alias the segment with its stripped form"
        )));
    }
    // Windows rejects these characters and control bytes in filenames.
    if segment.bytes().any(|b| {
        matches!(
            b,
            b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*' | 0x01..=0x1F
        )
    }) {
        return Err(BlobStoreError::InvalidInput(
            "blob key contains a Windows-reserved filename character (`<`, `>`, \
             `:`, `\"`, `|`, `?`, `*`, or a control byte) — keys must be portable \
             across local and S3 backends"
                .into(),
        ));
    }
    // Device names (`con/foo`, `nul.png`) fail on Windows. The name is the part
    // before the first `.`. `check_case_folding` rejects uppercase, so the lowercase set
    // suffices.
    let basename = segment.split('.').next().unwrap_or("");
    if WINDOWS_RESERVED_NAMES.contains(&basename) {
        return Err(BlobStoreError::InvalidInput(format!(
            "blob key segment {segment:?} starts with a Windows-reserved device name \
             (`con`, `prn`, `aux`, `nul`, `com1-9`, `lpt1-9`)"
        )));
    }
    Ok(())
}

fn check_reserved_suffixes(key: &str) -> Result<(), BlobStoreError> {
    // The local backend stores key `foo`'s metadata at `foo.meta`, so a key `foo.meta` would
    // collide with it. Reserved on every backend, case-insensitively, so keys stay portable.
    if let Some(last) = key.rsplit('/').next() {
        // Compare bytes: slicing a `&str` 5 bytes from the end can split a multi-byte char
        // and panic.
        let bytes = last.as_bytes();
        if bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".meta") {
            return Err(BlobStoreError::InvalidInput(
                "blob keys ending in `.meta` are reserved (local backend uses `<key>.meta` \
                 sidecar files for content-type metadata)"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn check_case_folding(key: &str) -> Result<(), BlobStoreError> {
    // Case-insensitive filesystems (NTFS, APFS by default) map `A.png` and `a.png` to one
    // file, while S3 keeps them apart. Reject any char that lowercases to something else,
    // ASCII or Unicode (`Ä`, `İ`). Apps that need case must encode it first.
    for c in key.chars() {
        let mut lower = c.to_lowercase();
        // Lowercase or caseless iff `to_lowercase` yields exactly `c`.
        let first = lower.next();
        let trailing = lower.next();
        if first != Some(c) || trailing.is_some() {
            return Err(BlobStoreError::InvalidInput(
                "blob keys must be lowercase (uppercase Unicode aliases on case-insensitive \
                 filesystems and breaks portability between local and S3)"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Windows reserves these device names regardless of file extension
/// (`con.txt`, `con/foo`, etc.). Lowercase-only because the uppercase
/// check in `validate_key` already enforces all-lowercase keys.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_accepts_typical_paths() {
        validate_key("avatars/123.png").unwrap();
        validate_key("a/b/c/d.txt").unwrap();
    }

    #[test]
    fn validate_key_rejects_traversal() {
        let err = validate_key("../etc/passwd").unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn validate_key_rejects_absolute() {
        let err = validate_key("/etc/passwd").unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn validate_key_rejects_empty() {
        let err = validate_key("").unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn validate_key_rejects_nul() {
        let err = validate_key("a\0b").unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn validate_key_rejects_windows_drive_letter() {
        for k in [r"C:\tmp\x", "C:/tmp/x", "z:\\foo", "a:bar"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_rejects_unc_paths() {
        for k in [r"\\server\share\file", "//server/share/file"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_rejects_dot_segments() {
        for k in ["a/./b", "./foo", "a/././b", "a/.\\b"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_rejects_empty_segments() {
        for k in ["a//b", "a/b/", "a///b"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_rejects_backslash_separator() {
        for k in [r"a\b", r"avatars\me.png", r"x\y\z"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_reserves_meta_suffix() {
        for k in ["foo.meta", "avatars/me.meta", "FOO.META", "x/y/Z.MeTa"] {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be reserved",
            );
        }
        for k in ["meta.png", "foo.metadata", "a.meta.gz", "metafile"] {
            validate_key(k).unwrap_or_else(|_| panic!("key {k:?} should be accepted"));
        }
    }

    #[test]
    fn validate_key_handles_non_ascii_without_panicking() {
        // For most of these, a `&str` slice 5 bytes from the end would split a char and panic.
        for k in ["ééé", "résumé.png", "東京", "cafe\u{0301}"] {
            validate_key(k).unwrap_or_else(|err| {
                panic!("non-ASCII key {k:?} should validate cleanly, got {err:?}")
            });
        }
        let err = validate_key("résumé.meta").unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn validate_key_rejects_uppercase() {
        let rejected = [
            // ASCII uppercase
            "Foo.png",
            "AVATARS/me.png",
            "aBc",
            "x/Y/z",
            // Unicode uppercase variants
            "Ärger.png",
            "documents/Émile.txt",
            "İstanbul/photo.jpg",
            "ΟΛΑ.txt", // Greek Omicron-Lambda-Alpha
        ];
        for k in rejected {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected for uppercase"
            );
        }
        let accepted = [
            "foo.png",
            "avatars/me.png",
            "résumé.png",
            "ärger.png",
            "émile.txt",
            "istanbul/photo.jpg",
            "東京/photo.jpg", // CJK ideographs are caseless
            "café/menu.txt",
        ];
        for k in accepted {
            validate_key(k)
                .unwrap_or_else(|err| panic!("key {k:?} should be accepted, got {err:?}"));
        }
    }

    #[test]
    fn validate_key_rejects_windows_reserved_chars() {
        let rejected = [
            "foo<bar",
            "foo>bar",
            "foo:bar",
            "foo\"bar",
            "foo|bar",
            "foo?bar",
            "foo*bar",
            "foo\x01bar",
            "foo\x1fbar",
        ];
        for k in rejected {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_key_rejects_windows_reserved_names() {
        let rejected = [
            "con",
            "con.png",
            "con/foo.png",
            "x/nul",
            "x/nul.txt",
            "aux.bin",
            "prn",
            "com1.log",
            "com9",
            "lpt1",
            "lpt9.txt",
        ];
        for k in rejected {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
        // Only the part before the first dot must not equal a reserved name.
        let accepted = [
            "console.png",
            "lptastic.txt",
            "x/auxiliary.bin",
            "con-tinuation.png",
            "com10.log", // reserved set is com1-9 only
        ];
        for k in accepted {
            validate_key(k)
                .unwrap_or_else(|err| panic!("key {k:?} should be accepted, got {err:?}"));
        }
    }

    #[test]
    fn validate_key_rejects_trailing_dot_or_space_segments() {
        let rejected = [
            "foo.",            // trailing dot
            "avatars/me.png.", // trailing dot on last segment
            "x./y",            // trailing dot mid-path
            "foo ",            // trailing space
            "x /y",            // trailing space mid-path
            "con ",            // would alias `con` after Windows normalization
            "con.",            // same
        ];
        for k in rejected {
            let err = validate_key(k).unwrap_err();
            assert!(
                matches!(err, BlobStoreError::InvalidInput(_)),
                "key {k:?} should be rejected"
            );
        }
        let accepted = ["foo.bar", "a b", " foo", "x/y/.hidden"];
        for k in accepted {
            validate_key(k)
                .unwrap_or_else(|err| panic!("key {k:?} should be accepted, got {err:?}"));
        }
    }

    #[test]
    fn error_status_mapping() {
        assert_eq!(
            BlobStoreError::NotFound("x".into()).status(),
            http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            BlobStoreError::PermissionDenied("x".into()).status(),
            http::StatusCode::FORBIDDEN
        );
        assert_eq!(
            BlobStoreError::InvalidInput("x".into()).status(),
            http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            BlobStoreError::Signature("x".into()).status(),
            http::StatusCode::FORBIDDEN
        );
        assert_eq!(
            BlobStoreError::PayloadTooLarge("x".into()).status(),
            http::StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            BlobStoreError::Unsupported("x".into()).status(),
            http::StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            BlobStoreError::Backend("x".into()).status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn error_into_autumn_error_preserves_status() {
        let err = BlobStoreError::NotFound("k".into()).into_autumn_error();
        assert_eq!(err.status(), http::StatusCode::NOT_FOUND);
    }

    // ── presign_put default returns Unsupported ─────────────────────────────

    struct NoOpStore;
    impl BlobStore for NoOpStore {
        fn provider_id(&self) -> &'static str {
            "noop"
        }
        fn put<'a>(&'a self, _: &'a str, _: &'a str, _: bytes::Bytes) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn put_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: ByteStream<'a>,
        ) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn get<'a>(&'a self, _: &'a str) -> BlobFuture<'a, bytes::Bytes> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn delete<'a>(&'a self, _: &'a str) -> BlobFuture<'a, ()> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn head<'a>(&'a self, _: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn presigned_url<'a>(&'a self, _: &'a str, _: Duration) -> BlobFuture<'a, String> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
    }

    /// A store whose `put` must not run.
    struct NoConditionalStore;
    impl BlobStore for NoConditionalStore {
        fn provider_id(&self) -> &'static str {
            "plain"
        }
        fn put<'a>(&'a self, _: &'a str, _: &'a str, _: bytes::Bytes) -> BlobFuture<'a, Blob> {
            panic!("put_if_absent must not fall back to an overwriting put")
        }
        fn put_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: ByteStream<'a>,
        ) -> BlobFuture<'a, Blob> {
            panic!("not used")
        }
        fn get<'a>(&'a self, key: &'a str) -> BlobFuture<'a, bytes::Bytes> {
            Box::pin(async move { Err(BlobStoreError::NotFound(key.to_owned())) })
        }
        fn delete<'a>(&'a self, _: &'a str) -> BlobFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn head<'a>(&'a self, _: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
            Box::pin(async { Ok(None) })
        }
        fn presigned_url<'a>(&'a self, _: &'a str, _: Duration) -> BlobFuture<'a, String> {
            Box::pin(async { Err(BlobStoreError::Unsupported("plain".into())) })
        }
    }

    #[tokio::test]
    async fn put_if_absent_default_refuses_instead_of_overwriting() {
        // A separate check and write could replace another writer's blob.
        let err = NoConditionalStore
            .put_if_absent("a/b.png", "image/png", bytes::Bytes::from_static(b"x"))
            .await
            .expect_err("no conditional create");
        assert!(matches!(err, BlobStoreError::Unsupported(_)), "{err:?}");
    }

    #[tokio::test]
    async fn presign_put_default_returns_unsupported() {
        let store = NoOpStore;
        let err = store
            .presign_put("avatars/me.png", "image/png", Duration::from_secs(300))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BlobStoreError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
        assert_eq!(err.status(), http::StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn presign_put_result_fields_accessible() {
        let r = PresignPutResult {
            url: "https://example.com/upload".into(),
            method: "PUT".into(),
            headers: std::collections::HashMap::from([("Content-Type".into(), "image/png".into())]),
            expires_in: Duration::from_secs(300),
        };
        assert_eq!(r.url, "https://example.com/upload");
        assert_eq!(r.method, "PUT");
        assert_eq!(
            r.headers.get("Content-Type").map(String::as_str),
            Some("image/png")
        );
        assert_eq!(r.expires_in, Duration::from_secs(300));
    }
}
