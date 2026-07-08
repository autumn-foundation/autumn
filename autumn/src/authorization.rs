//! Policy-based record-level authorization.
//!
//! Spring Security's role checks (`#[secured("admin")]`) answer
//! "are you allowed to call this *route*?" This module answers
//! "are you allowed to act on *this specific record*?" — the
//! question every multi-user CRUD app has to answer at every write
//! endpoint.
//!
//! The shape mirrors Pundit (Rails) and Bodyguard (Phoenix): one
//! [`Policy`] impl per resource, a [`Scope`] companion for list
//! queries, default-deny semantics, and an `#[authorize]` attribute
//! macro that wires the check declaratively.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use autumn_web::authorization::{Policy, PolicyContext};
//! use autumn_web::AutumnResult;
//!
//! #[derive(Default)]
//! pub struct PostPolicy;
//!
//! impl Policy<Post> for PostPolicy {
//!     fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _post: &'a Post)
//!         -> autumn_web::authorization::BoxFuture<'a, bool>
//!     {
//!         Box::pin(async { true }) // posts are public
//!     }
//!
//!     fn can_update<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
//!         -> autumn_web::authorization::BoxFuture<'a, bool>
//!     {
//!         Box::pin(async move {
//!             ctx.has_role("admin")
//!                 || ctx.user_id_i64() == Some(post.author_id)
//!         })
//!     }
//!
//!     fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
//!         -> autumn_web::authorization::BoxFuture<'a, bool>
//!     {
//!         Box::pin(async move {
//!             ctx.has_role("admin")
//!                 || ctx.user_id_i64() == Some(post.author_id)
//!         })
//!     }
//! }
//! ```
//!
//! Register the policy on the app builder and reference it from
//! `#[repository(api = "/posts", policy = PostPolicy)]` to enforce
//! the same checks on auto-generated REST endpoints.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use http::StatusCode;

use crate::session::Session;

/// Boxed future returned by [`Policy`] and [`Scope`] methods so the
/// traits remain object-safe (`dyn Policy<R>` works regardless of
/// rust edition).
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

// ── PolicyContext ────────────────────────────────────────────────

/// Per-request context handed to every policy and scope check.
///
/// Carries the resolved [`Session`], the authenticated user id (when
/// present), the active role set, the [`PolicyRegistry`] (so
/// `Post::scope(&ctx)` can resolve a registered scope without
/// re-threading state), and a clone of the database pool so
/// policies can consult related rows. `Clone + Send + Sync` — flows
/// freely across `.await` points.
#[derive(Clone)]
pub struct PolicyContext {
    /// The full per-request [`Session`]. Read raw values via
    /// [`Session::get`] when a policy needs data beyond the
    /// canonical user-id and role keys.
    pub session: Session,

    /// The authenticated user id, if any. Mirrors the configured
    /// session auth key (default: `"user_id"`).
    pub user_id: Option<String>,

    /// Active role set for the current user. Empty when the user
    /// has no role or is anonymous.
    pub roles: Vec<String>,

    /// Scopes (token abilities) granted to the authenticating service
    /// token, e.g. `posts:read`, `posts:write`. Empty for session-only
    /// requests. Distinct from the record-level [`Scope`] trait: these
    /// are flat permission strings carried by a scoped API token.
    /// Surfaced via [`has_scope`](Self::has_scope) and friends.
    pub scopes: Vec<String>,

    /// Database connection pool, cloned from `AppState`. Policies
    /// that need to consult related rows (e.g. group membership)
    /// can borrow a connection here.
    #[cfg(feature = "db")]
    pub pool:
        Option<diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>>,

    /// Registered [`Policy`] / [`Scope`] map, cloned from
    /// `AppState`. Lets the [`Scoped`] blanket trait resolve a
    /// registered scope from `&ctx` alone — the
    /// `Post::scope(&ctx).load(&mut db).await?` ergonomic the
    /// authorization guide documents.
    pub policy_registry: PolicyRegistry,
}

impl PolicyContext {
    /// Build a [`PolicyContext`] from a session alone.
    ///
    /// The resulting context has an empty [`PolicyRegistry`] and no
    /// pool — sufficient for hand-rolled policy unit tests that
    /// don't go through `AppState`. Production code paths construct
    /// a [`PolicyContext`] via [`from_request`](Self::from_request)
    /// instead.
    pub async fn from_session(session: &Session, auth_session_key: &str) -> Self {
        let user_id = session.get(auth_session_key).await;
        let role = session.get("role").await;
        let roles = role.into_iter().collect();
        Self {
            session: session.clone(),
            user_id,
            roles,
            scopes: Vec::new(),
            #[cfg(feature = "db")]
            pool: None,
            policy_registry: PolicyRegistry::default(),
        }
    }

    /// Build a fully-populated [`PolicyContext`] from `AppState` +
    /// `Session`. Used by the `#[authorize]` macro and
    /// `#[repository(policy = ...)]`-generated handlers.
    pub async fn from_request(state: &crate::AppState, session: &Session) -> Self {
        let mut ctx = Self::from_session(session, state.auth_session_key()).await;
        ctx.policy_registry = state.policy_registry().clone();
        #[cfg(feature = "db")]
        {
            if let Some(pool) = state.pool() {
                ctx.pool = Some(pool.clone());
            }
        }
        ctx
    }

