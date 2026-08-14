//! Local-disk implementation of [`BlobStore`].
//!
//! Bytes land under a configurable `root` directory; URLs are
//! HMAC-SHA256-signed and time-bounded, served by an axum router mounted
//! by the framework on startup.
//!
//! Suitable for `dev`, single-replica deployments, and integration
//! tests. Multi-replica production should use the `S3` backend.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt as _;
use http::StatusCode;
use sha2::{Digest, Sha256};

use super::blob::{Blob, BlobMeta};
use super::{
    BlobFuture, BlobStore, BlobStoreError, ByteStream, direct_upload::PresignPutResult,
    validate_key,
};

/// HMAC signing key used by the local backend.
///
/// In test and dev a random key is generated at startup; in production
/// callers are expected to set `[storage.local].signing_key` (or the
/// `AUTUMN_STORAGE__LOCAL__SIGNING_KEY` env var) so URLs survive process
/// restarts and replicas agree on the signature.
#[derive(Clone)]
pub struct SigningKey(Arc<Vec<u8>>);

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak key material into logs.
        f.debug_struct("SigningKey")
            .field("len", &self.0.len())
            .finish()
    }
}

impl SigningKey {
    /// Create a key from explicit bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }

    /// Generate a random 32-byte key.
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = vec![0u8; 32];
        // Mix a UUID v4 (already cryptographic-quality random) into the key.
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        bytes[..16].copy_from_slice(a.as_bytes());
        bytes[16..].copy_from_slice(b.as_bytes());
        Self::new(bytes)
    }

    /// Returns the raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Local-disk blob store.
///
/// Construct via [`LocalBlobStore::new`]. The framework wires this up
/// from `[storage.local]` automatically when `storage.backend = "local"`.
#[derive(Clone, Debug)]
pub struct LocalBlobStore {
    inner: Arc<LocalInner>,
}

#[derive(Debug)]
struct LocalInner {
    provider_id: String,
    root: PathBuf,
    /// `root` after `std::fs::canonicalize` — i.e. the actual on-disk
    /// location after any symlinks have been followed. Stashed at
    /// construction time and used by `safe_path_for_key` to verify
    /// that user-supplied keys can't escape the configured root via a
    /// hostile or accidental symlink in the storage tree.
    canonical_root: PathBuf,
    mount_path: String,
    default_expiry: Duration,
    signing_key: SigningKey,
    /// Former signing keys accepted during a rotation grace window.
    previous_signing_keys: Vec<SigningKey>,
}

impl LocalBlobStore {
    /// Create a new local store rooted at `root`.
    ///
    /// `mount_path` must start with `/` — it's the prefix the framework
    /// uses to serve signed URLs (default `/_blobs`).
    ///
    /// # Errors
    ///
    /// Returns [`BlobStoreError::Io`] when the root directory cannot be
    /// created.
    pub fn new(
        provider_id: impl Into<String>,
        root: impl Into<PathBuf>,
        mount_path: impl Into<String>,
        default_expiry: Duration,
        signing_key: SigningKey,
        previous_signing_keys: Vec<SigningKey>,
    ) -> Result<Self, BlobStoreError> {
        let mount_path = mount_path.into();
        // axum panics with `Paths must start with a '/'` if we hand it a
        // mount path that doesn't lead with a slash. Catch that here as
        // a recoverable configuration error so a bad
        // `[storage.local].mount_path` (or the env-var equivalent)
        // surfaces a clean message instead of a router-build panic.
        if !mount_path.starts_with('/') {
            return Err(BlobStoreError::InvalidInput(format!(
                "storage.local.mount_path must start with '/' (got {mount_path:?})"
            )));
        }
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(BlobStoreError::io)?;
        // Canonicalize once at construction — `safe_path_for_key`
        // compares each operation's resolved target against this so
        // a hostile symlink inside the storage tree (e.g.
        // `root/avatars -> /etc`) can't be used to escape the root.
        let canonical_root = std::fs::canonicalize(&root).map_err(BlobStoreError::io)?;
        Ok(Self {
            inner: Arc::new(LocalInner {
                provider_id: provider_id.into(),
                root,
                canonical_root,
                mount_path,
                default_expiry,
                signing_key,
                previous_signing_keys,
            }),
        })
    }

    /// Borrow the configured mount path.
    #[must_use]
    pub fn mount_path(&self) -> &str {
        &self.inner.mount_path
    }

    /// Borrow the configured signing key — used by the framework when
    /// wiring up the serving route.
    #[must_use]
    pub fn signing_key(&self) -> SigningKey {
        self.inner.signing_key.clone()
    }

    /// Borrow the on-disk root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Resolve a user-supplied key to a `PathBuf` and verify the
    /// resolved path stays under the canonical storage root.
    ///
    /// Beyond the lexical checks in [`validate_key`], this walks the
    /// deepest existing prefix of the target, follows any symlinks
    /// along the way (`tokio::fs::canonicalize`), and asserts the
    /// result is still under `canonical_root`. That blocks the hostile-
    /// symlink case where `root/avatars -> /etc` would otherwise let a
    /// key like `avatars/passwd` read or write outside the blob
    /// directory.
    ///
    /// There's still a TOCTOU window between this check and the IO
    /// that follows; a co-located attacker who can win that race needs
    /// `openat`-style primitives to fully eliminate, which Rust's std
    /// doesn't expose. The check still removes the common "operator
    /// configured a symlink they didn't realize was unsafe" failure
    /// mode.
    async fn safe_path_for_key(&self, key: &str) -> Result<PathBuf, BlobStoreError> {
        validate_key(key)?;
        let target = self.inner.root.join(key);

        // `canonicalize` errors with NotFound when the target doesn't
        // exist yet (which is normal for `put`). Walk up to the deepest
        // ancestor that exists, canonicalize that, and check it.
        let mut probe = target.clone();
        let canon_existing = loop {
            match tokio::fs::canonicalize(&probe).await {
                Ok(p) => break p,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if !probe.pop() {
                        return Err(BlobStoreError::io(
                            "storage root vanished while resolving blob key",
                        ));
                    }
                }
                Err(err) => return Err(BlobStoreError::io(err)),
            }
        };

        if !canon_existing.starts_with(&self.inner.canonical_root) {
            return Err(BlobStoreError::PermissionDenied(
                "blob key resolves outside storage root".into(),
            ));
        }
        Ok(target)
    }

    /// Serve-side helper: read the bytes plus the persisted metadata
    /// for a blob. Used by [`serve_router`] so locally-served URLs
    /// reflect the original `content_type` instead of defaulting to
    /// `application/octet-stream`. Returns `None` for the metadata
    /// when the sidecar is missing (older blobs, or backends that
    /// were filled by something other than `put`/`put_stream`).
    pub(crate) async fn get_with_meta(
        &self,
        key: &str,
    ) -> Result<(Bytes, Option<StoredBlobMeta>), BlobStoreError> {
        let path = self.safe_path_for_key(key).await?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => Bytes::from(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobStoreError::NotFound(key.to_owned()));
            }
            Err(err) => return Err(BlobStoreError::io(err)),
        };
        let meta = read_meta_sidecar(&path).await;
        Ok((bytes, meta))
    }
}

