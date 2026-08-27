//! The built-in Web Push endpoints.
//!
//! Mount them once and the browser side of the subscription dance needs no
//! application code:
//!
//! | Method | Path | Does |
//! |---|---|---|
//! | `GET`  | `/push/vapid-public-key` | serve the `applicationServerKey` the browser subscribes with |
//! | `POST` | `/push/subscribe`        | record the caller's browser `PushSubscription` |
//! | `POST` | `/push/unsubscribe`      | remove one of the caller's subscriptions |
//!
//! ```rust,ignore
//! autumn_web::app()
//!     .merge(autumn_web::push::router())
//!     .run()
//!     .await;
//! ```
//!
//! `autumn generate pwa` wires this in for you, along with the client snippet
//! that calls it.
//!
//! # Authentication
//!
//! A subscription is meaningless without an owner, so both mutating routes
//! resolve the caller server-side and `401` when they cannot — they never take
//! a principal from the request body, which would let anyone subscribe *as*
//! anyone. Resolution reads, in order:
//!
//! 1. [`Current::actor`](crate::current::Current::actor) — set by the
//!    framework's auth layers, so bearer-token and session auth both work; then
//! 2. the session value named by `[auth] session_key` (default `user_id`), so
//!    an app whose push routes are not themselves behind `#[secured]` still
//!    resolves the signed-in user.
//!
//! Until an app has authentication, these routes are simply dormant: every
//! call 401s and nothing is stored. `GET /push/vapid-public-key` is
//! deliberately **public** — the subscribe snippet fetches it before the user
//! has done anything, and it is public key material.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Router, extract::FromRequestParts};
use serde::Deserialize;

use super::WebPush;
use super::store::BrowserSubscription;
use crate::error::{AutumnError, AutumnResult};
use crate::state::AppState;

/// The path `GET`ting the VAPID public key. Kept as a constant so the
/// generated client snippet and the router can never drift apart.
pub const VAPID_PUBLIC_KEY_PATH: &str = "/push/vapid-public-key";
/// The path a browser `POST`s its `PushSubscription` to.
pub const SUBSCRIBE_PATH: &str = "/push/subscribe";
/// The path a browser `POST`s an endpoint to in order to unsubscribe.
pub const UNSUBSCRIBE_PATH: &str = "/push/unsubscribe";

/// Mount the built-in push routes. See the [module docs](self).
pub fn router() -> Router<AppState> {
    Router::new()
        .route(VAPID_PUBLIC_KEY_PATH, get(vapid_public_key))
        .route(SUBSCRIBE_PATH, post(subscribe))
        .route(UNSUBSCRIBE_PATH, post(unsubscribe))
}

/// Body of `POST /push/unsubscribe`.
#[derive(Debug, Deserialize)]
struct UnsubscribeBody {
    /// The endpoint to forget. Browsers have this on the `PushSubscription`
    /// they are about to discard.
    endpoint: String,
}

/// `GET /push/vapid-public-key`
async fn vapid_public_key(push: WebPush) -> impl IntoResponse {
    match push.vapid_public_key() {
        Ok(key) => (
            StatusCode::OK,
            [
                ("content-type", "text/plain; charset=utf-8"),
                // Per-deployment, not per-user, and stable — but never worth
                // a shared cache holding it across a key rotation.
                ("cache-control", "no-store"),
            ],
            key,
        )
            .into_response(),
        // 503, not 200-with-empty-body: the client must be able to tell "push
        // is not configured here" from "here is your key", and a
        // `NotConfigured` app is expected to start working once configured.
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("cache-control", "no-store")],
            e.to_string(),
        )
            .into_response(),
    }
}

/// `POST /push/subscribe`
async fn subscribe(
    State(state): State<AppState>,
    push: WebPush,
    parts: RequestPrincipal,
    Json(subscription): Json<BrowserSubscription>,
) -> AutumnResult<StatusCode> {
    let principal = parts.resolve(&state).await?;
    // `From<PushError>` carries the status mapping, so this route and an
    // application's own handler answer a malformed payload identically.
    push.subscribe(principal, &subscription).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /push/unsubscribe`
async fn unsubscribe(
    State(state): State<AppState>,
    push: WebPush,
    parts: RequestPrincipal,
    Json(body): Json<UnsubscribeBody>,
) -> AutumnResult<StatusCode> {
    let principal = parts.resolve(&state).await?;
    // The result is deliberately discarded: an endpoint that was already gone
    // (or belongs to someone else) is still a successful unsubscribe from the
    // caller's point of view, and reporting the difference would tell one user
    // whether an endpoint belongs to another.
    push.unsubscribe(principal, &body.endpoint).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The request parts needed to resolve who is calling.
///
/// Held as an extractor rather than resolved inline so the session lookup —
/// which is async and needs the session store — happens in the handler body,
/// where a failure is a clean `401` instead of an extractor rejection.
struct RequestPrincipal(axum::http::request::Parts);

impl FromRequestParts<AppState> for RequestPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.clone()))
    }
}

impl RequestPrincipal {
    /// Resolve the calling principal, or `401`.
    async fn resolve(mut self, state: &AppState) -> AutumnResult<String> {
        // 1. Whatever the framework's auth layers published (bearer tokens and
        //    `#[secured]` session auth both land here).
        //
        //    `scoped_actor`, not `actor`: the latter falls back to the
        //    PROCESS-WIDE default actor when no request scope is in effect,
        //    which an app sets for its job runner (`set_default_actor`). This
        //    router is a mergeable `Router<AppState>` that can be mounted on a
        //    trimmed stack, and a route whose whole security model is "resolve
        //    the caller server-side" must never resolve an *unauthenticated*
        //    request to some ambient global identity.
        if let Some(actor) = crate::current::Current::scoped_actor() {
            return Ok(actor);
        }

        // 2. The signed-in user on the session, for an app whose push routes
        //    are not themselves behind an auth layer.
        let session_key = state.config().auth.session_key.clone();
        if let Ok(session) = crate::session::Session::from_request_parts(&mut self.0, state).await
            && let Some(user_id) = session.get(&session_key).await
        {
            return Ok(user_id);
        }

        Err(AutumnError::unauthorized_msg(
            "a push subscription must belong to a signed-in user",
        ))
    }
}
