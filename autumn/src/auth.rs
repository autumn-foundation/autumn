//! Authentication utilities for Autumn applications.
//!
//! Provides password hashing, an [`Auth<T>`] extractor for retrieving the
//! authenticated user, and a [`RequireAuth`] middleware layer for protecting
//! routes.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::auth::{Auth, hash_password, verify_password};
//! use autumn_web::session::Session;
//!
//! #[derive(Clone)]
//! struct User { id: i64, name: String }
//!
//! #[post("/register")]
//! async fn register() -> AutumnResult<&'static str> {
//!     let hashed = hash_password("secret123").await?;
//!     // Save hashed password to database...
//!     Ok("registered")
//! }
//!
//! #[post("/login")]
//! async fn login(session: Session) -> AutumnResult<&'static str> {
//!     // Verify credentials...
//!     let stored_hash = "$2b$12$..."; // from database
//!     if verify_password("secret123", stored_hash).await? {
//!         session.insert("user_id", "42").await;
//!         Ok("logged in")
//!     } else {
//!         Err(AutumnError::bad_request_msg("invalid credentials"))
//!     }
//! }
//! ```
//!
//! ## Password hashing
//!
//! Uses bcrypt with a default cost of 12. The [`hash_password`] and
//! [`verify_password`] functions are simple wrappers that return
//! [`AutumnResult`](crate::AutumnResult).
//!
//! ## The `Auth<T>` extractor
//!
//! [`Auth<T>`] extracts the authenticated user from request extensions.
//! It is typically populated by a custom middleware which might call
//! `request.extensions_mut().insert(user)` in a handler. Returns `401 Unauthorized` if no
//! user is present.
//!
//! ## Route protection with `RequireAuth`
//!
//! The [`RequireAuth`] layer rejects unauthenticated requests with
//! `401 Unauthorized` before they reach the handler. It checks for the
//! presence of a session key (default: `"user_id"`).

#[cfg(feature = "oauth2")]
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
#[cfg(feature = "oauth2")]
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;
#[cfg(feature = "oauth2")]
use jsonwebtoken::jwk::JwkSet;
#[cfg(feature = "oauth2")]
use serde::Deserialize;
#[cfg(feature = "oauth2")]
use url::Url;

// ── Password hashing ────────────────────────────────────────────

/// Default bcrypt cost factor.
const DEFAULT_BCRYPT_COST: u32 = 12;

/// Hash a plaintext password using bcrypt.
///
/// Returns the hashed password string suitable for database storage.
///
/// # Errors
///
/// Returns an error if bcrypt hashing fails (extremely unlikely).
///
/// # Examples
///
/// ```rust
/// use autumn_web::auth::hash_password;
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let hashed = hash_password("my_secret").await.unwrap();
/// assert!(hashed.starts_with("$2b$"));
/// # });
/// ```
pub async fn hash_password(password: &str) -> crate::AutumnResult<String> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        bcrypt::hash(password, DEFAULT_BCRYPT_COST)
            .map_err(|e| crate::AutumnError::from(std::io::Error::other(e.to_string())))
    })
    .await
    .map_err(|e| crate::AutumnError::from(std::io::Error::other(e.to_string())))?
}

/// Verify a plaintext password against a bcrypt hash.
///
/// Returns `true` if the password matches the hash.
///
/// # Errors
///
/// Returns an error if bcrypt verification fails (e.g., invalid hash format).
///
/// # Examples
///
/// ```rust
/// use autumn_web::auth::{hash_password, verify_password};
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let hashed = hash_password("my_secret").await.unwrap();
/// assert!(verify_password("my_secret", &hashed).await.unwrap());
/// assert!(!verify_password("wrong_password", &hashed).await.unwrap());
/// # });
/// ```
pub async fn verify_password(password: &str, hash: &str) -> crate::AutumnResult<bool> {
    let password = password.to_string();

    // Parse the hash format outside the blocking task.
    // A valid bcrypt hash is typically 60 characters and starts with "$".
    let is_valid_format = hash.len() == 60 && hash.starts_with('$');

    let hash_to_verify = if is_valid_format {
        hash.to_string()
    } else {
        // To prevent timing attacks, perform a dummy verification against a known hash.
        "$2b$12$KIXe8K4j1sH6/xH.x9d71uJ5Jk8t6O4m6Q110g4H8y1r6J6O6O6O6".to_string()
    };

    let result = tokio::task::spawn_blocking(move || bcrypt::verify(&password, &hash_to_verify))
        .await
        .map_err(|e| crate::AutumnError::from(std::io::Error::other(e.to_string())))?;

    if !is_valid_format {
        return Ok(false);
    }

    result.map_err(|e| crate::AutumnError::from(std::io::Error::other(e.to_string())))
}

// ── Runtime check for #[secured] macro ──────────────────────────

/// Runtime authentication and authorization check used by the
/// `#[secured]` proc macro. **Not intended for direct use** -- use
/// `#[secured]` instead.
///
/// Checks the session for the configured auth key (default: `"user_id"`).
/// If `roles` is non-empty, also checks that the session's `"role"` value
/// matches at least one of the given roles.
///
/// Returns `401 Unauthorized` if not authenticated, or `403 Forbidden`
/// if the user lacks the required role.
#[doc(hidden)]
pub async fn __check_secured(
    session: &crate::session::Session,
    roles: &[&str],
) -> crate::AutumnResult<()> {
    __check_secured_with_key(session, "user_id", roles).await
}

/// Runtime check used by `#[secured]` when `AppState` is available.
///
/// Accepts the configured auth session key so generated login/signup/reset
/// handlers and `#[secured]` resolve authentication through the same session
/// entry.
#[doc(hidden)]
pub async fn __check_secured_with_key(
    session: &crate::session::Session,
    auth_session_key: &str,
    roles: &[&str],
) -> crate::AutumnResult<()> {
    // Check authentication: session must contain the auth key
    let Some(user_id) = session.get(auth_session_key).await else {
        return Err(crate::AutumnError::unauthorized_msg(
            "authentication required",
        ));
    };

    // Tag the request-scoped log context (#1169) with the authenticated user
    // so every subsequent event automatically carries `user_id`.
    crate::log::context::set_user_id(user_id);

    // Check authorization: if roles are specified, the session's "role"
    // must match at least one of them
    if !roles.is_empty() {
        let user_role = session.get("role").await.unwrap_or_default();
        if !roles.iter().any(|&r| r == user_role) {
            return Err(crate::AutumnError::forbidden_msg(
                "insufficient permissions",
            ));
        }
    }

    Ok(())
}

/// Runtime scope check used by `#[secured(scopes = [...])]`. **Not intended for
/// direct use** — use `#[secured]` instead.
///
/// Default-deny: with a non-empty `required_scopes`, every required scope must
/// be present in the authenticating token's granted scopes, otherwise `403
/// Forbidden`. An empty requirement is a no-op (`Ok`). A token with no granted
/// scopes (`granted == None`) is denied whenever a scope is required, so a pure
/// service token that lacks the scope is rejected.
// Async (with no await) to mirror `__check_secured_with_key`, so the macro can
// uniformly `.await` whichever check it emits.
#[doc(hidden)]
#[allow(clippy::unused_async)]
pub async fn __check_secured_scopes(
    granted: Option<&ApiTokenScopes>,
    required_scopes: &[&str],
) -> crate::AutumnResult<()> {
    if required_scopes.is_empty() {
        return Ok(());
    }
    let granted: &[String] = granted.map_or(&[], |g| g.0.as_slice());
    if required_scopes
        .iter()
        .all(|req| granted.iter().any(|g| g == req))
    {
        Ok(())
    } else {
        Err(crate::AutumnError::forbidden_msg("insufficient scope"))
    }
}

// ── Auth<T> extractor ───────────────────────────────────────────

/// Extractor that retrieves the authenticated user from request extensions.
///
/// Handlers can declare `Auth<MyUser>` as a parameter to access the
/// current user. If no user is present in the request extensions,
/// a `401 Unauthorized` response is returned automatically.
///
/// ## Populating the user
///
/// The user is typically inserted into request extensions by middleware.
/// For example, a custom middleware can load the user from the session
/// and call `request.extensions_mut().insert(user)`.
///
/// ## Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::auth::Auth;
///
/// #[derive(Clone)]
/// struct CurrentUser { id: i64, name: String }
///
/// #[get("/profile")]
/// async fn profile(Auth(user): Auth<CurrentUser>) -> String {
///     format!("Hello, {}!", user.name)
/// }
/// ```
pub struct Auth<T>(pub T);

impl<T, S> FromRequestParts<S> for Auth<T>
where
    T: Clone + Send + Sync + 'static,
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let user = parts.extensions.get::<T>().cloned();
        async move { user.map_or_else(|| Err(AuthRejection), |user| Ok(Self(user))) }
    }
}

/// Rejection type for [`Auth<T>`] when no authenticated user is present.
#[derive(Debug)]
pub struct AuthRejection;

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        crate::AutumnError::unauthorized_msg("authentication required").into_response()
    }
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authentication required")
    }
}

// ── RequireAuth middleware ───────────────────────────────────────

/// Tower [`tower::Layer`] that rejects unauthenticated requests with `401`.
///
/// Checks for a specific key in the session to determine if the request
/// is authenticated. If the key is missing, the request is rejected before
/// reaching the handler.
///
/// On success, the resolved principal value is also inserted as a
/// [`crate::security::RateLimitPrincipal`] extension so that
/// `key_strategy = "authenticated_principal"` works out of the box.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::auth::RequireAuth;
/// use autumn_web::reexports::axum::{Router, routing::get};
/// use autumn_web::AppState;
///
/// // Protect all routes under /admin
/// let admin_routes = Router::<AppState>::new()
///     .route("/dashboard", get(|| async { "admin" }))
///     .layer(RequireAuth::new("user_id"));
/// ```
#[derive(Clone)]
pub struct RequireAuth {
    session_key: Arc<str>,
}

impl RequireAuth {
    /// Create a new `RequireAuth` layer that checks for the given session key.
    pub fn new(session_key: impl Into<String>) -> Self {
        Self {
            session_key: Arc::from(session_key.into()),
        }
    }
}

impl<S> tower::Layer<S> for RequireAuth {
    type Service = RequireAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireAuthService {
            inner,
            session_key: Arc::clone(&self.session_key),
        }
    }
}

/// Tower [`tower::Service`] produced by [`RequireAuth`].
#[derive(Clone)]
pub struct RequireAuthService<S> {
    inner: S,
    session_key: Arc<str>,
}

impl<S, ResBody> tower::Service<axum::extract::Request> for RequireAuthService<S>
where
    S: tower::Service<axum::extract::Request, Response = Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: From<String> + Default + Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        let session_key = Arc::clone(&self.session_key);
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            // Check if session has the required key
            let session = req.extensions().get::<crate::session::Session>().cloned();

            let user_id = if let Some(ref session) = session {
                session.get(&session_key).await
            } else {
                None
            };

            if let Some(user_id) = user_id {
                // Fulfil the RateLimitPrincipal contract so key_strategy =
                // "authenticated_principal" works without an extra middleware shim.
                req.extensions_mut()
                    .insert(crate::security::RateLimitPrincipal(user_id.clone()));
                // Tag the request-scoped log context (#1169) so handler logs for
                // middleware-authenticated requests carry `user_id` too, matching
                // the `#[secured]` path.
                crate::log::context::set_user_id(user_id);
                inner.call(req).await
            } else {
                let body = crate::error::problem_details_json_string(
                    StatusCode::UNAUTHORIZED,
                    "authentication required",
                    None,
                    None,
                    req.extensions()
                        .get::<crate::middleware::RequestId>()
                        .map(std::string::ToString::to_string),
                    Some(req.uri().path().to_owned()),
                    true,
                );
                let response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(http::header::CONTENT_TYPE, "application/problem+json")
                    .body(ResBody::from(body))
                    .unwrap_or_default();
                Ok(response)
            }
        })
    }
}

// ── Auth configuration ──────────────────────────────────────────

/// Configuration for authentication.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `bcrypt_cost` | `12` |
/// | `session_key` | `"user_id"` |
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthConfig {
    /// Bcrypt cost factor for password hashing.
    #[serde(default = "default_bcrypt_cost")]
    pub bcrypt_cost: u32,

    /// Session key used to identify authenticated users.
    #[serde(default = "default_session_key")]
    pub session_key: String,

    /// OAuth2/OIDC provider configuration by provider key
    /// (for example: `github`, `google`, `okta`).
    #[cfg(feature = "oauth2")]
    #[serde(default)]
    pub oauth2: OAuth2Config,

    /// Account-linking policy for unknown OAuth2/OIDC identities.
    ///
    /// - `create_account` (default): a new local account is created on first sign-in.
    /// - `require_local_signup_first`: returns an error unless the user already has
    ///   a local account linked to their provider identity.
    #[cfg(feature = "oauth2")]
    #[serde(default)]
    pub oauth_linking_policy: OAuthLinkingPolicy,

    /// `WebAuthn` / passkey configuration.
    ///
    /// Required when using `autumn generate auth --passkeys`. Set in `autumn.toml`:
    ///
    /// ```toml
    /// [auth.webauthn]
    /// rp_id = "example.com"
    /// rp_name = "My App"
    /// rp_origin = "https://example.com"
    /// ```
    #[cfg(feature = "webauthn")]
    #[serde(default)]
    pub webauthn: WebAuthnConfig,

    /// Account lockout policy for the generated login endpoint.
    ///
    /// Protects individual accounts from credential-stuffing attacks by locking
    /// them after a burst of failed login attempts, even when those attempts
    /// arrive from rotating source IPs.
    ///
    /// Configure in `autumn.toml`:
    ///
    /// ```toml
    /// [auth.lockout]
    /// enabled = true          # set to false to disable (e.g. when using external policy)
    /// threshold = 10          # failed attempts before lockout
    /// window_secs = 60        # sliding window for counting failures
    /// cooloff_secs = 900      # lock duration in seconds (15 minutes)
    /// ```
    ///
    /// Set `threshold = 0` or `enabled = false` to disable lockout entirely and
    /// restore pre-lockout behaviour for apps with a stronger external policy.
    #[serde(default)]
    pub lockout: LockoutConfig,

    /// Step-up ("sudo mode") authentication configuration.
    ///
    /// Controls the global default freshness window for `#[step_up]`-protected
    /// routes. Individual routes can override the default with
    /// `#[step_up(max_age = "Nm")]`.
    ///
    /// Configure in `autumn.toml`:
    ///
    /// ```toml
    /// [auth.step_up]
    /// default_max_age_secs = 300  # 5 minutes (default)
    /// ```
    #[serde(default)]
    pub step_up: StepUpConfig,

    /// Active-session tracking and revocation policy (issue #819).
    ///
    /// Controls whether credential-changing events (password change, TOTP
    /// enrollment/disable, `WebAuthn` key add/remove) revoke all *other*
    /// login sessions (default: on), and how often `last_seen_at` is
    /// written per session.
    ///
    /// Configure in `autumn.toml`:
    ///
    /// ```toml
    /// [auth.sessions]
    /// revoke_on_credential_change = true
    /// last_seen_update_secs = 60
    /// ```
    #[serde(default)]
    pub sessions: SessionTrackingConfig,
}