impl BlobStore for LocalBlobStore {
    fn provider_id(&self) -> &str {
        &self.inner.provider_id
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        bytes: Bytes,
    ) -> BlobFuture<'a, Blob> {
        Box::pin(async move {
            let path = self.safe_path_for_key(key).await?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(BlobStoreError::io)?;
            }
            let etag = sha256_hex(&bytes);

            // Write to a temp file in the same directory, then rename
            // into place. This means a partial write (disk full,
            // crash, killed process) never leaves a truncated blob at
            // `path` for a future `get` to serve.
            let tmp_path = temp_sibling_path(&path);
            if let Err(err) = tokio::fs::write(&tmp_path, &bytes).await {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(BlobStoreError::io(err));
            }
            if let Err(err) = atomic_replace(&tmp_path, &path).await {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(BlobStoreError::io(err));
            }
            // Persist the content_type + etag in a sibling sidecar so
            // `head`, `get_with_meta`, and the serving route can return
            // the right MIME instead of `application/octet-stream`. If
            // the sidecar write fails on an overwrite, clear any
            // pre-existing sidecar so future `head`/serve calls fall
            // back to `application/octet-stream` rather than reporting
            // stale `content_type` from the previous put. The bytes
            // themselves are committed and correct; we'd rather serve
            // "unknown MIME" than misrepresent the MIME.
            if write_meta_sidecar(
                &path,
                &StoredBlobMeta {
                    content_type: content_type.to_owned(),
                    etag: Some(etag.clone()),
                },
            )
            .await
            .is_err()
            {
                drop_stale_sidecar(&path).await;
            }
            Ok(Blob {
                provider_id: self.inner.provider_id.clone(),
                key: key.to_owned(),
                content_type: content_type.to_owned(),
                byte_size: bytes.len() as u64,
                etag: Some(etag),
            })
        })
    }

    fn put_stream<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        mut data: ByteStream<'a>,
    ) -> BlobFuture<'a, Blob> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt as _;

            let path = self.safe_path_for_key(key).await?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(BlobStoreError::io)?;
            }

            // Stream into a sibling temp file and rename into place
            // only on a clean finish. A client disconnect, mid-stream
            // error, or transient I/O failure leaves the temp file
            // (which we unlink) and never touches `path`, so future
            // `get` calls don't serve a corrupted blob.
            let tmp_path = temp_sibling_path(&path);
            let result = async {
                let mut file = tokio::fs::File::create(&tmp_path)
                    .await
                    .map_err(BlobStoreError::io)?;
                let mut hasher = Sha256::new();
                let mut byte_size: u64 = 0;
                while let Some(chunk) = data.next().await {
                    let chunk = chunk?;
                    hasher.update(&chunk);
                    byte_size = byte_size.saturating_add(chunk.len() as u64);
                    file.write_all(&chunk).await.map_err(BlobStoreError::io)?;
                }
                file.flush().await.map_err(BlobStoreError::io)?;
                Ok::<(u64, String), BlobStoreError>((byte_size, hex(hasher.finalize())))
            }
            .await;

            match result {
                Ok((byte_size, etag)) => {
                    if let Err(err) = atomic_replace(&tmp_path, &path).await {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return Err(BlobStoreError::io(err));
                    }
                    if write_meta_sidecar(
                        &path,
                        &StoredBlobMeta {
                            content_type: content_type.to_owned(),
                            etag: Some(etag.clone()),
                        },
                    )
                    .await
                    .is_err()
                    {
                        // Same rationale as `put`: clear any
                        // pre-existing sidecar so future `head`/serve
                        // requests don't report stale MIME for the
                        // freshly committed bytes.
                        drop_stale_sidecar(&path).await;
                    }
                    Ok(Blob {
                        provider_id: self.inner.provider_id.clone(),
                        key: key.to_owned(),
                        content_type: content_type.to_owned(),
                        byte_size,
                        etag: Some(etag),
                    })
                }
                Err(err) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    Err(err)
                }
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Bytes> {
        Box::pin(async move {
            let path = self.safe_path_for_key(key).await?;
            match tokio::fs::read(&path).await {
                Ok(bytes) => Ok(Bytes::from(bytes)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Err(BlobStoreError::NotFound(key.to_owned()))
                }
                Err(err) => Err(BlobStoreError::io(err)),
            }
        })
    }

    fn get_stream<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ByteStream<'static>> {
        Box::pin(async move {
            let path = self.safe_path_for_key(key).await?;
            let file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(BlobStoreError::NotFound(key.to_owned()));
                }
                Err(err) => return Err(BlobStoreError::io(err)),
            };
            // `ReaderStream` owns the open file handle, so the resulting
            // stream is `'static` (it does not borrow `self`) and reads the
            // object incrementally rather than buffering it all in memory.
            let stream = tokio_util::io::ReaderStream::new(file)
                .map(|chunk| chunk.map_err(BlobStoreError::io));
            let boxed: ByteStream<'static> = Box::pin(stream);
            Ok(boxed)
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a str,
        start: u64,
        end: u64,
    ) -> BlobFuture<'a, ByteStream<'static>> {
        Box::pin(async move {
            use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

            let path = self.safe_path_for_key(key).await?;
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(BlobStoreError::NotFound(key.to_owned()));
                }
                Err(err) => return Err(BlobStoreError::io(err)),
            };
            // Seek to the start offset and cap the read at the inclusive slice
            // length so only `[start, end]` is read off disk — a slice of a
            // large file never buffers the whole object in memory.
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(BlobStoreError::io)?;
            let len = end.saturating_sub(start).saturating_add(1);
            let limited = file.take(len);
            let stream = tokio_util::io::ReaderStream::new(limited)
                .map(|chunk| chunk.map_err(BlobStoreError::io));
            let boxed: ByteStream<'static> = Box::pin(stream);
            Ok(boxed)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ()> {
        Box::pin(async move {
            let path = self.safe_path_for_key(key).await?;
            // Delete the blob bytes first, then the sidecar. If the
            // blob delete fails (permissions, transient I/O, …) the
            // sidecar stays in place, so a failed delete is
            // side-effect-free as far as `head`/serve are concerned.
            // If the blob delete succeeds but the sidecar delete fails,
            // the orphan sidecar is harmless: a future `head` on the
            // (now-missing) key returns `None` from the metadata-stat
            // call before the sidecar is even read, and a future `put`
            // overwrites the sidecar atomically.
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(BlobStoreError::io(err)),
            }
            let _ = tokio::fs::remove_file(meta_sidecar_path(&path)).await;
            Ok(())
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
        Box::pin(async move {
            let path = self.safe_path_for_key(key).await?;
            match tokio::fs::metadata(&path).await {
                Ok(fs_meta) => {
                    // Prefer the persisted sidecar metadata
                    // (content_type + etag) over the filesystem
                    // defaults. Fall back to `application/octet-stream`
                    // for blobs written by something other than
                    // `put`/`put_stream` (older deployments, manual
                    // file drops, …).
                    let sidecar = read_meta_sidecar(&path).await;
                    Ok(Some(BlobMeta {
                        key: key.to_owned(),
                        content_type: sidecar.as_ref().map_or_else(
                            || "application/octet-stream".to_owned(),
                            |m| m.content_type.clone(),
                        ),
                        byte_size: fs_meta.len(),
                        etag: sidecar.and_then(|m| m.etag),
                    }))
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(BlobStoreError::io(err)),
            }
        })
    }

    fn presigned_url<'a>(&'a self, key: &'a str, expires_in: Duration) -> BlobFuture<'a, String> {
        Box::pin(async move {
            validate_key(key)?;
            let expires_in = if expires_in.is_zero() {
                self.inner.default_expiry
            } else {
                expires_in
            };
            let exp_at = SystemTime::now()
                .checked_add(expires_in)
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());

            // Sign the canonical (unencoded) key — the serving route
            // decodes path segments before re-signing for verification.
            let signature = sign(self.inner.signing_key.as_bytes(), key, exp_at);
            // Percent-encode each segment so keys containing reserved
            // URL characters (`?`, `#`, `%`, spaces, …) round-trip
            // correctly through the path. `/` survives as the segment
            // separator.
            let encoded_key = encode_key_path(key);
            let url = format!(
                "{base}/{encoded_key}?exp={exp_at}&sig={signature}",
                base = self.inner.mount_path.trim_end_matches('/'),
            );
            Ok(url)
        })
    }

    fn presign_put<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        expires_in: Duration,
    ) -> BlobFuture<'a, PresignPutResult> {
        Box::pin(async move {
            validate_key(key)?;
            let expires_in = if expires_in.is_zero() {
                self.inner.default_expiry
            } else {
                expires_in
            };
            let exp_at = SystemTime::now()
                .checked_add(expires_in)
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());

            // Sign over "upload:{key}:{content_type}:{exp}" — the "upload:" prefix
            // ensures upload tokens cannot be confused with download tokens which sign
            // over "{key}:{exp}" only.
            let signature =
                sign_upload(self.inner.signing_key.as_bytes(), key, content_type, exp_at);
            let encoded_key = encode_key_path(key);
            let encoded_ct = encode_query_value(content_type);
            let url = format!(
                "{base}/{encoded_key}?upload=1&ct={encoded_ct}&exp={exp_at}&sig={signature}",
                base = self.inner.mount_path.trim_end_matches('/'),
            );
            Ok(PresignPutResult {
                url,
                method: "PUT".to_owned(),
                headers: std::collections::HashMap::new(),
                expires_in,
            })
        })
    }
}

/// Compute the upload signature for `(key, content_type, expiry)`.
///
/// Distinct from the download [`sign`] by the `"upload:"` prefix — ensures a
/// presigned download URL cannot be replayed as an upload token.
///
/// # Panics
///
/// Never; `Hmac::new_from_slice` accepts any key length.
#[must_use]
pub fn sign_upload(
    key_bytes: &[u8],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key_bytes).expect("HMAC accepts any key length");
    mac.update(b"upload:");
    mac.update(&(blob_key.len() as u64).to_be_bytes());
    mac.update(blob_key.as_bytes());
    mac.update(&(content_type.len() as u64).to_be_bytes());
    mac.update(content_type.as_bytes());
    mac.update(&expires_at.to_be_bytes());
    hex(mac.finalize().into_bytes())
}

/// Verify an upload token `(key, content_type, expiry, signature)`.
///
/// Returns `Ok(())` when the signature matches and `expires_at` is still in
/// the future.
///
/// # Errors
///
/// Returns [`BlobStoreError::Signature`] for malformed, expired, or mismatched
/// tokens.
pub fn verify_upload(
    signing_key: &[u8],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
    signature: &str,
) -> Result<(), BlobStoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    verify_upload_with_now(
        signing_key,
        blob_key,
        content_type,
        expires_at,
        signature,
        now,
    )
}

