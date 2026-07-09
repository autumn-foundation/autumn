//! On-demand image variants for stored blobs.
//!
//! Variants are lazily generated on first request, stored content-addressably
//! in the same [`BlobStore`] as the source, and served with a strong `ETag` and
//! far-future `Cache-Control: immutable` header from that point on.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use autumn_web::storage::{BlobStoreState, variant::{Transform, VariantBudget}};
//!
//! // In your route handler:
//! let store = blobs.store();
//! let budget = VariantBudget::default();
//! let handle = user.avatar.variant("thumb", &[Transform::resize_to_limit(200, 200)]);
//! let url = handle.url(&**store, &budget, Duration::from_secs(3600)).await?;
//! ```
//!
//! ## Content addressing
//!
//! The variant key is
//! `SHA-256(source_key + NUL + etag + NUL + content_type + NUL + JSON(transforms))`
//! encoded under `_variants/{h0}{h1}/{h2}{h3}/{hash}`.  The same
//! `(source content, transforms)` pair always maps to the same key; re-uploading
//! to the same storage key (new `ETag`) or correcting a mis-tagged MIME type
//! both produce a fresh variant key so stale thumbnails are never served.
//!
//! ## Budget
//!
//! Configurable via `[storage.variants]` in `autumn.toml`; the [`VariantBudget`]
//! default caps source blobs at 20 MiB and 10 000 × 10 000 px to prevent
//! runaway memory allocation.

use std::io::Cursor;
use std::time::Duration;

use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::{Blob, BlobStore, BlobStoreError};

// ── Public types ─────────────────────────────────────────────────────────────

/// A single transform applied to the source image.
///
/// Transforms are applied in order and serialised to JSON for content
/// addressing — the order matters for both the visual output and the
/// cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Transform {
    /// Resize to fit within `width × height`, preserving aspect ratio.
    /// Never upscales: an image smaller than the limit is stored as-is.
    ResizeToLimit {
        /// Maximum output width in pixels.
        width: u32,
        /// Maximum output height in pixels.
        height: u32,
    },
    /// Resize and crop to exactly `width × height`, centred.
    ResizeToFill {
        /// Exact output width in pixels.
        width: u32,
        /// Exact output height in pixels.
        height: u32,
    },
    /// Rotate clockwise by the given number of degrees.
    /// Values outside {90, 180, 270} are treated as no-op rotations.
    Rotate {
        /// Clockwise rotation in degrees (0, 90, 180, or 270).
        degrees: u16,
    },
    /// Strip all embedded metadata (EXIF, GPS, ICC profiles, comment chunks).
    ///
    /// Re-encoding through the `image` crate already drops EXIF data; this
    /// variant is a no-op on pixel data but explicitly documents the intent so
    /// the transform spec is unambiguous.
    StripMetadata,
}

impl Transform {
    /// Shorthand constructor for [`Transform::ResizeToLimit`].
    #[must_use]
    pub const fn resize_to_limit(width: u32, height: u32) -> Self {
        Self::ResizeToLimit { width, height }
    }

    /// Shorthand constructor for [`Transform::ResizeToFill`].
    #[must_use]
    pub const fn resize_to_fill(width: u32, height: u32) -> Self {
        Self::ResizeToFill { width, height }
    }

    /// Shorthand constructor for [`Transform::Rotate`].
    #[must_use]
    pub const fn rotate(degrees: u16) -> Self {
        Self::Rotate { degrees }
    }

    /// Shorthand constructor for [`Transform::StripMetadata`].
    #[must_use]
    pub const fn strip_metadata() -> Self {
        Self::StripMetadata
    }
}

/// Resource limits for variant generation.
///
/// Set via `[storage.variants]` in `autumn.toml`; the defaults are
/// deliberately conservative to prevent runaway memory allocation from
/// pathologically large source images.
#[derive(Debug, Clone)]
pub struct VariantBudget {
    /// Maximum byte size of the source blob. Default: 20 MiB.
    pub max_source_bytes: u64,
    /// Maximum pixel width of the source image. Default: 10 000.
    pub max_source_width: u32,
    /// Maximum pixel height of the source image. Default: 10 000.
    pub max_source_height: u32,
}

impl Default for VariantBudget {
    fn default() -> Self {
        Self {
            max_source_bytes: 20 * 1024 * 1024, // 20 MiB
            max_source_width: 10_000,
            max_source_height: 10_000,
        }
    }
}

