//! Request-scoped **current actor** — batteries-included "who did this" scope.
//!
//! This is the fourth request-scoped ambient value in Autumn, alongside the
//! [log context](crate::log::context), [tenancy](crate::tenancy), and the
//! per-request DB-timing accumulator. It answers a single question: *which
//! principal is responsible for the mutations happening right now?*
//!
//! Once the auth layer resolves an authenticated principal it publishes the
//! principal's id here via [`Current::set_actor`]. From that point on any code
//! in the request — most importantly the generated repository writes and audit
//! call sites — can read it ambiently with [`Current::actor`] without
//! re-extracting the session. Generated `#[repository(versioned)]` writes seed
//! [`MutationContext::actor`](crate::hooks::MutationContext::actor) from it, so
//! `VersionEntry.actor` records the authenticated user with **zero** per-call
//! wiring.
//!
//! # Layering
//!
//! - **Requests**: the log-context middleware establishes an empty actor scope
//!   for the whole request; the three auth resolution points then call
//!   [`Current::set_actor`] (mirroring [`log::context::set_user_id`]).
//! - **Background jobs / CLI / "on behalf of"**: wrap the work in
//!   [`with_actor`] to set an explicit actor for that scope.
//! - **Unconfigured non-request contexts**: fall back to the process-wide
//!   default actor token (see [`Current::set_default_actor`]); when that is
//!   unset the actor is simply `None`, exactly as before this feature existed.
//!
//! Every read goes through [`tokio::task_local`]'s `try_with`, so reads outside
//! a scope never panic — they return the configured default (or `None`).
//!
//! [`log::context::set_user_id`]: crate::log::context::set_user_id

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use http_body::{Body as HttpBody, Frame, SizeHint};
use pin_project_lite::pin_project;

tokio::task_local! {
    /// The current request's actor scope. Holds a cheap-to-clone,
    /// interior-mutable handle so the auth layer can publish the resolved
    /// principal *after* the scope is established (mirroring the log context).
    static CURRENT_ACTOR: ActorScope;
}

/// Process-wide default actor token for non-request contexts (jobs, scheduler,
/// CLI). `None` by default, in which case out-of-scope reads yield `None` and
/// behavior is unchanged from before this feature.
static DEFAULT_ACTOR: RwLock<Option<String>> = RwLock::new(None);

/// A cheap-to-clone, interior-mutable handle to one request's current actor.
///
/// Cloning shares the same underlying storage, so the value published by the
/// auth layer via [`Current::set_actor`] after the scope was established is
/// observed by every later read within the request.
#[derive(Clone)]
pub(crate) struct ActorScope(Arc<RwLock<Option<String>>>);

impl ActorScope {
    /// An actor scope with no actor yet resolved (the request default).
    fn empty() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    /// An actor scope seeded with an explicit value (used by [`with_actor`]).
    fn with_value(actor_id: Option<String>) -> Self {
        Self(Arc::new(RwLock::new(actor_id)))
    }

    /// Read the current actor id, if any.
    fn get(&self) -> Option<String> {
        self.0.read().ok().and_then(|guard| guard.clone())
    }

    /// Publish (or clear) the actor id on this scope.
    fn set(&self, actor_id: Option<String>) {
        if let Ok(mut guard) = self.0.write() {
            *guard = actor_id;
        }
    }
}

/// Public accessor for the ambient "current actor".
///
/// This is a zero-sized namespace type; all methods are associated functions.
#[derive(Debug, Clone, Copy)]
pub struct Current;

impl Current {
    /// The id of the actor responsible for the work in the current scope.
    ///
    /// Resolution order:
    /// 1. an explicit actor set on the current scope (via [`with_actor`] or
    ///    [`Current::set_actor`]);
    /// 2. `None` when inside a request scope that has not resolved a principal
    ///    (unauthenticated requests behave exactly as before);
    /// 3. the process-wide default actor token when **no** scope is in effect
    ///    (background jobs / CLI / startup) — see [`Current::set_default_actor`].
    ///
    /// Never panics: outside any scope it reads the default token, which is
    /// `None` unless configured.
    #[must_use]
    pub fn actor() -> Option<String> {
        match CURRENT_ACTOR.try_with(ActorScope::get) {
            Ok(Some(actor_id)) => Some(actor_id),
            Ok(None) => None,
            Err(_) => default_actor(),
        }
    }

