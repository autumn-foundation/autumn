//! Web Push (issue #1392).

pub(crate) mod encryption;
pub mod vapid;

pub use vapid::VapidKey;

/// Errors produced by the Web Push subsystem.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PushError {
    /// The configured VAPID key could not be loaded.
    #[error("invalid VAPID key: {0}")]
    InvalidVapidKey(String),
    /// A subscription endpoint URL is unusable.
    #[error("invalid push endpoint: {0}")]
    InvalidEndpoint(String),
    /// A browser-supplied subscription key (`p256dh` / `auth`) is malformed.
    #[error("invalid subscription key: {0}")]
    InvalidSubscriptionKey(String),
    /// The payload exceeds what push services are required to accept.
    #[error(
        "push payload is {len} bytes after JSON encoding, but the maximum a push service must \
         accept leaves room for only {max}; shorten the title/body or move detail behind the url"
    )]
    PayloadTooLarge {
        /// Actual plaintext length.
        len: usize,
        /// Maximum permitted plaintext length.
        max: usize,
    },
    /// Payload encryption failed.
    #[error("push payload encryption failed: {0}")]
    Encryption(String),
}
