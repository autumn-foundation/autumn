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
}