/// Account lockout policy configuration.
///
/// Read from the `[auth.lockout]` section of `autumn.toml`.
/// All fields have safe production defaults.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LockoutConfig {
    /// Whether account lockout is enabled (default: `true`).
    ///
    /// Set to `false` to disable lockout globally without removing the columns.
    #[serde(default = "default_lockout_enabled")]
    pub enabled: bool,

    /// Number of consecutive failed login attempts before an account is locked
    /// (default: `10`). Set to `0` to disable lockout.
    #[serde(default = "default_lockout_threshold")]
    pub threshold: i32,

    /// Sliding window in seconds over which `threshold` failures trigger lockout
    /// (default: `60`). Reserved for future per-window counting; current
    /// implementation counts all failures since the last successful login.
    #[serde(default = "default_lockout_window_secs")]
    pub window_secs: u64,

    /// Cool-off period in seconds before a locked account is automatically
    /// unlocked (default: `900`, i.e. 15 minutes). An account also unlocks
    /// immediately on the first successful login after cool-off elapses.
    #[serde(default = "default_lockout_cooloff_secs")]
    pub cooloff_secs: u64,
}

const fn default_lockout_enabled() -> bool {
    true
}

const fn default_lockout_threshold() -> i32 {
    10
}

const fn default_lockout_window_secs() -> u64 {
    60
}

const fn default_lockout_cooloff_secs() -> u64 {
    900
}

impl Default for LockoutConfig {
    fn default() -> Self {
        Self {
            enabled: default_lockout_enabled(),
            threshold: default_lockout_threshold(),
            window_secs: default_lockout_window_secs(),
            cooloff_secs: default_lockout_cooloff_secs(),
        }
    }
}
/// Step-up authentication configuration.
///
/// Read from the `[auth.step_up]` section of `autumn.toml`.
/// All fields have safe defaults.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StepUpConfig {
    /// Maximum age (in seconds) of the `last_strong_auth_at` session claim
    /// before the user must re-authenticate (default: `300`, i.e. 5 minutes).
    ///
    /// Individual routes can override this with
    /// `#[step_up(max_age = "Nm")]`.
    #[serde(default = "default_step_up_max_age_secs")]
    pub default_max_age_secs: u64,
}

const fn default_step_up_max_age_secs() -> u64 {
    crate::step_up::DEFAULT_MAX_AGE_SECS
}

impl Default for StepUpConfig {
    fn default() -> Self {
        Self {
            default_max_age_secs: crate::step_up::DEFAULT_MAX_AGE_SECS,
        }
    }
}

/// Active-session tracking configuration (issue #819).
///
/// Read from the `[auth.sessions]` section of `autumn.toml`. Used by the
/// session-management machinery emitted by `autumn generate auth`: a
/// persisted row per login session, a device list at `/account/sessions`,
/// and per-session / bulk revocation.
///
/// ```toml
/// [auth.sessions]
/// revoke_on_credential_change = true  # default
/// last_seen_update_secs = 60          # default
/// ```
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct SessionTrackingConfig {
    /// Revoke all *other* login sessions when credentials change —
    /// password change/reset, TOTP enrollment or disable, and `WebAuthn`
    /// key add/remove (default: `true`).
    ///
    /// Leave this on unless an external policy handles credential-change
    /// hygiene: it is the standard response to credential theft.
    #[serde(default = "default_true_flag")]
    pub revoke_on_credential_change: bool,

    /// Minimum number of seconds between `last_seen_at` writes for a given
    /// session (default: `60`).
    ///
    /// Bounds write amplification: authenticated requests inside the window
    /// skip the `UPDATE`, so a busy session costs at most one write per
    /// window rather than one per request.
    #[serde(default = "default_last_seen_update_secs")]
    pub last_seen_update_secs: u64,
}

const fn default_true_flag() -> bool {
    true
}

const fn default_last_seen_update_secs() -> u64 {
    60
}

impl Default for SessionTrackingConfig {
    fn default() -> Self {
        Self {
            revoke_on_credential_change: true,
            last_seen_update_secs: default_last_seen_update_secs(),
        }
    }
}

/// `WebAuthn` / passkey Relying Party configuration.
///
/// Read from the `[auth.webauthn]` section of `autumn.toml`.
#[cfg(feature = "webauthn")]
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebAuthnConfig {
    /// The Relying Party ID (typically the domain, e.g. `"example.com"`).
    #[serde(default = "default_rp_id")]
    pub rp_id: String,
    /// A human-readable name for the Relying Party shown in authenticator dialogs.
    #[serde(default = "default_rp_name")]
    pub rp_name: String,
    /// The full origin of the Relying Party (e.g. `"https://example.com"`).
    #[serde(default = "default_rp_origin")]
    pub rp_origin: String,
}

#[cfg(feature = "webauthn")]
impl Default for WebAuthnConfig {
    fn default() -> Self {
        Self {
            rp_id: default_rp_id(),
            rp_name: default_rp_name(),
            rp_origin: default_rp_origin(),
        }
    }
}

#[cfg(feature = "webauthn")]
const fn default_rp_id() -> String {
    String::new()
}

#[cfg(feature = "webauthn")]
fn default_rp_name() -> String {
    "My App".to_owned()
}

#[cfg(feature = "webauthn")]
const fn default_rp_origin() -> String {
    String::new()
}

const fn default_bcrypt_cost() -> u32 {
    DEFAULT_BCRYPT_COST
}

fn default_session_key() -> String {
    "user_id".to_owned()
}

#[cfg(feature = "oauth2")]
const fn default_provider_scope() -> String {
    String::new()
}

#[cfg(feature = "oauth2")]
const OAUTH_HTTP_TIMEOUT_SECS: u64 = 15;

#[cfg(feature = "oauth2")]
/// `OAuth2` provider map loaded from `autumn.toml`.
///
/// Example:
///
/// ```toml
/// [auth.oauth2.github]
/// client_id = "..."
/// client_secret = "..."
/// authorize_url = "https://github.com/login/oauth/authorize"
/// token_url = "https://github.com/login/oauth/access_token"
/// userinfo_url = "https://api.github.com/user"
/// redirect_uri = "http://localhost:3000/auth/github/callback"
/// scope = "read:user user:email"
/// ```
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct OAuth2Config {
    /// Dynamic provider table keyed by provider name.
    #[serde(flatten)]
    pub providers: HashMap<String, OAuth2ProviderConfig>,
}

#[cfg(feature = "oauth2")]
/// A single OAuth2/OIDC provider configuration entry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuth2ProviderConfig {
    /// The client ID provided by the `OAuth2` identity provider.
    #[serde(default)]
    pub client_id: String,
    /// The client secret provided by the `OAuth2` identity provider.
    #[serde(default)]
    pub client_secret: String,
    /// The authorization endpoint URL where users are redirected to authenticate.
    #[serde(default)]
    pub authorize_url: String,
    /// The token endpoint URL used to exchange an authorization code for tokens.
    #[serde(default)]
    pub token_url: String,
    /// The optional userinfo endpoint URL used to fetch profile details.
    #[serde(default)]
    pub userinfo_url: Option<String>,
    /// The local redirect URI registered with the identity provider (e.g., `http://localhost/auth/callback`).
    #[serde(default)]
    pub redirect_uri: String,
    /// The requested scope string (e.g., `openid profile email`).
    #[serde(default = "default_provider_scope")]
    pub scope: String,
    /// Expected OIDC issuer (`iss`) used for ID token validation.
    #[serde(default)]
    pub issuer: Option<String>,
    /// JWKS endpoint URL used to verify ID token signatures.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// OIDC discovery base URL (e.g. `https://accounts.google.com`).
    ///
    /// When set, the framework appends `/.well-known/openid-configuration` and fetches
    /// the discovery document to populate `authorize_url`, `token_url`, `userinfo_url`,
    /// `jwks_url`, and `issuer` automatically. Explicit fields take precedence.
    #[serde(default)]
    pub discovery_url: Option<String>,
}

#[cfg(feature = "oauth2")]
/// Policy for linking an OAuth2/OIDC identity to a local user account.
///
/// Configured under `[auth]` in `autumn.toml`:
///
/// ```toml
/// [auth]
/// oauth_linking_policy = "create_account"   # default
/// # or
/// oauth_linking_policy = "require_local_signup_first"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OAuthLinkingPolicy {
    /// An unknown `OAuth2` identity automatically creates a new local account.
    /// This is the default for apps where social login is the primary sign-up path.
    #[default]
    CreateAccount,
    /// An unknown `OAuth2` identity returns a clear error unless the user already
    /// has a local account (linked separately). Choose this when social login is
    /// supplemental and you want explicit control over account creation.
    RequireLocalSignupFirst,
}

#[cfg(feature = "oauth2")]
/// Returns a pre-populated [`OAuth2ProviderConfig`] for well-known providers.
///
/// `client_id`, `client_secret`, and `redirect_uri` are left empty and must be
/// supplied by the application from `autumn.toml` or environment variables.
///
/// # Supported providers
///
/// | Key | Protocol | Notes |
/// |-----|----------|-------|
/// | `google` | OIDC | Uses `discovery_url`; scopes: `openid profile email` |
/// | `github` | `OAuth2` | Userinfo endpoint; no OIDC discovery |
/// | `microsoft` | OIDC | Uses `discovery_url` (common tenant); scopes: `openid profile email` |
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::auth::provider_preset;
/// if let Some(mut preset) = provider_preset("google") {
///     preset.client_id = std::env::var("GOOGLE_CLIENT_ID").unwrap_or_default();
///     preset.client_secret = std::env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default();
///     preset.redirect_uri = "http://localhost:3000/auth/google/callback".into();
/// }
/// ```
#[must_use]
pub fn provider_preset(name: &str) -> Option<OAuth2ProviderConfig> {
    match name {
        "google" => Some(OAuth2ProviderConfig {
            client_id: String::new(),
            client_secret: String::new(),
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: Some("https://openidconnect.googleapis.com/v1/userinfo".into()),
            redirect_uri: String::new(),
            scope: "openid profile email".into(),
            issuer: Some("https://accounts.google.com".into()),
            jwks_url: Some("https://www.googleapis.com/oauth2/v3/certs".into()),
            discovery_url: Some("https://accounts.google.com".into()),
        }),
        "github" => Some(OAuth2ProviderConfig {
            client_id: String::new(),
            client_secret: String::new(),
            authorize_url: "https://github.com/login/oauth/authorize".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
            userinfo_url: Some("https://api.github.com/user".into()),
            redirect_uri: String::new(),
            scope: "read:user user:email".into(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        }),
        "microsoft" => Some(OAuth2ProviderConfig {
            client_id: String::new(),
            client_secret: String::new(),
            authorize_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            userinfo_url: None,
            redirect_uri: String::new(),
            scope: "openid profile email".into(),
            // ⚠ The `common` endpoint is a routing alias — ID tokens it issues carry
            // a tenant-specific `iss` claim (`https://login.microsoftonline.com/{tid}/v2.0`),
            // which will NOT match this issuer and will fail validation.
            //
            // Single-tenant apps: replace every `common` segment with your tenant ID.
            // Multi-tenant apps: use the `organizations` or `consumers` endpoint and
            // override `issuer` with the concrete tenant ID after decoding the `tid` claim.
            issuer: Some("https://login.microsoftonline.com/common/v2.0".into()),
            jwks_url: Some("https://login.microsoftonline.com/common/discovery/v2.0/keys".into()),
            discovery_url: Some("https://login.microsoftonline.com/common/v2.0".into()),
        }),
        _ => None,
    }
}

#[cfg(feature = "oauth2")]
/// Query extractor payload for `OAuth2` callback handlers.
#[derive(Debug, Clone, Deserialize)]
pub struct OAuth2Callback {
    /// The authorization code returned by the provider.
    pub code: String,
    /// The anti-CSRF state token passed during the authorization request.
    pub state: String,
}

#[cfg(feature = "oauth2")]
/// Identity information extracted from an OIDC ID token or userinfo endpoint.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    /// The primary subject identifier (`sub` claim) representing the user.
    pub subject: String,
    /// The user's email address, if available in the claims.
    pub email: Option<String>,
    /// The user's full name, if available in the claims.
    pub name: Option<String>,
    /// The user's preferred username or nickname, if available in the claims.
    pub preferred_username: Option<String>,
    /// The raw JSON claims extracted from the token or userinfo response.
    pub raw_claims: serde_json::Value,
}

#[cfg(feature = "oauth2")]
#[derive(Debug, Deserialize)]
struct OAuth2TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: Option<String>,
    id_token: Option<String>,
}

