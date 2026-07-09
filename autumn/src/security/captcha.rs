//! Bot protection via pluggable CAPTCHA providers (Issue #828).
//!
//! Protects public-facing forms against automated abuse by verifying a
//! CAPTCHA token server-side before allowing a request to reach its handler.
//!
//! # Quick start
//!
//! ## 1. Configure in `autumn.toml`
//!
//! ```toml
//! [bot_protection]
//! enabled = true
//! provider = "turnstile"   # "turnstile" (default) or "hcaptcha"
//! site_key  = "..."        # client-side widget key
//! secret_key = "..."       # server-side verification secret (use env var!)
//! ```
//!
//! ## 2. Add the widget to your Maud form
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::captcha::bot_protection_widget;
//!
//! #[get("/signup")]
//! async fn signup_form(config: AutumnConfig) -> Markup {
//!     html! {
//!         form method="POST" action="/signup" {
//!             input type="text" name="email";
//!             (bot_protection_widget(&config.bot_protection))
//!             button { "Sign up" }
//!         }
//!     }
//! }
//! ```
//!
//! ## 3. The middleware verifies automatically
//!
//! When `bot_protection.enabled = true` the framework wires [`BotProtectionLayer`]
//! into every POST/PUT/PATCH/DELETE request.  Requests without a valid CAPTCHA
//! token receive a `400 Bad Request` Problem Details response before reaching
//! the handler.
//!
//! ## Dev-mode bypass
//!
//! Set `dev_bypass = true` (the default when no `secret_key` is configured)
//! to skip verification in local development:
//!
//! ```toml
//! [bot_protection]
//! enabled = true
//! dev_bypass = true   # skip verification; any token (or none) passes
//! ```
//!
//! # Pluggable providers
//!
//! Implement [`CaptchaProvider`] to add a custom CAPTCHA backend.  Pass it to
//! [`BotProtectionLayer::new`] for use in tests or custom deployments.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::http::{Request, Response, StatusCode};
use tower::{Layer, Service};

use serde::Deserialize;

// ── Configuration ──────────────────────────────────────────────────────────

/// Which CAPTCHA backend to use.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptchaProviderKind {
    /// Cloudflare Turnstile — privacy-first, free tier, no PII collected.
    /// Form field: `cf-turnstile-response`.
    #[default]
    Turnstile,
    /// hCaptcha — widely deployed alternative.
    /// Form field: `h-captcha-response`.
    HCaptcha,
}

/// Bot-protection configuration block (`[bot_protection]` in `autumn.toml`).
///
/// # Example
///
/// ```toml
/// [bot_protection]
/// enabled    = true
/// provider   = "turnstile"
/// site_key   = "0x4AAAA..."          # shown in the widget (safe to commit)
/// secret_key = "..."                 # server-side secret (use AUTUMN_BOT_PROTECTION__SECRET_KEY env var)
/// dev_bypass = false
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BotProtectionConfig {
    /// Wire the bot-protection middleware globally. Default: `false`.
    ///
    /// When `false`, the global auto-wired layer is not applied but you can
    /// still apply [`BotProtectionLayer::new`] manually to a scoped router.
    /// [`bot_protection_widget`] renders the widget whenever `site_key` is set,
    /// regardless of this flag — so forms work correctly in the scoped pattern.
    #[serde(default)]
    pub enabled: bool,

    /// Which CAPTCHA provider to use. Default: `turnstile`.
    #[serde(default)]
    pub provider: CaptchaProviderKind,

    /// Public site key (safe to commit; rendered into the widget HTML).
    #[serde(default)]
    pub site_key: Option<String>,

    /// Private secret key used for server-side token verification.
    ///
    /// Set via the `AUTUMN_BOT_PROTECTION__SECRET_KEY` environment variable
    /// in production — never commit this value.
    #[serde(default)]
    pub secret_key: Option<String>,

    /// Override the default form field name for the CAPTCHA token.
    ///
    /// Defaults to the provider's canonical name:
    /// - Turnstile: `cf-turnstile-response`
    /// - hCaptcha: `h-captcha-response`
    #[serde(default)]
    pub form_field: Option<String>,

    /// Skip token verification entirely.
    ///
    /// When `true`, any request passes regardless of whether a CAPTCHA token
    /// is present or valid.  Use in local development and test environments.
    /// Default: `false`.
    #[serde(default)]
    pub dev_bypass: bool,
}