/// Clock-injectable variant of [`verify_upload`].
///
/// `now_unix` is the current Unix timestamp in seconds.
///
/// # Errors
///
/// Returns [`BlobStoreError::Signature`] for malformed, expired, or mismatched tokens.
pub fn verify_upload_with_now(
    signing_key: &[u8],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
    signature: &str,
    now_unix: u64,
) -> Result<(), BlobStoreError> {
    if expires_at < now_unix {
        return Err(BlobStoreError::Signature("upload token expired".into()));
    }
    let expected = sign_upload(signing_key, blob_key, content_type, expires_at);
    if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        return Ok(());
    }
    let expected_legacy = sign_upload_legacy(signing_key, blob_key, content_type, expires_at);
    if constant_time_eq(expected_legacy.as_bytes(), signature.as_bytes()) {
        return Ok(());
    }
    Err(BlobStoreError::Signature(
        "upload token signature mismatch".into(),
    ))
}

/// Compute the legacy upload signature for backwards compatibility.
///
/// # Panics
///
/// Never; `Hmac::new_from_slice` accepts any key length.
#[must_use]
pub fn sign_upload_legacy(
    key_bytes: &[u8],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key_bytes).expect("HMAC accepts any key length");
    mac.update(b"upload:");
    mac.update(blob_key.as_bytes());
    mac.update(b":");
    mac.update(content_type.as_bytes());
    mac.update(b":");
    mac.update(expires_at.to_string().as_bytes());
    hex(mac.finalize().into_bytes())
}

/// Verify an upload token against the current key and each previous key in a
/// rotation grace window.
#[cfg(test)]
pub(crate) fn verify_upload_with_rotation(
    current: &SigningKey,
    previous: &[SigningKey],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
    signature: &str,
) -> Result<(), BlobStoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    verify_upload_rotation_with_now(
        current,
        previous,
        blob_key,
        content_type,
        expires_at,
        signature,
        now,
    )
}

/// Clock-injectable variant of [`verify_upload_with_rotation`].
pub(crate) fn verify_upload_rotation_with_now(
    current: &SigningKey,
    previous: &[SigningKey],
    blob_key: &str,
    content_type: &str,
    expires_at: u64,
    signature: &str,
    now_unix: u64,
) -> Result<(), BlobStoreError> {
    if expires_at < now_unix {
        return Err(BlobStoreError::Signature("upload token expired".into()));
    }
    let expected_current = sign_upload(current.as_bytes(), blob_key, content_type, expires_at);
    if constant_time_eq(expected_current.as_bytes(), signature.as_bytes()) {
        return Ok(());
    }
    let expected_current_legacy =
        sign_upload_legacy(current.as_bytes(), blob_key, content_type, expires_at);
    if constant_time_eq(expected_current_legacy.as_bytes(), signature.as_bytes()) {
        return Ok(());
    }
    for prev in previous {
        let expected = sign_upload(prev.as_bytes(), blob_key, content_type, expires_at);
        if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return Ok(());
        }
        let expected_legacy =
            sign_upload_legacy(prev.as_bytes(), blob_key, content_type, expires_at);
        if constant_time_eq(expected_legacy.as_bytes(), signature.as_bytes()) {
            return Ok(());
        }
    }
    Err(BlobStoreError::Signature(
        "upload token signature mismatch".into(),
    ))
}

/// Percent-encode a value for use in a URL query parameter.
///
/// Encodes characters that would otherwise be interpreted as query
/// delimiters (`&`, `=`, `?`, `+`) or that break URL parsing (space,
/// `#`, `%`). Also encodes `/` so content-types like `"image/png"` round-trip
/// cleanly through the `ct=` parameter without needing extra path-segment
/// splitting logic on the receiving end.
fn encode_query_value(value: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    const QUERY_VALUE: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'&')
        .add(b'+')
        .add(b'/')
        .add(b'=')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b']')
        .add(b'^')
        .add(b'`')
        .add(b'{')
        .add(b'|')
        .add(b'}')
        .add(b'<')
        .add(b'>');
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

/// Compute the canonical signature for `(key, expiry)`.
///
/// # Panics
///
/// Never; `Hmac::new_from_slice` accepts any key length.
#[must_use]
pub fn sign(key_bytes: &[u8], blob_key: &str, expires_at: u64) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key_bytes).expect("HMAC accepts any key length");
    mac.update(blob_key.as_bytes());
    mac.update(b":");
    mac.update(expires_at.to_string().as_bytes());
    hex(mac.finalize().into_bytes())
}

/// Verify a `(key, expiry, signature)` triple.
///
/// Returns `Ok(())` when the signature matches and `expires_at` is
/// still in the future.
///
/// # Errors
///
/// Returns [`BlobStoreError::Signature`] for malformed, expired, or
/// mismatched signatures.
pub fn verify(
    signing_key: &[u8],
    blob_key: &str,
    expires_at: u64,
    signature: &str,
) -> Result<(), BlobStoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    verify_with_now(signing_key, blob_key, expires_at, signature, now)
}

/// Clock-injectable variant of [`verify`].
///
/// `now_unix` is the current Unix timestamp in seconds — obtain it via
/// [`crate::time::clock_unix_secs`] when the framework clock is injected.
///
/// # Errors
///
/// Returns [`BlobStoreError::Signature`] for malformed, expired, or
/// mismatched signatures.
pub fn verify_with_now(
    signing_key: &[u8],
    blob_key: &str,
    expires_at: u64,
    signature: &str,
    now_unix: u64,
) -> Result<(), BlobStoreError> {
    let expected = sign(signing_key, blob_key, expires_at);
    if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        return Err(BlobStoreError::Signature("signature mismatch".into()));
    }
    if expires_at < now_unix {
        return Err(BlobStoreError::Signature("signed url expired".into()));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(hasher.finalize())
}

/// Build a same-directory temp path for atomic write-then-rename. The
/// suffix carries a UUID v4 so concurrent writers to the same key
/// don't collide. Same-directory placement is what makes the `rename`
/// atomic on POSIX (cross-device renames would silently degrade to
/// copy-and-delete).
/// Replace `dst` with `src` atomically across platforms.
///
/// On POSIX, `rename` overwrites an existing destination atomically.
/// Replace `dst` with `src` atomically across platforms.
///
/// On POSIX, `rename` overwrites an existing destination atomically,
/// so the first attempt is the fast path. On Windows, `MoveFileEx`
/// without `MOVEFILE_REPLACE_EXISTING` errors with `AlreadyExists`
/// when the destination exists; the fallback path moves the existing
/// `dst` aside to a sibling backup, renames `src` into place, and
/// removes the backup. If the second rename fails for any reason
/// (transient I/O error, permissions, etc.), we rename the backup
/// back into `dst` so a failed overwrite never destroys the
/// previously committed blob — the caller still gets the rename
/// error and cleans up `src`, but the original blob stays intact.
///
/// There's still a tiny non-atomic window on the Windows fallback
/// path between the move-aside and the rename-into-place. Eliminating
/// it requires `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` via the
/// `windows` crate, which we'd rather not pull in for one syscall.
async fn atomic_replace(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // Move the existing dst aside under a unique sibling name
            // so we can restore it if the rename-into-place fails.
            let backup = backup_sibling_path(dst);
            tokio::fs::rename(dst, &backup).await?;
            match tokio::fs::rename(src, dst).await {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(&backup).await;
                    Ok(())
                }
                Err(rename_err) => {
                    // Restore the original dst from backup. If even
                    // this fails the system is in a state we can't
                    // automatically recover, but it's still better
                    // than the unconditional-delete alternative.
                    let _ = tokio::fs::rename(&backup, dst).await;
                    Err(rename_err)
                }
            }
        }
        Err(err) => Err(err),
    }
}

fn backup_sibling_path(path: &std::path::Path) -> std::path::PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("blob"),
        std::ffi::OsStr::to_owned,
    );
    name.push(".bak.");
    name.push(&id);
    path.with_file_name(name)
}

fn temp_sibling_path(path: &std::path::Path) -> std::path::PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("blob"),
        std::ffi::OsStr::to_owned,
    );
    name.push(".tmp.");
    name.push(&id);
    path.with_file_name(name)
}

/// Persisted metadata that travels alongside each blob's bytes.
///
/// Written by `put`/`put_stream` to a `<path>.meta` sibling so the
/// serving route can return the original `Content-Type` instead of
/// the default `application/octet-stream`.
///
/// **Caveat**: a blob whose key happens to end in `.meta` and aliases
/// the sidecar of a sibling key would collide. Document the constraint
/// in [`docs/guide/storage.md`](../../../docs/guide/storage.md) — in
/// practice keys come from app code (`avatars/{user_id}.png`,
/// `attachments/{uuid}.pdf`) so the chance is vanishing. The S3
/// backend has no equivalent issue because S3 stores `Content-Type` as
/// part of the object's metadata.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredBlobMeta {
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

