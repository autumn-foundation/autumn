// Stand-ins for the idempotency hooks. Built when `idempotency` is off.
//
// Unstable. Only macro output may name these items.

use axum::body::Body;
use axum::http::Response;
use std::{future::Future, pin::Pin};

/// Never resolves: no deferred commit exists without the layer.
type Never<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// No replay exists without the layer. Nothing builds one.
#[derive(Clone)]
pub struct IdempotencyReplayResponse;

/// A route layer that passes every request through.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdempotencyReplayLayer;

impl<S> tower::Layer<S> for IdempotencyReplayLayer {
    type Service = S;

    fn layer(&self, inner: S) -> S {
        inner
    }
}

#[must_use]
pub fn __replay_response(
    _: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    None
}

#[must_use]
pub fn __replay_finalized_session_response(
    _: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    None
}

pub async fn __replay_finalized_session_response_for_anonymous(
    _: &crate::session::Session,
    _: &str,
    _: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    None
}

#[derive(Clone)]
pub(crate) struct IdempotencySessionScope;

impl IdempotencySessionScope {
    pub(crate) fn new(_: Option<String>, _: Option<String>) -> Self {
        Self
    }
}

pub(crate) fn persistence_failed_response() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
    response
}

pub(crate) const fn has_deferred_session_commit(_: &Response<Body>) -> bool {
    false
}

pub(crate) fn finalize_deferred_session_commit(_: &mut Response<Body>) -> Option<Never<'_>> {
    None
}

pub(crate) fn add_deferred_session_replay_key(
    _: &Response<Body>,
    _: Option<&str>,
    _: bool,
    _: Option<&str>,
) {
}

pub(crate) fn reserve_deferred_session_replay_key(
    _: &Response<Body>,
    _: Option<&str>,
    _: bool,
    _: Option<&str>,
) -> Option<Never<'static>> {
    None
}

pub(crate) fn keep_deferred_session_commit_locked(_: &mut Response<Body>) {}