    /// Publish the authenticated principal onto the current request scope.
    ///
    /// Called by the auth layer right after it resolves the user id (next to
    /// [`log::context::set_user_id`](crate::log::context::set_user_id)). No-op
    /// when called outside of an established scope, so it is always safe.
    pub fn set_actor(actor_id: impl Into<String>) {
        let actor_id = actor_id.into();
        let _ = CURRENT_ACTOR.try_with(|scope| scope.set(Some(actor_id)));
    }

    /// Set (or clear) the process-wide default actor token used for non-request
    /// contexts. Pass `None` to restore the "no default" behavior.
    ///
    /// This lets a job runner or scheduler attribute writes to a configured
    /// identity (e.g. `"scheduler"`) instead of silently recording the
    /// hard-coded `SYSTEM_ACTOR`.
    pub fn set_default_actor(actor_id: Option<String>) {
        if let Ok(mut guard) = DEFAULT_ACTOR.write() {
            *guard = actor_id;
        }
    }

    /// The currently configured process-wide default actor token, if any.
    #[must_use]
    pub fn default_actor() -> Option<String> {
        default_actor()
    }
}

fn default_actor() -> Option<String> {
    DEFAULT_ACTOR.read().ok().and_then(|guard| guard.clone())
}

/// Run `future` with `actor_id` installed as the current actor.
///
/// The direct analog of [`tenancy::with_tenant`](crate::tenancy::with_tenant),
/// for background jobs, CLI tasks, and "acting on behalf of" flows. Nested
/// calls override the outer actor for the duration of the inner future.
pub async fn with_actor<F, R>(actor_id: impl Into<String>, future: F) -> R
where
    F: Future<Output = R>,
{
    CURRENT_ACTOR
        .scope(ActorScope::with_value(Some(actor_id.into())), future)
        .await
}

/// Establish an empty actor scope for a request future.
///
/// Used by the log-context middleware so the three auth resolution points can
/// publish the resolved principal via [`Current::set_actor`] after the request
/// future has started. Returns the scoping future directly so the middleware
/// can name it as a concrete `Future` type (no per-request box allocation).
pub(crate) fn scope_request<F: Future>(
    future: F,
) -> tokio::task::futures::TaskLocalFuture<ActorScope, F> {
    CURRENT_ACTOR.scope(ActorScope::empty(), future)
}

/// Clone the current request's actor-scope handle, if a scope is in effect.
///
/// The returned handle is the same cheap-to-clone, interior-mutable storage the
/// request scope holds, so re-entering it later — e.g. while producing a
/// lazy/streaming response body after the request future has resolved — observes
/// any actor the auth layer published during the request. Returns `None` outside
/// a scope. Mirrors [`log::context::current`](crate::log::context::current).
pub(crate) fn current_scope() -> Option<ActorScope> {
    CURRENT_ACTOR.try_with(Clone::clone).ok()
}

pin_project! {
    /// Response-body wrapper that re-establishes the request's current-actor
    /// scope on every frame poll, so a `#[repository(versioned)]` write performed
    /// while producing a lazy/streaming body (SSE, `Body::from_stream`) still
    /// records the request's resolved actor instead of falling back to
    /// `SYSTEM_ACTOR`. The sibling of [`LogContextBody`](crate::middleware::log_context)
    /// and [`TenantPropagatingBody`](crate::tenancy::TenantPropagatingBody).
    ///
    /// Carries the **resolved** [`ActorScope`] handle captured when the response
    /// was produced (shared, interior-mutable storage from the request), so any
    /// actor the auth layer set during the request is visible while streaming.
    pub struct CurrentActorBody<B> {
        #[pin]
        inner: B,
        // `None` only when the body was wrapped outside any request scope, in
        // which case poll_frame is a transparent pass-through (never panics).
        scope: Option<ActorScope>,
    }
}