    /// Returns `true` when the request has a resolved authenticated user.
    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.user_id.is_some()
    }

    /// Returns the user id parsed as an `i64`, when the session
    /// stored it as a numeric string. Convenient for the common
    /// case of `BIGSERIAL` primary keys.
    #[must_use]
    pub fn user_id_i64(&self) -> Option<i64> {
        self.user_id.as_deref().and_then(|s| s.parse().ok())
    }

    /// Returns `true` when the active role set contains `role`.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// Returns `true` when the active role set contains any of the
    /// supplied roles. Mirrors `#[secured("a", "b")]` semantics.
    #[must_use]
    pub fn has_any_role<I, S>(&self, candidates: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        candidates.into_iter().any(|c| self.has_role(c.as_ref()))
    }

    /// Returns `true` when the authenticating token granted `scope`.
    ///
    /// Mirrors [`has_role`](Self::has_role) for token abilities. Works for
    /// non-user principals: a pure service token with no roles still
    /// authorizes on its granted scopes.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Returns `true` when the token granted **any** of the supplied scopes.
    /// Mirrors [`has_any_role`](Self::has_any_role).
    #[must_use]
    pub fn has_any_scope<I, S>(&self, candidates: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        candidates.into_iter().any(|c| self.has_scope(c.as_ref()))
    }

    /// Returns `true` when the token granted **all** of the supplied scopes.
    /// An empty candidate set is vacuously `true`.
    #[must_use]
    pub fn has_all_scopes<I, S>(&self, candidates: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        candidates.into_iter().all(|c| self.has_scope(c.as_ref()))
    }

    /// Attach token scopes to the context. Used by the framework when
    /// authorizing a scoped-token request; tests and hand-written handlers
    /// can also call this to inject scopes by hand.
    #[must_use]
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Build a fully-populated [`PolicyContext`] from `AppState` + `Session`,
    /// additionally threading the authenticating token's granted scopes (from
    /// the [`crate::auth::ApiTokenScopes`] request extension) into the context.
    ///
    /// Use this from hand-written handlers that authorize a service principal on
    /// scopes: extract `Option<Extension<ApiTokenScopes>>` and pass it through.
    pub async fn from_request_parts(
        state: &crate::AppState,
        session: &Session,
        scopes: Option<&crate::auth::ApiTokenScopes>,
    ) -> Self {
        let ctx = Self::from_request(state, session).await;
        match scopes {
            Some(s) => ctx.with_scopes(s.0.clone()),
            None => ctx,
        }
    }

    /// Attach a database pool to the context. Used by the framework
    /// when constructing the context inside extractors; tests can
    /// also call this to inject a pool by hand.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_pool(
        mut self,
        pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    ) -> Self {
        self.pool = Some(pool);
        self
    }
}

// ── Policy trait ────────────────────────────────────────────────

/// Record-level authorization policy for a resource.
///
/// All four built-in actions (`can_show`, `can_create`,
/// `can_update`, `can_delete`) default to **denied**, which makes
/// opting into a policy safe-by-default: a freshly-introduced
/// policy with no overrides forbids every action until the
/// developer explicitly allows one.
///
/// # Object safety
///
/// Every method returns a [`BoxFuture`] so `dyn Policy<R>` is
/// usable behind an `Arc`. Tests can swap implementations via
/// `AppBuilder::policy::<R, P>(P::default())`.
///
/// # Custom verbs
///
/// Use [`Policy::can`] for verbs that are not one of the four
/// built-ins (e.g. `"publish"`, `"feature"`, `"archive"`). The
/// default impl dispatches the four built-ins and returns `false`
/// for unknown verbs.
pub trait Policy<R: Send + Sync + 'static>: Send + Sync + 'static {
    /// Decide whether the current user may *show* the resource.
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _resource: &'a R) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Decide whether the current user may *create* a resource of
    /// this type.
    ///
    /// `can_create` receives request context only. Policies that need
    /// the proposed JSON payload before insert can override
    /// [`Policy::can_create_payload`].
    fn can_create<'a>(&'a self, _ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Decide whether the current user may create the proposed
    /// payload. Default behavior preserves compatibility by
    /// delegating to [`Policy::can_create`].
    fn can_create_payload<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        _payload: &'a serde_json::Value,
    ) -> BoxFuture<'a, bool> {
        self.can_create(ctx)
    }

    /// Decide whether the current user may *update* the resource.
    fn can_update<'a>(&'a self, _ctx: &'a PolicyContext, _resource: &'a R) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Decide whether the current user may *delete* the resource.
    fn can_delete<'a>(&'a self, _ctx: &'a PolicyContext, _resource: &'a R) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Decide a custom verb. Defaults to dispatching the four
    /// built-ins by name. The `resource` argument is ignored when
    /// dispatching to `can_create`, since `can_create` operates
    /// pre-insert and has no resource instance.
    fn can<'a>(
        &'a self,
        action: &'a str,
        ctx: &'a PolicyContext,
        resource: &'a R,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match action {
                "show" | "read" => self.can_show(ctx, resource).await,
                "create" => self.can_create(ctx).await,
                "update" | "edit" => self.can_update(ctx, resource).await,
                "delete" | "destroy" => self.can_delete(ctx, resource).await,
                _ => false,
            }
        })
    }
}

// ── Scope trait ─────────────────────────────────────────────────

/// List-time companion to [`Policy`] for filtering record sets the
/// current user is allowed to read.
///
/// Default implementations return an **empty** list — fail closed.
/// `#[repository(scope = ...)]`-generated `GET /<api>` index
/// endpoints invoke the registered scope automatically; hand-
/// written list handlers can use the [`Scoped`] blanket trait to
/// invoke `Post::scope(&ctx).load(&mut db).await?`.
///
/// The `db` feature gates the connection parameter — without it,
/// the trait still exists but `list` takes no connection (use
/// `ctx.pool` to acquire one if needed).
#[cfg(feature = "db")]
pub trait Scope<R: Send + Sync + 'static>: Send + Sync + 'static {
    /// Return the records the current user is allowed to read.
    ///
    /// The default impl returns `Ok(Vec::new())` so a missing
    /// scope opt-in fails closed. Implementations typically run a
    /// Diesel query through `conn`, applying whatever filters the
    /// active `ctx.user_id` / `ctx.roles` warrant.
    fn list<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _conn: &'a mut diesel_async::AsyncPgConnection,
    ) -> BoxFuture<'a, crate::AutumnResult<Vec<R>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// `Scope` companion that compiles when the `db` feature is off.