impl BotProtectionConfig {
    /// Returns the form field name for the CAPTCHA token.
    ///
    /// Uses the custom field name if set, otherwise the provider's canonical default.
    #[must_use]
    pub fn effective_form_field(&self) -> &str {
        self.form_field.as_deref().unwrap_or(match self.provider {
            CaptchaProviderKind::Turnstile => "cf-turnstile-response",
            CaptchaProviderKind::HCaptcha => "h-captcha-response",
        })
    }
}

// ── Provider trait ─────────────────────────────────────────────────────────

/// Object-safe async CAPTCHA provider.
///
/// Implement this to add a custom CAPTCHA backend.  The built-in
/// implementations are [`TurnstileProvider`] and [`HCaptchaProvider`].
///
/// For testing, use [`AlwaysPassProvider`] or [`TestCaptchaProvider`].
pub trait CaptchaProvider: Send + Sync + 'static {
    /// Verify a CAPTCHA response token server-side.
    ///
    /// Returns `true` when the token is genuine and has not been replayed.
    fn verify<'a>(&'a self, token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

    /// The HTML form field name that holds the CAPTCHA response token.
    ///
    /// For example `"cf-turnstile-response"` (Turnstile) or
    /// `"h-captcha-response"` (hCaptcha).
    fn form_field_name(&self) -> &'static str;

    /// Whether a non-empty token must be present for verification to proceed.
    ///
    /// When `true` (the default), the middleware rejects requests with a missing
    /// or empty CAPTCHA field locally — without making an outbound verification
    /// call — so bots that simply omit the field are rejected cheaply.
    ///
    /// Override to return `false` for bypass providers like [`AlwaysPassProvider`]
    /// that should let requests through regardless of whether a token is present.
    fn requires_token(&self) -> bool {
        true
    }

    /// Emit the provider-specific widget `Markup` for embedding in Maud templates.
    ///
    /// Includes both the placeholder `<div>` and the provider `<script>` tag.
    #[cfg(feature = "maud")]
    fn widget_markup(&self, site_key: &str) -> maud::Markup;
}

// ── Built-in providers ─────────────────────────────────────────────────────

/// Cloudflare Turnstile CAPTCHA provider.
///
/// Verifies tokens against `https://challenges.cloudflare.com/turnstile/v0/siteverify`.
#[cfg(feature = "http-client")]
pub struct TurnstileProvider {
    secret_key: String,
    client: reqwest::Client,
}

#[cfg(feature = "http-client")]
impl TurnstileProvider {
    /// Create a new Turnstile provider with the given secret key.
    ///
    /// # Panics
    ///
    /// Panics if the underlying TLS backend cannot be initialised (extremely rare).
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self {
            secret_key: secret_key.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build reqwest client"),
        }
    }
}

#[cfg(feature = "http-client")]
impl CaptchaProvider for TurnstileProvider {
    fn verify<'a>(&'a self, token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let params = [("secret", self.secret_key.as_str()), ("response", token)];
            match self
                .client
                .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
                .form(&params)
                .send()
                .await
            {
                Ok(resp) => {
                    let json: serde_json::Value =
                        resp.json().await.unwrap_or(serde_json::Value::Null);
                    json.get("success")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                }
                Err(_) => false,
            }
        })
    }

    fn form_field_name(&self) -> &'static str {
        "cf-turnstile-response"
    }

    #[cfg(feature = "maud")]
    fn widget_markup(&self, site_key: &str) -> maud::Markup {
        maud::html! {
            div .cf-turnstile data-sitekey=(site_key) {}
            script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async="true" defer="true" {}
        }
    }
}