#[cfg(feature = "oauth2")]
/// Build an `OAuth2` authorization URL and persist anti-CSRF state + nonce in session.
///
/// PKCE (S256) is always enabled: a `code_verifier` is generated, stored in the
/// session, and the corresponding `code_challenge` is added to the URL.
///
/// # Errors
///
/// Returns an error if `authorize_url` is not a valid URL.
pub async fn oauth2_authorize_url(
    session: &crate::session::Session,
    provider_name: &str,
    provider: &OAuth2ProviderConfig,
) -> crate::AutumnResult<String> {
    use base64::Engine as _;
    use sha2::Digest as _;

    let state = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();

    // PKCE S256: generate a 32-byte random verifier, base64url-encode it.
    let mut verifier_bytes = [0u8; 32];
    getrandom::getrandom(&mut verifier_bytes).map_err(|e| {
        crate::AutumnError::service_unavailable_msg(format!("pkce rng failed: {e}"))
    })?;
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    // code_challenge = BASE64URL(SHA256(ASCII(code_verifier)))
    let digest = sha2::Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    session
        .insert(format!("oauth2:{provider_name}:state"), state.clone())
        .await;
    session
        .insert(format!("oauth2:{provider_name}:nonce"), nonce.clone())
        .await;
    session
        .insert(
            format!("oauth2:{provider_name}:code_verifier"),
            code_verifier,
        )
        .await;

    let mut url = Url::parse(&provider.authorize_url)
        .map_err(|e| crate::AutumnError::bad_request_msg(format!("invalid authorize_url: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &provider.client_id);
        q.append_pair("redirect_uri", &provider.redirect_uri);
        if !provider.scope.trim().is_empty() {
            q.append_pair("scope", &provider.scope);
        }
        q.append_pair("state", &state);
        q.append_pair("nonce", &nonce);
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
    }
    Ok(url.into())
}

#[cfg(feature = "oauth2")]
/// Exchange callback code for tokens, validate state/nonce, and return OIDC identity.
///
/// On success this method rotates the session ID (preventing session fixation) and
/// writes `auth_provider` to the session. It does **not** set the application
/// session key — callers are responsible for resolving or creating a local user
/// account and then calling `session.insert(session_key, local_user_id)`.
///
/// The PKCE `code_verifier` is read from the session (stored by
/// [`oauth2_authorize_url`]) and included in the token exchange for every
/// provider, regardless of whether the provider is confidential or public.
///
/// # Errors
///
/// Returns an error when callback state/nonce validation fails, token exchange
/// fails, ID token/userinfo payloads are invalid, or identity extraction fails.
pub async fn oauth2_finish_login(
    session: &crate::session::Session,
    provider_name: &str,
    provider: &OAuth2ProviderConfig,
    callback: &OAuth2Callback,
) -> crate::AutumnResult<OidcIdentity> {
    validate_callback_state(session, provider_name, callback).await?;
    // Retrieve (and consume) the PKCE code_verifier stored during authorize.
    let code_verifier = session
        .remove(&format!("oauth2:{provider_name}:code_verifier"))
        .await
        .ok_or_else(|| {
            crate::AutumnError::unauthorized_msg("oauth2 code_verifier missing from session")
        })?;
    let token = exchange_oauth2_token(provider, callback, code_verifier).await?;
    let (claims, source) = load_identity_claims(provider, &token).await?;
    validate_oidc_nonce(session, provider_name, &claims, source).await?;
    let subject = extract_subject(&claims, source)?;
    finalize_oauth2_session(session, provider_name, subject, claims).await
}

#[cfg(feature = "oauth2")]
async fn validate_callback_state(
    session: &crate::session::Session,
    provider_name: &str,
    callback: &OAuth2Callback,
) -> crate::AutumnResult<()> {
    let state_key = format!("oauth2:{provider_name}:state");
    // Read without removing so a stray/attacker-controlled callback with a
    // wrong state value cannot consume the real state and break the pending
    // legitimate redirect.
    let expected_state = session.get(&state_key).await.ok_or_else(|| {
        crate::AutumnError::unauthorized_msg("oauth2 state missing; restart login")
    })?;
    if subtle::ConstantTimeEq::ct_eq(expected_state.as_bytes(), callback.state.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(crate::AutumnError::unauthorized_msg(
            "oauth2 state mismatch",
        ));
    }
    // Remove the state only after a successful constant-time match.
    session.remove(&state_key).await;
    Ok(())
}

#[cfg(feature = "oauth2")]
async fn exchange_oauth2_token(
    provider: &OAuth2ProviderConfig,
    callback: &OAuth2Callback,
    code_verifier: String,
) -> crate::AutumnResult<OAuth2TokenResponse> {
    // Build the base form fields; PKCE code_verifier is appended.
    let form_fields: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", callback.code.clone()),
        ("redirect_uri", provider.redirect_uri.clone()),
        ("client_id", provider.client_id.clone()),
        ("client_secret", provider.client_secret.clone()),
        ("code_verifier", code_verifier),
    ];
    let token_response = oauth_http_client()?
        .post(&provider.token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form_fields)
        .send()
        .await
        .map_err(|e| {
            crate::AutumnError::service_unavailable_msg(format!("token request failed: {e}"))
        })?
        .error_for_status()
        .map_err(|e| crate::AutumnError::unauthorized_msg(format!("token exchange failed: {e}")))?;

    let token_content_type = token_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let token_body = token_response.text().await.map_err(|e| {
        crate::AutumnError::bad_request_msg(format!("invalid token response body: {e}"))
    })?;
    parse_oauth2_token_response(token_content_type.as_deref(), &token_body)
}

#[cfg(feature = "oauth2")]
async fn load_identity_claims(
    provider: &OAuth2ProviderConfig,
    token: &OAuth2TokenResponse,
) -> crate::AutumnResult<(serde_json::Value, IdentitySource)> {
    if let Some(id_token) = token.id_token.as_deref() {
        return Ok((
            validate_and_decode_id_token(id_token, provider).await?,
            IdentitySource::IdToken,
        ));
    }
    if let Some(userinfo_url) = &provider.userinfo_url {
        let claims = oauth_http_client()?
            .get(userinfo_url)
            .header(
                reqwest::header::USER_AGENT,
                concat!("autumn-web/", env!("CARGO_PKG_VERSION")),
            )
            .bearer_auth(&token.access_token)
            .send()
            .await
            .map_err(|e| {
                crate::AutumnError::service_unavailable_msg(format!("userinfo request failed: {e}"))
            })?
            .error_for_status()
            .map_err(|e| crate::AutumnError::unauthorized_msg(format!("userinfo failed: {e}")))?
            .json()
            .await
            .map_err(|e| {
                crate::AutumnError::bad_request_msg(format!("invalid userinfo payload: {e}"))
            })?;
        return Ok((claims, IdentitySource::UserInfo));
    }
    Err(crate::AutumnError::bad_request_msg(
        "provider must return id_token or configure userinfo_url",
    ))
}

#[cfg(feature = "oauth2")]
async fn validate_oidc_nonce(
    session: &crate::session::Session,
    provider_name: &str,
    claims: &serde_json::Value,
    source: IdentitySource,
) -> crate::AutumnResult<()> {
    let nonce_key = format!("oauth2:{provider_name}:nonce");
    let stored_nonce = session.remove(&nonce_key).await;
    if source == IdentitySource::IdToken {
        // The nonce MUST be present in the session for ID-token logins.
        // A missing nonce (e.g. session was partially cleared) must be
        // treated as an error to prevent replay/mix-up attacks.
        let expected_nonce = stored_nonce.ok_or_else(|| {
            crate::AutumnError::unauthorized_msg("oauth2 nonce missing from session")
        })?;
        let actual_nonce = claims
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| crate::AutumnError::unauthorized_msg("missing oidc nonce claim"))?;
        if subtle::ConstantTimeEq::ct_eq(expected_nonce.as_bytes(), actual_nonce.as_bytes())
            .unwrap_u8()
            != 1
        {
            return Err(crate::AutumnError::unauthorized_msg("oidc nonce mismatch"));
        }
    }
    Ok(())
}

#[cfg(feature = "oauth2")]
async fn finalize_oauth2_session(
    session: &crate::session::Session,
    provider_name: &str,
    subject: String,
    claims: serde_json::Value,
) -> crate::AutumnResult<OidcIdentity> {
    // Write provider metadata only — callers set the application session key
    // after resolving or creating the local user record.
    session.insert("auth_provider", provider_name).await;
    session.rotate_id().await;
    Ok(OidcIdentity {
        subject,
        email: claims
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        name: claims
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        preferred_username: claims
            .get("preferred_username")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_claims: claims,
    })
}

#[cfg(feature = "oauth2")]
fn parse_oauth2_token_response(
    content_type: Option<&str>,
    body: &str,
) -> crate::AutumnResult<OAuth2TokenResponse> {
    let looks_like_json = content_type.is_some_and(|v| v.contains("application/json"))
        || body.trim_start().starts_with('{');
    if looks_like_json {
        return serde_json::from_str(body).map_err(|e| {
            crate::AutumnError::bad_request_msg(format!("invalid json token response: {e}"))
        });
    }

    let mut access_token = None;
    let mut token_type = None;
    let mut id_token = None;

    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        match k.as_ref() {
            "access_token" => access_token = Some(v.into_owned()),
            "token_type" => token_type = Some(v.into_owned()),
            "id_token" => id_token = Some(v.into_owned()),
            _ => {}
        }
    }

    let access_token = access_token.ok_or_else(|| {
        crate::AutumnError::bad_request_msg("token response missing access_token")
    })?;

    Ok(OAuth2TokenResponse {
        access_token,
        token_type,
        id_token,
    })
}

#[cfg(feature = "oauth2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentitySource {
    IdToken,
    UserInfo,
}

#[cfg(feature = "oauth2")]
fn extract_subject(
    claims: &serde_json::Value,
    source: IdentitySource,
) -> crate::AutumnResult<String> {
    if let Some(sub) = claims.get("sub").and_then(serde_json::Value::as_str) {
        return Ok(sub.to_owned());
    }

    if source == IdentitySource::UserInfo {
        if let Some(id) = claims.get("id").and_then(serde_json::Value::as_i64) {
            return Ok(id.to_string());
        }
        if let Some(id) = claims.get("id").and_then(serde_json::Value::as_str) {
            return Ok(id.to_owned());
        }
        return Err(crate::AutumnError::bad_request_msg(
            "missing identity claim: expected sub or id from userinfo",
        ));
    }

    Err(crate::AutumnError::bad_request_msg("missing sub claim"))
}

#[cfg(feature = "oauth2")]
async fn validate_and_decode_id_token(
    token: &str,
    provider: &OAuth2ProviderConfig,
) -> crate::AutumnResult<serde_json::Value> {
    let issuer = provider
        .issuer
        .as_deref()
        .ok_or_else(|| crate::AutumnError::bad_request_msg("provider.issuer required for oidc"))?;
    let jwks_url = provider.jwks_url.as_deref().ok_or_else(|| {
        crate::AutumnError::bad_request_msg("provider.jwks_url required for oidc")
    })?;

    let header = jsonwebtoken::decode_header(token).map_err(|e| {
        crate::AutumnError::unauthorized_msg(format!("invalid id_token header: {e}"))
    })?;
    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| crate::AutumnError::unauthorized_msg("id_token header missing kid"))?;
    let alg = header.alg;

    let jwks: JwkSet = oauth_http_client()?
        .get(jwks_url)
        .send()
        .await
        .map_err(|e| {
            crate::AutumnError::service_unavailable_msg(format!("jwks request failed: {e}"))
        })?
        .error_for_status()
        .map_err(|e| crate::AutumnError::unauthorized_msg(format!("jwks fetch failed: {e}")))?
        .json()
        .await
        .map_err(|e| crate::AutumnError::bad_request_msg(format!("invalid jwks response: {e}")))?;

    let jwk = jwks
        .keys
        .iter()
        .find(|k| k.common.key_id.as_deref() == Some(kid))
        .ok_or_else(|| crate::AutumnError::unauthorized_msg("no jwk matched id_token kid"))?;
    let decoding_key = jsonwebtoken::DecodingKey::from_jwk(jwk)
        .map_err(|e| crate::AutumnError::unauthorized_msg(format!("invalid jwk key: {e}")))?;

    let mut validation = jsonwebtoken::Validation::new(alg);
    let mut issuers = vec![issuer.to_owned()];
    let is_multi_tenant = issuer.contains("/common/")
        || issuer.contains("/organizations/")
        || issuer.contains("/consumers/");
    if let (true, true, Some(payload_b64)) = (
        issuer.contains("login.microsoftonline.com"),
        is_multi_tenant,
        token.split('.').nth(1),
    ) {
        use base64::Engine as _;
        let extract_microsoft_iss = || -> Option<String> {
            let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload_b64)
                .ok()?;
            let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
            let unverified_iss = claims.get("iss")?.as_str()?;
            if unverified_iss.starts_with("https://login.microsoftonline.com/")
                && unverified_iss.ends_with("/v2.0")
            {
                Some(unverified_iss.to_owned())
            } else {
                None
            }
        };
        if let Some(unverified_iss) = extract_microsoft_iss() {
            issuers.push(unverified_iss);
        }
    }
    let issuer_refs: Vec<&str> = issuers.iter().map(String::as_str).collect();
    validation.set_issuer(&issuer_refs);
    validation.set_audience(std::slice::from_ref(&provider.client_id));
    validation.required_spec_claims = ["exp", "iss", "aud", "sub"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    validation.validate_exp = true;
    validation.validate_nbf = true;

    let claims = jsonwebtoken::decode::<serde_json::Value>(token, &decoding_key, &validation)
        .map_err(|e| crate::AutumnError::unauthorized_msg(format!("invalid id_token: {e}")))?;
    Ok(claims.claims)
}

#[cfg(feature = "oauth2")]
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

#[cfg(feature = "oauth2")]
pub struct HttpRequestBuilder {
    client: reqwest::Client,
    builder: reqwest::RequestBuilder,
}

#[cfg(feature = "oauth2")]
#[allow(
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::redundant_closure_for_method_calls
)]
impl HttpClient {
    #[must_use]
    pub const fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn post(&self, url: &str) -> HttpRequestBuilder {
        HttpRequestBuilder {
            client: self.inner.clone(),
            builder: self.inner.post(url),
        }
    }

    #[must_use]
    pub fn get(&self, url: &str) -> HttpRequestBuilder {
        HttpRequestBuilder {
            client: self.inner.clone(),
            builder: self.inner.get(url),
        }
    }
}

#[cfg(feature = "oauth2")]
#[allow(
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::redundant_closure_for_method_calls
)]
impl HttpRequestBuilder {
    #[must_use]
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        reqwest::header::HeaderName: TryFrom<K>,
        <reqwest::header::HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        reqwest::header::HeaderValue: TryFrom<V>,
        <reqwest::header::HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.builder = self.builder.header(key, value);
        self
    }

    #[must_use]
    pub fn bearer_auth<T>(mut self, token: T) -> Self
    where
        T: std::fmt::Display,
    {
        self.builder = self.builder.bearer_auth(token);
        self
    }

    #[must_use]
    pub fn form<T: serde::Serialize + ?Sized>(mut self, form: &T) -> Self {
        self.builder = self.builder.form(form);
        self
    }

    /// Sends the request through the interceptor chain.
    ///
    /// # Errors
    ///
    /// Returns a `reqwest::Error` if building the request, sending the request, or
    /// intercepting the call fails.
    pub async fn send(self) -> Result<reqwest::Response, reqwest::Error> {
        let req = self.builder.build()?;
        let interceptors = crate::interceptor::ACTIVE_HTTP_INTERCEPTORS
            .try_with(Clone::clone)
            .unwrap_or_default();
        run_http_chain(req, interceptors, self.client.clone(), 0).await
    }
}