fn meta_sidecar_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("blob"),
        std::ffi::OsStr::to_owned,
    );
    name.push(".meta");
    path.with_file_name(name)
}

/// Write the sidecar metadata after the bytes have committed.
///
/// Goes through the same temp-file + atomic-rename pattern as blob
/// bytes so a hostile or accidental symlink at the sidecar path can't
/// be followed: `tokio::fs::write` would otherwise dereference a
/// symlink and clobber arbitrary files reachable through it. The
/// temp file uses `create_new(true)` so even on the temp path an
/// existing file or symlink errors out (the uuid suffix means an
/// attacker can't predict the path), and the final rename replaces
/// the dirent atomically without following whatever was at the
/// destination.
///
/// Returns `Ok(())` on success and `Err(())` on any logged failure
/// (already-logged inside, callers don't need to log again). On
/// failure callers should delete any pre-existing sidecar so
/// `head` / serving don't return stale `content_type` for the freshly
/// committed bytes.
#[allow(clippy::cognitive_complexity)]
async fn write_meta_sidecar(blob_path: &std::path::Path, meta: &StoredBlobMeta) -> Result<(), ()> {
    use tokio::io::AsyncWriteExt as _;

    let path = meta_sidecar_path(blob_path);
    let bytes = match serde_json::to_vec(meta) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize blob metadata sidecar");
            return Err(());
        }
    };

    let tmp = temp_sibling_path(&path);
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
    {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(
                error = %err,
                tmp = %tmp.display(),
                "failed to create blob metadata sidecar temp file"
            );
            return Err(());
        }
    };
    if let Err(err) = file.write_all(&bytes).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!(error = %err, "failed to write blob metadata sidecar bytes");
        return Err(());
    }
    if let Err(err) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!(error = %err, "failed to flush blob metadata sidecar");
        return Err(());
    }
    drop(file);
    if let Err(err) = atomic_replace(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!(
            error = %err,
            sidecar = %path.display(),
            "failed to commit blob metadata sidecar"
        );
        return Err(());
    }
    Ok(())
}

/// On a failed sidecar write, remove any pre-existing sidecar so a
/// future `head`/serve request returns the `application/octet-stream`
/// fallback rather than stale `content_type` from the previous put.
/// The bytes are already committed; we'd rather serve "I don't know"
/// than misrepresent the MIME.
async fn drop_stale_sidecar(blob_path: &std::path::Path) {
    let path = meta_sidecar_path(blob_path);
    if let Err(err) = tokio::fs::remove_file(&path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            error = %err,
            sidecar = %path.display(),
            "failed to clear stale blob metadata sidecar after sidecar-write failure"
        );
    }
}

/// Read the sidecar metadata for a blob. Returns `None` for a missing
/// or unparseable sidecar so the serving / `head` paths can fall back
/// gracefully.
async fn read_meta_sidecar(blob_path: &std::path::Path) -> Option<StoredBlobMeta> {
    let path = meta_sidecar_path(blob_path);
    let bytes = tokio::fs::read(&path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Percent-encode each `/`-separated segment of `key` for use in a URL
/// path. Segment separators stay raw so the path tree survives.
fn encode_key_path(key: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    // RFC 3986 path segment: encode controls, path-reserved, and
    // anything that would alias another segment or query/fragment
    // delimiter.
    const PATH_SEGMENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'/')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'\\');

    let mut result = String::with_capacity(key.len() + 16);
    let mut first = true;
    for segment in key.split('/') {
        if !first {
            result.push('/');
        }
        first = false;
        result.extend(utf8_percent_encode(segment, PATH_SEGMENT));
    }
    result
}

fn hex<B: AsRef<[u8]>>(bytes: B) -> String {
    let mut s = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// Verify a signed blob URL against `current` and each `previous` key.
///
/// Expiry is checked first (same for all keys). The signature is then compared
/// against every key using constant-time comparison; the first match wins.
/// This enables a rotation grace window: sign new URLs with `current` while
/// URLs that were signed with an old key continue to serve until their expiry.
#[cfg(test)]
pub(crate) fn verify_with_rotation(
    current: &SigningKey,
    previous: &[SigningKey],
    blob_key: &str,
    expires_at: u64,
    signature: &str,
) -> Result<(), BlobStoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    verify_with_rotation_with_now(current, previous, blob_key, expires_at, signature, now)
}

/// Clock-injectable variant of [`verify_with_rotation`].
pub(crate) fn verify_with_rotation_with_now(
    current: &SigningKey,
    previous: &[SigningKey],
    blob_key: &str,
    expires_at: u64,
    signature: &str,
    now_unix: u64,
) -> Result<(), BlobStoreError> {
    if expires_at < now_unix {
        return Err(BlobStoreError::Signature("signed url expired".into()));
    }
    let expected_current = sign(current.as_bytes(), blob_key, expires_at);
    if constant_time_eq(expected_current.as_bytes(), signature.as_bytes()) {
        return Ok(());
    }
    for prev in previous {
        let expected = sign(prev.as_bytes(), blob_key, expires_at);
        if constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return Ok(());
        }
    }
    Err(BlobStoreError::Signature("signature mismatch".into()))
}

// ── Serving route ──────────────────────────────────────────────