/// hCaptcha CAPTCHA provider.
///
/// Verifies tokens against `https://hcaptcha.com/siteverify`.
#[cfg(feature = "http-client")]
pub struct HCaptchaProvider {
    secret_key: String,
    client: reqwest::Client,
}

#[cfg(feature = "http-client")]
impl HCaptchaProvider {
    /// Create a new hCaptcha provider with the given secret key.
    ///
    /// # Panics
    ///
    /// Panics if the underlying TLS backend cannot be initialised (extremely rare).
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self {
            secret_key: secret_key.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build reqwest client"),
        }
    }
}

#[cfg(feature = "http-client")]
impl CaptchaProvider for HCaptchaProvider {
    fn verify<'a>(&'a self, token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let params = [("secret", self.secret_key.as_str()), ("response", token)];
            match self
                .client
                .post("https://api.hcaptcha.com/siteverify")
                .form(&params)
                .send()
                .await
            {
                Ok(resp) => {
                    let json: serde_json::Value =
                        resp.json().await.unwrap_or(serde_json::Value::Null);
                    json.get("success")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                }
                Err(_) => false,
            }
        })
    }

    fn form_field_name(&self) -> &'static str {
        "h-captcha-response"
    }

    #[cfg(feature = "maud")]
    fn widget_markup(&self, site_key: &str) -> maud::Markup {
        maud::html! {
            div .h-captcha data-sitekey=(site_key) {}
            script src="https://js.hcaptcha.com/1/api.js" async="true" defer="true" {}
        }
    }
}

// ── Test / dev providers ───────────────────────────────────────────────────

/// A CAPTCHA provider that always passes verification.
///
/// Use this in dev environments and tests where you want requests to flow
/// through without any CAPTCHA challenge.
pub struct AlwaysPassProvider;

/// A CAPTCHA provider that always fails verification.
///
/// Used internally when bot protection is enabled but the `http-client`
/// feature is not compiled in — fail closed rather than silently bypass.
#[cfg(not(feature = "http-client"))]
pub(crate) struct AlwaysFailProvider;

#[cfg(not(feature = "http-client"))]
impl CaptchaProvider for AlwaysFailProvider {
    fn verify<'a>(&'a self, _token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(false))
    }

    fn form_field_name(&self) -> &'static str {
        "cf-turnstile-response"
    }

    #[cfg(feature = "maud")]
    fn widget_markup(&self, _site_key: &str) -> maud::Markup {
        maud::html! {}
    }
}

impl CaptchaProvider for AlwaysPassProvider {
    fn verify<'a>(&'a self, _token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(true))
    }

    fn form_field_name(&self) -> &'static str {
        "cf-turnstile-response"
    }

    fn requires_token(&self) -> bool {
        false
    }

    #[cfg(feature = "maud")]
    fn widget_markup(&self, site_key: &str) -> maud::Markup {
        maud::html! {
            div .cf-turnstile data-sitekey=(site_key) {}
        }
    }
}

/// A deterministic CAPTCHA provider for unit and integration tests.
///
/// Accepts exactly one hard-coded token value; all other tokens are rejected.
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use autumn_web::security::captcha::{BotProtectionLayer, TestCaptchaProvider};
///
/// let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("my-test-token")));
/// ```
pub struct TestCaptchaProvider {
    valid_token: String,
}

impl TestCaptchaProvider {
    /// Create a test provider that accepts only `valid_token`.
    pub fn new(valid_token: impl Into<String>) -> Self {
        Self {
            valid_token: valid_token.into(),
        }
    }
}

impl CaptchaProvider for TestCaptchaProvider {
    fn verify<'a>(&'a self, token: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let is_valid = token == self.valid_token;
        Box::pin(std::future::ready(is_valid))
    }

    fn form_field_name(&self) -> &'static str {
        "cf-turnstile-response"
    }

    #[cfg(feature = "maud")]
    fn widget_markup(&self, site_key: &str) -> maud::Markup {
        maud::html! {
            input type="hidden" name="cf-turnstile-response" value=(site_key);
        }
    }
}