#[cfg(feature = "oauth2")]
fn run_http_chain(
    req: reqwest::Request,
    interceptors: Vec<Arc<dyn crate::interceptor::HttpInterceptor>>,
    client: reqwest::Client,
    idx: usize,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>
            + Send
            + 'static,
    >,
> {
    Box::pin(async move {
        if idx < interceptors.len() {
            let interceptor = interceptors[idx].clone();
            let next_interceptors = interceptors.clone();
            let next_client = client.clone();
            let next_fn = move |r: reqwest::Request| {
                run_http_chain(r, next_interceptors.clone(), next_client.clone(), idx + 1)
            };
            let fut = interceptor.intercept(req, &next_fn);
            fut.await
        } else {
            client.execute(req).await
        }
    })
}

#[cfg(feature = "oauth2")]
fn oauth_http_client() -> crate::AutumnResult<HttpClient> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(OAUTH_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| {
            crate::AutumnError::service_unavailable_msg(format!(
                "failed to build oauth http client: {e}"
            ))
        })?;
    Ok(HttpClient::new(client))
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            bcrypt_cost: default_bcrypt_cost(),
            session_key: default_session_key(),
            #[cfg(feature = "oauth2")]
            oauth2: OAuth2Config::default(),
            #[cfg(feature = "oauth2")]
            oauth_linking_policy: OAuthLinkingPolicy::default(),
            #[cfg(feature = "webauthn")]
            webauthn: WebAuthnConfig::default(),
            lockout: LockoutConfig::default(),
            step_up: StepUpConfig::default(),
            sessions: SessionTrackingConfig::default(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// API Token Authentication
// ─────────────────────────────────────────────────────────────────────────────

/// A verified token: the principal it authenticates plus the scopes it grants.
///
/// Returned by [`ApiTokenStore::verify_scoped`]. The granted `scopes` are flat
/// permission strings (e.g. `posts:read`, `posts:write`) and are threaded into
/// the policy layer so handlers can authorize on them via
/// [`crate::authorization::PolicyContext::has_scope`] and
/// `#[secured(scopes = [...])]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifiedToken {
    /// The principal the token authenticates (e.g. `user:42`, `service:ci`).
    pub principal_id: String,
    /// Flat permission strings granted to this token.
    pub scopes: Vec<String>,
    /// Human-readable name supplied at mint time; used by [`ApiTokenStore::rotate`]
    /// to re-issue the replacement with the same name.
    pub name: String,
    /// Expiry carried through so [`ApiTokenStore::rotate`] can preserve it on
    /// the replacement token instead of issuing a non-expiring one.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Non-secret metadata for an issued token, returned by
/// [`ApiTokenStore::list`].
///
/// **Never** carries the raw token or its hash — listing a principal's tokens
/// must not expose anything that could be replayed as a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMetadata {
    /// Store-defined identifier (e.g. a `BIGSERIAL` id rendered as a string).
    pub id: String,
    /// Human-readable name supplied at mint time.
    pub name: String,
    /// The principal the token authenticates.
    pub principal_id: String,
    /// Flat permission strings granted to this token.
    pub scopes: Vec<String>,
    /// When the token was issued.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Optional expiry; `None` means the token never expires.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Last successful authentication, recorded on use.
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Revocation timestamp; `None` means the token is still active.
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Parameters for minting a scoped token via
/// [`ApiTokenStore::issue_scoped`] / [`issue_scoped_api_token`].
#[derive(Debug, Clone, Default)]
pub struct IssueTokenSpec<'a> {
    /// The principal the token authenticates.
    pub principal_id: &'a str,
    /// Human-readable name (e.g. `ci`, `partner-integration`).
    pub name: &'a str,
    /// Flat permission strings to grant.
    pub scopes: &'a [String],
    /// Optional expiry; `None` mints a non-expiring token.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Backend trait for storing and verifying API bearer tokens.
///
/// Implementations persist only the token hash — the raw token is never stored
/// at rest. The default backend for tests is [`InMemoryApiTokenStore`].
/// Production deployments should use a database-backed implementation.
///
/// All methods take `&self`; use interior mutability where write access is
/// needed.
///
/// # Scoped tokens
///
/// The original three methods (`issue` / `verify` / `revoke`) remain the
/// minimal contract. The scoped surface — [`issue_scoped`](Self::issue_scoped),
/// [`verify_scoped`](Self::verify_scoped), [`list`](Self::list), and
/// [`rotate`](Self::rotate) — is provided with **default implementations** that
/// delegate to the original three, so existing `impl ApiTokenStore` blocks keep
/// compiling unchanged. The built-in stores override them to carry names,
/// scopes, expiry, and `last_used_at`.
pub trait ApiTokenStore: Send + Sync + 'static {
    /// Issue a new token for `principal_id` and return the raw value.
    ///
    /// Only the hash is persisted. The raw token must be delivered to the
    /// caller immediately — it cannot be recovered later.
    fn issue<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>>;

    /// Verify `raw_token` and return its principal ID, or `None` for unknown,
    /// revoked, or expired tokens.
    fn verify<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>>;

    /// Revoke a token so that subsequent requests are rejected.
    fn revoke<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'a>>;

    /// Issue a token carrying a name, scopes, and an optional expiry.
    ///
    /// The default implementation delegates to [`issue`](Self::issue),
    /// dropping the name/scopes/expiry — so legacy stores keep working while
    /// returning unscoped tokens. Stores that support scopes override this.
    fn issue_scoped<'a>(
        &'a self,
        spec: IssueTokenSpec<'a>,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>> {
        Box::pin(async move { self.issue(spec.principal_id).await })
    }

    /// Verify `raw_token` and return the principal plus its granted scopes.
    ///
    /// The default implementation delegates to [`verify`](Self::verify) and
    /// yields an empty scope set, preserving legacy behavior.
    fn verify_scoped<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<VerifiedToken>>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self
                .verify(raw_token)
                .await?
                .map(|principal_id| VerifiedToken {
                    principal_id,
                    scopes: Vec::new(),
                    name: String::new(),
                    expires_at: None,
                }))
        })
    }

    /// List non-secret metadata for every token belonging to `principal_id`.
    ///
    /// The default implementation returns an empty list (opt-in). Listing
    /// **never** exposes the raw token or its hash.
    fn list<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Vec<TokenMetadata>>> + Send + 'a>> {
        let _ = principal_id;
        Box::pin(async move { Ok(Vec::new()) })
    }

    /// Rotate `raw_token`: revoke it and issue a replacement carrying the same
    /// name and scopes, returning the new raw token (or `None` if the token
    /// was unknown).
    ///
    /// The default implementation reads the token's scopes via
    /// [`verify_scoped`](Self::verify_scoped), revokes it, then mints a fresh
    /// token with the same scopes.
    fn rotate<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>> {
        Box::pin(async move {
            match self.verify_scoped(raw_token).await? {
                Some(vt) => {
                    self.revoke(raw_token).await?;
                    let scopes = vt.scopes.clone();
                    let raw = self
                        .issue_scoped(IssueTokenSpec {
                            principal_id: &vt.principal_id,
                            name: &vt.name,
                            scopes: &scopes,
                            expires_at: vt.expires_at,
                        })
                        .await?;
                    Ok(Some(raw))
                }
                None => Ok(None),
            }
        })
    }
}

/// Compute the SHA-256 hash of a raw API token as a lowercase 64-char hex string.
///
/// The hash is deterministic: the same input always produces the same output.
/// Only the hash is ever stored; the raw token is never persisted.
///
/// # Examples
///
/// ```rust
/// use autumn_web::auth::hash_api_token;
///
/// let h = hash_api_token("my_token");
/// assert_eq!(h.len(), 64);
/// assert_eq!(h, hash_api_token("my_token")); // deterministic
/// ```
#[must_use]
pub fn hash_api_token(raw: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(raw.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Generate a 256-bit random raw API token as a lowercase hex string.
///
/// Uses two UUID v4 values (128 bits each) concatenated for a 64-char result.
#[must_use]
pub fn generate_raw_token() -> String {
    let u1 = uuid::Uuid::new_v4();
    let u2 = uuid::Uuid::new_v4();
    format!("{}{}", u1.simple(), u2.simple())
}

/// In-memory API token store for development and testing.
///
/// Tokens are stored as SHA-256 hashes mapped to principal IDs inside a
/// `RwLock`-protected `HashMap`. **Not suitable for production** — state is
/// lost on restart and is not shared across processes.
///
/// # Examples
///
/// ```rust
/// use std::sync::Arc;
/// use autumn_web::auth::{ApiTokenStore, InMemoryApiTokenStore};
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let store = Arc::new(InMemoryApiTokenStore::default());
/// let token = store.issue("user:1").await.unwrap();
/// assert_eq!(store.verify(&token).await.unwrap(), Some("user:1".to_owned()));
/// store.revoke(&token).await.unwrap();
/// assert_eq!(store.verify(&token).await.unwrap(), None);
/// # });
/// ```
/// A token row held by [`InMemoryApiTokenStore`], keyed by token hash.
#[derive(Debug, Clone)]
struct StoredToken {
    id: u64,
    principal_id: String,
    name: String,
    scopes: Vec<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone)]
pub struct InMemoryApiTokenStore {
    // hash → stored token
    tokens: Arc<std::sync::RwLock<std::collections::HashMap<String, StoredToken>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    clock: Arc<dyn crate::time::ClockSource>,
}

impl Default for InMemoryApiTokenStore {
    fn default() -> Self {
        Self {
            tokens: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            clock: Arc::new(crate::time::SystemClock),
        }
    }
}

impl InMemoryApiTokenStore {
    /// Replace the clock used to evaluate expiry and stamp `last_used_at`.
    ///
    /// Defaults to [`crate::time::SystemClock`]; tests pass a
    /// [`crate::time::FixedClock`] / [`crate::time::TickingClock`] to make
    /// expiry and usage timestamps deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::time::ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    /// Insert a freshly-minted token and return its raw value.
    fn insert_token(&self, spec: &IssueTokenSpec<'_>) -> String {
        let raw = generate_raw_token();
        let hash = hash_api_token(&raw);
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let stored = StoredToken {
            id,
            principal_id: spec.principal_id.to_owned(),
            name: spec.name.to_owned(),
            scopes: spec.scopes.to_vec(),
            created_at: self.clock.now(),
            expires_at: spec.expires_at,
            last_used_at: None,
            revoked_at: None,
        };
        self.tokens
            .write()
            .expect("api token store lock poisoned")
            .insert(hash, stored);
        raw
    }

    /// Resolve a live token by raw value, stamping `last_used_at`.
    ///
    /// Returns `None` for unknown, revoked, or expired tokens.
    fn resolve_used(&self, raw_token: &str) -> Option<VerifiedToken> {
        let hash = hash_api_token(raw_token);
        let now = self.clock.now();
        let mut guard = self.tokens.write().expect("api token store lock poisoned");
        let stored = guard.get_mut(&hash)?;
        if stored.revoked_at.is_some() || stored.expires_at.is_some_and(|exp| exp <= now) {
            return None;
        }
        stored.last_used_at = Some(now);
        let verified = VerifiedToken {
            principal_id: stored.principal_id.clone(),
            scopes: stored.scopes.clone(),
            name: stored.name.clone(),
            expires_at: stored.expires_at,
        };
        drop(guard);
        Some(verified)
    }
}

impl ApiTokenStore for InMemoryApiTokenStore {
    fn issue<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self.insert_token(&IssueTokenSpec {
                principal_id,
                ..Default::default()
            }))
        })
    }

    fn verify<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>> {
        Box::pin(async move { Ok(self.resolve_used(raw_token).map(|vt| vt.principal_id)) })
    }

    fn revoke<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let hash = hash_api_token(raw_token);
            let now = self.clock.now();
            let mut guard = self.tokens.write().expect("api token store lock poisoned");
            if let Some(stored) = guard.get_mut(&hash) {
                stored.revoked_at.get_or_insert(now);
            }
            drop(guard);
            Ok(())
        })
    }

    fn issue_scoped<'a>(
        &'a self,
        spec: IssueTokenSpec<'a>,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>> {
        Box::pin(async move { Ok(self.insert_token(&spec)) })
    }

    fn verify_scoped<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<VerifiedToken>>> + Send + 'a>> {
        Box::pin(async move { Ok(self.resolve_used(raw_token)) })
    }

    fn list<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Vec<TokenMetadata>>> + Send + 'a>> {
        Box::pin(async move {
            let mut out: Vec<TokenMetadata> = {
                let guard = self.tokens.read().expect("api token store lock poisoned");
                guard
                    .values()
                    .filter(|s| s.principal_id == principal_id)
                    .map(|s| TokenMetadata {
                        id: s.id.to_string(),
                        name: s.name.clone(),
                        principal_id: s.principal_id.clone(),
                        scopes: s.scopes.clone(),
                        created_at: s.created_at,
                        expires_at: s.expires_at,
                        last_used_at: s.last_used_at,
                        revoked_at: s.revoked_at,
                    })
                    .collect()
            };
            out.sort_by(|a, b| {
                a.id.parse::<u64>()
                    .unwrap_or(0)
                    .cmp(&b.id.parse().unwrap_or(0))
            });
            Ok(out)
        })
    }
}

/// Issue a new API token for `principal_id` using `store`.
///
/// Returns the raw token string that must be transmitted to the client once.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn issue_api_token(
    store: &dyn ApiTokenStore,
    principal_id: &str,
) -> crate::AutumnResult<String> {
    store.issue(principal_id).await
}

/// Revoke a previously issued API token using `store`.
///
/// After revocation [`RequireApiToken`] rejects requests presenting this token.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn revoke_api_token(
    store: &dyn ApiTokenStore,
    raw_token: &str,
) -> crate::AutumnResult<()> {
    store.revoke(raw_token).await
}

/// Issue a named, scoped API token with an optional expiry using `store`.
///
/// Returns the raw token string that must be transmitted to the client once.
/// The granted scopes flow into the policy layer when the token authenticates
/// (see [`crate::authorization::PolicyContext::has_scope`] and
/// `#[secured(scopes = [...])]`).
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn issue_scoped_api_token(
    store: &dyn ApiTokenStore,
    spec: IssueTokenSpec<'_>,
) -> crate::AutumnResult<String> {
    store.issue_scoped(spec).await
}

/// List non-secret metadata for every token belonging to `principal_id`.
///
/// The returned [`TokenMetadata`] never includes the raw token or its hash, so
/// it is safe to surface in a management UI or CLI.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn list_api_tokens(
    store: &dyn ApiTokenStore,
    principal_id: &str,
) -> crate::AutumnResult<Vec<TokenMetadata>> {
    store.list(principal_id).await
}

/// Rotate an API token: revoke `raw_token` and mint a replacement carrying the
/// same name and scopes.
///
/// Returns the new raw token, or `None` if `raw_token` was unknown.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn rotate_api_token(
    store: &dyn ApiTokenStore,
    raw_token: &str,
) -> crate::AutumnResult<Option<String>> {
    store.rotate(raw_token).await
}

/// Private marker inserted into request extensions by [`RequireApiToken`] after
/// a bearer token is successfully verified.
#[derive(Clone)]
struct ApiTokenPrincipal(String);

/// Scopes granted to the authenticating service token, inserted into request
/// extensions by [`RequireApiToken`] after a bearer token is verified.
///
/// Public so policy code and the `#[secured(scopes = [...])]` gate can read the
/// granted scopes from request extensions. Absent when the request did not
/// authenticate via a scoped token (the empty-scope case).
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::auth::ApiTokenScopes;
/// use autumn_web::reexports::axum::extract::Extension;
///
/// #[get("/whoami")]
/// async fn whoami(scopes: Option<Extension<ApiTokenScopes>>) -> String {
///     match scopes {
///         Some(Extension(ApiTokenScopes(s))) => format!("scopes: {s:?}"),
///         None => "no token scopes".to_owned(),
///     }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiTokenScopes(pub Vec<String>);

/// Extractor that yields the verified principal ID from a bearer-protected route.
///
/// The principal ID is inserted by [`RequireApiToken`] after verifying the
/// `Authorization: Bearer <token>` header. Without `RequireApiToken` on the
/// route this extractor returns `401 Unauthorized`.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::auth::ApiToken;
///
/// #[get("/whoami")]
/// async fn whoami(ApiToken(principal): ApiToken) -> String {
///     format!("authenticated as {principal}")
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ApiToken(pub String);

impl<S> FromRequestParts<S> for ApiToken
where
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let principal = parts.extensions.get::<ApiTokenPrincipal>().cloned();
        async move { principal.map(|p| Self(p.0)).ok_or(AuthRejection) }
    }
}

