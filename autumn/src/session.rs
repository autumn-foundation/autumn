//! Cookie-based session management for Autumn applications.
//!
//! Provides a [`Session`] extractor that gives handlers access to a
//! per-user key-value store backed by a server-side [`SessionStore`].
//! Session IDs are transmitted via a configurable cookie (default:
//! `autumn.sid`).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::session::Session;
//!
//! #[get("/dashboard")]
//! async fn dashboard(session: Session) -> AutumnResult<String> {
//!     let user = session.get("user_id").await.unwrap_or_default();
//!     Ok(format!("Hello, {user}"))
//! }
//!
//! #[post("/login")]
//! async fn login(session: Session) -> &'static str {
//!     session.insert("user_id", "alice").await;
//!     "logged in"
//! }
//!
//! #[post("/logout")]
//! async fn logout(session: Session) -> &'static str {
//!     session.clear().await;
//!     "logged out"
//! }
//! ```
//!
//! ## Architecture
//!
//! The session system has three components:
//!
//! 1. **[`SessionStore`]** trait -- pluggable storage backend.
//! 2. **[`MemoryStore`]** -- default in-memory implementation (suitable for
//!    development; data is lost on restart).
//! 3. **[`SessionLayer`]** -- Tower middleware that loads/saves sessions and
//!    manages the session cookie.
//!
//! ## Configuration
//!
//! Configure via `autumn.toml`:
//!
//! ```toml
//! [session]
//! backend = "memory"
//! cookie_name = "autumn.sid"
//! max_age_secs = 86400       # 24 hours
//! secure = true               # true by default
//! same_site = "Lax"
//!
//! [session.redis]
//! url = "redis://127.0.0.1:6379"
//! key_prefix = "autumn:sessions"
//! ```
//!
//! Or via environment variables: `AUTUMN_SESSION__BACKEND`,
//! `AUTUMN_SESSION__COOKIE_NAME`, `AUTUMN_SESSION__MAX_AGE_SECS`,
//! `AUTUMN_SESSION__REDIS__URL`, etc.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::{FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use http::HeaderValue;
use http::StatusCode;
use http::header::{COOKIE, SET_COOKIE};
use http::request::Parts;
use thiserror::Error;
use tokio::sync::RwLock;
use tower::{Layer, Service};
use uuid::Uuid;

// ── Session data ────────────────────────────────────────────────

/// A handle to the current request's session data.
///
/// Obtained via the [`Session`] Axum extractor. All reads and writes
/// go through interior mutability so the extractor can be shared.
///
/// Changes are written back to the store automatically when the
/// response is sent (via [`SessionLayer`]).
#[derive(Clone, Debug)]
pub struct Session {
    inner: Arc<RwLock<SessionInner>>,
}

#[derive(Debug)]
struct SessionInner {
    id: String,
    old_id: Option<String>,
    data: HashMap<String, String>,
    cookie_backed: bool,
    dirty: bool,
    destroyed: bool,
}

impl Session {
    /// Create a session for testing purposes.
    #[doc(hidden)]
    #[must_use]
    pub fn new_for_test(id: String, data: HashMap<String, String>) -> Self {
        Self::new_cookie_backed(id, data)
    }

    /// Create a fresh, non-cookie-backed session for testing purposes (i.e.
    /// as [`Session::is_cookie_backed`] would report for a request with no
    /// prior session cookie).
    #[doc(hidden)]
    #[must_use]
    pub fn new_for_test_without_cookie(id: String, data: HashMap<String, String>) -> Self {
        Self::new(id, data)
    }

    fn new(id: String, data: HashMap<String, String>) -> Self {
        Self::with_cookie_state(id, data, false)
    }

    fn new_cookie_backed(id: String, data: HashMap<String, String>) -> Self {
        Self::with_cookie_state(id, data, true)
    }

    fn with_cookie_state(id: String, data: HashMap<String, String>, cookie_backed: bool) -> Self {
        Self {
            inner: Arc::new(RwLock::new(SessionInner {
                id,
                old_id: None,
                data,
                cookie_backed,
                dirty: false,
                destroyed: false,
            })),
        }
    }

    /// Returns the session ID.
    pub async fn id(&self) -> String {
        self.inner.read().await.id.clone()
    }

    /// Returns whether this session ID came from a valid request cookie rather
    /// than being generated for the current request.
    pub async fn is_cookie_backed(&self) -> bool {
        self.inner.read().await.cookie_backed
    }

    /// Force this session to be persisted and its cookie (re)issued on the
    /// response, even though no session data was read or written.
    ///
    /// `SessionLayer` only saves the session and sends `Set-Cookie` when it
    /// is dirty; a request that only calls [`Session::id`] never dirties it.
    /// Call this before handing [`Session::id`]'s value to something that
    /// will later be used to identify this session (e.g. binding an external
    /// capability to it) — otherwise, for a session with no cookie yet, the
    /// browser never receives the cookie needed to present that same id on a
    /// later request.
    pub async fn touch(&self) {
        self.inner.write().await.dirty = true;
    }

    pub(crate) async fn has_pending_changes(&self) -> bool {
        let inner = self.inner.read().await;
        inner.dirty || inner.destroyed
    }

    /// Get a value from the session.
    pub async fn get(&self, key: &str) -> Option<String> {
        self.inner.read().await.data.get(key).cloned()
    }

    /// Insert a key-value pair into the session.
    pub async fn insert(&self, key: impl Into<String>, value: impl Into<String>) {
        let mut inner = self.inner.write().await;
        inner.data.insert(key.into(), value.into());
        inner.dirty = true;
    }

