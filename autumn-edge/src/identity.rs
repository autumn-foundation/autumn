//! Normalized authenticated claims that may cross the edge wire.

use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::wire::{FALLTHROUGH_SENTINEL, FallthroughReason};

/// Stable, normalized user identifier safe to send to a capsule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeUserId(String);

impl EdgeUserId {
    /// Construct a normalized identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Read the normalized identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A normalized authorization role safe to send to a capsule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeRole(String);

impl EdgeRole {
    /// Construct a normalized role.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Read the normalized role.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The complete identity envelope permitted to cross the edge wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeIdentity {
    user_id: EdgeUserId,
    roles: Vec<EdgeRole>,
}

impl EdgeIdentity {
    /// Construct an identity from normalized claims only.
    #[must_use]
    pub fn new(user_id: EdgeUserId, roles: Vec<EdgeRole>) -> Self {
        Self { user_id, roles }
    }
    /// Authenticated user claim.
    #[must_use]
    pub fn user_id(&self) -> &EdgeUserId {
        &self.user_id
    }
    /// Normalized role claims.
    #[must_use]
    pub fn roles(&self) -> &[EdgeRole] {
        &self.roles
    }
}

/// Rejection for a route requiring identity when the host supplied none.
#[derive(Clone, Copy, Debug)]
pub struct EdgeIdentityRequired;

impl IntoResponse for EdgeIdentityRequired {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(
                FALLTHROUGH_SENTINEL,
                FallthroughReason::MissingCapability.as_str(),
            )],
            "edge identity required",
        )
            .into_response()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for EdgeIdentity {
    type Rejection = EdgeIdentityRequired;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or(EdgeIdentityRequired)
    }
}
