//! Spring Security-style protection for Autumn applications.
//!
//! This module provides automatic security hardening that follows OWASP
//! best practices. Like Spring Security, it applies sensible defaults
//! out of the box and can be customized via `autumn.toml`.
//!
//! ## What's included
//!
//! | Component | Module | Description |
//! |-----------|--------|-------------|
//! | Security headers | `headers` | X-Frame-Options, X-Content-Type-Options, HSTS, CSP, etc. |
//! | CSRF protection | `csrf` | Token-based CSRF validation for mutating requests |
//! | Rate limiting | `rate_limit` | Per-client-IP token-bucket; memory (default) or Redis backend for multi-replica global enforcement |
//! | Bot protection | `captcha` | Pluggable CAPTCHA verification (Turnstile, hCaptcha); dev-mode bypass (`[bot_protection]` in `autumn.toml`) |
//! | Configuration | `config` | `[security]` section in `autumn.toml` |
//!
//! Authentication, session management, and password hashing live in
//! their own top-level modules ([`crate::auth`], [`crate::session`]).
//!
//! ## Profile-aware defaults
//!
//! Like Spring Security's auto-configuration, Autumn adjusts security
//! settings based on the active profile:
//!
//! | Setting | `dev` | `prod` |
//! |---------|-------|--------|
//! | Security headers | Applied (all defaults) | Applied (all defaults + HSTS) |
//! | CSRF protection | Disabled | Enabled |
//! | HSTS | Off | On (1 year, includeSubDomains) |
//! | Session cookies | Not Secure | Secure |
//!
//! ## Configuration
//!
//! ```toml
//! # Bot protection / CAPTCHA (top-level section, not under [security])
//! [bot_protection]
//! enabled    = true
//! provider   = "turnstile"    # "turnstile" (default) or "hcaptcha"
//! site_key   = "0x4AAAA..."  # client-side widget key
//! secret_key = "..."          # server-side secret — use env var!
//! dev_bypass = false
//!
//! [security.headers]
//! x_frame_options = "DENY"            # or "SAMEORIGIN", "" to disable
//! x_content_type_options = true        # X-Content-Type-Options: nosniff
//! xss_protection = true                # X-XSS-Protection: 1; mode=block
//! strict_transport_security = true     # HSTS (auto-enabled in prod)
//! hsts_max_age_secs = 31536000         # 1 year
//! content_security_policy = "default-src 'self'; ..."  # htmx-friendly default; "" disables
//! referrer_policy = "strict-origin-when-cross-origin"
//! permissions_policy = ""              # set to enable Permissions-Policy
//!
//! # Per-request CSP nonces — removes 'unsafe-inline', enables CspNonce extractor
//! [security.headers.csp_nonce]
//! enabled = true
//!
//! [security.csrf]
//! enabled = true                       # auto-enabled in prod
//! token_header = "X-CSRF-Token"
//! cookie_name = "autumn-csrf"
//!
//! [security.rate_limit]
//! enabled = true                       # per-IP token bucket
//! requests_per_second = 10.0
//! burst = 20
//! trust_forwarded_headers = true       # only behind trusted proxies
//! trusted_proxies = ["10.0.0.10", "203.0.113.0/24"]
//!
//! # Multi-replica: share the budget globally across all pods
//! backend = "redis"                    # "memory" (default) or "redis"
//! on_backend_failure = "fail_open"     # "fail_open" (default) or "fail_closed"
//!
//! [security.rate_limit.redis]
//! url = "redis://redis:6379"
//! key_prefix = "myapp:rate_limit"
//! ```
//!
//! ## Quick start
//!
//! Security headers are applied automatically -- no setup needed.
//! For CSRF protection in templates:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::CsrfToken;
//!
//! #[get("/form")]
//! async fn form(csrf: CsrfToken) -> Markup {
//!     html! {
//!         form method="POST" action="/submit" {
//!             input type="hidden" name="_csrf" value=(csrf.token());
//!             input type="text" name="title";
//!             button { "Submit" }
//!         }
//!     }
//! }
//! ```
//!
//! For CSP nonces in inline scripts and styles
//! (requires `security.headers.csp_nonce.enabled = true`):
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::CspNonce;
//!
//! #[get("/page")]
//! async fn page(nonce: CspNonce) -> Markup {
//!     html! {
//!         script nonce=(nonce.value()) { "console.log('ready')" }
//!         style  nonce=(nonce.value()) { "body { margin: 0 }" }
//!     }
//! }
//! ```
//!
//! For bot protection on public forms (requires `bot_protection.enabled = true`):
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::config::AutumnConfig;
//! use autumn_web::security::captcha::bot_protection_widget;
//!
//! #[get("/signup")]
//! async fn signup_form(config: AutumnConfig) -> Markup {
//!     html! {
//!         form method="POST" action="/signup" {
//!             input type="email" name="email";
//!             (bot_protection_widget(&config.bot_protection))
//!             button { "Sign up" }
//!         }
//!     }
//! }
//!
//! #[post("/signup")]
//! async fn signup_submit() -> &'static str {
//!     // Only reached if CAPTCHA passes — bot protection verified automatically
//!     "Welcome!"
//! }
//! ```

pub mod captcha;
pub(crate) mod config;
pub(crate) mod csrf;
pub(crate) mod headers;
pub(crate) mod path;
pub mod proxy;
pub mod rate_limit;
pub(crate) mod submit_token;
pub(crate) mod trusted_proxies;

// Re-export commonly used types at the module level.
#[cfg(feature = "maud")]
pub use captcha::bot_protection_widget;
pub use captcha::{
    AlwaysPassProvider, BotProtectionConfig, BotProtectionLayer, CaptchaProvider,
    CaptchaProviderKind, TestCaptchaProvider,
};
pub use config::{
    CspNonceConfig, CsrfConfig, HeadersConfig, KeyStrategy, RateLimitBackend, RateLimitConfig,
    RateLimitNamedConfig, RateLimitTierConfig, SecurityConfig, SubmitTokenConfig,
    TrustedProxiesConfig, UploadConfig, default_content_security_policy, hmac_sha256_hex,
};
#[cfg(feature = "redis")]
pub use config::{RateLimitBackendFailure, RateLimitRedisConfig};
pub use csrf::{CsrfFormField, CsrfLayer, CsrfToken, CsrfTokenHeader};
pub use headers::{CspNonce, SecurityHeadersLayer};
pub use proxy::TrustedProxy;
#[doc(hidden)]
pub use rate_limit::{__check_throttle, __throttle_registry_reset};
pub use rate_limit::{
    RateLimitEnvelopeCounted, RateLimitExempt, RateLimitLayer, RateLimitOverride,
    RateLimitPrincipal, ThrottleSpec,
};
pub use submit_token::{SubmitFormField, SubmitToken, SubmitTokenLayer};
pub use trusted_proxies::{ProxyResolver, ResolvedClientIdentity, TrustedProxiesLayer};