    /// Remove a key from the session, returning the previous value.
    pub async fn remove(&self, key: &str) -> Option<String> {
        let mut inner = self.inner.write().await;
        let val = inner.data.remove(key);
        if val.is_some() {
            inner.dirty = true;
        }
        val
    }

    /// Remove all data from the session (but keep the session ID).
    pub async fn clear(&self) {
        let mut inner = self.inner.write().await;
        inner.data.clear();
        inner.dirty = true;
    }

    /// Rotate the session ID, generating a new ID for the same data.
    ///
    /// This is critical to call during privilege elevation (e.g., login)
    /// to prevent Session Fixation attacks.
    pub async fn rotate_id(&self) {
        let mut inner = self.inner.write().await;
        let new_id = Uuid::new_v4().to_string();
        if inner.old_id.is_none() {
            inner.old_id = Some(inner.id.clone());
        }
        inner.id = new_id;
        inner.dirty = true;
    }

    /// Destroy the session entirely. A new session ID will be issued
    /// on the next request.
    pub async fn destroy(&self) {
        let mut inner = self.inner.write().await;
        inner.data.clear();
        inner.destroyed = true;
        inner.dirty = true;
    }

    /// Returns `true` if this session contains the given key.
    pub async fn contains_key(&self, key: &str) -> bool {
        self.inner.read().await.data.contains_key(key)
    }
}

impl<S> FromRequestParts<S> for Session
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let session = parts
            .extensions
            .get::<Self>()
            .cloned()
            .expect("SessionLayer must be installed to use the Session extractor");
        async move { Ok(session) }
    }
}

// ── Session store trait ─────────────────────────────────────────

/// Pluggable session storage backend.
///
/// Implement this trait to store sessions in Redis, a database, etc.
/// The default implementation is [`MemoryStore`].
pub trait SessionStore: Send + Sync + 'static {
    /// Load session data for the given ID. Returns `None` if the session
    /// does not exist or has expired.
    fn load(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<HashMap<String, String>>, SessionStoreError>> + Send;

    /// Save session data under the given ID.
    fn save(
        &self,
        id: &str,
        data: HashMap<String, String>,
    ) -> impl Future<Output = Result<(), SessionStoreError>> + Send;

    /// Delete session data for the given ID.
    fn destroy(&self, id: &str) -> impl Future<Output = Result<(), SessionStoreError>> + Send;
}

/// An error that occurred during a session store operation.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct SessionStoreError {
    message: String,
}

impl SessionStoreError {
    /// Create a new session store error from an underlying backend error.
    #[must_use]
    pub fn backend(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            message: format!("{operation} failed: {error}"),
        }
    }
}

// ── In-memory store ─────────────────────────────────────────────

/// In-memory session store. Suitable for development and testing.
///
/// All data is lost when the process exits. For production use,
/// implement [`SessionStore`] backed by Redis or a database.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    sessions: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
}

impl MemoryStore {
    /// Create a new empty in-memory session store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemoryStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        Ok(self.sessions.read().await.get(id).cloned())
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        self.sessions.write().await.insert(id.to_owned(), data);
        Ok(())
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        self.sessions.write().await.remove(id);
        Ok(())
    }
}

// ── Erasure bridge for runtime-installed custom stores ─────────
//
// `SessionStore` uses RPIT (`-> impl Future + Send`) and is therefore not
// dyn-compatible. To let `AppBuilder::with_session_store(impl SessionStore)`
// erase the concrete type into something `AppBuilder` can store and
// `apply_session_layer` can wrap into a `SessionLayer`, we keep a
// pub(crate) dyn-compatible `BoxedSessionStore` shadow trait with a blanket
// impl over any `SessionStore`, plus an `ArcSessionStore` newtype that
// satisfies `SessionStore` by delegating through the trait object. Users
// only see `SessionStore`; the bridge stays an implementation detail.

pub(crate) type BoxedLoadFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<Option<HashMap<String, String>>, SessionStoreError>> + Send + 'a,
    >,
>;
pub(crate) type BoxedUnitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), SessionStoreError>> + Send + 'a>>;

pub(crate) trait BoxedSessionStore: Send + Sync + 'static {
    fn boxed_load<'a>(&'a self, id: &'a str) -> BoxedLoadFuture<'a>;

    fn boxed_save<'a>(&'a self, id: &'a str, data: HashMap<String, String>) -> BoxedUnitFuture<'a>;

    fn boxed_destroy<'a>(&'a self, id: &'a str) -> BoxedUnitFuture<'a>;
}

impl<S: SessionStore> BoxedSessionStore for S {
    fn boxed_load<'a>(&'a self, id: &'a str) -> BoxedLoadFuture<'a> {
        Box::pin(SessionStore::load(self, id))
    }

    fn boxed_save<'a>(&'a self, id: &'a str, data: HashMap<String, String>) -> BoxedUnitFuture<'a> {
        Box::pin(SessionStore::save(self, id, data))
    }

    fn boxed_destroy<'a>(&'a self, id: &'a str) -> BoxedUnitFuture<'a> {
        Box::pin(SessionStore::destroy(self, id))
    }
}

#[derive(Clone)]
pub(crate) struct ArcSessionStore(pub(crate) Arc<dyn BoxedSessionStore>);

impl SessionStore for ArcSessionStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        self.0.boxed_load(id).await
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        self.0.boxed_save(id, data).await
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        self.0.boxed_destroy(id).await
    }
}

// ── Session configuration ───────────────────────────────────────

