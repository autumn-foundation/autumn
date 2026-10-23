//! Normalized authenticated claims that may cross the edge wire.

use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::middleware::Next;
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
    pub const fn new(user_id: EdgeUserId, roles: Vec<EdgeRole>) -> Self {
        Self { user_id, roles }
    }
    /// Authenticated user claim.
    #[must_use]
    pub const fn user_id(&self) -> &EdgeUserId {
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

/// Require the host-resolved edge identity before invoking a native handler.
///
/// Route macros apply this middleware to origin mounts for handlers declaring
/// `#[edge(needs(identity))]`. This keeps origin fallback subject to the same
/// identity capability gate as capsule dispatch, even when the handler does
/// not extract [`EdgeIdentity`] itself.
pub async fn require_edge_identity(request: Request, next: Next) -> Response {
    if request.extensions().get::<EdgeIdentity>().is_none() {
        return EdgeIdentityRequired.into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::get};
    use tower::ServiceExt as _;

    use super::*;

    fn gated_router() -> Router {
        Router::new().route(
            "/",
            get(|| async { "protected" }).layer(axum::middleware::from_fn(require_edge_identity)),
        )
    }

    #[test]
    fn native_identity_gate_rejects_anonymous_requests() {
        let response = futures::executor::block_on(
            gated_router().oneshot(Request::new(axum::body::Body::empty())),
        )
        .expect("router is infallible");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(FALLTHROUGH_SENTINEL)
                .and_then(|value| value.to_str().ok()),
            Some(FallthroughReason::MissingCapability.as_str())
        );
    }

    #[test]
    fn native_identity_gate_forwards_authenticated_requests() {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .extensions_mut()
            .insert(EdgeIdentity::new(EdgeUserId::new("alice"), Vec::new()));

        let response = futures::executor::block_on(gated_router().oneshot(request))
            .expect("router is infallible");

        assert_eq!(response.status(), StatusCode::OK);
    }
}