/// Tower [`Layer`](tower::Layer) that validates `Authorization: Bearer <token>`
/// on every inbound request.
///
/// On success the verified principal ID is inserted into request extensions
/// so handlers can retrieve it via the [`ApiToken`] extractor.
/// Requests with a missing, malformed, or revoked token are rejected with
/// `401 Unauthorized` using the same Problem Details contract as
/// [`AuthRejection`].
///
/// Also inserts [`crate::security::RateLimitPrincipal`] so that
/// `key_strategy = "authenticated_principal"` works out of the box.
///
/// Composes with [`RequireAuth`] and session middleware without conflict.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken};
/// use autumn_web::reexports::axum::{Router, routing::get};
/// use autumn_web::AppState;
///
/// let store = Arc::new(InMemoryApiTokenStore::default());
/// let api_routes = Router::<AppState>::new()
///     .route("/data", get(|| async { "ok" }))
///     .layer(RequireApiToken::new(store));
/// ```
#[derive(Clone)]
pub struct RequireApiToken {
    store: Arc<dyn ApiTokenStore>,
}

impl RequireApiToken {
    /// Create a new [`RequireApiToken`] layer backed by `store`.
    ///
    /// Accepts any `Arc<S>` where `S: ApiTokenStore`, so callers do not need
    /// to explicitly cast to `Arc<dyn ApiTokenStore>`.
    #[must_use]
    pub fn new<S: ApiTokenStore + 'static>(store: Arc<S>) -> Self {
        Self { store }
    }
}

impl<S> tower::Layer<S> for RequireApiToken {
    type Service = RequireApiTokenService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireApiTokenService {
            inner,
            store: Arc::clone(&self.store),
        }
    }
}

/// Tower service produced by [`RequireApiToken`].
#[derive(Clone)]
pub struct RequireApiTokenService<S> {
    inner: S,
    store: Arc<dyn ApiTokenStore>,
}

impl<S, ResBody> tower::Service<axum::extract::Request> for RequireApiTokenService<S>
where
    S: tower::Service<axum::extract::Request, Response = Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: From<String> + Default + Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        let store = Arc::clone(&self.store);
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            // Parse "Authorization: Bearer <token>"
            let raw_token = req
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_bearer_token)
                .map(str::to_owned);

            let Some(raw_token) = raw_token else {
                let (request_id, instance) = api_token_problem_context(&req);
                return Ok(api_token_unauthorized_response(request_id, instance));
            };

            match store.verify_scoped(&raw_token).await {
                Ok(Some(verified)) => {
                    let VerifiedToken {
                        principal_id,
                        scopes,
                        ..
                    } = verified;
                    // Fulfil the RateLimitPrincipal contract so key_strategy =
                    // "authenticated_principal" works without an extra middleware shim.
                    req.extensions_mut()
                        .insert(crate::security::RateLimitPrincipal(principal_id.clone()));
                    // Expose the granted scopes via request extensions (NOT the
                    // session — writing to the session would persist a cookie
                    // and leak scopes onto later cookie-only requests). The
                    // `#[secured(scopes = …)]` gate and `PolicyContext`
                    // scope-aware helpers read this extension.
                    req.extensions_mut().insert(ApiTokenScopes(scopes));
                    req.extensions_mut().insert(ApiTokenPrincipal(principal_id));
                    inner.call(req).await
                }
                Ok(None) => {
                    let (request_id, instance) = api_token_problem_context(&req);
                    Ok(api_token_unauthorized_response(request_id, instance))
                }
                Err(err) => {
                    let (request_id, instance) = api_token_problem_context(&req);
                    Ok(api_token_error_response(&err, request_id, instance))
                }
            }
        })
    }
}

fn parse_bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}

/// Build a `401 Unauthorized` response using the standard Problem Details body.
fn api_token_unauthorized_response<ResBody: From<String> + Default>(
    request_id: Option<String>,
    instance: Option<String>,
) -> Response<ResBody> {
    let body = crate::error::problem_details_json_string(
        StatusCode::UNAUTHORIZED,
        "authentication required",
        None,
        None,
        request_id,
        instance,
        true,
    );
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(http::header::CONTENT_TYPE, "application/problem+json")
        .body(ResBody::from(body))
        .unwrap_or_default()
}

/// Build a Problem Details response from the API token store error.
fn api_token_error_response<ResBody: From<String> + Default>(
    err: &crate::AutumnError,
    request_id: Option<String>,
    instance: Option<String>,
) -> Response<ResBody> {
    let status = err.status();
    let message = err.to_string();
    let body = crate::error::problem_details_json_string(
        status,
        message.clone(),
        None,
        None,
        request_id,
        instance,
        true,
    );
    let mut response = Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/problem+json")
        .body(ResBody::from(body))
        .unwrap_or_default();
    response
        .extensions_mut()
        .insert(crate::middleware::AutumnErrorInfo {
            status,
            message,
            details: None,
            problem_type: None,
            backtrace_string: None,
        });
    response
}

fn api_token_problem_context(req: &axum::extract::Request) -> (Option<String>, Option<String>) {
    (
        req.extensions()
            .get::<crate::middleware::RequestId>()
            .map(std::string::ToString::to_string),
        Some(req.uri().path().to_owned()),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Diesel-backed API Token Store
// ─────────────────────────────────────────────────────────────────────────────

/// Embedded Diesel migrations for the `api_tokens` table.
///
/// Include this in your application's `.migrations()` call so that dev/test
/// startup migration checks can create and validate the `api_tokens` table
/// alongside your own migrations. In production, `autumn migrate` applies the
/// matching framework migration before token commands or `DbApiTokenStore`
/// need the table:
///
/// ```rust,ignore
/// use autumn_web::auth::API_TOKEN_MIGRATIONS;
///
/// #[autumn_web::main]
/// async fn main() {
///     autumn_web::app()
///         .migrations(API_TOKEN_MIGRATIONS)
///         .run()
///         .await;
/// }
/// ```
#[cfg(feature = "db")]
pub const API_TOKEN_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("migrations");

#[cfg(feature = "db")]
mod db_store {
    use std::future::Future;
    use std::pin::Pin;

    use std::sync::Arc;

    use chrono::{DateTime, NaiveDateTime, Utc};
    use diesel::OptionalExtension as _;
    use diesel::prelude::*;
    use diesel_async::AsyncPgConnection;
    use diesel_async::RunQueryDsl;
    use diesel_async::pooled_connection::deadpool::Pool;

    use super::{
        ApiTokenStore, IssueTokenSpec, TokenMetadata, VerifiedToken, generate_raw_token,
        hash_api_token,
    };
    use crate::error::AutumnError;
    use crate::time::{ClockSource, SystemClock};

    diesel::table! {
        api_tokens (id) {
            id -> Int8,
            token_hash -> Text,
            principal_id -> Text,
            created_at -> Timestamp,
            revoked_at -> Nullable<Timestamp>,
            name -> Text,
            scopes -> Jsonb,
            expires_at -> Nullable<Timestamp>,
            last_used_at -> Nullable<Timestamp>,
        }
    }

    #[derive(Insertable)]
    #[diesel(table_name = api_tokens)]
    struct NewApiToken<'a> {
        token_hash: &'a str,
        principal_id: &'a str,
        name: &'a str,
        scopes: serde_json::Value,
        expires_at: Option<NaiveDateTime>,
    }

    /// Row shape returned by [`DbApiTokenStore::list`].
    #[derive(Queryable)]
    struct TokenRow {
        id: i64,
        name: String,
        principal_id: String,
        scopes: serde_json::Value,
        created_at: NaiveDateTime,
        expires_at: Option<NaiveDateTime>,
        last_used_at: Option<NaiveDateTime>,
        revoked_at: Option<NaiveDateTime>,
    }

    const fn to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
        DateTime::from_naive_utc_and_offset(naive, Utc)
    }

    fn scopes_to_json(scopes: &[String]) -> serde_json::Value {
        serde_json::Value::Array(
            scopes
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        )
    }

    #[must_use]
    pub fn scopes_from_json(value: &serde_json::Value) -> Vec<String> {
        value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Postgres-backed [`ApiTokenStore`].
    ///
    /// Tokens are hashed at rest (SHA-256) and never stored in plaintext.
    /// Suitable for production deployments where token state must survive
    /// process restarts and be shared across instances.
    ///
    /// Carries name, scopes, optional expiry, and `last_used_at` in the managed
    /// `api_tokens` table (see [`super::API_TOKEN_MIGRATIONS`]). Expired tokens
    /// fail to verify; `last_used_at` is stamped on every successful use.
    ///
    /// # Setup
    ///
    /// Pass [`super::API_TOKEN_MIGRATIONS`] to your app builder so dev/test
    /// startup migration checks can create and validate the `api_tokens`
    /// table automatically. In production, run `autumn migrate`; the CLI
    /// applies the matching framework migration explicitly.
    ///
    /// ```rust,ignore
    /// use autumn_web::auth::{API_TOKEN_MIGRATIONS, DbApiTokenStore};
    /// use autumn_web::db::Pool;
    ///
    /// let store = DbApiTokenStore::new(pool.clone());
    /// autumn_web::app()
    ///     .migrations(API_TOKEN_MIGRATIONS)
    ///     .run()
    ///     .await;
    /// ```
    #[derive(Clone)]
    pub struct DbApiTokenStore {
        pool: Pool<AsyncPgConnection>,
        clock: Arc<dyn ClockSource>,
    }

    impl DbApiTokenStore {
        /// Create a [`DbApiTokenStore`] backed by `pool`.
        #[must_use]
        pub fn new(pool: Pool<AsyncPgConnection>) -> Self {
            Self {
                pool,
                clock: Arc::new(SystemClock),
            }
        }

        /// Replace the clock used to evaluate expiry and stamp `last_used_at`.
        ///
        /// Defaults to [`SystemClock`]; tests pass a fixed/ticking clock to make
        /// expiry and usage timestamps deterministic.
        #[must_use]
        pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
            self.clock = clock;
            self
        }

        async fn conn(
            &self,
        ) -> crate::AutumnResult<diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>>
        {
            self.pool
                .get()
                .await
                .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))
        }
    }

    impl ApiTokenStore for DbApiTokenStore {
        fn issue<'a>(
            &'a self,
            principal_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>> {
            self.issue_scoped(IssueTokenSpec {
                principal_id,
                ..Default::default()
            })
        }

        fn verify<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(self
                    .verify_scoped(raw_token)
                    .await?
                    .map(|vt| vt.principal_id))
            })
        }

        fn revoke<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'a>> {
            Box::pin(async move {
                let hash = hash_api_token(raw_token);
                let now = self.clock.now().naive_utc();
                let mut conn = self.conn().await?;
                diesel::update(api_tokens::table)
                    .filter(api_tokens::token_hash.eq(&hash))
                    .filter(api_tokens::revoked_at.is_null())
                    .set(api_tokens::revoked_at.eq(Some(now)))
                    .execute(&mut conn)
                    .await
                    .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
                Ok(())
            })
        }

        fn issue_scoped<'a>(
            &'a self,
            spec: IssueTokenSpec<'a>,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<String>> + Send + 'a>> {
            Box::pin(async move {
                let raw = generate_raw_token();
                let hash = hash_api_token(&raw);
                let mut conn = self.conn().await?;
                diesel::insert_into(api_tokens::table)
                    .values(NewApiToken {
                        token_hash: &hash,
                        principal_id: spec.principal_id,
                        name: spec.name,
                        scopes: scopes_to_json(spec.scopes),
                        expires_at: spec.expires_at.map(|dt| dt.naive_utc()),
                    })
                    .execute(&mut conn)
                    .await
                    .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
                Ok(raw)
            })
        }

        fn rotate<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>>
        {
            Box::pin(async move {
                #[derive(diesel::QueryableByName)]
                struct CountRow {
                    #[diesel(sql_type = diesel::sql_types::BigInt)]
                    count: i64,
                }

                let old_hash = hash_api_token(raw_token);
                let new_raw = generate_raw_token();
                let new_hash = hash_api_token(&new_raw);
                let now = self.clock.now().naive_utc();
                let mut conn = self.conn().await?;
                // Atomic CTE: revoke the old token and insert a replacement carrying
                // the same name/scopes/expiry in a single statement. If the old hash
                // is unknown or already revoked the UPDATE returns 0 rows, the INSERT
                // is a no-op, and COUNT(*) returns 0 — the caller sees None.
                // $3 is the store clock's `now` so the expiry predicate matches the
                // same instant used by verify_scoped, avoiding clock-skew races.
                let row: CountRow = diesel::sql_query(
                    "WITH rotated AS ( \
                        UPDATE api_tokens \
                        SET revoked_at = $3 \
                        WHERE token_hash = $1 AND revoked_at IS NULL \
                            AND (expires_at IS NULL OR expires_at > $3) \
                        RETURNING principal_id, name, scopes, expires_at \
                     ), \
                     inserted AS ( \
                        INSERT INTO api_tokens (token_hash, principal_id, name, scopes, expires_at) \
                        SELECT $2, principal_id, name, scopes, expires_at FROM rotated \
                        RETURNING 1 \
                     ) \
                     SELECT COUNT(*)::bigint AS count FROM inserted",
                )
                .bind::<diesel::sql_types::Text, _>(&old_hash)
                .bind::<diesel::sql_types::Text, _>(&new_hash)
                .bind::<diesel::sql_types::Timestamp, _>(now)
                .get_result(&mut conn)
                .await
                .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
                if row.count == 0 {
                    Ok(None)
                } else {
                    Ok(Some(new_raw))
                }
            })
        }

        fn verify_scoped<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Option<VerifiedToken>>> + Send + 'a>>
        {
            Box::pin(async move {
                let hash = hash_api_token(raw_token);
                let now = self.clock.now().naive_utc();
                let mut conn = self.conn().await?;
                // Live tokens only: not revoked, and either no expiry or not yet expired.
                let row: Option<(
                    i64,
                    String,
                    String,
                    Option<NaiveDateTime>,
                    serde_json::Value,
                )> = api_tokens::table
                    .filter(api_tokens::token_hash.eq(&hash))
                    .filter(api_tokens::revoked_at.is_null())
                    .filter(
                        api_tokens::expires_at
                            .is_null()
                            .or(api_tokens::expires_at.gt(now)),
                    )
                    .select((
                        api_tokens::id,
                        api_tokens::principal_id,
                        api_tokens::name,
                        api_tokens::expires_at,
                        api_tokens::scopes,
                    ))
                    .first(&mut conn)
                    .await
                    .optional()
                    .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;

                let Some((id, principal_id, name, expires_at_naive, scopes_json)) = row else {
                    return Ok(None);
                };
                // Throttled usage stamp: only write last_used_at when it is NULL or
                // older than 5 minutes, avoiding a write on every single request.
                let threshold = now - chrono::Duration::minutes(5);
                let _ = diesel::update(
                    api_tokens::table.filter(api_tokens::id.eq(id)).filter(
                        api_tokens::last_used_at
                            .is_null()
                            .or(api_tokens::last_used_at.lt(threshold)),
                    ),
                )
                .set(api_tokens::last_used_at.eq(Some(now)))
                .execute(&mut conn)
                .await;
                Ok(Some(VerifiedToken {
                    principal_id,
                    scopes: scopes_from_json(&scopes_json),
                    name,
                    expires_at: expires_at_naive.map(to_utc),
                }))
            })
        }

        fn list<'a>(
            &'a self,
            principal_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<Vec<TokenMetadata>>> + Send + 'a>>
        {
            Box::pin(async move {
                let mut conn = self.conn().await?;
                let rows: Vec<TokenRow> = api_tokens::table
                    .filter(api_tokens::principal_id.eq(principal_id))
                    .order(api_tokens::id.asc())
                    .select((
                        api_tokens::id,
                        api_tokens::name,
                        api_tokens::principal_id,
                        api_tokens::scopes,
                        api_tokens::created_at,
                        api_tokens::expires_at,
                        api_tokens::last_used_at,
                        api_tokens::revoked_at,
                    ))
                    .load(&mut conn)
                    .await
                    .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
                Ok(rows
                    .into_iter()
                    .map(|r| TokenMetadata {
                        id: r.id.to_string(),
                        name: r.name,
                        principal_id: r.principal_id,
                        scopes: scopes_from_json(&r.scopes),
                        created_at: to_utc(r.created_at),
                        expires_at: r.expires_at.map(to_utc),
                        last_used_at: r.last_used_at.map(to_utc),
                        revoked_at: r.revoked_at.map(to_utc),
                    })
                    .collect())
            })
        }
    }
}