/// Configuration for session management.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `cookie_name` | `"autumn.sid"` |
/// | `max_age_secs` | `86400` (24 hours) |
/// | `backend` | `memory` |
/// | `secure` | `true` |
/// | `same_site` | `"Lax"` |
/// | `http_only` | `true` |
/// | `path` | `"/"` |
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionConfig {
    /// Storage backend used for session data.
    #[serde(default)]
    pub backend: SessionBackend,

    /// Name of the session cookie.
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,

    /// Maximum age of the session cookie in seconds.
    #[serde(default = "default_max_age_secs")]
    pub max_age_secs: u64,

    /// Whether the cookie should only be sent over HTTPS.
    #[serde(default = "default_true")]
    pub secure: bool,

    /// `SameSite` attribute for the cookie.
    #[serde(default = "default_same_site")]
    pub same_site: String,

    /// Whether the cookie should be inaccessible to JavaScript.
    #[serde(default = "default_true")]
    pub http_only: bool,

    /// Path scope for the cookie.
    #[serde(default = "default_path")]
    pub path: String,

    /// Suppress the production warning for process-local session storage.
    #[serde(default)]
    pub allow_memory_in_production: bool,

    /// Redis session backend configuration.
    #[serde(default)]
    pub redis: SessionRedisConfig,
}

/// Supported session storage backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionBackend {
    /// In-memory storage. Resets on application restart.
    #[default]
    Memory,
    /// Redis-backed storage. Suitable for production and multi-instance deployments.
    Redis,
}

impl SessionBackend {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Configuration specific to the Redis session backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionRedisConfig {
    /// The Redis connection URL (e.g. `redis://127.0.0.1:6379`).
    #[serde(default)]
    pub url: Option<String>,

    /// Prefix used for session keys in Redis. Defaults to `autumn:sessions`.
    #[serde(default = "default_redis_key_prefix")]
    pub key_prefix: String,
}

impl Default for SessionRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_redis_key_prefix(),
        }
    }
}

fn default_redis_key_prefix() -> String {
    "autumn:sessions".to_owned()
}

/// Represents the resolved plan for which session backend to initialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBackendPlan {
    /// Use the in-memory store.
    Memory {
        /// Whether to log a warning because memory sessions are used in production.
        warn_in_production: bool,
    },
    /// Use the Redis store.
    Redis {
        /// The validated Redis connection URL.
        url: String,
        /// The prefix to use for session keys.
        key_prefix: String,
    },
}

/// Errors that can occur when resolving the session backend configuration.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionBackendConfigError {
    /// Redis was selected, but no URL was provided.
    #[error("session.backend=redis requires session.redis.url")]
    MissingRedisUrl,
    /// The provided Redis URL could not be parsed.
    #[error("session.redis.url is not a valid Redis URL: {0}")]
    InvalidRedisUrl(String),
    /// Redis was selected, but the `redis` crate feature is not enabled.
    #[error("session.backend=redis requires the `redis` feature")]
    RedisFeatureDisabled,
}

fn default_cookie_name() -> String {
    "autumn.sid".to_owned()
}
const fn default_max_age_secs() -> u64 {
    86400
}
fn default_same_site() -> String {
    "Lax".to_owned()
}
const fn default_true() -> bool {
    true
}
fn default_path() -> String {
    "/".to_owned()
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            backend: SessionBackend::default(),
            cookie_name: default_cookie_name(),
            max_age_secs: default_max_age_secs(),
            secure: true,
            same_site: default_same_site(),
            http_only: default_true(),
            path: default_path(),
            allow_memory_in_production: false,
            redis: SessionRedisConfig::default(),
        }
    }
}

impl SessionConfig {
    /// Resolve the concrete session backend plan from config.
    ///
    /// # Errors
    ///
    /// Returns [`SessionBackendConfigError`] when the configured backend is
    /// incomplete or invalid, such as Redis without a URL.
    pub fn backend_plan(
        &self,
        profile: Option<&str>,
    ) -> Result<SessionBackendPlan, SessionBackendConfigError> {
        match self.backend {
            SessionBackend::Memory => Ok(SessionBackendPlan::Memory {
                warn_in_production: is_production_profile(profile)
                    && !self.allow_memory_in_production,
            }),
            SessionBackend::Redis => {
                let Some(url) = self.redis.url.clone().filter(|url| !url.trim().is_empty()) else {
                    return Err(SessionBackendConfigError::MissingRedisUrl);
                };

                #[cfg(feature = "redis")]
                {
                    if let Err(error) = redis::Client::open(url.clone()) {
                        return Err(SessionBackendConfigError::InvalidRedisUrl(
                            error.to_string(),
                        ));
                    }

                    Ok(SessionBackendPlan::Redis {
                        url,
                        key_prefix: self.redis.key_prefix.clone(),
                    })
                }

                #[cfg(not(feature = "redis"))]
                {
                    let _ = url;
                    Err(SessionBackendConfigError::RedisFeatureDisabled)
                }
            }
        }
    }
}

fn is_production_profile(profile: Option<&str>) -> bool {
    matches!(profile, Some("prod" | "production"))
}

// ── Cookie helpers ──────────────────────────────────────────────

