//! Error type for the billing plugin.

use autumn_web::AutumnError;
use thiserror::Error;

use crate::money::MoneyError;

/// A billing failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BillingError {
    /// The plugin configuration is not valid.
    #[error("billing config: {0}")]
    Config(String),
    /// The provider call failed (transport or provider error).
    #[error("billing provider {provider}: {message}")]
    Provider {
        /// Provider name.
        provider: &'static str,
        /// Redacted message. Never contains a secret or a request body.
        message: String,
    },
    /// A known event kind could not be decoded.
    #[error("malformed billing event: {0}")]
    Malformed(String),
    /// The operation is not supported by this provider.
    #[error("unsupported billing operation: {0}")]
    Unsupported(&'static str),
    /// The mirror store failed.
    #[error("billing store: {0}")]
    Store(String),
    /// A record was not found.
    #[error("billing {0} not found")]
    NotFound(&'static str),
    /// The request conflicts with the current state.
    #[error("billing conflict: {0}")]
    Conflict(String),
    /// A money value is not valid.
    #[error(transparent)]
    Money(#[from] MoneyError),
    /// No authenticated user in the session.
    #[error("billing requires an authenticated user")]
    Unauthenticated,
    /// The user has no entitlement for the request.
    #[error("billing forbidden: {0}")]
    Forbidden(String),
}

impl BillingError {
    /// Build a provider error.
    #[must_use]
    pub fn provider(provider: &'static str, message: impl Into<String>) -> Self {
        Self::Provider {
            provider,
            message: message.into(),
        }
    }

    /// Build a store error.
    #[must_use]
    pub fn store(message: impl Into<String>) -> Self {
        Self::Store(message.into())
    }
}

impl BillingError {
    /// Map to an HTTP-shaped [`AutumnError`].
    ///
    /// The blanket `From<E: Error>` on `AutumnError` would give every variant
    /// a 500, so handlers call this instead of `?`.
    #[must_use]
    pub fn into_autumn(self) -> AutumnError {
        let message = self.to_string();
        match self {
            Self::Unauthenticated => AutumnError::unauthorized_msg(message),
            Self::Forbidden(_) => AutumnError::forbidden_msg(message),
            Self::NotFound(_) => AutumnError::not_found_msg(message),
            Self::Conflict(_) => AutumnError::conflict_msg(message),
            Self::Money(_) | Self::Unsupported(_) => AutumnError::bad_request_msg(message),
            Self::Provider { .. } => AutumnError::service_unavailable_msg(message),
            Self::Config(_) | Self::Malformed(_) | Self::Store(_) => {
                AutumnError::internal_server_error_msg(message)
            }
        }
    }
}