// ── Tower Layer ────────────────────────────────────────────────────────────

/// Shared bot-protection settings, threaded through the service clone.
#[derive(Clone)]
struct BotProtectionSettings {
    provider: Arc<dyn CaptchaProvider>,
    dev_bypass: bool,
    /// Effective form field name (may override provider default via config).
    form_field: String,
    /// Maximum body bytes scanned when searching for the CAPTCHA token field.
    max_scan_bytes: usize,
    /// Paths that are exempt from CAPTCHA verification (e.g. inbound webhook endpoints).
    exempt_paths: Vec<String>,
}

/// Tower [`Layer`] that enforces CAPTCHA verification on mutating requests.
///
/// Applied automatically when `bot_protection.enabled = true` in config.
/// For custom test providers, use [`BotProtectionLayer::new`].
///
/// # Layer ordering
///
/// Bot protection is applied after the rate limiter and CSRF layer so that
/// abusive bots are rejected as early as possible.
#[derive(Clone)]
pub struct BotProtectionLayer {
    settings: Arc<BotProtectionSettings>,
}

impl BotProtectionLayer {
    /// Create a layer from an already-constructed provider.
    ///
    /// Prefer this constructor in tests and custom deployments.
    ///
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// use autumn_web::security::captcha::{BotProtectionLayer, TestCaptchaProvider};
    ///
    /// let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("my-token")));
    /// ```
    pub fn new(provider: Arc<dyn CaptchaProvider>) -> Self {
        let form_field = provider.form_field_name().to_owned();
        Self {
            settings: Arc::new(BotProtectionSettings {
                provider,
                dev_bypass: false,
                form_field,
                max_scan_bytes: 2 * 1024 * 1024,
                exempt_paths: Vec::new(),
            }),
        }
    }

    /// Create a layer from [`BotProtectionConfig`].
    ///
    /// Selects the built-in provider based on `config.provider`.
    /// When `config.enabled` is `false` or `config.dev_bypass` is `true`,
    /// an [`AlwaysPassProvider`] is used so that manually-scoped layers respect
    /// the same flag that controls the global auto-wired middleware.
    ///
    /// # Panics
    ///
    /// Does not panic. When `secret_key` is absent and `dev_bypass` is `false`
    /// the real provider is still constructed (requests will always fail
    /// verification because the secret is empty).
    pub fn from_config(config: &BotProtectionConfig) -> Self {
        let provider: Arc<dyn CaptchaProvider> = if !config.enabled || config.dev_bypass {
            Arc::new(AlwaysPassProvider)
        } else {
            let secret = config.secret_key.clone().unwrap_or_default();
            if secret.is_empty() {
                tracing::warn!(
                    "bot_protection: enabled is true and dev_bypass is false, but secret_key is \
                     missing or empty — all CAPTCHA verifications will fail!"
                );
            }
            match config.provider {
                #[cfg(feature = "http-client")]
                CaptchaProviderKind::Turnstile => Arc::new(TurnstileProvider::new(secret)),
                #[cfg(feature = "http-client")]
                CaptchaProviderKind::HCaptcha => {
                    if config.form_field.is_some() {
                        tracing::warn!(
                            "bot_protection: hCaptcha does not support the form_field override — \
                             the widget always submits as \"h-captcha-response\"; \
                             set form_field only when using Turnstile"
                        );
                    }
                    Arc::new(HCaptchaProvider::new(secret))
                }
                #[cfg(not(feature = "http-client"))]
                _ => {
                    tracing::warn!(
                        "bot_protection: http-client feature is disabled; \
                         CAPTCHA verification is unavailable — all protected form \
                         submissions will be rejected (fail closed)"
                    );
                    Arc::new(AlwaysFailProvider)
                }
            }
        };

        let form_field = config.effective_form_field().to_owned();
        Self {
            settings: Arc::new(BotProtectionSettings {
                provider,
                dev_bypass: config.dev_bypass,
                form_field,
                max_scan_bytes: 2 * 1024 * 1024,
                exempt_paths: Vec::new(),
            }),
        }
    }