/// Extract a named cookie value from the Cookie header.
pub(crate) fn get_cookie(headers: &http::HeaderMap, name: &str) -> Option<String> {
    let mut found_token = None;

    for cookie_header in headers.get_all(COOKIE) {
        let Ok(cookie_str) = cookie_header.to_str() else {
            continue;
        };

        for pair in cookie_str.split(';') {
            let pair = pair.trim();
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };

            if k.trim() != name {
                continue;
            }

            if found_token.is_some() {
                // Multiple cookies with the same name found.
                // This indicates a potential Cookie Tossing attack!
                // Reject by returning None.
                return None;
            }

            found_token = Some(v.trim().to_owned());
        }
    }
    found_token
}

/// Build a Set-Cookie header value.
fn build_set_cookie(config: &SessionConfig, session_id: &str) -> String {
    use std::fmt::Write;
    let mut cookie = format!(
        "{}={}; Path={}",
        config.cookie_name, session_id, config.path
    );
    let _ = write!(cookie, "; Max-Age={}", config.max_age_secs);
    if config.http_only {
        cookie.push_str("; HttpOnly");
    }
    if config.secure {
        cookie.push_str("; Secure");
    }
    let _ = write!(cookie, "; SameSite={}", config.same_site);
    cookie
}

/// Build a Set-Cookie header that expires the cookie immediately.
fn build_expire_cookie(config: &SessionConfig) -> String {
    format!(
        "{}=; Path={}; Max-Age=0; HttpOnly; SameSite={}",
        config.cookie_name, config.path, config.same_site
    )
}

// ── Session layer (Tower middleware) ────────────────────────────

/// Tower [`Layer`] that manages session loading, saving, and cookie handling.
///
/// Install this on the Autumn router to enable the [`Session`] extractor.
/// The framework installs it automatically when a session store is configured.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::session::{SessionLayer, MemoryStore, SessionConfig};
///
/// let layer = SessionLayer::new(MemoryStore::new(), SessionConfig::default());
/// ```
#[derive(Clone)]
pub struct SessionLayer<S: SessionStore> {
    store: Arc<S>,
    config: Arc<SessionConfig>,
    signing_keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
}

impl<S: SessionStore> SessionLayer<S> {
    /// Create a new session layer with the given store and configuration.
    pub fn new(store: S, config: SessionConfig) -> Self {
        Self {
            store: Arc::new(store),
            config: Arc::new(config),
            signing_keys: None,
        }
    }

    /// Attach signing keys so session cookies are HMAC-signed.
    ///
    /// When set, the cookie value becomes `{session_id}.{hmac_hex}`. Cookies
    /// without a valid HMAC are treated as absent (new session started).
    /// Previous keys (see `ResolvedSigningKeys`) are tried during verification
    /// so existing sessions remain valid across a key rotation.
    #[must_use]
    pub fn with_signing_keys(
        mut self,
        keys: Arc<crate::security::config::ResolvedSigningKeys>,
    ) -> Self {
        self.signing_keys = Some(keys);
        self
    }
}

impl<S: SessionStore + Clone, Inner> Layer<Inner> for SessionLayer<S> {
    type Service = SessionService<S, Inner>;