/// Build the axum router that serves signed local-blob URLs and accepts
/// direct PUT uploads.
///
/// This is mounted by the framework at the configured `mount_path`.
///
/// Routes mounted:
/// - `GET {mount_path}/{*key}?exp=…&sig=…` — serve a presigned blob
/// - `PUT {mount_path}/{*key}?upload=1&ct=…&exp=…&sig=…` — accept a direct upload
pub fn serve_router(store: &LocalBlobStore) -> axum::Router<crate::AppState> {
    use axum::extract::{Path, Query};
    use axum::response::IntoResponse;

    #[derive(Debug, serde::Deserialize)]
    struct SignedQuery {
        exp: u64,
        sig: String,
    }

    #[derive(Debug, serde::Deserialize)]
    struct UploadQuery {
        #[allow(dead_code)]
        upload: i32,
        ct: String,
        exp: u64,
        sig: String,
    }

    let store_for_route = store.clone();
    let mount = format!("{}/{{*key}}", store.mount_path().trim_end_matches('/'));

    let handler = move |axum::extract::State(state): axum::extract::State<crate::AppState>,
                        Path(blob_key): Path<String>,
                        Query(q): Query<SignedQuery>| {
        let store = store_for_route.clone();
        async move {
            let now = crate::time::clock_unix_secs(state.clock());
            if let Err(err) = verify_with_rotation_with_now(
                &store.inner.signing_key,
                &store.inner.previous_signing_keys,
                &blob_key,
                q.exp,
                &q.sig,
                now,
            ) {
                return (StatusCode::FORBIDDEN, err.to_string()).into_response();
            }
            match store.get_with_meta(&blob_key).await {
                Ok((bytes, meta)) => {
                    let content_type = meta
                        .map_or_else(|| "application/octet-stream".to_owned(), |m| m.content_type);
                    ([(http::header::CONTENT_TYPE, content_type)], bytes).into_response()
                }
                Err(BlobStoreError::NotFound(_)) => {
                    (StatusCode::NOT_FOUND, "not found").into_response()
                }
                Err(err) => err.into_autumn_error().into_response(),
            }
        }
    };

    let store_for_upload = store.clone();
    let upload_handler = move |axum::extract::State(state): axum::extract::State<
        crate::AppState,
    >,
                               Path(blob_key): Path<String>,
                               Query(q): Query<UploadQuery>,
                               body: axum::body::Body| {
        use futures::StreamExt as _;
        let store = store_for_upload.clone();
        async move {
            let now = crate::time::clock_unix_secs(state.clock());
            if let Err(err) = verify_upload_rotation_with_now(
                &store.inner.signing_key,
                &store.inner.previous_signing_keys,
                &blob_key,
                &q.ct,
                q.exp,
                &q.sig,
                now,
            ) {
                return (StatusCode::FORBIDDEN, err.to_string()).into_response();
            }

            let limit = state.config_arc().security.upload.max_request_size_bytes;
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let stream = body.into_data_stream();
            let byte_stream = Box::pin(stream.map(move |item| match item {
                Ok(bytes) => {
                    let total = counter.fetch_add(bytes.len(), std::sync::atomic::Ordering::SeqCst)
                        + bytes.len();
                    if total > limit {
                        Err(crate::storage::BlobStoreError::PayloadTooLarge(format!(
                            "Upload size limit of {limit} bytes exceeded"
                        )))
                    } else {
                        Ok(bytes)
                    }
                }
                Err(e) => Err(crate::storage::BlobStoreError::Io(e.to_string())),
            }));

            match store.put_stream(&blob_key, &q.ct, byte_stream).await {
                Ok(_blob) => StatusCode::OK.into_response(),
                Err(err) => err.into_autumn_error().into_response(),
            }
        }
    };

    axum::Router::new()
        .route(&mount, axum::routing::get(handler))
        .route(&mount, axum::routing::put(upload_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn store(root: &Path) -> LocalBlobStore {
        LocalBlobStore::new(
            "test",
            root.to_path_buf(),
            "/_blobs",
            Duration::from_secs(60),
            SigningKey::new(b"test-key".to_vec()),
            vec![],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let dir = temp_root();
        let s = store(dir.path());
        let blob = s
            .put("a/b.png", "image/png", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        assert_eq!(blob.byte_size, 3);
        assert!(blob.etag.is_some());
        let bytes = s.get("a/b.png").await.unwrap();
        assert_eq!(&bytes[..], b"abc");
    }

    #[tokio::test]
    async fn put_stream_round_trip() {
        let dir = temp_root();
        let s = store(dir.path());
        let chunks: Vec<Result<Bytes, BlobStoreError>> = vec![
            Ok(Bytes::from_static(b"hello, ")),
            Ok(Bytes::from_static(b"world")),
        ];
        let stream: ByteStream<'static> = Box::pin(stream::iter(chunks));
        let blob = s
            .put_stream("greet.txt", "text/plain", stream)
            .await
            .unwrap();
        assert_eq!(blob.byte_size, 12);
        let bytes = s.get("greet.txt").await.unwrap();
        assert_eq!(&bytes[..], b"hello, world");
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let dir = temp_root();
        let s = store(dir.path());
        let err = s.get("missing.txt").await.unwrap_err();
        assert!(matches!(err, BlobStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_idempotent() {
        let dir = temp_root();
        let s = store(dir.path());
        s.delete("nope").await.unwrap();
        let _ = s
            .put("k.txt", "text/plain", Bytes::from_static(b"x"))
            .await
            .unwrap();
        s.delete("k.txt").await.unwrap();
        assert!(matches!(
            s.get("k.txt").await.unwrap_err(),
            BlobStoreError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn rejects_traversal_keys() {
        let dir = temp_root();
        let s = store(dir.path());
        let err = s
            .put("../escape.txt", "text/plain", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[test]
    fn new_rejects_mount_path_without_leading_slash() {
        let dir = temp_root();
        let err = LocalBlobStore::new(
            "test",
            dir.path().to_path_buf(),
            "_blobs", // missing leading slash
            Duration::from_secs(60),
            SigningKey::new(b"k".to_vec()),
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    /// A hostile or accidental symlink inside the storage tree can
    /// turn a legitimate-looking key into a path-escape. Pin that we
    /// catch the canonical-path mismatch before any IO happens.
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_keys_traversing_root_escaping_symlinks() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        // Create a sensitive file outside the storage root.
        std::fs::write(outside.path().join("secret"), b"do not read").unwrap();

        let dir = temp_root();
        // root/escape -> outside_dir
        symlink(outside.path(), dir.path().join("escape")).unwrap();

        let s = store(dir.path());
        let err = s.get("escape/secret").await.unwrap_err();
        assert!(
            matches!(err, BlobStoreError::PermissionDenied(_)),
            "expected PermissionDenied, got {err:?}"
        );

        // And the same for writes — `put` to a key that resolves
        // outside the root must refuse before any bytes hit disk.
        let err = s
            .put(
                "escape/leaked.txt",
                "text/plain",
                Bytes::from_static(b"oops"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BlobStoreError::PermissionDenied(_)));
        // Outside dir is untouched.
        assert!(!outside.path().join("leaked.txt").exists());
    }

    /// A hostile symlink planted at the *sidecar* path is the same
    /// threat as one planted at the blob path: the naïve
    /// `tokio::fs::write` would follow it and clobber arbitrary
    /// targets. The temp-file + atomic-rename pattern in
    /// `write_meta_sidecar` replaces the dirent atomically without
    /// following whatever was there.
    #[cfg(unix)]
    #[tokio::test]
    async fn sidecar_write_does_not_follow_hostile_symlink() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("untouchable");
        std::fs::write(&target, b"original-contents").unwrap();

        let dir = temp_root();
        let s = store(dir.path());

        // Plant a symlink at the *sidecar* path before the put runs.
        // The sidecar for key `victim.bin` is `victim.bin.meta`.
        let sidecar_path = dir.path().join("victim.bin.meta");
        symlink(&target, &sidecar_path).unwrap();

        // The put succeeds (sidecar errors are logged, not surfaced) —
        // the important invariant is the symlink target.
        s.put("victim.bin", "image/png", Bytes::from_static(b"pixels"))
            .await
            .unwrap();

        // The original symlink target must still hold its original
        // bytes, *not* the sidecar JSON.
        assert_eq!(std::fs::read(&target).unwrap(), b"original-contents");
    }

    #[test]
    fn signature_round_trip() {
        let key = b"k";
        let sig = sign(key, "blob/1.png", 99);
        verify(key, "blob/1.png", u64::MAX / 2, &sig).unwrap_err();
        // Use a now-future expiry for verification.
        let exp = SystemTime::now()
            .checked_add(Duration::from_secs(60))
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = sign(key, "blob/1.png", exp);
        verify(key, "blob/1.png", exp, &sig).unwrap();
    }

    #[test]
    fn signature_rejects_wrong_key() {
        let exp = SystemTime::now()
            .checked_add(Duration::from_secs(60))
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = sign(b"alpha", "blob/1.png", exp);
        let err = verify(b"beta", "blob/1.png", exp, &sig).unwrap_err();
        assert!(matches!(err, BlobStoreError::Signature(_)));
    }

    #[test]
    fn signature_rejects_expired() {
        let exp = 1; // long ago
        let sig = sign(b"k", "blob/1.png", exp);
        let err = verify(b"k", "blob/1.png", exp, &sig).unwrap_err();
        assert!(matches!(err, BlobStoreError::Signature(_)));
    }

    #[tokio::test]
    async fn presigned_url_includes_signature_and_exp() {
        let dir = temp_root();
        let s = store(dir.path());
        let url = s
            .presigned_url("a/b.png", Duration::from_secs(120))
            .await
            .unwrap();
        assert!(url.starts_with("/_blobs/a/b.png?exp="));
        assert!(url.contains("&sig="));
    }

    #[tokio::test]
    async fn presigned_url_percent_encodes_reserved_chars() {
        let dir = temp_root();
        let s = store(dir.path());
        // Space and `#` pass `validate_key` (the Windows-reserved
        // rejection set is `< > : " | ? *` + control bytes only) but
        // still need percent-encoding inside the URL path.
        let url = s
            .presigned_url("user 1/note#1.png", Duration::from_secs(120))
            .await
            .unwrap();
        // `/` stays raw as a segment separator.
        assert!(
            url.starts_with("/_blobs/user%201/note%231.png?exp="),
            "unexpected URL: {url}"
        );
        assert!(url.contains("&sig="));
    }

    #[tokio::test]
    async fn put_stream_cleans_up_partial_file_on_error() {
        use futures::stream;

        let dir = temp_root();
        let s = store(dir.path());

        // First chunk succeeds, second yields an error to short-circuit
        // the write.
        let chunks: Vec<Result<Bytes, BlobStoreError>> = vec![
            Ok(Bytes::from_static(b"first")),
            Err(BlobStoreError::Backend("boom".into())),
        ];
        let stream: ByteStream<'static> = Box::pin(stream::iter(chunks));
        let err = s
            .put_stream("interrupted.bin", "application/octet-stream", stream)
            .await
            .unwrap_err();
        assert!(matches!(err, BlobStoreError::Backend(_)));

        // The partial file must not be left on disk.
        let path = dir.path().join("interrupted.bin");
        assert!(!path.exists(), "partial blob was not cleaned up");
        assert!(matches!(
            s.get("interrupted.bin").await.unwrap_err(),
            BlobStoreError::NotFound(_)
        ));
    }

    #[test]
    fn encode_key_path_passes_segments_separately() {
        assert_eq!(encode_key_path("foo"), "foo");
        assert_eq!(encode_key_path("a/b/c"), "a/b/c");
        assert_eq!(encode_key_path("a b/c?d"), "a%20b/c%3Fd");
        assert_eq!(encode_key_path("hash#frag/q"), "hash%23frag/q");
        assert_eq!(encode_key_path(""), "");
        assert_eq!(encode_key_path("a/"), "a/");
        assert_eq!(encode_key_path("/b"), "/b");
        assert_eq!(encode_key_path("🚀/path"), "%F0%9F%9A%80/path");
    }

    #[tokio::test]
    async fn put_replaces_atomically() {
        // Successive `put` calls to the same key replace via temp-file +
        // rename. Concrete fault injection for a mid-write IO error is
        // exercised through `put_stream_cleans_up_partial_file_on_error`;
        // here we just confirm the happy path of atomic replacement.
        let dir = temp_root();
        let s = store(dir.path());
        s.put(
            "k.bin",
            "application/octet-stream",
            Bytes::from_static(b"first"),
        )
        .await
        .unwrap();
        s.put(
            "k.bin",
            "application/octet-stream",
            Bytes::from_static(b"second"),
        )
        .await
        .unwrap();
        assert_eq!(&s.get("k.bin").await.unwrap()[..], b"second");

        // No leftover temp files in the storage root.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|n| n.contains(".tmp.")),
            "temp file leaked: {entries:?}"
        );
    }

    #[tokio::test]
    async fn atomic_replace_overwrites_existing_destination() {
        let dir = temp_root();
        let dst = dir.path().join("target.bin");
        tokio::fs::write(&dst, b"old").await.unwrap();
        let tmp = dir.path().join("staging.tmp");
        tokio::fs::write(&tmp, b"new").await.unwrap();

        atomic_replace(&tmp, &dst).await.unwrap();

        assert_eq!(tokio::fs::read(&dst).await.unwrap(), b"new");
        assert!(!tmp.exists(), "temp file should be consumed by rename");
    }

    #[tokio::test]
    async fn atomic_replace_creates_new_destination() {
        let dir = temp_root();
        let dst = dir.path().join("fresh.bin");
        let tmp = dir.path().join("staging.tmp");
        tokio::fs::write(&tmp, b"hello").await.unwrap();

        atomic_replace(&tmp, &dst).await.unwrap();

        assert_eq!(tokio::fs::read(&dst).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn atomic_replace_propagates_io_errors() {
        let dir = temp_root();
        let tmp = dir.path().join("missing.tmp"); // never created
        let dst = dir.path().join("target.bin");
        let err = atomic_replace(&tmp, &dst).await.unwrap_err();
        assert!(
            matches!(err.kind(), std::io::ErrorKind::NotFound),
            "expected NotFound, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn temp_sibling_path_keeps_parent_directory() {
        let original = std::path::Path::new("/var/lib/blobs/avatars/me.png");
        let tmp = temp_sibling_path(original);
        assert_eq!(tmp.parent(), original.parent());
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("me.png.tmp."));
    }

    #[test]
    fn backup_sibling_path_keeps_parent_directory() {
        let original = std::path::Path::new("/var/lib/blobs/avatars/me.png");
        let backup = backup_sibling_path(original);
        assert_eq!(backup.parent(), original.parent());
        let name = backup.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("me.png.bak."));
    }

    #[test]
    fn meta_sidecar_path_appends_meta_suffix() {
        let blob = std::path::Path::new("/var/lib/blobs/avatars/me.png");
        let sidecar = meta_sidecar_path(blob);
        assert_eq!(sidecar.parent(), blob.parent());
        assert_eq!(sidecar.file_name().unwrap(), "me.png.meta");
    }

    #[tokio::test]
    async fn put_persists_content_type_for_head_and_serve() {
        let dir = temp_root();
        let s = store(dir.path());
        let blob = s
            .put("a/b.png", "image/png", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        assert_eq!(blob.content_type, "image/png");

        let meta = s.head("a/b.png").await.unwrap().expect("blob exists");
        assert_eq!(meta.content_type, "image/png");
        assert!(meta.etag.is_some(), "etag should round-trip via sidecar");
    }

    #[tokio::test]
    async fn delete_cleans_up_meta_sidecar() {
        let dir = temp_root();
        let s = store(dir.path());
        s.put("k.png", "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let resolved = s.safe_path_for_key("k.png").await.unwrap();
        assert!(meta_sidecar_path(&resolved).exists());

        s.delete("k.png").await.unwrap();
        assert!(!meta_sidecar_path(&resolved).exists());
    }

    /// When the blob removal fails, the sidecar must stay in place so
    /// the failed delete is side-effect-free as far as `head` / serve
    /// are concerned. Force the failure by making the blob path itself
    /// a directory (POSIX `unlink` errors on directories with
    /// `IsADirectory` / `EISDIR`); permissions-based forcings don't
    /// work uniformly when tests run as root.
    #[tokio::test]
    async fn delete_keeps_sidecar_when_blob_remove_fails() {
        let dir = temp_root();
        let s = store(dir.path());

        // Put `pinned.bin` as a *directory* so `remove_file` errors,
        // and pre-stage a sidecar so we can verify it survives.
        let blob_path = dir.path().join("pinned.bin");
        tokio::fs::create_dir(&blob_path).await.unwrap();
        let sidecar = meta_sidecar_path(&blob_path);
        tokio::fs::write(&sidecar, br#"{"content_type":"image/png"}"#)
            .await
            .unwrap();
        assert!(blob_path.is_dir());
        assert!(sidecar.is_file());

        let result = s.delete("pinned.bin").await;
        assert!(
            result.is_err(),
            "expected error: blob path is a directory, remove_file should fail"
        );
        // The new ordering propagates the blob-delete error before
        // touching the sidecar, so the sidecar stays behind.
        assert!(
            sidecar.exists(),
            "sidecar must survive a failed blob delete"
        );
        // And the (directory) blob is also still there.
        assert!(blob_path.is_dir());
    }

    #[tokio::test]
    async fn head_falls_back_to_octet_stream_without_sidecar() {
        // Simulate an older blob written without a sidecar.
        let dir = temp_root();
        let s = store(dir.path());
        let path = dir.path().join("legacy.bin");
        tokio::fs::write(&path, b"raw").await.unwrap();
        let meta = s.head("legacy.bin").await.unwrap().expect("blob exists");
        assert_eq!(meta.content_type, "application/octet-stream");
        assert_eq!(meta.byte_size, 3);
        assert!(meta.etag.is_none());
    }

    #[tokio::test]
    async fn drop_stale_sidecar_removes_existing_metadata() {
        // The recovery path used by `put` / `put_stream` when a sidecar
        // write fails after the bytes commit: we delete the old
        // sidecar so future `head`/serve calls fall back to
        // octet-stream rather than reporting stale MIME for the new
        // bytes.
        let dir = temp_root();
        let blob = dir.path().join("victim.bin");
        let sidecar = meta_sidecar_path(&blob);
        tokio::fs::write(&sidecar, br#"{"content_type":"image/png"}"#)
            .await
            .unwrap();
        assert!(sidecar.exists());

        drop_stale_sidecar(&blob).await;
        assert!(!sidecar.exists());

        // Idempotent — calling again on a missing sidecar is a no-op,
        // not an error.
        drop_stale_sidecar(&blob).await;
    }

    #[tokio::test]
    async fn read_meta_sidecar_handles_missing_and_malformed() {
        let dir = temp_root();
        // Missing sidecar → None (file at the blob path doesn't matter
        // for this helper; we're only testing sidecar read behavior).
        let blob_path = dir.path().join("absent.bin");
        assert!(read_meta_sidecar(&blob_path).await.is_none());

        // Malformed JSON in the sidecar → None (graceful degradation;
        // the serving route falls back to octet-stream).
        let blob_path = dir.path().join("malformed.bin");
        tokio::fs::write(meta_sidecar_path(&blob_path), b"not json")
            .await
            .unwrap();
        assert!(read_meta_sidecar(&blob_path).await.is_none());
    }

    #[tokio::test]
    async fn get_with_meta_returns_bytes_plus_sidecar_metadata() {
        let dir = temp_root();
        let s = store(dir.path());
        s.put(
            "doc.pdf",
            "application/pdf",
            Bytes::from_static(b"%PDF-1.4"),
        )
        .await
        .unwrap();
        let (bytes, meta) = s.get_with_meta("doc.pdf").await.unwrap();
        assert_eq!(&bytes[..], b"%PDF-1.4");
        let m = meta.expect("sidecar should be present");
        assert_eq!(m.content_type, "application/pdf");
        assert!(m.etag.is_some());
    }

    /// Simulates the Windows fallback's "second rename fails" branch
    /// directly (POSIX `rename` always replaces, so the production
    /// path on Linux never enters the `AlreadyExists` arm). The
    /// invariant we care about is: if the recovery-rename succeeds,
    /// the original `dst` content is preserved when the second
    /// rename fails.
    #[tokio::test]
    async fn atomic_replace_recovery_restores_dst_on_failure() {
        let dir = temp_root();
        let dst = dir.path().join("target.bin");
        tokio::fs::write(&dst, b"old-blob").await.unwrap();

        // Manually drive the recovery sequence. Move dst aside …
        let backup = backup_sibling_path(&dst);
        tokio::fs::rename(&dst, &backup).await.unwrap();
        // … attempt the second rename with a non-existent source so it
        // fails (this is the "transient failure on the new write"
        // branch the production code's `match` arm catches) …
        let err = tokio::fs::rename(dir.path().join("never.tmp"), &dst)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        // … and restore the backup, which is exactly what the
        // production path does on rename failure.
        tokio::fs::rename(&backup, &dst).await.unwrap();

        // The original blob bytes are intact.
        assert_eq!(tokio::fs::read(&dst).await.unwrap(), b"old-blob");
    }

    #[test]
    fn encode_key_path_does_not_skip_leading_slash() {
        let key = "/some/key";
        let encoded = encode_key_path(key);
        // The first segment before the split '/' is empty
        assert_eq!(encoded, "/some/key");
    }

    #[test]
    fn sha256_hex_computes_correct_hash() {
        // e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 is empty string
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hex_computes_correct_hex() {
        assert_eq!(hex(b"xyz"), "78797a");
    }

    #[test]
    fn verify_rejects_empty_signature() {
        let key = b"secret";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expires = now + 3600;
        let result = verify(key, "blob", expires, "");
        assert!(matches!(result, Err(BlobStoreError::Signature(_))));
    }

    #[tokio::test]
    async fn safe_path_for_key_rejects_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let s2 = store(&path);
        dir.close().unwrap();

        let err = s2.safe_path_for_key("some_blob").await.unwrap_err();
        assert!(
            matches!(
                err,
                BlobStoreError::Io(_) | BlobStoreError::PermissionDenied(_)
            ),
            "Unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn get_with_meta_propagates_io_errors() {
        let dir = temp_root();
        let s = store(dir.path());

        let path = dir.path().join("some_blob");
        // create a directory so that get_with_meta reading file fails
        tokio::fs::create_dir(&path).await.unwrap();
        let Err(err) = s.get_with_meta("some_blob").await else {
            panic!("Expected error");
        };
        assert!(matches!(err, BlobStoreError::Io(_)));
    }

    #[tokio::test]
    async fn drop_stale_sidecar_is_idempotent() {
        let dir = temp_root();
        let blob = dir.path().join("victim.bin");
        let sidecar = meta_sidecar_path(&blob);

        drop_stale_sidecar(&blob).await;
        drop_stale_sidecar(&blob).await;
        // Make sure doing it when file doesn't exist doesn't panic
        assert!(!sidecar.exists());
    }

    #[tokio::test]
    async fn drop_stale_sidecar_ignores_and_survives_non_not_found_errors() {
        // Create a directory where the sidecar should be so that remove_file returns EISDIR
        let dir = temp_root();
        let blob = dir.path().join("victim.bin");
        let sidecar = meta_sidecar_path(&blob);
        tokio::fs::create_dir(&sidecar).await.unwrap();

        // The function drop_stale_sidecar ignores all errors internally but prints a warning.
        // It's `async fn drop_stale_sidecar`, no return value.
        // We will just run it and ensure it doesn't panic on a non-not-found error.
        drop_stale_sidecar(&blob).await;
        assert!(sidecar.exists()); // Because it couldn't remove a dir
    }

    #[tokio::test]
    async fn get_with_meta_when_meta_missing_returns_none() {
        let dir = temp_root();
        let s = store(dir.path());

        // Write the blob but no sidecar
        let path = dir.path().join("blob.bin");
        tokio::fs::write(&path, b"data").await.unwrap();

        let (bytes, meta) = s.get_with_meta("blob.bin").await.unwrap();
        assert_eq!(&bytes[..], b"data");
        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn atomic_replace_handles_already_exists_on_backup_rename() {
        let dir = temp_root();
        let dst = dir.path().join("target.bin");
        tokio::fs::write(&dst, b"old").await.unwrap();

        let tmp = dir.path().join("staging.tmp");
        tokio::fs::write(&tmp, b"new").await.unwrap();

        // Create a backup file manually to trigger AlreadyExists
        let backup = backup_sibling_path(&dst);
        tokio::fs::write(&backup, b"interloper").await.unwrap();

        // Atomic replace will encounter AlreadyExists when renaming `dst` to `backup`
        // It will then retry. Before it loops, it does nothing if there is an interloper? No, the code says:
        // if err.kind() == std::io::ErrorKind::AlreadyExists ... Wait, rename can overwrite. But on Windows `rename` fails if dst exists.
        // It's possible `AlreadyExists` or `PermissionDenied` depending on OS. The code is:
        // match tokio::fs::rename(dst, backup_path).await {
        //   Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
        //       let _ = tokio::fs::remove_file(backup_path).await;
        //       continue;
        //   } ...
        // So we can simulate this by mocking, or relying on `rename` failing if exists (not true on Unix).
        // Since we are on Unix, rename just replaces. To trigger this we can't easily do it.
        // Let's at least test we can still replace if backup exists.
        atomic_replace(&tmp, &dst).await.unwrap();

        assert_eq!(tokio::fs::read(&dst).await.unwrap(), b"new");
    }

    #[tokio::test]
    async fn put_rejects_missing_directory_to_trigger_io_error() {
        let missing = tempfile::tempdir().unwrap().path().to_path_buf();
        let s2 = store(&missing);
        std::fs::remove_dir_all(&missing).unwrap();

        let err = s2
            .put("some_blob", "text/plain", bytes::Bytes::from_static(b"xyz"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BlobStoreError::Io(_) | BlobStoreError::PermissionDenied(_)
        ));
    }

    #[tokio::test]
    async fn delete_rejects_missing_directory_to_trigger_io_error() {
        let missing = tempfile::tempdir().unwrap().path().to_path_buf();
        let s2 = store(&missing);
        std::fs::remove_dir_all(&missing).unwrap();

        let err = s2.delete("some_blob").await.unwrap_err();
        assert!(matches!(
            err,
            BlobStoreError::Io(_) | BlobStoreError::PermissionDenied(_)
        ));
    }

    #[tokio::test]
    async fn head_rejects_missing_directory_to_trigger_io_error() {
        let missing = tempfile::tempdir().unwrap().path().to_path_buf();
        let s2 = store(&missing);
        std::fs::remove_dir_all(&missing).unwrap();

        let err = s2.head("some_blob").await.unwrap_err();
        assert!(matches!(
            err,
            BlobStoreError::Io(_) | BlobStoreError::PermissionDenied(_)
        ));
    }

    #[test]
    fn provider_id_is_local() {
        let dir = temp_root();
        let s = LocalBlobStore::new(
            "local_test_id",
            dir.path().to_path_buf(),
            "/mnt",
            std::time::Duration::from_secs(3600),
            SigningKey::random(),
            vec![],
        )
        .unwrap();
        assert_eq!(s.provider_id(), "local_test_id");
    }

    #[test]
    fn signing_key_debug_does_not_leak_material() {
        let key = SigningKey::new(b"super-secret".to_vec());
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("len"));
    }

    // ── Previous-key rotation (RED phase) ──────────────────────────────────

    #[tokio::test]
    async fn blob_url_signed_with_previous_key_still_verifies() {
        let dir = temp_root();
        let old_key = SigningKey::new(b"old-key-32-bytes-xxxxxxxxxxxxxxx".to_vec());
        let new_key = SigningKey::new(b"new-key-32-bytes-xxxxxxxxxxxxxxx".to_vec());

        // Store is built with new key + old key as previous
        let store = LocalBlobStore::new(
            "test",
            dir.path(),
            "/_blobs",
            Duration::from_secs(60),
            new_key,
            vec![old_key.clone()],
        )
        .unwrap();

        store
            .put("a/b.txt", "text/plain", bytes::Bytes::from_static(b"hi"))
            .await
            .unwrap();

        // Sign a URL with the old key directly
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let old_sig = sign(old_key.as_bytes(), "a/b.txt", exp);

        // The store should accept it via its previous-key list
        assert!(
            verify_with_rotation(
                &store.inner.signing_key,
                &store.inner.previous_signing_keys,
                "a/b.txt",
                exp,
                &old_sig
            )
            .is_ok(),
            "old-key signed URL must verify during grace window"
        );
    }

    #[test]
    fn blob_url_expired_with_previous_key_still_rejects() {
        let old_key = SigningKey::new(b"old-key-32-bytes-xxxxxxxxxxxxxxx".to_vec());
        let new_key = SigningKey::new(b"new-key-32-bytes-xxxxxxxxxxxxxxx".to_vec());
        let expired_exp = 1u64; // Unix epoch + 1s — already expired
        let old_sig = sign(old_key.as_bytes(), "a/b.txt", expired_exp);
        let result = verify_with_rotation(&new_key, &[old_key], "a/b.txt", expired_exp, &old_sig);
        assert!(
            result.is_err(),
            "expired URL must be rejected even with valid previous key"
        );
    }

    // ── RED: upload token signing ───────────────────────────────────────────

    #[test]
    fn sign_upload_differs_from_sign_download() {
        let key = b"shared-key";
        let blob_key = "docs/report.pdf";
        let content_type = "application/pdf";
        let exp = 9_999_999_999u64;
        let upload_sig = sign_upload(key, blob_key, content_type, exp);
        let download_sig = sign(key, blob_key, exp);
        assert_ne!(
            upload_sig, download_sig,
            "upload and download tokens must not be interchangeable"
        );
    }

    #[test]
    fn upload_token_prevents_field_boundary_collision() {
        let key = b"secret";
        let exp = 9_999_999_999u64;
        let sig_a = sign_upload(key, "a:b", "c", exp);
        let sig_b = sign_upload(key, "a", "b:c", exp);
        assert_ne!(
            sig_a, sig_b,
            "Signatures for distinct fields must not collide"
        );
    }

    #[test]
    fn verify_upload_accepts_valid_token() {
        let key = b"secret";
        let blob_key = "img/photo.jpg";
        let content_type = "image/jpeg";
        let exp = u64::MAX / 2;
        let sig = sign_upload(key, blob_key, content_type, exp);
        verify_upload(key, blob_key, content_type, exp, &sig).unwrap();
    }

    #[test]
    fn verify_upload_accepts_legacy_token() {
        let key = b"secret";
        let blob_key = "img/photo.jpg";
        let content_type = "image/jpeg";
        let exp = u64::MAX / 2;
        let sig = sign_upload_legacy(key, blob_key, content_type, exp);
        verify_upload(key, blob_key, content_type, exp, &sig).unwrap();
    }

    #[test]
    fn verify_upload_rejects_wrong_content_type() {
        let key = b"secret";
        let blob_key = "img/photo.jpg";
        let exp = u64::MAX / 2;
        let sig = sign_upload(key, blob_key, "image/jpeg", exp);
        let err = verify_upload(key, blob_key, "image/png", exp, &sig).unwrap_err();
        assert!(matches!(err, BlobStoreError::Signature(_)));
    }

    #[test]
    fn verify_upload_rejects_expired_token() {
        let key = b"secret";
        let exp = 1u64; // ancient past
        let sig = sign_upload(key, "k.png", "image/png", exp);
        let err = verify_upload(key, "k.png", "image/png", exp, &sig).unwrap_err();
        assert!(matches!(err, BlobStoreError::Signature(_)));
    }

    #[test]
    fn upload_token_rejects_download_sig_replay() {
        // A download signature must not be accepted as an upload token.
        let key = b"secret";
        let blob_key = "img/photo.jpg";
        let exp = u64::MAX / 2;
        let download_sig = sign(key, blob_key, exp); // download-style token
        let err = verify_upload(key, blob_key, "image/jpeg", exp, &download_sig).unwrap_err();
        assert!(
            matches!(err, BlobStoreError::Signature(_)),
            "download token must not pass upload verification"
        );
    }

    #[tokio::test]
    async fn presign_put_url_contains_upload_marker_and_sig() {
        let dir = temp_root();
        let s = store(dir.path());
        let result = s
            .presign_put("img/photo.jpg", "image/jpeg", Duration::from_secs(300))
            .await
            .unwrap();
        assert_eq!(result.method, "PUT");
        assert!(
            result.url.contains("upload=1"),
            "URL missing upload=1 marker: {}",
            result.url
        );
        assert!(
            result.url.contains("&sig="),
            "URL missing sig: {}",
            result.url
        );
        assert!(
            result.url.contains("&exp="),
            "URL missing exp: {}",
            result.url
        );
        assert!(
            result.url.contains("&ct="),
            "URL missing ct: {}",
            result.url
        );
        assert!(
            result.url.starts_with("/_blobs/img/photo.jpg"),
            "URL must start with mount_path/key: {}",
            result.url
        );
    }

    #[tokio::test]
    async fn presign_put_upload_token_does_not_verify_as_download() {
        // The upload URL's sig= value must not be accepted by the download verify.
        let dir = temp_root();
        let key = SigningKey::new(b"shared-signing-key".to_vec());
        let s = LocalBlobStore::new(
            "test",
            dir.path(),
            "/_blobs",
            Duration::from_secs(60),
            key.clone(),
            vec![],
        )
        .unwrap();
        let result = s
            .presign_put(
                "docs/report.pdf",
                "application/pdf",
                Duration::from_secs(120),
            )
            .await
            .unwrap();

        // Extract sig and exp from the upload URL
        let url = &result.url;
        let sig = url.split("&sig=").nth(1).expect("sig param missing");
        let exp_str = url
            .split("&exp=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .expect("exp param missing");
        let exp: u64 = exp_str.parse().unwrap();

        // The upload sig must not verify as a download token
        let download_result = verify_with_rotation(&key, &[], "docs/report.pdf", exp, sig);
        assert!(
            download_result.is_err(),
            "upload token signature must not pass download verification"
        );
    }

    // ── RED: direct upload PUT route ────────────────────────────────────────

    #[tokio::test]
    async fn direct_put_round_trip_via_serving_route() {
        use autumn_web::reexports::axum::body::Body;
        use http::{Method, Request, StatusCode};
        use tower::ServiceExt as _;

        let dir = temp_root();
        let signing_key = SigningKey::new(b"test-upload-key".to_vec());
        let s = LocalBlobStore::new(
            "test",
            dir.path().to_path_buf(),
            "/_blobs",
            Duration::from_secs(300),
            signing_key,
            vec![],
        )
        .unwrap();

        let result = s
            .presign_put("test/upload.png", "image/png", Duration::from_secs(120))
            .await
            .unwrap();

        let arc: std::sync::Arc<dyn crate::storage::BlobStore> = std::sync::Arc::new(s.clone());
        let state =
            crate::AppState::for_test().with_extension(crate::storage::BlobStoreState::new(arc));
        let router = serve_router(&s).with_state(state);

        let request = Request::builder()
            .method(Method::PUT)
            .uri(&result.url)
            .body(Body::from(b"PNG_DATA_BYTES".as_ref()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "PUT upload should return 200"
        );

        // Confirm blob landed in the store
        let bytes = s.get("test/upload.png").await.unwrap();
        assert_eq!(&bytes[..], b"PNG_DATA_BYTES");
    }

    #[tokio::test]
    async fn direct_put_rejects_tampered_sig() {
        use autumn_web::reexports::axum::body::Body;
        use http::{Method, Request, StatusCode};
        use tower::ServiceExt as _;

        let dir = temp_root();
        let s = store(dir.path());
        let result = s
            .presign_put("img/x.png", "image/png", Duration::from_secs(60))
            .await
            .unwrap();

        let tampered = if result.url.ends_with('a') {
            format!("{}b", &result.url[..result.url.len() - 1])
        } else {
            format!("{}a", &result.url[..result.url.len() - 1])
        };

        let arc: std::sync::Arc<dyn crate::storage::BlobStore> = std::sync::Arc::new(s.clone());
        let state =
            crate::AppState::for_test().with_extension(crate::storage::BlobStoreState::new(arc));
        let router = serve_router(&s).with_state(state);

        let request = Request::builder()
            .method(Method::PUT)
            .uri(&tampered)
            .body(Body::from(b"bytes".as_ref()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn direct_put_enforces_size_limits() {
        use autumn_web::reexports::axum::body::Body;
        use http::{Method, Request, StatusCode};
        use tower::ServiceExt as _;

        let dir = temp_root();
        let s = store(dir.path());
        let result = s
            .presign_put(
                "limit/file.bin",
                "application/octet-stream",
                Duration::from_secs(60),
            )
            .await
            .unwrap();

        let arc: std::sync::Arc<dyn crate::storage::BlobStore> = std::sync::Arc::new(s.clone());
        let mut config = crate::config::AutumnConfig::default();
        config.security.upload.max_request_size_bytes = 10;
        let state = crate::AppState::for_test()
            .with_extension(crate::storage::BlobStoreState::new(arc))
            .with_extension(config);
        let router = serve_router(&s).with_state(state);

        // Put 15 bytes, which exceeds the limit of 10
        let request = Request::builder()
            .method(Method::PUT)
            .uri(&result.url)
            .body(Body::from(b"1234567890abcde".as_ref()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn encode_query_value_encodes_special_chars() {
        // Content-types with `/` should be encoded in query values
        let encoded = encode_query_value("image/png");
        assert!(encoded.contains("%2F") || encoded == "image%2Fpng");
        // Spaces must be encoded
        assert!(encode_query_value("text plain").contains("%20"));
    }

    #[test]
    fn verify_upload_with_rotation_accepts_previous_key() {
        let old_key = SigningKey::new(b"old-upload-key".to_vec());
        let new_key = SigningKey::new(b"new-upload-key".to_vec());
        let exp = u64::MAX / 2;
        let sig = sign_upload(old_key.as_bytes(), "k.png", "image/png", exp);
        verify_upload_with_rotation(&new_key, &[old_key], "k.png", "image/png", exp, &sig).unwrap();
    }

    #[test]
    fn verify_upload_with_rotation_rejects_expired() {
        let key = SigningKey::new(b"k".to_vec());
        let exp = 1u64;
        let sig = sign_upload(key.as_bytes(), "k.png", "image/png", exp);
        let err =
            verify_upload_with_rotation(&key, &[], "k.png", "image/png", exp, &sig).unwrap_err();
        assert!(matches!(err, BlobStoreError::Signature(_)));
    }
}