impl<B> CurrentActorBody<B> {
    pub(crate) const fn new(inner: B, scope: Option<ActorScope>) -> Self {
        Self { inner, scope }
    }
}

impl<B: HttpBody> HttpBody for CurrentActorBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this.scope.clone() {
            Some(scope) => CURRENT_ACTOR.sync_scope(scope, || this.inner.poll_frame(cx)),
            None => this.inner.poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize tests that touch the process-wide default actor so they don't
    // race each other (the default is global state).
    static DEFAULT_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn actor_is_none_outside_scope_by_default() {
        let _g = DEFAULT_GUARD.lock().unwrap();
        Current::set_default_actor(None);
        assert_eq!(Current::actor(), None);
    }

    #[tokio::test]
    async fn with_actor_sets_the_ambient_actor() {
        with_actor("u1", async {
            assert_eq!(Current::actor(), Some("u1".to_owned()));
        })
        .await;
    }

    #[tokio::test]
    async fn nested_with_actor_overrides_inner_and_restores_outer() {
        with_actor("outer", async {
            assert_eq!(Current::actor(), Some("outer".to_owned()));
            with_actor("inner", async {
                assert_eq!(Current::actor(), Some("inner".to_owned()));
            })
            .await;
            // Back in the outer scope the outer actor is restored.
            assert_eq!(Current::actor(), Some("outer".to_owned()));
        })
        .await;
    }

    #[tokio::test]
    async fn set_actor_publishes_onto_an_established_scope() {
        // Mirrors the request path: an empty scope is established, then the
        // auth layer publishes the principal via set_actor.
        scope_request(async {
            assert_eq!(Current::actor(), None);
            Current::set_actor("resolved-user");
            assert_eq!(Current::actor(), Some("resolved-user".to_owned()));
        })
        .await;
    }

    #[tokio::test]
    async fn set_actor_is_a_noop_outside_a_scope() {
        let _g = DEFAULT_GUARD.lock().unwrap();
        Current::set_default_actor(None);
        // Must not panic and must not somehow persist a value.
        Current::set_actor("nobody");
        assert_eq!(Current::actor(), None);
    }

    #[tokio::test]
    async fn default_actor_is_used_only_out_of_scope() {
        // The default-dependent assertions touch process-global state, so guard
        // them (and reset) without holding the lock across an `.await`.
        {
            let _g = DEFAULT_GUARD.lock().unwrap();
            Current::set_default_actor(Some("scheduler".to_owned()));
            // Out of scope: falls back to the configured default.
            assert_eq!(Current::actor(), Some("scheduler".to_owned()));
            // Reset before releasing the guard so no other test observes it.
            Current::set_default_actor(None);
            assert_eq!(Current::actor(), None);
        }

        // The scoped reads below do not depend on the process default: an empty
        // request scope reads None, and an explicit scope overrides everything.
        scope_request(async {
            assert_eq!(Current::actor(), None);
        })
        .await;

        with_actor("u1", async {
            assert_eq!(Current::actor(), Some("u1".to_owned()));
        })
        .await;
    }

    #[test]
    fn reads_never_panic_without_a_runtime_scope() {
        // No tokio task-local scope in effect; must be inert, not panic.
        let _ = Current::actor();
        Current::set_actor("x");
    }

    #[tokio::test]
    async fn captured_actor_scope_handle_is_reentrant() {
        // Mirrors the streaming-body path: establish a request scope, let the
        // auth layer publish the principal, and capture the interior-mutable
        // scope handle while still inside the request.
        let handle = scope_request(async {
            Current::set_actor("streamed-user");
            current_scope().expect("a scope is established here")
        })
        .await;

        // The original request scope has now ended. Re-enter the captured handle
        // from a DIFFERENT task-local frame (exactly what `CurrentActorBody` does
        // on each `poll_frame`) and confirm the actor set during the request is
        // still visible — so writes performed while streaming stay attributed.
        let seen = CURRENT_ACTOR.sync_scope(handle, Current::actor);
        assert_eq!(seen, Some("streamed-user".to_owned()));
    }
}