    fn layer(&self, inner: Inner) -> Self::Service {
        SessionService {
            inner,
            store: Arc::clone(&self.store),
            config: Arc::clone(&self.config),
            signing_keys: self.signing_keys.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`SessionLayer`].
#[derive(Clone)]
pub struct SessionService<S: SessionStore, Inner> {
    inner: Inner,
    store: Arc<S>,
    config: Arc<SessionConfig>,
    signing_keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
}

/// `true` when the response was produced by the request-timeout layer
/// cancelling the handler future (it stamps [`RequestDeadlineCancelled`]).
///
/// The session layer is applied *outer* to the timeout layer, so a cancelled
/// handler can leave the shared `Session` handle dirty with a partial mutation.
/// Persisting it would commit half-finished state (e.g. a login that set the
/// user id but never completed), so the caller skips session persistence
/// entirely when this returns `true`.
///
/// [`RequestDeadlineCancelled`]: crate::router::RequestDeadlineCancelled
fn response_was_deadline_cancelled(response: &Response) -> bool {
    response
        .extensions()
        .get::<crate::router::RequestDeadlineCancelled>()
        .is_some()
}

impl<St, Inner> Service<Request> for SessionService<St, Inner>
where
    St: SessionStore + Clone,
    Inner: Service<Request, Response = Response> + Clone + Send + 'static,
    Inner::Future: Send + 'static,
    Inner::Error: Send + 'static,
{
    type Response = Response;
    type Error = Inner::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[allow(clippy::too_many_lines)]
    fn call(&mut self, mut req: Request) -> Self::Future {
        let store = Arc::clone(&self.store);
        let config = Arc::clone(&self.config);
        let signing_keys = self.signing_keys.clone();
        let mut inner = self.inner.clone();
        // Swap to ensure correct poll_ready semantics
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            // 1. Extract or create session ID (verify HMAC if signing is active)
            let raw_cookie = get_cookie(req.headers(), &config.cookie_name);
            let existing_id: Option<String> = match (raw_cookie, &signing_keys) {
                (None, _) => None,
                (Some(raw), None) => Some(raw),
                (Some(raw), Some(keys)) => {
                    // Signed format: "{session_id}.{hmac_hex}"
                    if let Some((id, sig)) = raw.split_once('.') {
                        if keys.verify(id.as_bytes(), sig) {
                            Some(id.to_owned())
                        } else {
                            None // bad HMAC — treat as no session
                        }
                    } else {
                        None // unsigned cookie when signing is required
                    }
                }
            };

            let mut stale_cookie_session_id = None;
            let (session_id, data) = if let Some(ref id) = existing_id {
                match store.load(id).await {
                    Ok(Some(data)) => (id.clone(), data),
                    Ok(None) => {
                        stale_cookie_session_id = Some(id.clone());
                        (Uuid::new_v4().to_string(), HashMap::new())
                    }
                    Err(error) => return Ok(session_store_unavailable_response(&error)),
                }
            } else {
                (Uuid::new_v4().to_string(), HashMap::new())
            };

            // 2. Create session handle and insert into extensions
            let cookie_backed = existing_id.as_ref().is_some_and(|id| id == &session_id);
            let session = if cookie_backed {
                Session::new_cookie_backed(session_id.clone(), data)
            } else {
                Session::new(session_id.clone(), data)
            };
            let current_session_scope = cookie_backed.then(|| session_id.clone());
            req.extensions_mut()
                .insert(crate::idempotency::IdempotencySessionScope::new(
                    current_session_scope,
                    stale_cookie_session_id,
                ));
            req.extensions_mut().insert(session.clone());

            // 3. Call inner service
            let mut response = inner.call(req).await?;

            // 4. Save or destroy session — but skip persistence when the deadline
            // cancelled the handler, so a partial mutation isn't committed.
            if response_was_deadline_cancelled(&response) {
                return Ok(response);
            }

            let inner_guard = session.inner.read().await;
            if inner_guard.destroyed {
                if let Err(error) = store.destroy(&session_id).await {
                    crate::idempotency::keep_deferred_session_commit_locked(&mut response);
                    return Ok(session_store_unavailable_response(&error));
                }
                if let Ok(val) = HeaderValue::from_str(&build_expire_cookie(&config)) {
                    response.headers_mut().append(SET_COOKIE, val);
                }
                crate::idempotency::add_deferred_session_replay_key(
                    &response,
                    None,
                    inner_guard.cookie_backed,
                );
            } else if inner_guard.dirty {
                let data = inner_guard.data.clone();
                let sid = inner_guard.id.clone();
                let primary_replay_after_guard_denial =
                    inner_guard.cookie_backed && inner_guard.old_id.is_some();
                if let Some(ref old_id) = inner_guard.old_id
                    && let Err(error) = store.destroy(old_id).await
                {
                    drop(inner_guard);
                    crate::idempotency::keep_deferred_session_commit_locked(&mut response);
                    return Ok(session_store_unavailable_response(&error));
                }
                drop(inner_guard);
                if let Err(error) = store.save(&sid, data).await {
                    crate::idempotency::keep_deferred_session_commit_locked(&mut response);
                    return Ok(session_store_unavailable_response(&error));
                }
                // Sign the session ID when signing keys are active
                let cookie_value = signing_keys.as_ref().map_or_else(
                    || sid.clone(),
                    |keys| format!("{sid}.{}", keys.sign(sid.as_bytes())),
                );
                if let Ok(val) = HeaderValue::from_str(&build_set_cookie(&config, &cookie_value)) {
                    response.headers_mut().append(SET_COOKIE, val);
                }
                crate::idempotency::add_deferred_session_replay_key(
                    &response,
                    Some(&sid),
                    primary_replay_after_guard_denial,
                );
            }

            if crate::idempotency::finalize_deferred_session_commit(&mut response).is_err() {
                return Ok(crate::idempotency::persistence_failed_response());
            }

            Ok(response)
        })
    }
}