    /// Override the form field name scanned for the CAPTCHA token.
    ///
    /// Use this when applying a scoped layer with [`BotProtectionLayer::new`]
    /// and `bot_protection.form_field` is set in `autumn.toml` — otherwise the
    /// widget submits under the configured field name while the layer scans the
    /// provider default, causing every submission to be rejected.
    ///
    /// ```rust,ignore
    /// BotProtectionLayer::new(Arc::new(TurnstileProvider::new(secret)))
    ///     .with_form_field(config.bot_protection.effective_form_field())
    /// ```
    #[must_use]
    pub fn with_form_field(mut self, field: impl Into<String>) -> Self {
        let settings = Arc::make_mut(&mut self.settings);
        settings.form_field = field.into();
        self
    }

    /// Override the maximum number of body bytes scanned when looking for the
    /// CAPTCHA token field.  Wired to `security.upload.max_request_size_bytes`
    /// by the framework so the limit matches the configured request size.
    #[must_use]
    pub fn with_max_scan_bytes(mut self, bytes: usize) -> Self {
        let settings = Arc::make_mut(&mut self.settings);
        settings.max_scan_bytes = bytes;
        self
    }

    /// Set the path prefixes exempt from CAPTCHA verification.
    ///
    /// POST requests whose URI path starts with any prefix in `paths` skip the
    /// CAPTCHA check entirely. Use this for webhook endpoints that call into
    /// the application programmatically (e.g. inbound mail, payment callbacks)
    /// and therefore cannot present a CAPTCHA token.
    ///
    /// The framework wires CSRF-exempt paths and webhook endpoint paths
    /// automatically; use this builder only when constructing a scoped
    /// [`BotProtectionLayer`] directly.
    #[must_use]
    pub fn with_exempt_paths(mut self, paths: Vec<String>) -> Self {
        let settings = Arc::make_mut(&mut self.settings);
        settings.exempt_paths = paths;
        self
    }
}

impl<S> Layer<S> for BotProtectionLayer {
    type Service = BotProtectionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BotProtectionService {
            inner,
            settings: Arc::clone(&self.settings),
        }
    }
}

// ── Tower Service ──────────────────────────────────────────────────────────

/// Tower [`Service`] produced by [`BotProtectionLayer`].
#[derive(Clone)]
pub struct BotProtectionService<S> {
    inner: S,
    settings: Arc<BotProtectionSettings>,
}

/// Safe HTTP methods that are exempt from CAPTCHA verification.
const fn is_safe_method(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::GET
            | axum::http::Method::HEAD
            | axum::http::Method::OPTIONS
            | axum::http::Method::TRACE
    )
}

/// Extract the CAPTCHA token from a `application/x-www-form-urlencoded` body.
///
/// Temporarily consumes the body, scans for `field_name`, then restores the
/// body so downstream handlers can still parse it.
async fn extract_token_from_form(
    req: &mut Request<axum::body::Body>,
    field_name: &str,
    max_bytes: usize,
) -> Option<String> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if !content_type.starts_with("application/x-www-form-urlencoded") {
        return None;
    }

    let body = std::mem::replace(req.body_mut(), axum::body::Body::empty());
    let bytes = axum::body::to_bytes(body, max_bytes)
        .await
        .unwrap_or_default();

    let mut token = None;
    for (key, value) in url::form_urlencoded::parse(&bytes) {
        if key == field_name {
            token = Some(value.into_owned());
            break;
        }
    }

    // Restore body for downstream handlers/extractors.
    *req.body_mut() = axum::body::Body::from(bytes);
    token
}