/// The `db`-gated form takes `&mut AsyncPgConnection`; this one
/// has no connection arg.
#[cfg(not(feature = "db"))]
pub trait Scope<R: Send + Sync + 'static>: Send + Sync + 'static {
    fn list<'a>(&'a self, _ctx: &'a PolicyContext) -> BoxFuture<'a, crate::AutumnResult<Vec<R>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

// ── `Post::scope(&ctx).load(&mut db).await?` ergonomics ─────────

/// Deferred query handle returned by [`Scoped::scope`].
///
/// Holds a borrow on the [`PolicyContext`] so the registered
/// [`Scope`] for `R` can be resolved at `.load()` time. The
/// pattern mirrors Pundit's `policy_scope(Post)` and Phoenix's
/// `Bodyguard.scope/4`: a query you can run when the connection
/// is available.
pub struct ScopeQuery<'a, R: Send + Sync + 'static> {
    ctx: &'a PolicyContext,
    _marker: std::marker::PhantomData<fn() -> R>,
}

#[cfg(feature = "db")]
impl<R: Send + Sync + 'static> ScopeQuery<'_, R> {
    /// Load the records the current user is allowed to read.
    ///
    /// Resolves the [`Scope`] registered on the app's
    /// [`PolicyRegistry`] (carried in [`PolicyContext`]) and runs
    /// its `list` method against `conn`.
    ///
    /// # Errors
    ///
    /// Returns a `500` when no scope is registered for `R`; the
    /// scope's own errors otherwise.
    pub async fn load(
        self,
        conn: &mut diesel_async::AsyncPgConnection,
    ) -> crate::AutumnResult<Vec<R>> {
        let scope = self.ctx.policy_registry.scope::<R>().ok_or_else(|| {
            crate::AutumnError::from(std::io::Error::other(format!(
                "no scope registered for resource type {}",
                std::any::type_name::<R>()
            )))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        scope.list(self.ctx, conn).await
    }
}

#[cfg(not(feature = "db"))]
impl<R: Send + Sync + 'static> ScopeQuery<'_, R> {
    pub async fn load(self) -> crate::AutumnResult<Vec<R>> {
        let scope = self.ctx.policy_registry.scope::<R>().ok_or_else(|| {
            crate::AutumnError::from(std::io::Error::other(format!(
                "no scope registered for resource type {}",
                std::any::type_name::<R>()
            )))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        scope.list(self.ctx).await
    }
}

/// Blanket trait that adds `T::scope(&ctx)` to every type, so
/// hand-written list handlers can mirror the
/// `#[repository(scope = ...)]`-generated path:
///
/// ```rust,ignore
/// use autumn_web::authorization::Scoped;
///
/// let posts = Post::scope(&ctx).load(&mut db).await?;
/// ```
///
/// Auto-implemented for every `Send + Sync + 'static` type. Bring
/// the trait into scope with `use autumn_web::authorization::Scoped;`
/// (or via `autumn_web::prelude::*`) to use the syntax.
pub trait Scoped: Send + Sync + Sized + 'static {
    /// Open a deferred [`ScopeQuery`] for this type. Resolves the
    /// registered scope at `.load()` time, not here.
    #[must_use]
    fn scope(ctx: &PolicyContext) -> ScopeQuery<'_, Self> {
        ScopeQuery {
            ctx,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: Send + Sync + 'static> Scoped for T {}

// ── PolicyRegistry ──────────────────────────────────────────────

/// Process-wide registry of resource → policy and resource →
/// scope bindings.
///
/// Stored on [`AppState`](crate::AppState) so handlers and
/// `#[repository]`-generated endpoints can resolve a policy by
/// resource type via [`AppState::policy::<R>`](crate::AppState::policy).
#[derive(Clone, Default)]
pub struct PolicyRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    policies: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    scopes: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl PolicyRegistry {
    /// Register the [`Policy`] implementation for resource `R`.
    ///
    /// # Panics
    ///
    /// Panics if a policy is already registered for `R`. The issue
    /// spec is explicit: multiple policies per resource are not
    /// supported in this slice; double-registration must surface
    /// as a startup-time error rather than silent override.
    pub fn register_policy<R, P>(&self, policy: P)
    where
        R: Send + Sync + 'static,
        P: Policy<R>,
    {
        let mut inner = self.inner.write().expect("policy registry lock poisoned");
        let key = TypeId::of::<R>();
        assert!(
            !inner.policies.contains_key(&key),
            "Policy for {} already registered. Multiple policies per resource are not supported.",
            std::any::type_name::<R>()
        );
        let boxed: Arc<dyn Policy<R>> = Arc::new(policy);
        inner.policies.insert(key, Arc::new(boxed));
    }

    /// Register the [`Scope`] implementation for resource `R`.
    ///
    /// # Panics
    ///
    /// Panics if a scope is already registered for `R`.
    pub fn register_scope<R, S>(&self, scope: S)
    where
        R: Send + Sync + 'static,
        S: Scope<R>,
    {
        let mut inner = self.inner.write().expect("policy registry lock poisoned");
        let key = TypeId::of::<R>();
        assert!(
            !inner.scopes.contains_key(&key),
            "Scope for {} already registered. Multiple scopes per resource are not supported.",
            std::any::type_name::<R>()
        );
        let boxed: Arc<dyn Scope<R>> = Arc::new(scope);
        inner.scopes.insert(key, Arc::new(boxed));
    }

    /// Resolve the registered [`Policy`] for resource `R`.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal `RwLock` is poisoned (a
    /// previous writer panicked while holding the lock).
    #[must_use]
    pub fn policy<R: Send + Sync + 'static>(&self) -> Option<Arc<dyn Policy<R>>> {
        let inner = self.inner.read().expect("policy registry lock poisoned");
        inner
            .policies
            .get(&TypeId::of::<R>())
            .and_then(|a| a.downcast_ref::<Arc<dyn Policy<R>>>().cloned())
    }

    /// Resolve the registered [`Scope`] for resource `R`.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal `RwLock` is poisoned.
    #[must_use]
    pub fn scope<R: Send + Sync + 'static>(&self) -> Option<Arc<dyn Scope<R>>> {
        let inner = self.inner.read().expect("policy registry lock poisoned");
        inner
            .scopes
            .get(&TypeId::of::<R>())
            .and_then(|a| a.downcast_ref::<Arc<dyn Scope<R>>>().cloned())
    }

    /// Returns `true` when a policy is registered for resource `R`.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal `RwLock` is poisoned.
    #[must_use]
    pub fn has_policy<R: Send + Sync + 'static>(&self) -> bool {
        self.inner
            .read()
            .expect("policy registry lock poisoned")
            .policies
            .contains_key(&TypeId::of::<R>())
    }
}

impl std::fmt::Debug for PolicyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read().expect("policy registry lock poisoned");
        f.debug_struct("PolicyRegistry")
            .field("policies", &inner.policies.len())
            .field("scopes", &inner.scopes.len())
            .finish()
    }
}

// ── Forbidden response shape ────────────────────────────────────

/// HTTP status the framework returns when a [`Policy`] denies an
/// action.
///
/// Defaults to `404 Not Found` so unauthorized clients cannot
/// distinguish "the record exists but you cannot touch it" from
/// "the record does not exist." This mirrors Rails / Phoenix
/// defaults; flip to `403` via
/// `[security] forbidden_response = "403"` in `autumn.toml` when
/// the leak is acceptable (e.g. internal admin tooling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForbiddenResponse {
    /// Return `403 Forbidden`.
    Forbidden403,
    /// Return `404 Not Found` (default, hides existence).
    #[default]
    NotFound404,
}