/// Errors returned by variant operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VariantError {
    /// The source blob's content type is not a supported image format.
    /// Only `image/jpeg`, `image/png`, and `image/webp` are accepted.
    #[error("unsupported MIME type for image variant: {0}")]
    UnsupportedMimeType(String),

    /// The source blob exceeds [`VariantBudget::max_source_bytes`].
    #[error(
        "source blob too large for variant generation: {byte_size} bytes \
         (budget: {max_bytes} bytes)"
    )]
    SourceTooLarge { byte_size: u64, max_bytes: u64 },

    /// The decoded source image exceeds the pixel budget.
    #[error(
        "source image dimensions too large: {width}×{height} px \
         (budget: {max_width}×{max_height} px)"
    )]
    SourceDimensionsTooLarge {
        /// Actual source width.
        width: u32,
        /// Actual source height.
        height: u32,
        /// Configured maximum width.
        max_width: u32,
        /// Configured maximum height.
        max_height: u32,
    },

    /// The source bytes could not be decoded as the claimed image format.
    #[error("image decode/encode error: {0}")]
    DecodeError(String),

    /// A storage operation failed while checking for or writing the variant.
    #[error(transparent)]
    Storage(#[from] BlobStoreError),
}

impl VariantError {
    /// HTTP status code that best represents this error.
    ///
    /// Routes using `?` on [`VariantHandle::url`] or
    /// [`VariantHandle::ensure_generated`] get 500 by default via the blanket
    /// `impl From<E: Error> for AutumnError`; call
    /// [`VariantError::into_autumn_error`] instead to preserve the precise
    /// status.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            Self::UnsupportedMimeType(_) | Self::DecodeError(_) => {
                http::StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::SourceTooLarge { .. } | Self::SourceDimensionsTooLarge { .. } => {
                http::StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::Storage(e) => e.status(),
        }
    }

    /// Promote into an [`AutumnError`](crate::AutumnError) carrying the
    /// status from [`VariantError::status`].
    ///
    /// Use this instead of `?` when the route needs accurate HTTP status codes
    /// for variant failures (e.g. 422 for a corrupt image, 413 for an
    /// oversized source):
    ///
    /// ```rust,ignore
    /// let url = handle.url(&*store, &budget, expires)
    ///     .await
    ///     .map_err(VariantError::into_autumn_error)?;
    /// ```
    #[must_use]
    pub fn into_autumn_error(self) -> crate::AutumnError {
        let status = self.status();
        crate::AutumnError::internal_server_error(self).with_status(status)
    }
}

/// Handle for a named, lazily-generated image variant.
///
/// Created by [`Blob::variant`]; call [`VariantHandle::url`] to generate
/// the variant on first access and obtain a presigned serving URL.
#[derive(Debug, Clone)]
pub struct VariantHandle {
    source: Blob,
    name: String,
    transforms: Vec<Transform>,
    /// Content-addressed storage key for this variant.
    variant_key: String,
}