/// Build a 400 Problem Details response for a missing or invalid CAPTCHA token.
fn bot_protection_problem_response<ResBody: From<String> + Default>(
    request_id: Option<String>,
    instance: Option<String>,
) -> Response<ResBody> {
    let detail = "CAPTCHA token missing or invalid. Please complete the challenge and try again.";
    let mut problem = crate::error::problem_details(
        StatusCode::BAD_REQUEST,
        detail.to_owned(),
        None,
        Some("https://autumn.dev/problems/bot-protection"),
        request_id,
        instance,
        true,
    );
    "autumn.bot_protection".clone_into(&mut problem.code);
    let body = crate::error::problem_details_to_json_string(&problem);

    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(axum::http::header::CONTENT_TYPE, "application/problem+json")
        .body(ResBody::from(body))
        .unwrap_or_default()
}

impl<S, ResBody> Service<Request<axum::body::Body>> for BotProtectionService<S>
where
    S: Service<Request<axum::body::Body>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: From<String> + Default + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<axum::body::Body>) -> Self::Future {
        // Safe methods (GET, HEAD, OPTIONS, TRACE) are always exempt.
        if is_safe_method(req.method()) {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }

        // Webhook/API paths that cannot carry a CAPTCHA token are exempt.
        // Require an exact match or a path-segment boundary (trailing `/`) so
        // that exempting `/inbound/mailgun` does not accidentally exempt
        // `/inbound/mailgun-settings` or other adjacent routes. Match against
        // the normalized path so dot-segment tricks like
        // `/inbound/mailgun/../other` cannot satisfy an exemption prefix
        // while targeting another route.
        let clean = crate::security::path::clean_path(req.uri().path());
        if self.settings.exempt_paths.iter().any(|ep| {
            let path = clean.as_str();
            let e = ep.as_str();
            path == e
                || path.starts_with(e)
                    && (e.ends_with('/') || path.as_bytes().get(e.len()) == Some(&b'/'))
        }) {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }

        // Dev bypass: skip verification entirely.
        if self.settings.dev_bypass {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }

        // Only enforce CAPTCHA on application/x-www-form-urlencoded requests.
        // JSON APIs, multipart uploads, and external webhooks pass through unchallenged.
        // Use ASCII lowercase comparison — HTTP media types are case-insensitive.
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !content_type.starts_with("application/x-www-form-urlencoded") {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }

        let settings = Arc::clone(&self.settings);
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            // Skip body scanning entirely for providers that don't require a token
            // (e.g. AlwaysPassProvider). Scanning would consume the body and, if
            // the form exceeds max_scan_bytes, restore an empty body to downstream
            // handlers — corrupting the request silently.
            let token = if settings.provider.requires_token() {
                extract_token_from_form(&mut req, &settings.form_field, settings.max_scan_bytes)
                    .await
            } else {
                None
            };

            let token_str = token.as_deref().unwrap_or("");

            // Reject missing/empty tokens locally before any outbound call, unless
            // the provider explicitly allows missing tokens (e.g. AlwaysPassProvider).
            let valid = if token_str.is_empty() && settings.provider.requires_token() {
                false
            } else {
                settings.provider.verify(token_str).await
            };

            if !valid {
                let request_id = req
                    .extensions()
                    .get::<crate::middleware::RequestId>()
                    .map(std::string::ToString::to_string);
                let instance = Some(req.uri().path().to_owned());
                tracing::debug!(
                    path = %req.uri().path(),
                    token_present = token.is_some(),
                    "bot_protection: CAPTCHA token missing or invalid"
                );
                return Ok(bot_protection_problem_response(request_id, instance));
            }

            inner.call(req).await
        })
    }
}

// ── Maud widget helper ─────────────────────────────────────────────────────