fn session_store_unavailable_response(error: &SessionStoreError) -> Response {
    tracing::error!("session store unavailable: {error}");
    (StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable").into_response()
}

pub(crate) fn apply_session_layer<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
    config: &SessionConfig,
    profile: Option<&str>,
    custom_store: Option<Arc<dyn BoxedSessionStore>>,
    signing_keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
) -> Result<axum::Router<S>, SessionBackendConfigError> {
    if let Some(store) = custom_store {
        tracing::debug!(
            "Custom session store installed via with_session_store(); skipping config-driven backend selection"
        );
        let mut layer = SessionLayer::new(ArcSessionStore(store), config.clone());
        if let Some(keys) = signing_keys {
            layer = layer.with_signing_keys(keys);
        }
        return Ok(router.layer(layer));
    }

    match config.backend_plan(profile)? {
        SessionBackendPlan::Memory { warn_in_production } => {
            if warn_in_production {
                tracing::warn!(
                    "prod profile is using in-memory sessions; set session.backend=redis or \
                     session.allow_memory_in_production=true to acknowledge the risk"
                );
            }
            let mut layer = SessionLayer::new(MemoryStore::new(), config.clone());
            if let Some(keys) = signing_keys {
                layer = layer.with_signing_keys(keys);
            }
            Ok(router.layer(layer))
        }
        SessionBackendPlan::Redis { .. } => {
            #[cfg(feature = "redis")]
            {
                let store = crate::session_redis::RedisStore::from_config(config)?;
                let mut layer = SessionLayer::new(store, config.clone());
                if let Some(keys) = signing_keys {
                    layer = layer.with_signing_keys(keys);
                }
                Ok(router.layer(layer))
            }

            #[cfg(not(feature = "redis"))]
            {
                let _ = router;
                Err(SessionBackendConfigError::RedisFeatureDisabled)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use http::Request as HttpRequest;
    use tower::ServiceExt;

    #[tokio::test]
    async fn touch_marks_the_session_dirty_without_reading_or_writing_data() {
        let session = Session::new_for_test_without_cookie("fresh-id".to_owned(), HashMap::new());
        assert!(
            !session.has_pending_changes().await,
            "sanity: not dirty yet"
        );

        session.touch().await;

        assert!(
            session.has_pending_changes().await,
            "touch() must dirty the session so SessionLayer persists it and sets the cookie"
        );
        assert_eq!(
            session.id().await,
            "fresh-id",
            "touch() must not change the id"
        );
    }

    /// Sentinel store for verifying that the type-erased `BoxedSessionStore`
    /// bridge actually delegates back to the user's `SessionStore` impl
    /// instead of silently picking up the default memory store.
    #[derive(Clone, Default)]
    struct SentinelStore {
        load_calls: Arc<RwLock<u32>>,
    }

    impl SessionStore for SentinelStore {
        async fn load(
            &self,
            _id: &str,
        ) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
            *self.load_calls.write().await += 1;
            // Return a recognisable session payload so the test can prove the
            // wrapper actually went through this impl.
            let mut data = HashMap::new();
            data.insert("from".to_owned(), "sentinel".to_owned());
            Ok(Some(data))
        }

        async fn save(
            &self,
            _id: &str,
            _data: HashMap<String, String>,
        ) -> Result<(), SessionStoreError> {
            Ok(())
        }

        async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn arc_session_store_wrapper_delegates_to_inner_session_store() {
        let inner = SentinelStore::default();
        let load_counter = inner.load_calls.clone();
        let arc: Arc<dyn BoxedSessionStore> = Arc::new(inner);
        let wrapper = ArcSessionStore(arc);

        let result = wrapper
            .load("session-id")
            .await
            .expect("wrapped store should succeed");

        assert_eq!(*load_counter.read().await, 1);
        assert_eq!(
            result
                .as_ref()
                .and_then(|m| m.get("from"))
                .map(String::as_str),
            Some("sentinel"),
            "wrapper must return data from the wrapped impl, not a default"
        );
    }

    #[tokio::test]
    async fn boxed_session_store_blanket_impl_works_for_any_session_store() {
        // Any SessionStore type erases via the BoxedSessionStore blanket impl.
        let store = SentinelStore::default();
        let boxed: Arc<dyn BoxedSessionStore> = Arc::new(store);
        let result = boxed.boxed_load("session-id").await.unwrap();
        assert!(result.is_some());
    }

    #[derive(Clone)]
    struct FailingStore {
        fail_on_load: bool,
        fail_on_save: bool,
        fail_on_destroy: bool,
    }

    impl SessionStore for FailingStore {
        async fn load(
            &self,
            _id: &str,
        ) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
            if self.fail_on_load {
                Err(SessionStoreError::backend("load", "boom"))
            } else {
                Ok(None)
            }
        }

        async fn save(
            &self,
            _id: &str,
            _data: HashMap<String, String>,
        ) -> Result<(), SessionStoreError> {
            if self.fail_on_save {
                Err(SessionStoreError::backend("save", "boom"))
            } else {
                Ok(())
            }
        }

        async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
            if self.fail_on_destroy {
                Err(SessionStoreError::backend("destroy", "boom"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn memory_store_save_and_load() {
        let store = MemoryStore::new();
        let mut data = HashMap::new();
        data.insert("user".into(), "alice".into());
        store.save("sess1", data).await.unwrap();

        let loaded = store.load("sess1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().get("user").unwrap(), "alice");
    }

    #[tokio::test]
    async fn memory_store_destroy() {
        let store = MemoryStore::new();
        store.save("sess1", HashMap::new()).await.unwrap();
        store.destroy("sess1").await.unwrap();
        assert!(store.load("sess1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_store_load_missing() {
        let store = MemoryStore::new();
        assert!(store.load("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_insert_and_get() {
        let session = Session::new("test".into(), HashMap::new());
        session.insert("key", "value").await;
        assert_eq!(session.get("key").await, Some("value".to_owned()));
    }

    #[tokio::test]
    async fn session_remove() {
        let mut data = HashMap::new();
        data.insert("key".into(), "value".into());
        let session = Session::new("test".into(), data);
        let removed = session.remove("key").await;
        assert_eq!(removed, Some("value".to_owned()));
        assert!(session.get("key").await.is_none());
    }

    #[tokio::test]
    async fn session_clear() {
        let mut data = HashMap::new();
        data.insert("a".into(), "1".into());
        data.insert("b".into(), "2".into());
        let session = Session::new("test".into(), data);
        session.clear().await;
        assert!(session.get("a").await.is_none());
        assert!(session.get("b").await.is_none());
    }

    #[tokio::test]
    async fn session_contains_key() {
        let mut data = HashMap::new();
        data.insert("exists".into(), "yes".into());
        let session = Session::new("test".into(), data);
        assert!(session.contains_key("exists").await);
        assert!(!session.contains_key("missing").await);
    }

    #[tokio::test]
    async fn session_destroy_marks_destroyed() {
        let session = Session::new("test".into(), HashMap::new());
        session.insert("key", "value").await;
        session.destroy().await;
        let inner = session.inner.read().await;
        let destroyed = inner.destroyed;
        let empty = inner.data.is_empty();
        drop(inner);
        assert!(destroyed);
        assert!(empty);
    }

    #[test]
    fn get_cookie_extracts_value() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("autumn.sid=abc123; other=xyz"),
        );
        assert_eq!(get_cookie(&headers, "autumn.sid"), Some("abc123".into()));
        assert_eq!(get_cookie(&headers, "other"), Some("xyz".into()));
        assert_eq!(get_cookie(&headers, "missing"), None);
    }

    #[test]
    fn get_cookie_rejects_multiple_cookies() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("autumn.sid=abc123; autumn.sid=xyz456"),
        );
        assert_eq!(get_cookie(&headers, "autumn.sid"), None);

        let mut headers2 = http::HeaderMap::new();
        headers2.append(COOKIE, HeaderValue::from_static("autumn.sid=abc123"));
        headers2.append(COOKIE, HeaderValue::from_static("autumn.sid=xyz456"));
        assert_eq!(get_cookie(&headers2, "autumn.sid"), None);
    }

    #[test]
    fn build_set_cookie_contains_required_parts() {
        let config = SessionConfig::default();
        let cookie = build_set_cookie(&config, "test-id");
        assert!(cookie.contains("autumn.sid=test-id"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=86400"));
    }

    #[test]
    fn build_expire_cookie_has_zero_max_age() {
        let config = SessionConfig::default();
        let cookie = build_expire_cookie(&config);
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn session_config_defaults() {
        let config = SessionConfig::default();
        assert_eq!(config.backend, SessionBackend::Memory);
        assert_eq!(config.cookie_name, "autumn.sid");
        assert_eq!(config.max_age_secs, 86400);
        assert!(config.secure);
        assert_eq!(config.same_site, "Lax");
        assert!(config.http_only);
        assert_eq!(config.path, "/");
        assert!(!config.allow_memory_in_production);
        assert!(config.redis.url.is_none());
        assert_eq!(config.redis.key_prefix, "autumn:sessions");
    }

    #[test]
    fn session_backend_plan_warns_for_prod_memory_without_ack() {
        let config = SessionConfig::default();
        let plan = config.backend_plan(Some("prod")).unwrap();
        assert_eq!(
            plan,
            SessionBackendPlan::Memory {
                warn_in_production: true
            }
        );
    }

    #[test]
    fn session_backend_plan_suppresses_prod_warning_when_acknowledged() {
        let config = SessionConfig {
            allow_memory_in_production: true,
            ..SessionConfig::default()
        };
        let plan = config.backend_plan(Some("prod")).unwrap();
        assert_eq!(
            plan,
            SessionBackendPlan::Memory {
                warn_in_production: false
            }
        );
    }

    #[test]
    fn session_backend_plan_requires_redis_url() {
        let config = SessionConfig {
            backend: SessionBackend::Redis,
            ..SessionConfig::default()
        };
        let error = config.backend_plan(None).unwrap_err();
        assert_eq!(error, SessionBackendConfigError::MissingRedisUrl);
    }

    #[tokio::test]
    async fn session_layer_sets_cookie_on_new_session() {
        use crate::state::AppState;
        async fn handler(session: Session) -> String {
            session.insert("visited", "true").await;
            "ok".to_owned()
        }

        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: false,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };

        let app = Router::new()
            .route("/", get(handler))
            .layer(SessionLayer::new(
                MemoryStore::new(),
                SessionConfig::default(),
            ))
            .with_state(state);

        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::OK);
        let set_cookie = response
            .headers()
            .get(SET_COOKIE)
            .expect("should set session cookie");
        let cookie_str = set_cookie.to_str().unwrap();
        assert!(cookie_str.contains("autumn.sid="));
    }

    fn test_state() -> crate::state::AppState {
        crate::state::AppState {
            extensions: Arc::new(std::sync::RwLock::new(HashMap::new())),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: false,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        }
    }

    #[tokio::test]
    async fn session_layer_persists_data_across_requests() {
        async fn write_handler(session: Session) -> String {
            session.insert("user", "alice").await;
            "saved".to_owned()
        }

        async fn read_handler(session: Session) -> String {
            session.get("user").await.unwrap_or_default()
        }

        let store = MemoryStore::new();
        let config = SessionConfig::default();
        let state = test_state();

        let app = Router::new()
            .route("/write", get(write_handler))
            .route("/read", get(read_handler))
            .layer(SessionLayer::new(store, config))
            .with_state(state);

        // First request: write to session
        let resp1 = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/write")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let cookie = resp1
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // Extract just the cookie value for the next request
        let session_cookie = cookie.split(';').next().unwrap();

        // Second request: read from session
        let resp2 = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/read")
                    .header(COOKIE, session_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "alice");
    }

    #[tokio::test]
    async fn session_destroy_expires_cookie() {
        async fn handler(session: Session) -> String {
            session.destroy().await;
            "destroyed".to_owned()
        }

        let state = test_state();

        let store = MemoryStore::new();
        store
            .save("existing-id", HashMap::from([("k".into(), "v".into())]))
            .await
            .unwrap();

        let app = Router::new()
            .route("/", get(handler))
            .layer(SessionLayer::new(store.clone(), SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=existing-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let cookie = response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("Max-Age=0"), "cookie should be expired");

        // Store should no longer have the session
        assert!(store.load("existing-id").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_layer_returns_503_when_store_load_fails() {
        let state = test_state();

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SessionLayer::new(
                FailingStore {
                    fail_on_load: true,
                    fail_on_save: false,
                    fail_on_destroy: false,
                },
                SessionConfig::default(),
            ))
            .with_state(state);

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=existing-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn session_layer_returns_503_when_store_save_fails() {
        let state = test_state();

        let app = Router::new()
            .route(
                "/",
                get(|session: Session| async move {
                    session.insert("user", "alice").await;
                    "ok"
                }),
            )
            .layer(SessionLayer::new(
                FailingStore {
                    fail_on_load: false,
                    fail_on_save: true,
                    fail_on_destroy: false,
                },
                SessionConfig::default(),
            ))
            .with_state(state);

        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn deadline_cancelled_response_skips_partial_session_save() {
        let state = test_state();
        let store = MemoryStore::new();
        // Seed an existing cookie-backed session with no application data.
        store
            .save("existing-id", HashMap::new())
            .await
            .expect("seed save");

        // A handler that mutates the session and then exceeds an inner deadline,
        // standing in for the real request-timeout layer (which is applied inner
        // to the session layer and stamps `RequestDeadlineCancelled` on its 503).
        let timeout_layer = axum::middleware::from_fn(
            |req: HttpRequest<Body>, next: axum::middleware::Next| async move {
                tokio::time::timeout(std::time::Duration::from_millis(50), next.run(req))
                    .await
                    .unwrap_or_else(|_| {
                        let mut resp = StatusCode::SERVICE_UNAVAILABLE.into_response();
                        resp.extensions_mut()
                            .insert(crate::router::RequestDeadlineCancelled);
                        resp
                    })
            },
        );

        let app = Router::new()
            .route(
                "/",
                get(|session: Session| async move {
                    session.insert("user", "alice").await;
                    // Far exceeds the 50ms inner deadline; the handler future is
                    // cancelled before it returns.
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    "ok"
                }),
            )
            .layer(timeout_layer)
            .layer(SessionLayer::new(store.clone(), SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=existing-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // The partial mutation must NOT have been persisted: the stored session
        // is still the seeded empty map, with no `user` key.
        let saved = store
            .load("existing-id")
            .await
            .expect("load")
            .expect("session still present");
        assert!(
            !saved.contains_key("user"),
            "a deadline-cancelled request must not persist partial session changes"
        );
        // The session layer must also not emit a Set-Cookie for the skipped save.
        assert!(
            response.headers().get(SET_COOKIE).is_none(),
            "no Set-Cookie should be written when the partial save is skipped"
        );
    }

    // ── Signed session cookies (RED phase) ─────────────────────────────────

    #[tokio::test]
    async fn session_cookie_is_signed_when_signing_keys_set() {
        use crate::security::config::{SigningSecretConfig, resolve_signing_keys};
        use std::sync::Arc;

        let config = SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        };
        let keys = Arc::new(resolve_signing_keys(&config));

        let app = Router::new()
            .route(
                "/",
                get(|s: Session| async move {
                    s.insert("k", "v").await;
                    "ok"
                }),
            )
            .layer(
                SessionLayer::new(MemoryStore::new(), SessionConfig::default())
                    .with_signing_keys(keys),
            );

        let req = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();

        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .expect("should set cookie")
            .to_str()
            .unwrap();
        let cookie_value = set_cookie
            .split('=')
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim();

        assert!(
            cookie_value.contains('.'),
            "signed session cookie must be {{id}}.{{hmac}}, got: {cookie_value}"
        );
        let (id_part, sig_part) = cookie_value.split_once('.').unwrap();
        assert!(!id_part.is_empty());
        assert_eq!(sig_part.len(), 64, "HMAC-SHA256 hex must be 64 chars");
    }

    #[tokio::test]
    async fn session_layer_rejects_tampered_cookie() {
        use crate::security::config::{SigningSecretConfig, resolve_signing_keys};
        use std::sync::Arc;

        let keys = Arc::new(resolve_signing_keys(&SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        }));

        let store = MemoryStore::new();
        let session_id = Uuid::new_v4().to_string();
        let mut data = HashMap::new();
        data.insert("user".to_owned(), "alice".to_owned());
        store.save(&session_id, data).await.unwrap();

        let app = Router::new()
            .route(
                "/",
                get(|s: Session| async move {
                    s.get("user").await.unwrap_or_else(|| "none".to_owned())
                }),
            )
            .layer(SessionLayer::new(store, SessionConfig::default()).with_signing_keys(keys));

        // Valid UUID but bad 64-char hex HMAC
        let bad_sig = "0".repeat(64);
        let bad_cookie = format!("autumn.sid={session_id}.{bad_sig}");
        let req = HttpRequest::builder()
            .uri("/")
            .header("cookie", bad_cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"none", "tampered cookie must not load session");
    }

    #[tokio::test]
    async fn session_layer_accepts_previous_key_signed_cookie() {
        use crate::security::config::{
            ResolvedSigningKeys, SigningSecretConfig, resolve_signing_keys,
        };
        use std::sync::Arc;

        let old_secret = "old-key".repeat(5); // 35 bytes
        let old_keys = resolve_signing_keys(&SigningSecretConfig {
            secret: Some(old_secret.clone()),
            previous_secrets: vec![],
        });

        let session_id = Uuid::new_v4().to_string();
        let old_sig = old_keys.sign(session_id.as_bytes());
        let signed_value = format!("{session_id}.{old_sig}");

        let new_keys = Arc::new(ResolvedSigningKeys::new(
            "new-key".repeat(5).into_bytes(),
            vec![old_secret.into_bytes()],
        ));

        let store = MemoryStore::new();
        let mut data = HashMap::new();
        data.insert("user".to_owned(), "bob".to_owned());
        store.save(&session_id, data).await.unwrap();

        let app = Router::new()
            .route(
                "/",
                get(|s: Session| async move {
                    s.get("user").await.unwrap_or_else(|| "none".to_owned())
                }),
            )
            .layer(SessionLayer::new(store, SessionConfig::default()).with_signing_keys(new_keys));

        let req = HttpRequest::builder()
            .uri("/")
            .header("cookie", format!("autumn.sid={signed_value}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(
            &body[..],
            b"bob",
            "previous-key-signed cookie must load session"
        );
    }
}