impl VariantHandle {
    /// The content-addressed key under which this variant is stored.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.variant_key
    }

    /// The human-readable label for this variant (does not affect caching).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The transforms applied to the source image.
    #[must_use]
    pub fn transforms(&self) -> &[Transform] {
        &self.transforms
    }

    /// Ensure the variant is generated, then return a presigned serving URL.
    ///
    /// On the first call the source image is fetched, transformed, and
    /// persisted under the content-addressed key.  Subsequent calls skip
    /// generation (`head` cache hit) and call [`BlobStore::presigned_url`]
    /// directly.
    ///
    /// The returned URL is identical in form to any other presigned URL from
    /// the backend: a route-served HMAC-signed URL for the Local backend and
    /// a real S3 presigned URL for S3 backends.
    ///
    /// # Errors
    ///
    /// Returns [`VariantError`] when the source MIME type is unsupported,
    /// the source exceeds the budget, decoding fails, or a storage operation
    /// fails.
    pub async fn url(
        &self,
        store: &dyn BlobStore,
        budget: &VariantBudget,
        expires_in: Duration,
    ) -> Result<String, VariantError> {
        self.ensure_generated(store, budget).await?;
        Ok(store.presigned_url(&self.variant_key, expires_in).await?)
    }

    /// Ensure the variant blob exists in the store, returning its handle.
    ///
    /// Idempotent: if the variant is already cached this is a single `head`
    /// call with no image processing.
    ///
    /// # Errors
    ///
    /// Returns [`VariantError`] on generation or storage failure.
    pub async fn ensure_generated(
        &self,
        store: &dyn BlobStore,
        budget: &VariantBudget,
    ) -> Result<Blob, VariantError> {
        // Fast path: variant already in store.
        if let Some(meta) = store.head(&self.variant_key).await? {
            return Ok(Blob {
                provider_id: store.provider_id().to_owned(),
                key: self.variant_key.clone(),
                content_type: meta.content_type,
                byte_size: meta.byte_size,
                etag: meta.etag,
            });
        }
        self.generate(store, budget).await
    }

    /// Fetch the source, decode, transform, encode, and persist.
    async fn generate(
        &self,
        store: &dyn BlobStore,
        budget: &VariantBudget,
    ) -> Result<Blob, VariantError> {
        // Guard byte size before we even fetch — avoids streaming a huge blob
        // into memory just to reject it.
        if self.source.byte_size > budget.max_source_bytes {
            return Err(VariantError::SourceTooLarge {
                byte_size: self.source.byte_size,
                max_bytes: budget.max_source_bytes,
            });
        }

        // Reject non-image MIME types before touching the store.
        check_image_mime_type(&self.source.content_type)?;

        let source_bytes = store.get(&self.source.key).await?;

        // Secondary guard on the real byte count — the Blob metadata could be
        // stale or crafted; check the actual payload length too.
        if source_bytes.len() as u64 > budget.max_source_bytes {
            return Err(VariantError::SourceTooLarge {
                byte_size: source_bytes.len() as u64,
                max_bytes: budget.max_source_bytes,
            });
        }

        // Offload CPU-bound decode/transform/encode to a blocking thread so
        // Tokio worker threads are not stalled during image processing.
        let transforms = self.transforms.clone();
        let content_type = self.source.content_type.clone();
        let max_width = budget.max_source_width;
        let max_height = budget.max_source_height;

        let (output_bytes, output_content_type) = tokio::task::spawn_blocking(
            move || -> Result<(Vec<u8>, &'static str), VariantError> {
                // Use decoder limits to guard against image bombs: a compressed
                // file under `max_source_bytes` can still expand to gigabytes of
                // pixel data without this guard.
                let mut reader = image::ImageReader::new(Cursor::new(&source_bytes[..]))
                    .with_guessed_format()
                    .map_err(|e| VariantError::DecodeError(e.to_string()))?;
                // image::Limits is #[non_exhaustive]; build via Default + field mutation.
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(max_width);
                limits.max_image_height = Some(max_height);
                reader.limits(limits);
                let img = reader.decode().map_err(|e| match &e {
                    image::ImageError::Limits(_) => VariantError::SourceDimensionsTooLarge {
                        width: 0,
                        height: 0,
                        max_width,
                        max_height,
                    },
                    _ => VariantError::DecodeError(e.to_string()),
                })?;

                // Belt-and-suspenders: if the decoder didn't enforce limits,
                // catch oversized images here where we know the actual size.
                if img.width() > max_width || img.height() > max_height {
                    return Err(VariantError::SourceDimensionsTooLarge {
                        width: img.width(),
                        height: img.height(),
                        max_width,
                        max_height,
                    });
                }

                let transformed = apply_transforms(img, &transforms);
                let (output_format, output_content_type) = output_format_and_mime(&content_type);
                let output_bytes = encode_image(&transformed, output_format)?;
                Ok((output_bytes, output_content_type))
            },
        )
        .await
        .map_err(|e| VariantError::DecodeError(format!("variant worker panicked: {e}")))??;

        let blob = store
            .put(
                &self.variant_key,
                output_content_type,
                Bytes::from(output_bytes),
            )
            .await?;

        Ok(blob)
    }
}

// ── `Blob` extension ─────────────────────────────────────────────────────────