/// Emit the provider-specific CAPTCHA widget markup for embedding in Maud forms.
///
/// Renders the placeholder `<div>` and the provider `<script>` tag required to
/// load the widget JavaScript.  No manual `<script>` tags needed.
///
/// The widget renders whenever `site_key` is set, regardless of
/// `config.enabled`.  This allows the scoped-layer pattern (where `enabled =
/// false` disables global auto-wiring but a manually-applied
/// [`BotProtectionLayer`] protects specific routes) to still render the widget
/// in forms.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::security::captcha::bot_protection_widget;
///
/// #[get("/signup")]
/// async fn signup_form(config: AutumnConfig) -> Markup {
///     html! {
///         form method="POST" {
///             input type="text" name="email";
///             (bot_protection_widget(&config.bot_protection))
///             button { "Sign up" }
///         }
///     }
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn bot_protection_widget(config: &BotProtectionConfig) -> maud::Markup {
    if config.dev_bypass {
        // Dev mode: render an invisible placeholder so the form submits cleanly.
        return maud::html! {
            input type="hidden" name=(config.effective_form_field()) value="dev-bypass";
        };
    }

    let site_key = match config.site_key.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => {
            tracing::warn!(
                "bot_protection: site_key is not configured; \
                 rendering no widget — CAPTCHA tokens cannot be generated \
                 and every form submission will be rejected"
            );
            return maud::html! {};
        }
    };

    // Turnstile supports data-response-field-name to customise the submitted
    // field name; hCaptcha does not — the token is always submitted as
    // "h-captcha-response" regardless of any attribute.
    let custom_field = config.form_field.as_deref();

    match config.provider {
        CaptchaProviderKind::Turnstile => maud::html! {
            div .cf-turnstile
                data-sitekey=(site_key)
                data-response-field-name=[custom_field]
                {}
            script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async="true" defer="true" {}
        },
        CaptchaProviderKind::HCaptcha => maud::html! {
            div .h-captcha
                data-sitekey=(site_key)
                {}
            script src="https://js.hcaptcha.com/1/api.js" async="true" defer="true" {}
        },
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::post;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn router_with_layer(layer: BotProtectionLayer) -> Router {
        Router::new()
            .route("/submit", post(ok_handler))
            .layer(layer)
    }

    #[tokio::test]
    async fn always_pass_provider_allows_any_token() {
        let provider = Arc::new(AlwaysPassProvider);
        assert!(provider.verify("anything").await);
        assert!(provider.verify("").await);
    }

    #[tokio::test]
    async fn test_provider_accepts_valid_token() {
        let provider = TestCaptchaProvider::new("secret");
        assert!(provider.verify("secret").await);
        assert!(!provider.verify("wrong").await);
        assert!(!provider.verify("").await);
    }

    #[tokio::test]
    async fn missing_token_returns_400() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("tok")));
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("field=value"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(ct.contains("application/problem+json"));
    }

    #[tokio::test]
    async fn valid_token_passes_through() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("correct")));
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("cf-turnstile-response=correct&other=val"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_returns_400() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("correct")));
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("cf-turnstile-response=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dev_bypass_skips_verification() {
        let settings = BotProtectionConfig {
            dev_bypass: true,
            ..Default::default()
        };
        let layer = BotProtectionLayer::from_config(&settings);
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("field=value"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_request_passes_without_token() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")));
        let app = Router::new()
            .route("/page", axum::routing::get(ok_handler))
            .layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/page")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn widget_turnstile_contains_script_and_div() {
        let config = BotProtectionConfig {
            enabled: true,
            provider: CaptchaProviderKind::Turnstile,
            site_key: Some("test-key".to_string()),
            ..Default::default()
        };
        let html = bot_protection_widget(&config).into_string();
        assert!(html.contains("cf-turnstile"));
        assert!(html.contains("test-key"));
        assert!(html.contains("challenges.cloudflare.com"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn widget_hcaptcha_contains_script_and_div() {
        let config = BotProtectionConfig {
            enabled: true,
            provider: CaptchaProviderKind::HCaptcha,
            site_key: Some("hkey".to_string()),
            ..Default::default()
        };
        let html = bot_protection_widget(&config).into_string();
        assert!(html.contains("h-captcha"));
        assert!(html.contains("hkey"));
        assert!(html.contains("js.hcaptcha.com"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn widget_dev_bypass_emits_hidden_input() {
        let config = BotProtectionConfig {
            enabled: true,
            dev_bypass: true,
            ..Default::default()
        };
        let html = bot_protection_widget(&config).into_string();
        assert!(html.contains("type=\"hidden\""));
        assert!(html.contains("dev-bypass"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn widget_renders_when_enabled_false_but_site_key_set() {
        // Scoped-layer pattern: enabled=false disables global auto-wiring but
        // the widget must still render so the manually-layered router can verify tokens.
        let config = BotProtectionConfig {
            enabled: false,
            provider: CaptchaProviderKind::Turnstile,
            site_key: Some("0x4AAAA".to_string()),
            ..Default::default()
        };
        let html = bot_protection_widget(&config).into_string();
        assert!(html.contains("cf-turnstile"));
        assert!(html.contains("0x4AAAA"));
        assert!(html.contains("challenges.cloudflare.com"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn widget_empty_when_no_site_key() {
        let config = BotProtectionConfig {
            enabled: false,
            ..Default::default()
        };
        let html = bot_protection_widget(&config).into_string();
        assert!(html.is_empty());
    }

    #[tokio::test]
    async fn exempt_path_does_not_bleed_to_adjacent_routes() {
        // Exempting "/webhook/inbound" must not exempt "/webhook/inbound-other".
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")))
            .with_exempt_paths(vec!["/webhook/inbound".to_string()]);
        let app = Router::new()
            .route("/webhook/inbound", post(ok_handler))
            .route("/webhook/inbound-other", post(ok_handler))
            .layer(layer);

        let exempt = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/inbound")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("field=value"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            exempt.status(),
            StatusCode::OK,
            "exact path should be exempt"
        );

        let adjacent = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/inbound-other")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("field=value"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            adjacent.status(),
            StatusCode::BAD_REQUEST,
            "adjacent route must not be exempt"
        );
    }

    #[tokio::test]
    async fn dot_segment_path_does_not_match_exemption() {
        // `/webhook/../protected` normalizes to `/protected`, which is not
        // exempt, so the CAPTCHA check must still run (and reject the request
        // because no token is present). Same for percent-encoded dot-segments.
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")))
            .with_exempt_paths(vec!["/webhook/".to_string()]);
        let app = Router::new()
            .route("/webhook/inbound", post(ok_handler))
            .route("/protected", post(ok_handler))
            .layer(layer);

        for uri in ["/webhook/../protected", "/webhook/%2e%2e/protected"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .body(Body::from("field=value"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "dot-segment path {uri} must not be treated as exempt"
            );
        }
    }

    #[tokio::test]
    async fn exempt_path_bypasses_captcha() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")))
            .with_exempt_paths(vec!["/webhook/".to_string()]);
        let app = Router::new()
            .route("/webhook/inbound", post(ok_handler))
            .layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/inbound")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("field=value"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn json_post_passes_without_captcha_token() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")));
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"key":"value"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn multipart_post_passes_without_captcha_token() {
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("required")));
        let app = router_with_layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "multipart/form-data; boundary=----boundary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn effective_form_field_uses_custom_if_set() {
        let config = BotProtectionConfig {
            form_field: Some("my-captcha".to_string()),
            ..Default::default()
        };
        assert_eq!(config.effective_form_field(), "my-captcha");
    }

    #[test]
    fn effective_form_field_defaults_to_turnstile() {
        let config = BotProtectionConfig::default();
        assert_eq!(config.effective_form_field(), "cf-turnstile-response");
    }

    #[test]
    fn effective_form_field_defaults_to_hcaptcha() {
        let config = BotProtectionConfig {
            provider: CaptchaProviderKind::HCaptcha,
            ..Default::default()
        };
        assert_eq!(config.effective_form_field(), "h-captcha-response");
    }

    #[tokio::test]
    async fn with_form_field_overrides_provider_default() {
        // Scoped layer using a custom form field: widget submits "my-captcha",
        // so the layer must also scan "my-captcha", not "cf-turnstile-response".
        let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("tok")))
            .with_form_field("my-captcha");
        let app = router_with_layer(layer);

        // Correct token in the custom field → 200
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("my-captcha=tok"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Token in the provider default field (wrong field) → 400
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("cf-turnstile-response=tok"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