impl ForbiddenResponse {
    /// HTTP status code for the deny response.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::Forbidden403 => StatusCode::FORBIDDEN,
            Self::NotFound404 => StatusCode::NOT_FOUND,
        }
    }

    /// Human-readable message for the deny response body. Kept
    /// generic so a `404`-mode response does not accidentally
    /// reveal existence via the message text.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Forbidden403 => "forbidden",
            Self::NotFound404 => "not found",
        }
    }

    /// Build the [`AutumnError`](crate::AutumnError) used by the
    /// `#[authorize]` macro and `#[repository]`-generated
    /// endpoints when a policy denies an action.
    #[must_use]
    pub fn into_error(self) -> crate::AutumnError {
        crate::AutumnError::from(std::io::Error::other(self.message())).with_status(self.status())
    }
}

impl std::str::FromStr for ForbiddenResponse {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "403" | "forbidden" | "Forbidden" => Ok(Self::Forbidden403),
            "404" | "not_found" | "NotFound" | "" => Ok(Self::NotFound404),
            other => Err(format!(
                "invalid forbidden_response: {other:?} (expected \"403\" or \"404\")"
            )),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ForbiddenResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

// ── Runtime authorization helpers ───────────────────────────────

/// Resolve the registered [`Policy`] for resource `R`, run the
/// named action, and return the configured deny response on
/// failure.
///
/// This is the workhorse called by the `#[authorize]` attribute
/// macro and by `#[repository(policy = ...)]`-generated handlers.
/// Hand-written handlers can call it directly to short-circuit a
/// route after loading the resource — the inline pattern that
/// replaces the hand-rolled `if record.author_id != user_id { ... }`
/// snippets the reddit-clone migration removes.
///
/// # Errors
///
/// Returns the configured [`ForbiddenResponse`] error when the
/// policy denies the action, or a `500` when no policy is
/// registered for `R`.
///
/// # Examples
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::authorization::authorize;
///
/// async fn delete_post(
///     state: AppState,
///     session: Session,
///     mut db: Db,
///     post: Post,
/// ) -> AutumnResult<()> {
///     authorize::<Post>(&state, &session, "delete", &post).await?;
///     // ... actually delete
///     Ok(())
/// }
/// ```
pub async fn authorize<R>(
    state: &crate::AppState,
    session: &Session,
    action: &str,
    resource: &R,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    let policy = state.policy_registry().policy::<R>().ok_or_else(|| {
        crate::AutumnError::from(std::io::Error::other(format!(
            "no policy registered for resource type {}",
            std::any::type_name::<R>()
        )))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let ctx = PolicyContext::from_request(state, session).await;

    if policy.can(action, &ctx, resource).await {
        Ok(())
    } else {
        Err(state.forbidden_response().into_error())
    }
}

/// Like [`authorize`], but additionally threads the authenticating token's
/// granted scopes into the [`PolicyContext`] so the policy can decide on
/// `ctx.has_scope(...)`.
///
/// Use from hand-written handlers that authorize a service principal: extract
/// `Option<Extension<ApiTokenScopes>>` and pass it through. A pure service
/// token (no session user) is authorized purely on its scopes.
///
/// # Errors
///
/// Returns the configured deny response when the policy denies. Returns `500`
/// when no policy is registered for `R`.
pub async fn authorize_with_scopes<R>(
    state: &crate::AppState,
    session: &Session,
    scopes: Option<&crate::auth::ApiTokenScopes>,
    action: &str,
    resource: &R,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    let policy = state.policy_registry().policy::<R>().ok_or_else(|| {
        crate::AutumnError::from(std::io::Error::other(format!(
            "no policy registered for resource type {}",
            std::any::type_name::<R>()
        )))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let ctx = PolicyContext::from_request_parts(state, session, scopes).await;

    if policy.can(action, &ctx, resource).await {
        Ok(())
    } else {
        Err(state.forbidden_response().into_error())
    }
}

/// Internal alias used by the `#[authorize]` proc-macro and the
/// `#[repository(policy = ...)]` generated handlers. **Not part of
/// the public API** — call [`authorize`] from user code.
#[doc(hidden)]
pub async fn __check_policy<R>(
    state: &crate::AppState,
    session: &Session,
    action: &str,
    resource: &R,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    authorize(state, session, action, resource).await
}

/// Scope-aware variant of [`__check_policy`] emitted by the `#[authorize]`
/// proc-macro. Threads the authenticating token's granted scopes into the
/// [`PolicyContext`] so policies can decide on `ctx.has_scope(...)`.
///
/// **Not part of the public API** — call [`authorize_with_scopes`] from user
/// code.
#[doc(hidden)]
pub async fn __check_policy_scoped<R>(
    state: &crate::AppState,
    session: &Session,
    scopes: Option<&crate::auth::ApiTokenScopes>,
    action: &str,
    resource: &R,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    authorize_with_scopes(state, session, scopes, action, resource).await
}

/// Pre-insert authorization helper for the
/// `#[repository(policy = ...)]`-generated `POST` endpoint.
///
/// Resolves the registered [`Policy`] for `R` and calls
/// [`Policy::can_create`] *before* the row is persisted, closing
/// the "deny still wrote a row" hole that catches naive
/// after-the-fact policy checks. Use [`authorize_create`] from user
/// code; this is the framework's backward-compatible `__`-prefixed
/// alias for older macro output.
#[doc(hidden)]
pub async fn __check_policy_create<R>(
    state: &crate::AppState,
    session: &Session,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    authorize_create::<R>(state, session).await
}

/// Payload-aware pre-insert authorization helper for
/// `#[repository(policy = ...)]`-generated `POST` endpoints.
///
/// Newer macro output uses this helper when it has the raw JSON
/// request payload available. The two-argument
/// [`__check_policy_create`] alias is kept so applications compiled
/// with older `autumn-macros` output remain source-compatible when
/// only `autumn-web` is upgraded.
#[doc(hidden)]
pub async fn __check_policy_create_payload<R>(
    state: &crate::AppState,
    session: &Session,
    payload: &serde_json::Value,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    authorize_create_payload::<R>(state, session, payload).await
}

/// Scope-aware variant of [`__check_policy_create_payload`] emitted by the
/// `#[repository(policy = ...)]` proc-macro. Threads the authenticating token's
/// granted scopes into the [`PolicyContext`] so policies can decide on
/// `ctx.has_scope(...)`.
///
/// **Not part of the public API.**
#[doc(hidden)]
pub async fn __check_policy_create_payload_scoped<R>(
    state: &crate::AppState,
    session: &Session,
    scopes: Option<&crate::auth::ApiTokenScopes>,
    payload: &serde_json::Value,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    let policy = state.policy_registry().policy::<R>().ok_or_else(|| {
        crate::AutumnError::from(std::io::Error::other(format!(
            "no policy registered for resource type {}",
            std::any::type_name::<R>()
        )))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    let ctx = PolicyContext::from_request_parts(state, session, scopes).await;
    if policy.can_create_payload(&ctx, payload).await {
        Ok(())
    } else {
        Err(state.forbidden_response().into_error())
    }
}

/// Run a policy's `can_create` check before persisting a new record.
///
/// Mirrors [`authorize`] but takes no resource argument: at create
/// time, no record instance exists yet, so policies decide based on
/// `ctx.user_id` and `ctx.roles` alone.
///
/// # Errors
///
/// Returns the configured deny response when the policy denies.
/// Returns `500` when no policy is registered for `R`.
pub async fn authorize_create<R>(
    state: &crate::AppState,
    session: &Session,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    let policy = state.policy_registry().policy::<R>().ok_or_else(|| {
        crate::AutumnError::from(std::io::Error::other(format!(
            "no policy registered for resource type {}",
            std::any::type_name::<R>()
        )))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let ctx = PolicyContext::from_request(state, session).await;

    if policy.can_create(&ctx).await {
        Ok(())
    } else {
        Err(state.forbidden_response().into_error())
    }
}

/// Run a policy's payload-aware `can_create_payload` check before
/// persisting a new record.
///
/// Use this when a create policy must inspect the proposed JSON
/// payload before insert, such as tenant/owner invariants. Existing
/// custom handlers that only need context-based create authorization
/// should keep calling [`authorize_create`].
///
/// # Errors
///
/// Returns the configured deny response when the policy denies.
/// Returns `500` when no policy is registered for `R`.
pub async fn authorize_create_payload<R>(
    state: &crate::AppState,
    session: &Session,
    payload: &serde_json::Value,
) -> crate::AutumnResult<()>
where
    R: Send + Sync + 'static,
{
    let policy = state.policy_registry().policy::<R>().ok_or_else(|| {
        crate::AutumnError::from(std::io::Error::other(format!(
            "no policy registered for resource type {}",
            std::any::type_name::<R>()
        )))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let ctx = PolicyContext::from_request(state, session).await;

    if policy.can_create_payload(&ctx, payload).await {
        Ok(())
    } else {
        Err(state.forbidden_response().into_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Debug, Clone, PartialEq)]
    struct Note {
        author_id: i64,
    }

    #[derive(Default)]
    struct AdminOrOwnerPolicy;

    impl Policy<Note> for AdminOrOwnerPolicy {
        fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _note: &'a Note) -> BoxFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn can_update<'a>(&'a self, ctx: &'a PolicyContext, note: &'a Note) -> BoxFuture<'a, bool> {
            Box::pin(
                async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(note.author_id) },
            )
        }
        fn can_delete<'a>(
            &'a self,
            ctx: &'a PolicyContext,
            _note: &'a Note,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async move { ctx.has_role("admin") })
        }
    }

    fn ctx(user_id: Option<&str>, role: Option<&str>) -> PolicyContext {
        let session = Session::new_for_test(String::new(), HashMap::new());
        PolicyContext {
            session,
            user_id: user_id.map(str::to_owned),
            roles: role.into_iter().map(str::to_owned).collect(),
            scopes: Vec::new(),
            #[cfg(feature = "db")]
            pool: None,
            policy_registry: PolicyRegistry::default(),
        }
    }

    #[tokio::test]
    async fn default_impls_deny() {
        struct EmptyPolicy;
        impl Policy<Note> for EmptyPolicy {}
        let policy = EmptyPolicy;
        let c = ctx(Some("1"), None);
        let n = Note { author_id: 1 };
        assert!(!policy.can_show(&c, &n).await);
        assert!(!policy.can_create(&c).await);
        assert!(!policy.can_update(&c, &n).await);
        assert!(!policy.can_delete(&c, &n).await);
        assert!(!policy.can("publish", &c, &n).await);
    }

    #[tokio::test]
    async fn owner_can_update() {
        let policy = AdminOrOwnerPolicy;
        let c = ctx(Some("42"), None);
        let n = Note { author_id: 42 };
        assert!(policy.can_update(&c, &n).await);
        assert!(!policy.can_delete(&c, &n).await);
    }

    #[tokio::test]
    async fn non_owner_cannot_update() {
        let policy = AdminOrOwnerPolicy;
        let c = ctx(Some("99"), None);
        let n = Note { author_id: 42 };
        assert!(!policy.can_update(&c, &n).await);
    }

    #[tokio::test]
    async fn admin_can_delete() {
        let policy = AdminOrOwnerPolicy;
        let c = ctx(Some("99"), Some("admin"));
        let n = Note { author_id: 42 };
        assert!(policy.can_delete(&c, &n).await);
    }

    #[tokio::test]
    async fn can_dispatches_named_actions() {
        let policy = AdminOrOwnerPolicy;
        let c = ctx(Some("42"), None);
        let n = Note { author_id: 42 };
        assert!(policy.can("show", &c, &n).await);
        assert!(policy.can("update", &c, &n).await);
        assert!(policy.can("edit", &c, &n).await);
        assert!(!policy.can("publish", &c, &n).await);
    }

    #[test]
    fn policy_registry_stores_and_resolves() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
        assert!(registry.has_policy::<Note>());
        assert!(registry.policy::<Note>().is_some());
        assert!(registry.scope::<Note>().is_none());
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn double_policy_registration_panics() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
    }

    #[test]
    fn forbidden_response_default_is_404() {
        let resp = ForbiddenResponse::default();
        assert_eq!(resp, ForbiddenResponse::NotFound404);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn forbidden_response_parses_strings() {
        assert_eq!(
            "403".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::Forbidden403
        );
        assert_eq!(
            "404".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::NotFound404
        );
        assert_eq!(
            "forbidden".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::Forbidden403
        );
        assert!("418".parse::<ForbiddenResponse>().is_err());
    }

    #[test]
    fn policy_context_helpers() {
        let c = ctx(Some("42"), Some("editor"));
        assert!(c.is_authenticated());
        assert_eq!(c.user_id_i64(), Some(42));
        assert!(c.has_role("editor"));
        assert!(!c.has_role("admin"));
        assert!(c.has_any_role(["admin", "editor"]));
        assert!(!c.has_any_role(["viewer", "guest"]));
    }

    #[test]
    fn anonymous_context_is_not_authenticated() {
        let c = ctx(None, None);
        assert!(!c.is_authenticated());
        assert!(c.user_id_i64().is_none());
        assert!(!c.has_role("admin"));
        assert!(!c.has_any_role(["admin", "editor"]));
    }

    #[test]
    fn user_id_i64_handles_non_numeric_session_value() {
        let c = ctx(Some("not-a-number"), None);
        assert!(c.user_id_i64().is_none());
    }

    #[test]
    fn scope_accessors_mirror_roles() {
        let c = ctx(Some("42"), Some("editor"))
            .with_scopes(vec!["posts:read".to_owned(), "posts:write".to_owned()]);
        assert!(c.has_scope("posts:read"));
        assert!(!c.has_scope("posts:delete"));
        assert!(c.has_any_scope(["posts:delete", "posts:write"]));
        assert!(!c.has_any_scope(["a", "b"]));
        assert!(c.has_all_scopes(["posts:read", "posts:write"]));
        assert!(!c.has_all_scopes(["posts:read", "posts:delete"]));
        // Empty requirement is vacuously satisfied.
        assert!(c.has_all_scopes(std::iter::empty::<&str>()));
    }

    #[test]
    fn non_user_principal_authorizes_purely_on_scopes() {
        // No user id, no roles — a pure service token — yet scopes authorize.
        let c = ctx(None, None).with_scopes(vec!["posts:write".to_owned()]);
        assert!(!c.is_authenticated());
        assert!(c.user_id_i64().is_none());
        assert!(!c.has_role("admin"));
        assert!(c.has_scope("posts:write"));
    }

    #[tokio::test]
    async fn from_session_leaves_scopes_empty() {
        let session = session_with(Some("42"), Some("editor"));
        let c = PolicyContext::from_session(&session, "user_id").await;
        assert!(c.scopes.is_empty());
        assert!(!c.has_scope("posts:read"));
    }

    #[test]
    fn forbidden_response_status_and_message_round_trip() {
        assert_eq!(
            ForbiddenResponse::Forbidden403.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ForbiddenResponse::NotFound404.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(ForbiddenResponse::Forbidden403.message(), "forbidden");
        assert_eq!(ForbiddenResponse::NotFound404.message(), "not found");
    }

    #[test]
    fn forbidden_response_into_error_carries_status_and_message() {
        let err = ForbiddenResponse::NotFound404.into_error();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.to_string(), "not found");

        let err = ForbiddenResponse::Forbidden403.into_error();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.to_string(), "forbidden");
    }

    #[test]
    fn forbidden_response_parses_empty_string_as_default_404() {
        assert_eq!(
            "".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::NotFound404
        );
        assert_eq!(
            "not_found".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::NotFound404
        );
        assert_eq!(
            "NotFound".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::NotFound404
        );
        assert_eq!(
            "Forbidden".parse::<ForbiddenResponse>().unwrap(),
            ForbiddenResponse::Forbidden403
        );
    }

    #[test]
    fn forbidden_response_parse_error_carries_input_value() {
        let err = "418".parse::<ForbiddenResponse>().unwrap_err();
        assert!(err.contains("418"));
        assert!(err.contains("403"));
        assert!(err.contains("404"));
    }

    #[test]
    fn forbidden_response_deserializes_from_toml() {
        #[derive(Debug, serde::Deserialize)]
        struct Holder {
            value: ForbiddenResponse,
        }
        let h: Holder = toml::from_str(r#"value = "403""#).unwrap();
        assert_eq!(h.value, ForbiddenResponse::Forbidden403);
        let h: Holder = toml::from_str(r#"value = "404""#).unwrap();
        assert_eq!(h.value, ForbiddenResponse::NotFound404);
        let err = toml::from_str::<Holder>(r#"value = "418""#).unwrap_err();
        assert!(err.to_string().contains("418"));
    }

    #[test]
    fn registry_scope_double_registration_panics_with_clear_message() {
        let registry = PolicyRegistry::default();
        registry.register_scope::<Note, _>(EmptyScope);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.register_scope::<Note, _>(EmptyScope);
        }))
        .unwrap_err();
        let msg = panicked
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panicked.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("already registered"),
            "expected double-registration panic, got {msg:?}"
        );
    }

    struct OtherResource;
    struct OtherPolicy;
    impl Policy<OtherResource> for OtherPolicy {}
    struct ThirdResource;
    struct EmptyScope;
    impl Scope<Note> for EmptyScope {}

    #[test]
    fn registry_resolves_distinct_resource_types_independently() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
        registry.register_policy::<OtherResource, _>(OtherPolicy);

        assert!(registry.has_policy::<Note>());
        assert!(registry.has_policy::<OtherResource>());
        // Resources without registrations don't false-positive.
        assert!(!registry.has_policy::<ThirdResource>());
        assert!(registry.scope::<Note>().is_none());
    }

    #[test]
    fn registry_debug_shows_counts() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
        registry.register_scope::<Note, _>(EmptyScope);
        let dbg = format!("{registry:?}");
        assert!(dbg.contains("PolicyRegistry"));
        assert!(dbg.contains("policies"));
        assert!(dbg.contains("scopes"));
    }

    fn detached_state_with(
        _registry: PolicyRegistry,
        forbidden: ForbiddenResponse,
    ) -> crate::AppState {
        crate::AppState::detached()
            .with_forbidden_response(forbidden)
            .with_auth_session_key("user_id")
    }

    fn session_with(user_id: Option<&str>, role: Option<&str>) -> Session {
        let mut data = HashMap::new();
        if let Some(u) = user_id {
            data.insert("user_id".to_owned(), u.to_owned());
        }
        if let Some(r) = role {
            data.insert("role".to_owned(), r.to_owned());
        }
        Session::new_for_test(String::new(), data)
    }

    #[tokio::test]
    async fn authorize_returns_500_when_no_policy_registered() {
        let state = detached_state_with(PolicyRegistry::default(), ForbiddenResponse::default());
        let session = session_with(Some("42"), None);
        let n = Note { author_id: 42 };
        let err = authorize::<Note>(&state, &session, "update", &n)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn authorize_returns_configured_deny_when_policy_denies() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<Note, _>(AdminOrOwnerPolicy);
        let state = detached_state_with(registry.clone(), ForbiddenResponse::Forbidden403);
        // Inject the registry into the live state's registry.
        let live = state.policy_registry();
        // Move registrations from `registry` into the state's registry.
        // (`detached()` starts with an empty registry; we copy in.)
        live.register_policy::<Note, _>(AdminOrOwnerPolicy);

        let session = session_with(Some("99"), None); // not the owner, no role
        let n = Note { author_id: 42 };
        let err = authorize::<Note>(&state, &session, "update", &n)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authorize_returns_ok_when_policy_allows() {
        let state = crate::AppState::detached();
        state
            .policy_registry()
            .register_policy::<Note, _>(AdminOrOwnerPolicy);
        let session = session_with(Some("42"), None); // owner
        let n = Note { author_id: 42 };
        authorize::<Note>(&state, &session, "update", &n)
            .await
            .expect("owner is allowed to update");
    }

    #[tokio::test]
    async fn authorize_create_returns_500_when_no_policy_registered() {
        let state = crate::AppState::detached();
        let session = session_with(Some("42"), None);
        let err = authorize_create::<Note>(&state, &session)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn authorize_create_dispatches_can_create() {
        struct AuthOnlyCreatePolicy;
        impl Policy<Note> for AuthOnlyCreatePolicy {
            fn can_create<'a>(&'a self, ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {
                Box::pin(async move { ctx.is_authenticated() })
            }
        }

        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(AuthOnlyCreatePolicy);

        let anon = session_with(None, None);
        let err = authorize_create::<Note>(&state, &anon).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);

        let user = session_with(Some("1"), None);
        authorize_create::<Note>(&state, &user)
            .await
            .expect("authenticated user passes can_create");
    }

    #[tokio::test]
    async fn authorize_create_payload_dispatches_can_create_payload() {
        struct OwnerPayloadPolicy;
        impl Policy<Note> for OwnerPayloadPolicy {
            fn can_create_payload<'a>(
                &'a self,
                ctx: &'a PolicyContext,
                payload: &'a serde_json::Value,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    payload.get("author_id").and_then(serde_json::Value::as_i64)
                        == ctx.user_id_i64()
                })
            }
        }

        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(OwnerPayloadPolicy);

        let user = session_with(Some("1"), None);
        let own_payload = serde_json::json!({"author_id": 1});
        authorize_create_payload::<Note>(&state, &user, &own_payload)
            .await
            .expect("owner payload passes can_create_payload");

        let other_payload = serde_json::json!({"author_id": 2});
        let err = authorize_create_payload::<Note>(&state, &user, &other_payload)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn check_policy_create_alias_preserves_two_arg_shape() {
        struct AuthOnlyCreatePolicy;
        impl Policy<Note> for AuthOnlyCreatePolicy {
            fn can_create<'a>(&'a self, ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {
                Box::pin(async move { ctx.is_authenticated() })
            }
        }

        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(AuthOnlyCreatePolicy);

        let anon = session_with(None, None);
        let err = __check_policy_create::<Note>(&state, &anon)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);

        let user = session_with(Some("1"), None);
        __check_policy_create::<Note>(&state, &user)
            .await
            .expect("old generated create policy alias remains compatible");
    }

    #[tokio::test]
    async fn check_policy_create_payload_alias_dispatches_payload() {
        struct OwnerPayloadPolicy;
        impl Policy<Note> for OwnerPayloadPolicy {
            fn can_create_payload<'a>(
                &'a self,
                ctx: &'a PolicyContext,
                payload: &'a serde_json::Value,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    payload.get("author_id").and_then(serde_json::Value::as_i64)
                        == ctx.user_id_i64()
                })
            }
        }

        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(OwnerPayloadPolicy);

        let user = session_with(Some("1"), None);
        let payload = serde_json::json!({"author_id": 1});
        __check_policy_create_payload::<Note>(&state, &user, &payload)
            .await
            .expect("new generated create policy alias passes payload");
    }

    #[tokio::test]
    async fn check_policy_alias_round_trips() {
        let state = crate::AppState::detached();
        state
            .policy_registry()
            .register_policy::<Note, _>(AdminOrOwnerPolicy);
        let session = session_with(Some("42"), None);
        let n = Note { author_id: 42 };
        // The macro-internal alias goes through `authorize` — exercise
        // the full round-trip.
        __check_policy::<Note>(&state, &session, "update", &n)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn from_request_clones_pool_and_registry_from_state() {
        let state = crate::AppState::detached();
        state
            .policy_registry()
            .register_policy::<Note, _>(AdminOrOwnerPolicy);
        let session = session_with(Some("7"), Some("admin"));
        let ctx = PolicyContext::from_request(&state, &session).await;
        assert_eq!(ctx.user_id.as_deref(), Some("7"));
        assert!(ctx.has_role("admin"));
        // The registry was cloned from state — `Note` resolves.
        assert!(ctx.policy_registry.has_policy::<Note>());
    }

    #[tokio::test]
    async fn scoped_blanket_trait_constructible_without_registered_scope() {
        let state = crate::AppState::detached();
        let session = session_with(Some("1"), None);
        let ctx = PolicyContext::from_request(&state, &session).await;
        // No scope registered for `Note`.
        let _query = Note::scope(&ctx);
        // The `db`-feature `load(&mut conn)` form is exercised by the
        // testcontainer suite; here we just confirm the registry-miss
        // surfaces only at `.load()` time, not at `scope(&ctx)` time.
        assert!(ctx.policy_registry.scope::<Note>().is_none());
    }

    // ── authorize_with_scopes ──────────────────────────────────────────────────

    #[tokio::test]
    async fn authorize_with_scopes_returns_500_when_no_policy_registered() {
        let state = crate::AppState::detached();
        let session = session_with(None, None);
        let err =
            authorize_with_scopes::<Note>(&state, &session, None, "update", &Note { author_id: 1 })
                .await
                .unwrap_err();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn authorize_with_scopes_returns_deny_when_policy_denies() {
        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(AdminOrOwnerPolicy);
        let session = session_with(Some("99"), None); // not owner, no admin role
        let n = Note { author_id: 42 };
        let err = authorize_with_scopes::<Note>(&state, &session, None, "update", &n)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authorize_with_scopes_threads_scopes_into_policy_context() {
        struct ScopeGatedPolicy;
        impl Policy<Note> for ScopeGatedPolicy {
            fn can_update<'a>(
                &'a self,
                ctx: &'a PolicyContext,
                _doc: &'a Note,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move { ctx.has_scope("posts:write") })
            }
        }

        let state = crate::AppState::detached();
        state
            .policy_registry()
            .register_policy::<Note, _>(ScopeGatedPolicy);
        let session = session_with(None, None);
        let n = Note { author_id: 1 };
        let scopes = crate::auth::ApiTokenScopes(vec!["posts:write".to_owned()]);

        // Scopes present → allow.
        authorize_with_scopes::<Note>(&state, &session, Some(&scopes), "update", &n)
            .await
            .expect("scope allows update");

        // No scopes → deny (default 404).
        authorize_with_scopes::<Note>(&state, &session, None, "update", &n)
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn from_request_parts_propagates_scopes() {
        let state = crate::AppState::detached();
        let session = session_with(Some("7"), Some("admin"));
        let scopes = crate::auth::ApiTokenScopes(vec!["posts:write".to_owned()]);
        let ctx = PolicyContext::from_request_parts(&state, &session, Some(&scopes)).await;
        assert_eq!(ctx.user_id.as_deref(), Some("7"));
        assert!(ctx.has_role("admin"));
        assert!(ctx.has_scope("posts:write"));
        assert!(!ctx.has_scope("posts:read"));
    }

    #[tokio::test]
    async fn from_request_parts_with_no_scopes_leaves_scopes_empty() {
        let state = crate::AppState::detached();
        let session = session_with(Some("7"), None);
        let ctx = PolicyContext::from_request_parts(&state, &session, None).await;
        assert!(ctx.scopes.is_empty());
    }

    #[tokio::test]
    async fn check_policy_scoped_round_trips_through_authorize_with_scopes() {
        let state = crate::AppState::detached();
        state
            .policy_registry()
            .register_policy::<Note, _>(AdminOrOwnerPolicy);
        let session = session_with(Some("42"), None); // owner
        let n = Note { author_id: 42 };
        __check_policy_scoped::<Note>(&state, &session, None, "update", &n)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_policy_create_payload_scoped_threads_scopes() {
        struct ScopedCreatePolicy;
        impl Policy<Note> for ScopedCreatePolicy {
            fn can_create_payload<'a>(
                &'a self,
                ctx: &'a PolicyContext,
                _payload: &'a serde_json::Value,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move { ctx.has_scope("posts:write") })
            }
        }

        let state =
            crate::AppState::detached().with_forbidden_response(ForbiddenResponse::Forbidden403);
        state
            .policy_registry()
            .register_policy::<Note, _>(ScopedCreatePolicy);
        let session = session_with(None, None);
        let payload = serde_json::json!({"title": "Hello"});
        let scopes = crate::auth::ApiTokenScopes(vec!["posts:write".to_owned()]);

        __check_policy_create_payload_scoped::<Note>(&state, &session, Some(&scopes), &payload)
            .await
            .expect("scope grants create");

        let err = __check_policy_create_payload_scoped::<Note>(&state, &session, None, &payload)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn check_policy_create_payload_scoped_returns_500_when_no_policy() {
        let state = crate::AppState::detached();
        let session = session_with(None, None);
        let err = __check_policy_create_payload_scoped::<Note>(
            &state,
            &session,
            None,
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