impl Blob {
    /// Create a [`VariantHandle`] for the given `name` and `transforms`.
    ///
    /// The content-addressed storage key is derived from
    /// `SHA-256(source_key || NUL || JSON(transforms))` so identical
    /// transform specs always map to the same cached artifact — the
    /// human-readable `name` is stored on the handle but does not affect
    /// the cache key.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::storage::variant::{Transform, VariantBudget};
    ///
    /// let handle = user.avatar.variant("thumb", &[Transform::resize_to_limit(200, 200)]);
    /// let url = handle.url(&*store, &budget, Duration::from_secs(3600)).await?;
    /// ```
    #[must_use]
    pub fn variant(&self, name: &str, transforms: &[Transform]) -> VariantHandle {
        let key = content_addressed_key(self, transforms);
        VariantHandle {
            source: self.clone(),
            name: name.to_owned(),
            transforms: transforms.to_vec(),
            variant_key: key,
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Return an error when the content type is not a supported image format.
pub(crate) fn check_image_mime_type(content_type: &str) -> Result<(), VariantError> {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if base.eq_ignore_ascii_case("image/jpeg")
        || base.eq_ignore_ascii_case("image/jpg")
        || base.eq_ignore_ascii_case("image/png")
        || base.eq_ignore_ascii_case("image/webp")
    {
        Ok(())
    } else {
        Err(VariantError::UnsupportedMimeType(base.to_owned()))
    }
}

/// Derive the content-addressed storage key for a given source blob and
/// transform spec.
///
/// Format: `_variants/{h[0..2]}/{h[2..4]}/{h}` where `h` is the lowercase
/// hex SHA-256 of
/// `source_key + NUL + etag + NUL + content_type + NUL + JSON(transforms)`.
///
/// Including `etag` ensures re-uploading to the same key invalidates cached
/// variants.  Including `content_type` ensures that correcting a mis-tagged
/// MIME type (PNG stored as `image/webp`) also produces a fresh variant key
/// — the two produce different output formats, so the same bytes should not
/// share a cached result.
pub(crate) fn content_addressed_key(source_blob: &Blob, transforms: &[Transform]) -> String {
    let spec = serde_json::to_string(transforms).expect("Transform is always serialisable");
    let mut hasher = Sha256::new();
    hasher.update(source_blob.key.as_bytes());
    hasher.update(b"\0");
    if let Some(etag) = &source_blob.etag {
        hasher.update(etag.as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(source_blob.content_type.as_bytes());
    hasher.update(b"\0");
    hasher.update(spec.as_bytes());
    let hash = hasher.finalize();
    let hash_hex = hex::encode(hash);
    format!(
        "_variants/{}/{}/{}",
        &hash_hex[..2],
        &hash_hex[2..4],
        hash_hex
    )
}

/// Determine the output image format and its MIME type from the source MIME.
///
/// - JPEG sources → JPEG output (`image/jpeg`)
/// - PNG sources  → PNG output  (`image/png`)
/// - WebP sources → JPEG output (`image/jpeg`) — WebP encode path is kept
///   simple by transcoding; lossless round-trip is not a requirement here
fn output_format_and_mime(content_type: &str) -> (image::ImageFormat, &'static str) {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if base.eq_ignore_ascii_case("image/png") {
        (image::ImageFormat::Png, "image/png")
    } else {
        (image::ImageFormat::Jpeg, "image/jpeg")
    }
}

/// Apply each transform in order to the image.
fn apply_transforms(mut img: image::DynamicImage, transforms: &[Transform]) -> image::DynamicImage {
    use image::imageops::FilterType;

    for transform in transforms {
        img = match transform {
            Transform::ResizeToLimit { width, height } => {
                // Never upscale: if the image already fits, return as-is.
                if img.width() <= *width && img.height() <= *height {
                    img
                } else {
                    img.resize(*width, *height, FilterType::Lanczos3)
                }
            }
            Transform::ResizeToFill { width, height } => {
                img.resize_to_fill(*width, *height, FilterType::Lanczos3)
            }
            Transform::Rotate { degrees } => match degrees % 360 {
                90 => img.rotate90(),
                180 => img.rotate180(),
                270 => img.rotate270(),
                _ => img,
            },
            // Re-encoding through the `image` crate already drops EXIF; this
            // variant documents intent without additional pixel mutation.
            Transform::StripMetadata => img,
        };
    }
    img
}

/// Encode `img` to bytes using `format`.
fn encode_image(
    img: &image::DynamicImage,
    format: image::ImageFormat,
) -> Result<Vec<u8>, VariantError> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, format)
        .map_err(|e| VariantError::DecodeError(e.to_string()))?;
    Ok(buf.into_inner())
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// TDD phases:
//   RED   – test bodies written first; without the implementation above they
//           would not compile / would panic.
//   GREEN – the implementation above makes all tests pass.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local::{LocalBlobStore, SigningKey};
    use std::path::Path;
    use std::time::Duration;

    // ── test helpers ────────────────────────────────────────────────────

    fn test_store(root: &Path) -> LocalBlobStore {
        LocalBlobStore::new(
            "test",
            root.to_path_buf(),
            "/_blobs",
            Duration::from_secs(60),
            SigningKey::new(b"test-variant-key".to_vec()),
            vec![],
        )
        .unwrap()
    }

    /// Generate a solid-colour test image in the given format.
    fn make_test_image(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, format).unwrap();
        buf.into_inner()
    }

    // ── RED: content addressing ──────────────────────────────────────────────

    #[test]
    fn transform_serialisation_is_stable() {
        let t = Transform::resize_to_limit(200, 200);
        let j1 = serde_json::to_string(&t).unwrap();
        let j2 = serde_json::to_string(&t).unwrap();
        assert_eq!(j1, j2, "serialisation must be deterministic");
    }

    #[test]
    fn same_spec_produces_same_key() {
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1024);
        let h1 = blob.variant("thumb", &[Transform::resize_to_limit(200, 200)]);
        let h2 = blob.variant("thumbnail", &[Transform::resize_to_limit(200, 200)]);
        assert_eq!(
            h1.key(),
            h2.key(),
            "different names but same spec → same content-addressed key"
        );
    }