#[cfg(feature = "db")]
pub use db_store::DbApiTokenStore;
/// Convert a JSONB `serde_json::Value` (array of strings) to a flat scope list.
///
/// Returns an empty `Vec` for non-array values; non-string array elements are
/// silently skipped. This is the canonical deserializer for the `scopes` JSONB
/// column shared by [`DbApiTokenStore`] and the admin panel.
#[cfg(feature = "db")]
#[doc(hidden)]
pub use db_store::scopes_from_json;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `AppState` for middleware tests, parameterized only by
    /// the auth session key (the sole field these tests vary). Collapses the
    /// otherwise-identical struct literal that each test would copy verbatim.
    fn test_app_state(auth_session_key: &str) -> crate::state::AppState {
        crate::state::AppState {
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
            auth_session_key: auth_session_key.to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        }
    }

    #[tokio::test]
    async fn hash_and_verify_password() {
        let hash = hash_password("test_password").await.unwrap();
        assert!(hash.starts_with("$2b$"));
        assert!(verify_password("test_password", &hash).await.unwrap());
        assert!(!verify_password("wrong_password", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn verify_invalid_hash_returns_false() {
        let result = verify_password("test", "not-a-valid-hash").await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn verify_password_rejects_invalid_hash_format_safely() {
        // Test short hash
        let result = verify_password("test", "short").await;
        assert!(result.is_ok());
        assert!(!result.unwrap());

        // Test hash with correct length but not starting with $
        let bad_prefix = "a".repeat(60);
        let result = verify_password("test", &bad_prefix).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());

        // Test hash with incorrect length but starting with $
        let bad_length = "$2b$12$short";
        let result = verify_password("test", bad_length).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn auth_config_defaults() {
        let config = AuthConfig::default();
        assert_eq!(config.bcrypt_cost, 12);
        assert_eq!(config.session_key, "user_id");
        #[cfg(feature = "oauth2")]
        assert!(config.oauth2.providers.is_empty());
    }

    /// Issue #819 — credential-changing events revoke other sessions by
    /// default, and `last_seen_at` writes are throttled to one per minute.
    #[test]
    fn session_tracking_config_defaults_to_revoke_on_credential_change() {
        let config = AuthConfig::default();
        assert!(config.sessions.revoke_on_credential_change);
        assert_eq!(config.sessions.last_seen_update_secs, 60);
    }

    /// `[auth.sessions]` can be disabled / tuned from `autumn.toml`.
    #[test]
    fn session_tracking_config_deserializes_from_toml() {
        let cfg: crate::config::AutumnConfig = toml::from_str(
            r"
            [auth.sessions]
            revoke_on_credential_change = false
            last_seen_update_secs = 5
            ",
        )
        .expect("config must parse");
        assert!(!cfg.auth.sessions.revoke_on_credential_change);
        assert_eq!(cfg.auth.sessions.last_seen_update_secs, 5);
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn oauth2_config_deserializes_provider_tables() {
        let cfg: crate::config::AutumnConfig = toml::from_str(
            r#"
            [auth.oauth2.github]
            client_id = "cid"
            client_secret = "secret"
            authorize_url = "https://github.com/login/oauth/authorize"
            token_url = "https://github.com/login/oauth/access_token"
            redirect_uri = "http://localhost:3000/auth/github/callback"
            "#,
        )
        .unwrap();
        let provider = cfg.auth.oauth2.providers.get("github").unwrap();
        assert_eq!(provider.client_id, "cid");
        assert_eq!(provider.scope, "");
        assert!(provider.issuer.is_none());
        assert!(provider.jwks_url.is_none());
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn oauth2_authorize_url_sets_state_and_nonce() {
        let session = crate::session::Session::new_for_test("s1".into(), HashMap::new());
        let provider = OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: "openid profile".into(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        };
        let url = oauth2_authorize_url(&session, "github", &provider)
            .await
            .unwrap();
        assert!(url.contains("response_type=code"));
        assert!(session.get("oauth2:github:state").await.is_some());
        assert!(session.get("oauth2:github:nonce").await.is_some());
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn oauth2_authorize_url_omits_scope_when_empty() {
        let session = crate::session::Session::new_for_test("s1".into(), HashMap::new());
        let provider = OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: String::new(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        };
        let url = oauth2_authorize_url(&session, "github", &provider)
            .await
            .unwrap();
        assert!(!url.contains("scope="));
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn validate_id_token_requires_oidc_metadata() {
        let provider = OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: "openid profile".into(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        };
        let err = validate_and_decode_id_token("bad.token.value", &provider)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "provider.issuer required for oidc");
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn parse_oauth2_token_response_supports_form_encoded_payload() {
        let token = parse_oauth2_token_response(
            Some("application/x-www-form-urlencoded"),
            "access_token=abc123&token_type=bearer&id_token=xyz789&extra_field=ignored",
        )
        .unwrap();
        assert_eq!(token.access_token, "abc123");
        assert_eq!(token.token_type.as_deref(), Some("bearer"));
        assert_eq!(token.id_token.as_deref(), Some("xyz789"));
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn parse_oauth2_token_response_fails_without_access_token() {
        let err = parse_oauth2_token_response(
            Some("application/x-www-form-urlencoded"),
            "token_type=bearer&id_token=xyz789",
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "token response missing access_token");
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn extract_subject_allows_userinfo_id_fallback() {
        let claims = serde_json::json!({ "id": 42 });
        let subject = extract_subject(&claims, IdentitySource::UserInfo).unwrap();
        assert_eq!(subject, "42");
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn validate_callback_state_preserves_state_on_mismatch() {
        // An attacker hitting the callback with a wrong state must NOT
        // consume the real state stored in the session; the legitimate
        // provider redirect must still succeed.
        let session = crate::session::Session::new_for_test("s1".into(), HashMap::new());
        session
            .insert("oauth2:github:state".to_owned(), "real-state".to_owned())
            .await;
        let bad_callback = OAuth2Callback {
            code: "c".into(),
            state: "wrong-state".into(),
        };
        let err = validate_callback_state(&session, "github", &bad_callback)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
        // Real state must still be present after the failed attempt.
        assert_eq!(
            session.get("oauth2:github:state").await.as_deref(),
            Some("real-state")
        );
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn validate_oidc_nonce_rejects_missing_nonce_for_id_token() {
        // ID-token logins must fail when there is no stored nonce (e.g.,
        // session was partially cleared or forged).
        let session = crate::session::Session::new_for_test("s1".into(), HashMap::new());
        // No nonce key inserted — simulates a cleared / missing session.
        let claims = serde_json::json!({ "nonce": "any" });
        let err = validate_oidc_nonce(&session, "github", &claims, IdentitySource::IdToken)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nonce missing from session"));
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn extract_subject_requires_sub_for_id_token() {
        let claims = serde_json::json!({ "id": "abc" });
        let err = extract_subject(&claims, IdentitySource::IdToken).unwrap_err();
        assert_eq!(err.to_string(), "missing sub claim");
    }

    #[test]
    fn auth_rejection_is_401() {
        let rejection = AuthRejection;
        let response = rejection.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn auth_rejection_display() {
        assert_eq!(AuthRejection.to_string(), "authentication required");
    }

    #[tokio::test]
    async fn auth_extractor_returns_401_when_no_user() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use tower::ServiceExt;

        #[derive(Clone)]
        struct TestUser {
            name: String,
        }

        async fn handler(Auth(user): Auth<TestUser>) -> String {
            user.name
        }

        let state = test_app_state("user_id");

        let app = Router::new().route("/", get(handler)).with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_extractor_returns_user_when_present() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use tower::ServiceExt;

        #[derive(Clone)]
        struct TestUser {
            name: String,
        }

        async fn handler(Auth(user): Auth<TestUser>) -> String {
            user.name
        }

        let state = test_app_state("user_id");

        // Middleware that inserts a user into extensions
        let app = Router::new()
            .route("/", get(handler))
            .layer(axum::middleware::from_fn(
                |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(TestUser {
                        name: "alice".into(),
                    });
                    next.run(req).await
                },
            ))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "alice");
    }

    #[tokio::test]
    async fn require_auth_rejects_unauthenticated() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer};

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/protected", get(|| async { "secret" }))
            .layer(RequireAuth::new("user_id"))
            .layer(SessionLayer::new(
                MemoryStore::new(),
                SessionConfig::default(),
            ))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // ── __check_secured tests ────────────────────────────────

    #[tokio::test]
    async fn check_secured_rejects_unauthenticated() {
        let session =
            crate::session::Session::new_for_test(String::new(), std::collections::HashMap::new());
        let result = __check_secured(&session, &[]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.to_string(), "authentication required");
    }

    #[tokio::test]
    async fn check_secured_allows_authenticated() {
        let data = std::collections::HashMap::from([("user_id".into(), "42".into())]);
        let session = crate::session::Session::new_for_test("sess".into(), data);
        let result = __check_secured(&session, &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_secured_rejects_wrong_role() {
        let data = std::collections::HashMap::from([
            ("user_id".into(), "42".into()),
            ("role".into(), "viewer".into()),
        ]);
        let session = crate::session::Session::new_for_test("sess".into(), data);
        let result = __check_secured(&session, &["admin"]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.to_string(), "insufficient permissions");
    }

    #[tokio::test]
    async fn check_secured_allows_matching_role() {
        let data = std::collections::HashMap::from([
            ("user_id".into(), "42".into()),
            ("role".into(), "admin".into()),
        ]);
        let session = crate::session::Session::new_for_test("sess".into(), data);
        let result = __check_secured(&session, &["admin"]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_secured_allows_any_of_multiple_roles() {
        let data = std::collections::HashMap::from([
            ("user_id".into(), "42".into()),
            ("role".into(), "editor".into()),
        ]);
        let session = crate::session::Session::new_for_test("sess".into(), data);
        let result = __check_secured(&session, &["admin", "editor"]).await;
        assert!(result.is_ok());
    }

    // ── #[secured] macro integration tests ──────────────────────

    #[tokio::test]
    async fn secured_macro_rejects_unauthenticated() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer};

        #[autumn_macros::secured]
        async fn protected_handler() -> crate::AutumnResult<&'static str> {
            Ok("secret")
        }

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/", get(protected_handler))
            .layer(SessionLayer::new(
                MemoryStore::new(),
                SessionConfig::default(),
            ))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn secured_macro_allows_authenticated() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        #[autumn_macros::secured]
        async fn protected_handler() -> crate::AutumnResult<&'static str> {
            Ok("secret")
        }

        let store = MemoryStore::new();
        store
            .save(
                "sess1",
                std::collections::HashMap::from([("user_id".into(), "42".into())]),
            )
            .await
            .unwrap();

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/", get(protected_handler))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=sess1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "secret");
    }

    #[tokio::test]
    async fn secured_macro_honors_configured_auth_session_key() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        #[autumn_macros::secured]
        async fn account_handler() -> crate::AutumnResult<&'static str> {
            Ok("account")
        }

        let store = MemoryStore::new();
        store
            .save(
                "sess1",
                std::collections::HashMap::from([
                    ("uid".into(), "42".into()),
                    ("account_id".into(), "42".into()),
                ]),
            )
            .await
            .unwrap();

        let state = test_app_state("uid");

        let app = Router::new()
            .route("/account", get(account_handler))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/account")
                    .header(COOKIE, "autumn.sid=sess1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "account");
    }

    #[tokio::test]
    async fn secured_macro_with_role_rejects_wrong_role() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        #[autumn_macros::secured("admin")]
        async fn admin_only() -> crate::AutumnResult<&'static str> {
            Ok("admin area")
        }

        let store = MemoryStore::new();
        store
            .save(
                "sess1",
                std::collections::HashMap::from([
                    ("user_id".into(), "42".into()),
                    ("role".into(), "viewer".into()),
                ]),
            )
            .await
            .unwrap();

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/", get(admin_only))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=sess1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn secured_macro_with_multiple_roles_allows_match() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        #[autumn_macros::secured("admin", "editor")]
        async fn content_handler() -> crate::AutumnResult<&'static str> {
            Ok("content")
        }

        let store = MemoryStore::new();
        store
            .save(
                "sess1",
                std::collections::HashMap::from([
                    ("user_id".into(), "42".into()),
                    ("role".into(), "editor".into()),
                ]),
            )
            .await
            .unwrap();

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/", get(content_handler))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(COOKIE, "autumn.sid=sess1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "content");
    }

    #[tokio::test]
    async fn require_auth_allows_authenticated() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        let store = MemoryStore::new();
        // Pre-populate a session with user_id
        let mut session_data = std::collections::HashMap::new();
        session_data.insert("user_id".into(), "42".into());
        store.save("valid-session", session_data).await.unwrap();

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/protected", get(|| async { "secret" }))
            .layer(RequireAuth::new("user_id"))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .header(COOKIE, "autumn.sid=valid-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "secret");
    }

    #[tokio::test]
    async fn require_auth_sets_rate_limit_principal() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::security::RateLimitPrincipal;
        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        async fn handler(
            axum::Extension(principal): axum::Extension<RateLimitPrincipal>,
        ) -> String {
            principal.0
        }

        let store = MemoryStore::new();
        let mut session_data = std::collections::HashMap::new();
        session_data.insert("user_id".into(), "42".into());
        store.save("valid-session", session_data).await.unwrap();

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/protected", get(handler))
            .layer(RequireAuth::new("user_id"))
            .layer(SessionLayer::new(store, SessionConfig::default()))
            .with_state(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .header(COOKIE, "autumn.sid=valid-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "42");
    }

    #[tokio::test]
    async fn require_auth_rate_limits_by_session_principal() {
        // Verifies end-to-end: RequireAuth (outer) sets RateLimitPrincipal so a
        // route-scoped RateLimitLayer (inner) keys on the principal, not the IP.
        // Layer composition:  Session → RequireAuth → RateLimitLayer → handler
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use http::header::COOKIE;
        use tower::ServiceExt;

        use crate::security::{KeyStrategy, RateLimitConfig, RateLimitLayer};
        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        let session_store = MemoryStore::new();
        let mut data_a = std::collections::HashMap::new();
        data_a.insert("user_id".into(), "user-1".into());
        session_store.save("sess-a", data_a).await.unwrap();
        let mut data_b = std::collections::HashMap::new();
        data_b.insert("user_id".into(), "user-2".into());
        session_store.save("sess-b", data_b).await.unwrap();

        let rl_config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1,
            key_strategy: KeyStrategy::AuthenticatedPrincipal,
            ..Default::default()
        };

        let state = test_app_state("user_id");

        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(RateLimitLayer::from_config(&rl_config)) // inner — reads principal
            .layer(RequireAuth::new("user_id")) // outer — sets RateLimitPrincipal
            .layer(SessionLayer::new(session_store, SessionConfig::default()))
            .with_state(state);

        // user-1 first request: allowed (1 token in bucket).
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .header(COOKIE, "autumn.sid=sess-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // user-1 second request: bucket exhausted → 429.
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .header(COOKIE, "autumn.sid=sess-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        // user-2 first request: separate bucket → allowed.
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/protected")
                    .header(COOKIE, "autumn.sid=sess-b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_auth_poll_ready_propagates() {
        use std::task::{Context, Poll};
        use tower::{Layer, Service};

        #[derive(Clone)]
        struct MockService {
            ready: bool,
            poll_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Service<axum::extract::Request> for MockService {
            type Response = axum::response::Response;
            type Error = std::convert::Infallible;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                self.poll_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.ready {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }

            fn call(&mut self, _req: axum::extract::Request) -> Self::Future {
                std::future::ready(Ok(axum::response::Response::new(axum::body::Body::empty())))
            }
        }

        let layer = RequireAuth::new("user_id");
        let poll_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock_service = MockService {
            ready: false,
            poll_count: poll_count.clone(),
        };
        let mut service = layer.layer(mock_service);

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        // When inner is not ready, RequireAuthService should not be ready
        let poll = service.poll_ready(&mut cx);
        assert!(poll.is_pending());
        assert_eq!(poll_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // When inner is ready, RequireAuthService should be ready
        let mock_service_ready = MockService {
            ready: true,
            poll_count: poll_count.clone(),
        };
        let mut service_ready = layer.layer(mock_service_ready);
        let poll_ready = service_ready.poll_ready(&mut cx);
        assert!(poll_ready.is_ready());
        assert_eq!(poll_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn auth_rejection_into_response() {
        let rejection = AuthRejection;
        let response = rejection.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 401);
        assert_eq!(json["detail"], "authentication required");
        assert_eq!(json["code"], "autumn.unauthorized");
    }

    #[test]
    fn test_auth_config_defaults() {
        let config = AuthConfig::default();
        assert_eq!(config.bcrypt_cost, DEFAULT_BCRYPT_COST);
        assert_eq!(config.session_key, "user_id");
    }

    #[tokio::test]
    async fn test_hash_password() {
        let test_input = uuid::Uuid::new_v4().to_string();

        // Test hashing
        let hash = super::hash_password(&test_input)
            .await
            .expect("Failed to hash password");
        assert!(hash.starts_with("$2b$"));

        // Test verification with correct password
        let is_valid = super::verify_password(&test_input, &hash)
            .await
            .expect("Failed to verify password");
        assert!(is_valid, "Password should be verified successfully");

        // Test verification with incorrect password
        let is_invalid = super::verify_password(&uuid::Uuid::new_v4().to_string(), &hash)
            .await
            .expect("Failed to verify wrong password");
        assert!(!is_invalid, "Wrong password should not be verified");
    }

    #[tokio::test]
    async fn test_hash_password_empty() {
        let test_input = String::new();
        let hash = super::hash_password(&test_input)
            .await
            .expect("Failed to hash empty password");
        assert!(hash.starts_with("$2b$"));

        let is_valid = super::verify_password(&test_input, &hash)
            .await
            .expect("Failed to verify empty password");
        assert!(is_valid, "Empty password should be verified successfully");
    }

    #[tokio::test]
    async fn test_hash_password_long() {
        // bcrypt truncates after 72 bytes. We just want to ensure it doesn't crash.
        let test_input = "a".repeat(100);
        let hash = super::hash_password(&test_input)
            .await
            .expect("Failed to hash long password");
        assert!(hash.starts_with("$2b$"));

        let is_valid = super::verify_password(&test_input, &hash)
            .await
            .expect("Failed to verify long password");
        assert!(is_valid, "Long password should be verified successfully");
    }

    #[tokio::test]
    async fn test_hash_password_unicode() {
        // Test with non-ascii characters
        let test_input = format!("{}🚀my_secrët_passwörd🔑", uuid::Uuid::new_v4());
        let hash = super::hash_password(&test_input)
            .await
            .expect("Failed to hash unicode password");
        assert!(hash.starts_with("$2b$"));

        let is_valid = super::verify_password(&test_input, &hash)
            .await
            .expect("Failed to verify unicode password");
        assert!(is_valid, "Unicode password should be verified successfully");
    }

    #[tokio::test]
    async fn test_verify_password_invalid_hash() {
        // Ensure that providing invalid hashes doesn't crash or cause issues, but returns an error/false
        let test_input = uuid::Uuid::new_v4().to_string();

        // Invalid prefix
        let result = super::verify_password(&test_input, "invalid_hash_string").await;
        assert!(result.is_err() || !result.unwrap());

        // Truncated hash
        let result2 = super::verify_password(&test_input, "$2b$04$").await;
        assert!(result2.is_err() || !result2.unwrap());
    }
}

// ── HttpRequestBuilder interceptor task-local scope tests ────────────────────

#[cfg(feature = "oauth2")]
#[cfg(test)]
mod http_interceptor_task_local_tests {
    use crate::interceptor::{ACTIVE_HTTP_INTERCEPTORS, HttpInterceptor, HttpInterceptorFuture};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct FlagInterceptor {
        fired: Arc<AtomicBool>,
    }

    impl HttpInterceptor for FlagInterceptor {
        fn intercept<'a>(
            &'a self,
            req: reqwest::Request,
            next: &'a dyn Fn(reqwest::Request) -> HttpInterceptorFuture<'a>,
        ) -> HttpInterceptorFuture<'a> {
            self.fired.store(true, Ordering::SeqCst);
            // Delegate to next so the caller gets a real (likely connection-refused)
            // error back — we discard it in the test with `let _ = ...`.
            next(req)
        }
    }

    /// Proves the task-local scope contract: when `ACTIVE_HTTP_INTERCEPTORS` is
    /// set via `.scope()` (as `run_one_off_task_mode` must do), the interceptor
    /// fires on every `HttpRequestBuilder::send` call within that scope.
    #[tokio::test]
    async fn http_request_builder_send_fires_interceptor_inside_scope() {
        let fired = Arc::new(AtomicBool::new(false));
        let interceptor: Arc<dyn HttpInterceptor> = Arc::new(FlagInterceptor {
            fired: Arc::clone(&fired),
        });

        let client = reqwest::Client::new();
        let http_client = super::HttpClient::new(client);

        ACTIVE_HTTP_INTERCEPTORS
            .scope(vec![interceptor], async {
                let _ = http_client
                    .get("http://127.0.0.1:54321/noreply")
                    .send()
                    .await;
            })
            .await;

        assert!(
            fired.load(Ordering::SeqCst),
            "interceptor must fire when ACTIVE_HTTP_INTERCEPTORS scope is established"
        );
    }

    /// Proves the regression: without a scope, the interceptor is silently
    /// skipped. The fix in `run_one_off_task_mode` wraps the task handler in
    /// `ACTIVE_HTTP_INTERCEPTORS.scope(...)` so that registered interceptors are
    /// always active during task execution.
    #[tokio::test]
    async fn http_request_builder_send_skips_interceptor_outside_scope() {
        let fired = Arc::new(AtomicBool::new(false));
        let _interceptor: Arc<dyn HttpInterceptor> = Arc::new(FlagInterceptor {
            fired: Arc::clone(&fired),
        });

        // Intentionally do NOT establish a scope — simulating pre-fix task mode.
        let client = reqwest::Client::new();
        let http_client = super::HttpClient::new(client);
        let _ = http_client
            .get("http://127.0.0.1:54321/noreply")
            .send()
            .await;

        assert!(
            !fired.load(Ordering::SeqCst),
            "interceptor must NOT fire when ACTIVE_HTTP_INTERCEPTORS scope is absent"
        );
    }
}

#[cfg(test)]
mod api_token_tests {
    use std::sync::Arc;

    use http::StatusCode;

    use super::{
        ApiToken, ApiTokenStore, InMemoryApiTokenStore, RequireApiToken, hash_api_token,
        issue_api_token, revoke_api_token,
    };

    struct FailingApiTokenStore;

    impl ApiTokenStore for FailingApiTokenStore {
        fn issue<'a>(
            &'a self,
            _principal_id: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<String>> + Send + 'a>,
        > {
            Box::pin(async {
                Err(crate::AutumnError::service_unavailable_msg(
                    "api token store unavailable",
                ))
            })
        }

        fn verify<'a>(
            &'a self,
            _raw_token: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>,
        > {
            Box::pin(async {
                Err(crate::AutumnError::service_unavailable_msg(
                    "api token store unavailable",
                ))
            })
        }

        fn revoke<'a>(
            &'a self,
            _raw_token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
        {
            Box::pin(async {
                Err(crate::AutumnError::service_unavailable_msg(
                    "api token store unavailable",
                ))
            })
        }
    }

    // ── hash_api_token ───────────────────────────────────────────────────────

    #[test]
    fn hash_api_token_is_deterministic() {
        let h1 = hash_api_token("abc123");
        let h2 = hash_api_token("abc123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_api_token_produces_64_char_hex() {
        let hash = hash_api_token("any_raw_token");
        assert_eq!(hash.len(), 64, "SHA-256 hex must be 64 chars");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be lowercase hex digits"
        );
    }

    #[test]
    fn hash_api_token_differs_from_input() {
        let raw = "my_raw_token";
        assert_ne!(hash_api_token(raw), raw);
    }

    #[test]
    fn hash_api_token_different_inputs_produce_different_hashes() {
        assert_ne!(hash_api_token("token_a"), hash_api_token("token_b"));
    }

    // ── InMemoryApiTokenStore ────────────────────────────────────────────────

    #[tokio::test]
    async fn in_memory_store_issue_returns_unique_tokens() {
        let store = InMemoryApiTokenStore::default();
        let t1 = store.issue("user:1").await.unwrap();
        let t2 = store.issue("user:1").await.unwrap();
        assert_ne!(t1, t2, "each issued token must be unique");
        assert!(t1.len() >= 32, "token must have sufficient entropy");
    }

    #[tokio::test]
    async fn in_memory_store_verify_returns_principal_for_valid_token() {
        let store = InMemoryApiTokenStore::default();
        let raw = store.issue("user:42").await.unwrap();
        let principal = store.verify(&raw).await.unwrap();
        assert_eq!(principal, Some("user:42".to_owned()));
    }

    #[tokio::test]
    async fn in_memory_store_verify_returns_none_for_unknown_token() {
        let store = InMemoryApiTokenStore::default();
        let result = store.verify("not_a_real_token").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn in_memory_store_revoke_invalidates_token() {
        let store = InMemoryApiTokenStore::default();
        let raw = store.issue("user:7").await.unwrap();
        assert_eq!(
            store.verify(&raw).await.unwrap(),
            Some("user:7".to_owned()),
            "token must be valid before revoking"
        );
        store.revoke(&raw).await.unwrap();
        assert_eq!(store.verify(&raw).await.unwrap(), None);
    }

    #[tokio::test]
    async fn in_memory_store_raw_token_not_stored_verbatim() {
        let store = InMemoryApiTokenStore::default();
        let raw = store.issue("user:1").await.unwrap();
        // Appending a character changes the hash → lookup must return None.
        let tampered = format!("{raw}x");
        assert_eq!(store.verify(&tampered).await.unwrap(), None);
    }

    #[tokio::test]
    async fn issue_api_token_helper_issues_verifiable_token() {
        let store = InMemoryApiTokenStore::default();
        let raw = issue_api_token(&store, "user:5").await.unwrap();
        assert_eq!(store.verify(&raw).await.unwrap(), Some("user:5".to_owned()));
    }

    #[tokio::test]
    async fn revoke_api_token_helper_revokes_token() {
        let store = InMemoryApiTokenStore::default();
        let raw = store.issue("user:6").await.unwrap();
        revoke_api_token(&store, &raw).await.unwrap();
        assert_eq!(store.verify(&raw).await.unwrap(), None);
    }

    // ── Scoped service tokens (issue #1158) ──────────────────────────────────

    use super::{
        __check_secured_scopes, ApiTokenScopes, IssueTokenSpec, issue_scoped_api_token,
        list_api_tokens, rotate_api_token,
    };
    use crate::time::{FixedClock, TickingClock};
    use chrono::{Duration as ChronoDuration, TimeZone as _, Utc};

    fn scopes(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    /// A legacy store implementing only the original three methods relies on
    /// the default `verify_scoped`, which must yield empty scopes — proving the
    /// scoped surface is purely additive.
    #[derive(Default)]
    struct LegacyOnlyStore(InMemoryApiTokenStore);

    impl ApiTokenStore for LegacyOnlyStore {
        fn issue<'a>(
            &'a self,
            principal_id: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<String>> + Send + 'a>,
        > {
            self.0.issue(principal_id)
        }
        fn verify<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<Option<String>>> + Send + 'a>,
        > {
            self.0.verify(raw_token)
        }
        fn revoke<'a>(
            &'a self,
            raw_token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
        {
            self.0.revoke(raw_token)
        }
    }

    #[tokio::test]
    async fn legacy_store_default_verify_scoped_yields_empty_scopes() {
        let store = LegacyOnlyStore::default();
        let raw = store.issue("user:1").await.unwrap();
        let verified = store.verify_scoped(&raw).await.unwrap().unwrap();
        assert_eq!(verified.principal_id, "user:1");
        assert!(verified.scopes.is_empty());
    }

    #[tokio::test]
    async fn issue_scoped_round_trips_name_and_scopes() {
        let store = InMemoryApiTokenStore::default();
        let granted = scopes(&["posts:read", "posts:write"]);
        let raw = issue_scoped_api_token(
            &store,
            IssueTokenSpec {
                principal_id: "service:ci",
                name: "ci",
                scopes: &granted,
                expires_at: None,
            },
        )
        .await
        .unwrap();

        let verified = store.verify_scoped(&raw).await.unwrap().unwrap();
        assert_eq!(verified.principal_id, "service:ci");
        assert_eq!(verified.scopes, granted);

        let listed = list_api_tokens(&store, "service:ci").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "ci");
        assert_eq!(listed[0].scopes, granted);
    }

    #[tokio::test]
    async fn expired_token_verifies_as_none() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let store = InMemoryApiTokenStore::default().with_clock(Arc::new(FixedClock::at(now)));
        let granted = scopes(&["posts:read"]);
        let raw = store
            .issue_scoped(IssueTokenSpec {
                principal_id: "service:ci",
                name: "ci",
                scopes: &granted,
                expires_at: Some(now - ChronoDuration::seconds(1)),
            })
            .await
            .unwrap();

        assert_eq!(store.verify(&raw).await.unwrap(), None);
        assert!(store.verify_scoped(&raw).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unexpired_token_verifies_then_records_last_used_at() {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        let store = InMemoryApiTokenStore::default().with_clock(Arc::new(clock));
        let granted = scopes(&["posts:read"]);
        let raw = store
            .issue_scoped(IssueTokenSpec {
                principal_id: "service:ci",
                name: "ci",
                scopes: &granted,
                expires_at: Some(start + ChronoDuration::days(30)),
            })
            .await
            .unwrap();

        // Before use, last_used_at is unset.
        assert!(
            list_api_tokens(&store, "service:ci").await.unwrap()[0]
                .last_used_at
                .is_none()
        );

        assert!(store.verify_scoped(&raw).await.unwrap().is_some());

        assert!(
            list_api_tokens(&store, "service:ci").await.unwrap()[0]
                .last_used_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn list_metadata_carries_no_secret_and_reflects_revocation() {
        let store = InMemoryApiTokenStore::default();
        let granted = scopes(&["posts:read"]);
        let raw = store
            .issue_scoped(IssueTokenSpec {
                principal_id: "service:ci",
                name: "ci",
                scopes: &granted,
                expires_at: None,
            })
            .await
            .unwrap();

        let listed = list_api_tokens(&store, "service:ci").await.unwrap();
        assert_eq!(listed.len(), 1);
        // Metadata must not expose anything replayable as a credential: the raw
        // token and its hash never appear in TokenMetadata's fields.
        assert!(listed[0].revoked_at.is_none());

        store.revoke(&raw).await.unwrap();
        assert_eq!(store.verify(&raw).await.unwrap(), None);
        let listed = list_api_tokens(&store, "service:ci").await.unwrap();
        assert!(listed[0].revoked_at.is_some());
    }

    #[tokio::test]
    async fn rotate_revokes_old_and_preserves_scopes() {
        let store = InMemoryApiTokenStore::default();
        let granted = scopes(&["posts:read", "posts:write"]);
        let old = store
            .issue_scoped(IssueTokenSpec {
                principal_id: "service:ci",
                name: "ci",
                scopes: &granted,
                expires_at: None,
            })
            .await
            .unwrap();

        let new = rotate_api_token(&store, &old).await.unwrap().unwrap();
        assert_ne!(new, old);
        // Old token no longer authenticates.
        assert!(store.verify_scoped(&old).await.unwrap().is_none());
        // New token carries the same scopes.
        let verified = store.verify_scoped(&new).await.unwrap().unwrap();
        assert_eq!(verified.scopes, granted);
        assert_eq!(verified.principal_id, "service:ci");

        // Rotating an unknown token yields None.
        assert!(rotate_api_token(&store, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn check_secured_scopes_is_default_deny_and_all_must_match() {
        // Empty requirement is a no-op.
        assert!(__check_secured_scopes(None, &[]).await.is_ok());
        // No granted scopes but a requirement => denied (403).
        let err = __check_secured_scopes(None, &["posts:write"])
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        // Subset present => allowed.
        let granted = ApiTokenScopes(scopes(&["posts:read", "posts:write"]));
        assert!(
            __check_secured_scopes(Some(&granted), &["posts:write"])
                .await
                .is_ok()
        );
        // Missing one of several required => denied (all-must-match).
        assert!(
            __check_secured_scopes(Some(&granted), &["posts:write", "posts:delete"])
                .await
                .is_err()
        );
    }

    // ── RequireApiToken middleware ───────────────────────────────────────────

    #[tokio::test]
    async fn require_api_token_rejects_missing_authorization_header() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_api_token_rejects_non_bearer_scheme() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_api_token_rejects_unknown_bearer_token() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, "Bearer unknown_token_xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_api_token_propagates_store_verify_errors() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(FailingApiTokenStore);
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, "Bearer valid_client_token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap_or_default()),
            Some("application/problem+json")
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 503);
        assert_eq!(json["code"], "autumn.service_unavailable");
        assert_eq!(json["detail"], "api token store unavailable");
    }

    #[tokio::test]
    async fn require_api_token_allows_valid_bearer_token() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = store.issue("user:1").await.unwrap();
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(Arc::clone(&store)));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_api_token_accepts_case_insensitive_bearer_scheme() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = store.issue("user:1").await.unwrap();

        for scheme in ["bearer", "bEaReR"] {
            let app = axum::Router::new()
                .route("/", axum::routing::get(|| async { "ok" }))
                .layer(RequireApiToken::new(Arc::clone(&store)));

            let response = app
                .oneshot(
                    http::Request::builder()
                        .uri("/")
                        .header(http::header::AUTHORIZATION, format!("{scheme} {raw}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "scheme {scheme}");
        }
    }

    #[tokio::test]
    async fn require_api_token_rejects_revoked_token() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = store.issue("user:1").await.unwrap();
        store.revoke(&raw).await.unwrap();
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(Arc::clone(&store)));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_api_token_401_response_has_problem_details() {
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or_default()),
            Some("application/problem+json")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 401);
        assert_eq!(json["code"], "autumn.unauthorized");
        assert!(json["detail"].as_str().is_some());
    }

    #[tokio::test]
    async fn require_api_token_401_problem_details_include_request_context() {
        use crate::middleware::RequestIdLayer;
        use axum::body::Body;
        use tower::ServiceExt;

        let store = Arc::new(InMemoryApiTokenStore::default());
        let app = axum::Router::new()
            .route("/api/private", axum::routing::get(|| async { "ok" }))
            .layer(RequireApiToken::new(store))
            .layer(RequestIdLayer);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/api/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("request id header should be present")
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["request_id"], request_id);
        assert_eq!(json["instance"], "/api/private");
    }

    // ── ApiToken extractor ───────────────────────────────────────────────────

    #[tokio::test]
    async fn api_token_extractor_yields_principal_id_to_handler() {
        use axum::body::Body;
        use tower::ServiceExt;

        async fn handler(ApiToken(principal): ApiToken) -> String {
            principal
        }

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = store.issue("user:99").await.unwrap();
        let app = axum::Router::new()
            .route("/", axum::routing::get(handler))
            .layer(RequireApiToken::new(Arc::clone(&store)));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header(http::header::AUTHORIZATION, format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "user:99");
    }

    #[tokio::test]
    async fn api_token_extractor_rejects_when_no_principal_in_extensions() {
        use axum::body::Body;
        use tower::ServiceExt;

        async fn handler(ApiToken(principal): ApiToken) -> String {
            principal
        }

        let app = axum::Router::new().route("/", axum::routing::get(handler));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // ── Composition with session auth ────────────────────────────────────────

    #[tokio::test]
    async fn api_token_and_session_auth_compose_without_conflict() {
        use axum::body::Body;
        use tower::ServiceExt;

        use crate::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

        async fn api_handler(ApiToken(principal): ApiToken) -> String {
            principal
        }

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = store.issue("api_user").await.unwrap();

        let session_store = MemoryStore::new();
        session_store
            .save(
                "sess1",
                std::collections::HashMap::from([("user_id".into(), "session_user".into())]),
            )
            .await
            .unwrap();

        let app = axum::Router::new()
            .route(
                "/api",
                axum::routing::get(api_handler).layer(RequireApiToken::new(Arc::clone(&store))),
            )
            .layer(SessionLayer::new(session_store, SessionConfig::default()));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/api")
                    .header(http::header::AUTHORIZATION, format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "api_user");
    }

    // ── poll_ready propagation ───────────────────────────────────────────────

    #[tokio::test]
    async fn require_api_token_poll_ready_propagates_to_inner() {
        use std::task::{Context, Poll};
        use tower::{Layer, Service};

        #[derive(Clone)]
        struct MockService {
            ready: bool,
        }

        impl tower::Service<axum::extract::Request> for MockService {
            type Response = axum::response::Response;
            type Error = std::convert::Infallible;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                if self.ready {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }

            fn call(&mut self, _req: axum::extract::Request) -> Self::Future {
                std::future::ready(Ok(axum::response::Response::new(axum::body::Body::empty())))
            }
        }

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let store = Arc::new(InMemoryApiTokenStore::default());
        let layer = RequireApiToken::new(store);
        let mut svc = layer.layer(MockService { ready: false });
        assert!(svc.poll_ready(&mut cx).is_pending());

        let store2 = Arc::new(InMemoryApiTokenStore::default());
        let layer2 = RequireApiToken::new(store2);
        let mut svc2 = layer2.layer(MockService { ready: true });
        assert!(svc2.poll_ready(&mut cx).is_ready());
    }

    #[tokio::test]
    async fn require_api_token_rate_limits_by_principal() {
        // Verifies end-to-end: RequireApiToken (outer) sets RateLimitPrincipal
        // with the VERIFIED principal ID so a route-scoped RateLimitLayer (inner)
        // keys on the principal — two different tokens for the same principal share
        // one bucket.  Layer composition: RequireApiToken → RateLimitLayer → handler
        use axum::body::Body;
        use tower::ServiceExt;

        use crate::security::{KeyStrategy, RateLimitConfig, RateLimitLayer};

        let rl_config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1,
            key_strategy: KeyStrategy::AuthenticatedPrincipal,
            ..Default::default()
        };

        let store = Arc::new(InMemoryApiTokenStore::default());
        let token_a1 = issue_api_token(&*store, "principal-1").await.unwrap();
        let token_a2 = issue_api_token(&*store, "principal-1").await.unwrap(); // second token, same principal
        let token_b = issue_api_token(&*store, "principal-2").await.unwrap();

        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(RateLimitLayer::from_config(&rl_config)) // inner — reads principal
            .layer(RequireApiToken::new(Arc::clone(&store))); // outer — sets RateLimitPrincipal

        // principal-1, first token: allowed.
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token_a1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // principal-1, different token: shares the same bucket → 429.
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token_a2}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "second token for the same principal must share the rate-limit bucket"
        );

        // principal-2: separate bucket → allowed.
        let r = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token_b}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_api_token_sets_rate_limit_principal() {
        use axum::body::Body;
        use tower::ServiceExt;

        use crate::security::RateLimitPrincipal;

        async fn handler(
            axum::Extension(principal): axum::Extension<RateLimitPrincipal>,
        ) -> String {
            principal.0
        }

        let store = Arc::new(InMemoryApiTokenStore::default());
        let raw = issue_api_token(&*store, "agent:bot").await.unwrap();

        let app = axum::Router::new()
            .route("/", axum::routing::get(handler))
            .layer(RequireApiToken::new(Arc::clone(&store)));

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "agent:bot");
    }
}

// ── OAuth2 unit tests (separate module for clean imports) ─────────────────────

#[cfg(feature = "oauth2")]
#[cfg(test)]
mod oauth2_unit_tests {
    use std::collections::HashMap;

    use super::{
        AuthConfig, OAuth2ProviderConfig, OAuthLinkingPolicy, oauth2_authorize_url, provider_preset,
    };

    #[allow(dead_code)]
    fn make_provider(authorize_url: &str) -> OAuth2ProviderConfig {
        OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: authorize_url.into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: "openid profile".into(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        }
    }

    #[test]
    fn provider_preset_google_returns_oidc_config() {
        let preset = provider_preset("google").expect("google preset must exist");
        assert!(
            !preset.authorize_url.is_empty(),
            "google authorize_url must not be empty"
        );
        assert!(
            !preset.token_url.is_empty(),
            "google token_url must not be empty"
        );
        assert!(
            preset.discovery_url.is_some(),
            "google must have discovery_url for OIDC"
        );
        assert!(
            preset.scope.contains("openid"),
            "google preset scope must include openid: {}",
            preset.scope
        );
        assert!(
            preset.scope.contains("email"),
            "google preset scope must include email: {}",
            preset.scope
        );
        assert_eq!(
            preset.client_id, "",
            "client_id must be empty in preset (user fills in)"
        );
        assert_eq!(
            preset.client_secret, "",
            "client_secret must be empty in preset"
        );
        assert_eq!(
            preset.redirect_uri, "",
            "redirect_uri must be empty in preset"
        );
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn provider_preset_github_returns_pure_oauth2_config() {
        let preset = provider_preset("github").expect("github preset must exist");
        assert!(
            !preset.authorize_url.is_empty(),
            "github authorize_url must not be empty"
        );
        assert!(
            !preset.token_url.is_empty(),
            "github token_url must not be empty"
        );
        assert!(
            preset.userinfo_url.is_some(),
            "github must have userinfo_url (it is not OIDC)"
        );
        assert!(
            preset.discovery_url.is_none(),
            "github must NOT have discovery_url (pure OAuth2)"
        );
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn provider_preset_microsoft_returns_oidc_config() {
        let preset = provider_preset("microsoft").expect("microsoft preset must exist");
        assert!(
            !preset.authorize_url.is_empty(),
            "microsoft authorize_url must not be empty"
        );
        assert!(
            preset.discovery_url.is_some(),
            "microsoft must have discovery_url for OIDC"
        );
        assert!(
            preset.scope.contains("openid"),
            "microsoft preset scope must include openid: {}",
            preset.scope
        );
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn provider_preset_unknown_returns_none() {
        assert!(
            provider_preset("nonexistent_provider_xyz").is_none(),
            "unknown provider must return None"
        );
    }

    #[cfg(feature = "oauth2")]
    #[tokio::test]
    async fn oauth2_authorize_url_includes_pkce_code_challenge() {
        let session = crate::session::Session::new_for_test("s1".into(), HashMap::new());
        let provider = OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: "openid profile".into(),
            issuer: None,
            jwks_url: None,
            discovery_url: None,
        };
        let url = oauth2_authorize_url(&session, "testprovider", &provider)
            .await
            .unwrap();
        assert!(
            url.contains("code_challenge="),
            "PKCE code_challenge must be present in URL: {url}"
        );
        assert!(
            url.contains("code_challenge_method=S256"),
            "PKCE method must be S256: {url}"
        );
        assert!(
            session
                .get("oauth2:testprovider:code_verifier")
                .await
                .is_some(),
            "code_verifier must be stored in session for later exchange"
        );
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn oauth2_provider_config_has_discovery_url_field() {
        let provider = OAuth2ProviderConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: None,
            redirect_uri: "http://localhost:3000/callback".into(),
            scope: String::new(),
            issuer: None,
            jwks_url: None,
            discovery_url: Some("https://idp.example".into()),
        };
        assert_eq!(
            provider.discovery_url.as_deref(),
            Some("https://idp.example"),
            "discovery_url must be accessible as a field"
        );
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn auth_config_has_oauth_linking_policy() {
        let config = AuthConfig::default();
        // Default policy must be CreateAccount so unknown provider identities
        // automatically create a local user record.
        assert!(
            matches!(
                config.oauth_linking_policy,
                OAuthLinkingPolicy::CreateAccount
            ),
            "default linking policy must be CreateAccount"
        );
    }
}
