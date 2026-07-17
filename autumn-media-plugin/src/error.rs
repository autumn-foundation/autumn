//! Shared error type for the media plugin's storage (and, in later slices,
//! encode/transport) surfaces.
//!
//! AWS SDK error types are deliberately **stringified** into the `message`
//! fields here rather than surfaced directly, so the public API never leaks an
//! `aws-sdk-s3` type — and, by construction, never a credential (the SDK's
//! `Display` for these errors carries only request/response metadata, and the
//! signing key never appears in it).

use thiserror::Error;

/// Errors returned by the media plugin's storage operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MediaError {
    /// The S3 backend was selected without a bucket name.
    #[error("media.storage.bucket is required when backend = \"s3\"")]
    MissingBucket,

    /// Exactly one of `access_key_id` / `secret_access_key` was configured for
    /// the S3 backend. Both must be provided together, or neither (to use the
    /// ambient AWS credential chain).
    #[error(
        "partial S3 credentials: media.storage.access_key_id and \
         media.storage.secret_access_key must both be set, or neither"
    )]
    PartialS3Credentials,

    /// A caller-supplied storage key contained a dot-only (`.`/`..`) or empty
    /// path segment.
    ///
    /// Percent-encoding such a segment is not enough: WHATWG URL parsers
    /// normalize `%2E` back to `.`, so the advertised public URL could
    /// path-normalize down to a *different* object than the one stored. The key
    /// is refused at the storage boundary instead. `key` is a caller object key
    /// (not a secret — it already appears in public URLs and the S3 error
    /// variants).
    #[error("invalid storage key `{key}`: path segments must not be `.`, `..`, or empty")]
    InvalidKeySegment {
        /// The offending caller-relative key.
        key: String,
    },

    /// A local file staged for persistence could not be read.
    #[error("failed to read local file `{path}`: {source}")]
    LocalRead {
        /// The local filesystem path that could not be read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An S3 object upload failed. `message` is the stringified SDK error.
    #[error("failed to upload s3://{bucket}/{key}: {message}")]
    S3Upload {
        /// Target bucket.
        bucket: String,
        /// Target object key.
        key: String,
        /// Stringified SDK error (no AWS type, no credentials).
        message: String,
    },

    /// An S3 object delete failed. `message` is the stringified SDK error.
    #[error("failed to delete s3://{bucket}/{key}: {message}")]
    S3Delete {
        /// Target bucket.
        bucket: String,
        /// Target object key.
        key: String,
        /// Stringified SDK error (no AWS type, no credentials).
        message: String,
    },

    /// A source recording an encode command needed was not present on disk.
    #[error("ffmpeg source recording does not exist: {path}")]
    FfmpegSourceMissing {
        /// The recording-segment path that was missing.
        path: String,
    },

    /// The `FFmpeg` child process could not be started.
    #[error("failed to start ffmpeg `{bin}`: {source}")]
    FfmpegSpawn {
        /// The configured `FFmpeg` binary path.
        bin: String,
        /// The underlying spawn/I-O error.
        #[source]
        source: std::io::Error,
    },

    /// The `FFmpeg` child process exited with a non-zero status.
    ///
    /// `stderr_tail` is bounded to the last ~2 KiB of `FFmpeg`'s stderr (see
    /// [`stderr_tail`]) so a broken or hostile encoder cannot flood logs, and
    /// only `FFmpeg` diagnostic output is carried — never the input URL beyond
    /// what `FFmpeg` itself echoes.
    #[error("ffmpeg exited with status {code}: {stderr_tail}")]
    FfmpegNonZeroExit {
        /// Exit code as a string, or `"signal"` when terminated by a signal.
        code: String,
        /// Bounded tail of `FFmpeg`'s stderr.
        stderr_tail: String,
    },

    /// The `FFmpeg` child process exceeded its deadline and was killed.
    #[error("ffmpeg exceeded its deadline")]
    FfmpegTimeout,

    /// The `FFmpeg` concat-list file could not be written.
    #[error("failed to write ffmpeg concat list `{path}`: {source}")]
    ConcatList {
        /// The concat-list (or parent directory) path involved.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An encode command's output directory or file could not be created or
    /// inspected.
    #[error("failed to access ffmpeg output `{path}`: {source}")]
    FfmpegOutputIo {
        /// The output (or parent directory) path involved.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A derived media artifact (e.g. a seek-preview `WebVTT` track) could not
    /// be written to its staging path before persistence.
    #[error("failed to write media artifact `{path}`: {source}")]
    ArtifactWrite {
        /// The artifact (or parent directory) path involved.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The blocking encode task panicked or was cancelled before completing.
    ///
    /// The encode primitives run synchronously on a blocking thread
    /// ([`tokio::task::spawn_blocking`]); a panic there surfaces here rather
    /// than unwinding the async worker.
    #[error("media encode task did not complete: {message}")]
    EncodeTaskJoin {
        /// The stringified join error.
        message: String,
    },
}

/// Maximum number of stderr bytes carried in [`MediaError::FfmpegNonZeroExit`].
const STDERR_TAIL_MAX_BYTES: usize = 2048;

/// Render the last ~2 KiB of an `FFmpeg` stderr stream as a `String`.
///
/// The bytes are first decoded lossily (`FFmpeg` diagnostics are UTF-8 in
/// practice, but a truncated multibyte sequence must never panic), then the
/// result is truncated to its last [`STDERR_TAIL_MAX_BYTES`] bytes on a
/// `char` boundary so a broken or hostile encoder cannot flood logs with an
/// unbounded error body.
#[must_use]
pub(crate) fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= STDERR_TAIL_MAX_BYTES {
        return text.into_owned();
    }
    let mut start = text.len() - STDERR_TAIL_MAX_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod stderr_tail_tests {
    use super::{STDERR_TAIL_MAX_BYTES, stderr_tail};

    #[test]
    fn short_stderr_is_returned_verbatim() {
        assert_eq!(stderr_tail(b"boom"), "boom");
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        // Lone continuation byte: lossy decode replaces it, no panic.
        let out = stderr_tail(&[0xff, b'x']);
        assert!(out.contains('x'));
    }

    #[test]
    fn long_stderr_is_bounded_to_the_tail() {
        let big = "a".repeat(STDERR_TAIL_MAX_BYTES * 3);
        let out = stderr_tail(big.as_bytes());
        assert_eq!(out.len(), STDERR_TAIL_MAX_BYTES);
        assert!(big.ends_with(&out));
    }

    #[test]
    fn tail_boundary_never_splits_a_codepoint() {
        // Fill with 3-byte codepoints so the naive cut point lands mid-char.
        let big = "€".repeat(STDERR_TAIL_MAX_BYTES);
        let out = stderr_tail(big.as_bytes());
        // Valid UTF-8 (round-trips) and bounded at or under the cap.
        assert!(out.len() <= STDERR_TAIL_MAX_BYTES);
        assert!(big.ends_with(&out));
    }
}