    #[test]
    fn different_specs_produce_different_keys() {
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1024);
        let h1 = blob.variant("thumb", &[Transform::resize_to_limit(200, 200)]);
        let h2 = blob.variant("large", &[Transform::resize_to_limit(400, 400)]);
        assert_ne!(h1.key(), h2.key());
    }

    #[test]
    fn different_sources_produce_different_keys() {
        let b1 = Blob::new("local", "a/1.png", "image/png", 1024);
        let b2 = Blob::new("local", "a/2.png", "image/png", 1024);
        let spec = [Transform::resize_to_limit(200, 200)];
        assert_ne!(b1.variant("t", &spec).key(), b2.variant("t", &spec).key());
    }

    #[test]
    fn same_source_key_different_etag_produces_different_keys() {
        // Simulates a re-upload to the same key (e.g. avatars/{id}.bin):
        // the new ETag must produce a different variant key so the old
        // cached thumbnail is not served for the new content.
        let mut b1 = Blob::new("local", "avatars/1.png", "image/png", 1024);
        b1.etag = Some("sha256-aabbcc".to_owned());
        let mut b2 = Blob::new("local", "avatars/1.png", "image/png", 2048);
        b2.etag = Some("sha256-ddeeff".to_owned());
        let spec = [Transform::resize_to_limit(100, 100)];
        assert_ne!(
            b1.variant("thumb", &spec).key(),
            b2.variant("thumb", &spec).key(),
            "different ETags on the same source key must yield different variant keys"
        );
    }

    #[test]
    fn variant_key_passes_blob_validation() {
        use crate::storage::validate_key;
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1024);
        let key = blob
            .variant("t", &[Transform::resize_to_limit(200, 200)])
            .key()
            .to_owned();
        validate_key(&key).unwrap_or_else(|e| panic!("variant key {key:?} failed validation: {e}"));
    }

    #[test]
    fn variant_key_starts_with_variants_prefix() {
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1024);
        let key = blob
            .variant("t", &[Transform::resize_to_limit(200, 200)])
            .key()
            .to_owned();
        assert!(key.starts_with("_variants/"), "key: {key}");
    }

    // ── RED: MIME-type enforcement ───────────────────────────────────────────

    #[test]
    fn check_image_mime_type_accepts_jpeg() {
        check_image_mime_type("image/jpeg").unwrap();
        check_image_mime_type("image/jpg").unwrap();
        check_image_mime_type("image/jpeg; charset=utf-8").unwrap();
    }

    #[test]
    fn check_image_mime_type_accepts_png() {
        check_image_mime_type("image/png").unwrap();
    }

    #[test]
    fn check_image_mime_type_accepts_webp() {
        check_image_mime_type("image/webp").unwrap();
    }

    #[test]
    fn check_image_mime_type_rejects_pdf() {
        let err = check_image_mime_type("application/pdf").unwrap_err();
        assert!(matches!(err, VariantError::UnsupportedMimeType(_)));
    }

    #[test]
    fn check_image_mime_type_rejects_video() {
        let err = check_image_mime_type("video/mp4").unwrap_err();
        assert!(matches!(err, VariantError::UnsupportedMimeType(_)));
    }

    #[test]
    fn check_image_mime_type_rejects_plaintext() {
        let err = check_image_mime_type("text/plain").unwrap_err();
        assert!(matches!(err, VariantError::UnsupportedMimeType(_)));
    }

    #[test]
    fn check_image_mime_type_rejects_octet_stream() {
        let err = check_image_mime_type("application/octet-stream").unwrap_err();
        assert!(matches!(err, VariantError::UnsupportedMimeType(_)));
    }

    // ── RED: budget enforcement ──────────────────────────────────────────────

    #[tokio::test]
    async fn variant_rejects_non_image_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        store
            .put(
                "doc.pdf",
                "application/pdf",
                Bytes::from_static(b"%PDF-1.4"),
            )
            .await
            .unwrap();
        let blob = Blob::new("test", "doc.pdf", "application/pdf", 8);
        let handle = blob.variant("thumb", &[Transform::resize_to_limit(200, 200)]);
        let err = handle
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, VariantError::UnsupportedMimeType(_)),
            "expected UnsupportedMimeType, got {err:?}"
        );
    }

    #[tokio::test]
    async fn variant_rejects_oversized_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(4, 4, image::ImageFormat::Jpeg);
        let actual_size = jpeg.len() as u64;
        store
            .put("img.jpg", "image/jpeg", Bytes::from(jpeg))
            .await
            .unwrap();
        // Budget smaller than the actual stored blob.
        let budget = VariantBudget {
            max_source_bytes: actual_size - 1,
            ..Default::default()
        };
        // Lie about byte_size in the Blob so the pre-fetch check fires.
        let blob = Blob::new("test", "img.jpg", "image/jpeg", actual_size);
        let err = blob
            .variant("t", &[Transform::resize_to_limit(2, 2)])
            .ensure_generated(&store, &budget)
            .await
            .unwrap_err();
        assert!(
            matches!(err, VariantError::SourceTooLarge { .. }),
            "expected SourceTooLarge, got {err:?}"
        );
    }

    #[tokio::test]
    async fn variant_rejects_oversized_source_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // 4×4 image.
        let jpeg = make_test_image(4, 4, image::ImageFormat::Jpeg);
        store
            .put("big.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let budget = VariantBudget {
            max_source_width: 3, // less than 4
            max_source_height: 3,
            max_source_bytes: jpeg.len() as u64 * 10, // byte budget is fine
        };
        let blob = Blob::new("test", "big.jpg", "image/jpeg", jpeg.len() as u64);
        let err = blob
            .variant("t", &[Transform::resize_to_limit(2, 2)])
            .ensure_generated(&store, &budget)
            .await
            .unwrap_err();
        assert!(
            matches!(err, VariantError::SourceDimensionsTooLarge { .. }),
            "expected SourceDimensionsTooLarge, got {err:?}"
        );
    }

    // ── RED: generation and caching ──────────────────────────────────────────

    #[tokio::test]
    async fn variant_generates_on_first_call() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(100, 100, image::ImageFormat::Jpeg);
        store
            .put("photo.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "photo.jpg", "image/jpeg", jpeg.len() as u64);
        let handle = blob.variant("thumb", &[Transform::resize_to_limit(50, 50)]);

        let variant_blob = handle
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();
        assert_eq!(variant_blob.key, handle.key());
        assert!(variant_blob.byte_size > 0);
        assert_eq!(variant_blob.content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn variant_is_idempotent_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(60, 60, image::ImageFormat::Jpeg);
        store
            .put("avatar.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "avatar.jpg", "image/jpeg", jpeg.len() as u64);
        let handle = blob.variant("thumb", &[Transform::resize_to_limit(30, 30)]);
        let budget = VariantBudget::default();

        let first = handle.ensure_generated(&store, &budget).await.unwrap();
        let second = handle.ensure_generated(&store, &budget).await.unwrap();
        assert_eq!(first.key, second.key);
        assert_eq!(first.byte_size, second.byte_size);
    }

    // ── RED: transforms ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn resize_to_limit_respects_max_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(100, 80, image::ImageFormat::Jpeg);
        store
            .put("img.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "img.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("t", &[Transform::resize_to_limit(50, 50)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(
                blob.variant("t", &[Transform::resize_to_limit(50, 50)])
                    .key(),
            )
            .await
            .unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert!(
            out_img.width() <= 50 && out_img.height() <= 50,
            "expected ≤50×50, got {}×{}",
            out_img.width(),
            out_img.height()
        );
        assert!(out_img.width() > 0 && out_img.height() > 0);
    }

    #[tokio::test]
    async fn resize_to_limit_does_not_upscale() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // Source is already smaller than the limit.
        let jpeg = make_test_image(20, 20, image::ImageFormat::Jpeg);
        store
            .put("small.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "small.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("large", &[Transform::resize_to_limit(200, 200)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(
                blob.variant("large", &[Transform::resize_to_limit(200, 200)])
                    .key(),
            )
            .await
            .unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert!(
            out_img.width() <= 20 && out_img.height() <= 20,
            "must not upscale: got {}×{}",
            out_img.width(),
            out_img.height()
        );
    }

    #[tokio::test]
    async fn resize_to_fill_produces_exact_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(100, 150, image::ImageFormat::Jpeg);
        store
            .put("portrait.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "portrait.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("square", &[Transform::resize_to_fill(50, 50)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(
                blob.variant("square", &[Transform::resize_to_fill(50, 50)])
                    .key(),
            )
            .await
            .unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert_eq!(
            (out_img.width(), out_img.height()),
            (50, 50),
            "resize_to_fill must produce exact dimensions"
        );
    }

    #[tokio::test]
    async fn rotate_90_swaps_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // 100×50 landscape image.
        let jpeg = make_test_image(100, 50, image::ImageFormat::Jpeg);
        store
            .put("land.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "land.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("rotated", &[Transform::rotate(90)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(blob.variant("rotated", &[Transform::rotate(90)]).key())
            .await
            .unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert_eq!(
            (out_img.width(), out_img.height()),
            (50, 100),
            "90° rotation must swap width and height"
        );
    }

    #[tokio::test]
    async fn rotate_180_preserves_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(100, 60, image::ImageFormat::Jpeg);
        store
            .put("img.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "img.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("r180", &[Transform::rotate(180)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(blob.variant("r180", &[Transform::rotate(180)]).key())
            .await
            .unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert_eq!(
            (out_img.width(), out_img.height()),
            (100, 60),
            "180° rotation must preserve dimensions"
        );
    }

    #[tokio::test]
    async fn strip_metadata_produces_valid_image() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(10, 10, image::ImageFormat::Jpeg);
        store
            .put("meta.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "meta.jpg", "image/jpeg", jpeg.len() as u64);
        blob.variant("stripped", &[Transform::strip_metadata()])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();

        let out_bytes = store
            .get(
                blob.variant("stripped", &[Transform::strip_metadata()])
                    .key(),
            )
            .await
            .unwrap();
        assert!(
            image::load_from_memory(&out_bytes).is_ok(),
            "StripMetadata output must be a valid image"
        );
    }

    // ── RED: PNG source ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn png_source_is_processed_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let png = make_test_image(40, 40, image::ImageFormat::Png);
        store
            .put("icon.png", "image/png", Bytes::from(png.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "icon.png", "image/png", png.len() as u64);
        let variant_blob = blob
            .variant("small", &[Transform::resize_to_limit(20, 20)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();
        assert_eq!(variant_blob.content_type, "image/png");

        let out_bytes = store.get(&variant_blob.key).await.unwrap();
        let out_img = image::load_from_memory(&out_bytes).unwrap();
        assert!(
            out_img.width() <= 20 && out_img.height() <= 20,
            "PNG variant dimensions: {}×{}",
            out_img.width(),
            out_img.height()
        );
    }

    // ── RED: WebP source ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn webp_source_is_processed_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // Encode the source as WebP.
        let webp = make_test_image(40, 40, image::ImageFormat::WebP);
        store
            .put("photo.webp", "image/webp", Bytes::from(webp.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "photo.webp", "image/webp", webp.len() as u64);
        let variant_blob = blob
            .variant("thumb", &[Transform::resize_to_limit(20, 20)])
            .ensure_generated(&store, &VariantBudget::default())
            .await
            .unwrap();
        // WebP sources are transcoded to JPEG.
        assert_eq!(
            variant_blob.content_type, "image/jpeg",
            "WebP source must produce JPEG variant"
        );
        let out_bytes = store.get(&variant_blob.key).await.unwrap();
        assert!(image::load_from_memory(&out_bytes).is_ok());
    }

    // ── RED: URL helper ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn variant_url_returns_presigned_url() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let jpeg = make_test_image(100, 100, image::ImageFormat::Jpeg);
        store
            .put("photo.jpg", "image/jpeg", Bytes::from(jpeg.clone()))
            .await
            .unwrap();
        let blob = Blob::new("test", "photo.jpg", "image/jpeg", jpeg.len() as u64);
        let url = blob
            .variant("thumb", &[Transform::resize_to_limit(50, 50)])
            .url(&store, &VariantBudget::default(), Duration::from_secs(300))
            .await
            .unwrap();
        assert!(
            url.contains("_variants/"),
            "URL must contain variant key: {url}"
        );
        assert!(url.contains("exp="), "URL must contain expiry param: {url}");
        assert!(url.contains("sig="), "URL must contain signature: {url}");
    }

    // ── RED: handle accessors ────────────────────────────────────────────────

    #[test]
    fn handle_name_accessor() {
        let blob = Blob::new("local", "a.png", "image/png", 1);
        let h = blob.variant("thumbnail", &[Transform::resize_to_limit(200, 200)]);
        assert_eq!(h.name(), "thumbnail");
    }

    #[test]
    fn handle_transforms_accessor() {
        let transforms = vec![
            Transform::resize_to_limit(200, 200),
            Transform::strip_metadata(),
        ];
        let blob = Blob::new("local", "a.png", "image/png", 1);
        let h = blob.variant("t", &transforms);
        assert_eq!(h.transforms(), &transforms);
    }

    // ── RED: content_addressed_key internals ─────────────────────────────────

    #[test]
    fn content_addressed_key_hex_is_64_chars() {
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1024);
        let key = content_addressed_key(&blob, &[Transform::resize_to_limit(200, 200)]);
        // format: _variants/xx/xx/<64-hex-chars>
        let hash_part = key.rsplit('/').next().unwrap();
        assert_eq!(hash_part.len(), 64, "hash part must be 64 hex chars");
    }

    #[test]
    fn order_of_transforms_matters_for_content_addressing() {
        let blob = Blob::new("local", "a.png", "image/png", 1);
        let t1 = [Transform::resize_to_limit(200, 200), Transform::rotate(90)];
        let t2 = [Transform::rotate(90), Transform::resize_to_limit(200, 200)];
        assert_ne!(
            blob.variant("x", &t1).key(),
            blob.variant("x", &t2).key(),
            "transform order must affect the content-addressed key"
        );
    }

    // ── status() and into_autumn_error() ────────────────────────────────────

    #[test]
    fn variant_error_status_unsupported_mime_is_422() {
        assert_eq!(
            VariantError::UnsupportedMimeType("image/bmp".into()).status(),
            http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn variant_error_status_decode_error_is_422() {
        assert_eq!(
            VariantError::DecodeError("bad pixels".into()).status(),
            http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn variant_error_status_source_too_large_is_413() {
        assert_eq!(
            VariantError::SourceTooLarge {
                byte_size: 999,
                max_bytes: 100
            }
            .status(),
            http::StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn variant_error_status_dimensions_too_large_is_413() {
        assert_eq!(
            VariantError::SourceDimensionsTooLarge {
                width: 200,
                height: 200,
                max_width: 100,
                max_height: 100
            }
            .status(),
            http::StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn variant_error_status_storage_delegates_to_blob_store_error() {
        use crate::storage::BlobStoreError;
        assert_eq!(
            VariantError::Storage(BlobStoreError::NotFound("k".into())).status(),
            http::StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn variant_error_into_autumn_error_carries_correct_status() {
        let err = VariantError::SourceTooLarge {
            byte_size: 999,
            max_bytes: 100,
        }
        .into_autumn_error();
        assert_eq!(err.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    }
}
