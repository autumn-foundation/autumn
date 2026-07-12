//! Security configuration for Autumn applications.
//!
//! Controls security headers and CSRF protection. All settings have
//! sensible defaults and are profile-aware:
//!
//! - **`dev`**: Relaxed -- CSRF disabled, HSTS off, permissive headers.
//! - **`prod`**: Strict -- CSRF enabled, HSTS on, all protective headers active.
//!
//! Session and authentication configuration live in their own modules
//! ([`crate::session::SessionConfig`], [`crate::auth::AuthConfig`]).
//!
//! # `autumn.toml` example
//!
//! ```toml
//! [security.headers]
//! x_frame_options = "DENY"
//! content_security_policy = "default-src 'self'"
//!
//! # Enable per-request CSP nonces — removes 'unsafe-inline' from the default
//! # style-src and makes the nonce available via the CspNonce extractor.
//! [security.headers.csp_nonce]
//! enabled = true
//!
//! [security.csrf]
//! enabled = true
//!
//! [security.rate_limit]
//! enabled = true
//! requests_per_second = 10.0
//! burst = 20
//! ```
//!
//! # Environment variable reference
//!
//! | Variable | Config field | Type |
//! |----------|-------------|------|
//! | `AUTUMN_SECURITY__HEADERS__X_FRAME_OPTIONS` | `security.headers.x_frame_options` | `String` |
//! | `AUTUMN_SECURITY__HEADERS__HSTS_MAX_AGE_SECS` | `security.headers.hsts_max_age_secs` | `u64` |
//! | `AUTUMN_SECURITY__HEADERS__CONTENT_SECURITY_POLICY` | `security.headers.content_security_policy` | `String` |
//! | `AUTUMN_SECURITY__HEADERS__CSP_NONCE__ENABLED` | `security.headers.csp_nonce.enabled` | `bool` |
//! | `AUTUMN_SECURITY__CSRF__ENABLED` | `security.csrf.enabled` | `bool` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__ENABLED` | `security.rate_limit.enabled` | `bool` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__REQUESTS_PER_SECOND` | `security.rate_limit.requests_per_second` | `f64` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__BURST` | `security.rate_limit.burst` | `u32` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS` | `security.rate_limit.trust_forwarded_headers` | `bool` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES` | `security.rate_limit.trusted_proxies` | comma-separated `String` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__BACKEND` | `security.rate_limit.backend` | `memory` / `redis` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__ON_BACKEND_FAILURE` | `security.rate_limit.on_backend_failure` | `fail_open` / `fail_closed` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__REDIS__URL` | `security.rate_limit.redis.url` | `String` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__REDIS__KEY_PREFIX` | `security.rate_limit.redis.key_prefix` | `String` |
//! | `AUTUMN_SECURITY__TRUSTED_PROXIES__RANGES` | `security.trusted_proxies.ranges` | comma-separated `String` |
//! | `AUTUMN_SECURITY__TRUSTED_PROXIES__TRUST_FORWARDED_HEADERS` | `security.trusted_proxies.trust_forwarded_headers` | `bool` |
//! | `AUTUMN_SECURITY__TRUSTED_PROXIES__TRUSTED_HOPS` | `security.trusted_proxies.trusted_hops` | `u32` |
//! | `AUTUMN_SECURITY__UPLOAD__MAX_REQUEST_SIZE_BYTES` | `security.upload.max_request_size_bytes` | `usize` |
//! | `AUTUMN_SECURITY__UPLOAD__MAX_FILE_SIZE_BYTES` | `security.upload.max_file_size_bytes` | `usize` |
//! | `AUTUMN_SECURITY__UPLOAD__ALLOWED_MIME_TYPES` | `security.upload.allowed_mime_types` | comma-separated `String` |
//! | `AUTUMN_SECURITY__UPLOAD__REJECT_ON_CONTENT_TYPE_MISMATCH` | `security.upload.reject_on_content_type_mismatch` | `bool` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND` | `security.webhooks.replay.backend` | `memory` / `redis` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL` | `security.webhooks.replay.redis.url` | `String` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX` | `security.webhooks.replay.redis.key_prefix` | `String` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION` | `security.webhooks.replay.allow_memory_in_production` | `bool` |
//! | per-endpoint `secret_env` | `security.webhooks.endpoints[*].secret` | environment variable name |
//!
//! Setting any header value to an empty string disables it (the header is
//! not emitted). This is the escape hatch for opting out of a default.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

// ── Signing secret contract ────────────────────────────────────────────────

/// Minimum byte length for a valid production signing secret (32 bytes / 256 bits).
///
/// A hex-encoded 32-byte value is 64 characters. Anything shorter is rejected
/// at production startup.
pub const MIN_SECRET_LEN: usize = 32;

/// Known demo / template / placeholder values that must never reach production.
const DEMO_VALUES: &[&str] = &[
    "changeme",
    "change_me",
    "change-me",
    "secret",
    "supersecret",
    "super-secret",
    "super_secret",
    "your-secret-here",
    "your_secret_here",
    "insert-secret-here",
    "replace-this",
    "replace_me",
    "todo",
    "fixme",
    "example",
    "placeholder",
    "dev_only",
    "dev-only",
    "test_secret",
    "test-secret",
    "test",
    "password",
];

/// Signing-secret configuration for HMAC-signed framework surfaces.
///
/// The signing secret is the shared key used to sign sessions, CSRF tokens,
/// flash/signed-cookie state, and local-storage signed URLs.
///
/// # Development and test
///
/// Leave `secret` unset. An ephemeral per-process key is generated automatically.
/// This means sessions and signed URLs do **not** survive process restarts and
/// replicas cannot share state — acceptable in dev, unacceptable in production.
///
/// # Production
///
/// Set `secret` via the `AUTUMN_SECURITY__SIGNING_SECRET` environment variable
/// (or `[security.signing_secret] secret` in `autumn.toml`). The secret must be:
/// - At least [`MIN_SECRET_LEN`] bytes long.
/// - Not a known template/demo value.
/// - Stable across restarts and identical on every replica.
///
/// Generate a secret: `openssl rand -hex 32`
///
/// # Rotation
///
/// When rotating, move the current secret to `previous_secrets` and set the
/// new value in `secret`. New signatures use `secret`; tokens signed with any
/// entry in `previous_secrets` continue to validate during the grace window.
/// Remove expired entries from `previous_secrets` after the maximum relevant
/// cookie/token lifetime has elapsed.
///
/// # `autumn.toml` example
///
/// ```toml
/// [security.signing_secret]
/// # secret set via AUTUMN_SECURITY__SIGNING_SECRET env var (never commit this)
///
/// # rotation grace window — leave populated until all existing tokens expire:
/// previous_secrets = ["oldsecretvalue..."]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SigningSecretConfig {
    /// The current signing secret. In production, must come from an environment
    /// variable or external secrets manager — never a committed literal.
    pub secret: Option<String>,

    /// Previous signing secrets accepted during a rotation grace window.
    ///
    /// New signatures always use `secret`. Tokens signed with an entry here
    /// remain valid until removed. Remove entries after the maximum relevant
    /// cookie/token lifetime has elapsed (e.g. `session.max_age_secs`).
    #[serde(default)]
    pub previous_secrets: Vec<String>,
}

/// Error returned when a signing secret fails production validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningSecretError {
    /// No secret is configured but the production profile requires one.
    MissingInProduction,
    /// The secret is too short to meet the minimum entropy requirement.
    TooShort {
        /// Actual byte length of the supplied secret.
        actual: usize,
        /// Minimum required byte length ([`MIN_SECRET_LEN`]).
        required: usize,
    },
    /// The secret matches a known insecure demo or template value.
    KnownWeakValue(String),
}

impl std::fmt::Display for SigningSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingInProduction => write!(
                f,
                "signing secret is required in production; set \
                 AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)"
            ),
            Self::TooShort { actual, required } => write!(
                f,
                "signing secret is too short ({actual} bytes, minimum {required}); \
                 generate one with `openssl rand -hex 32`"
            ),
            Self::KnownWeakValue(v) => write!(
                f,
                "signing secret looks like a template/demo value ({v:?}); \
                 generate one with `openssl rand -hex 32`"
            ),
        }
    }
}

/// Validate a signing secret for production use.
///
/// In development and test the check is skipped — any value (including `None`)
/// is accepted so zero-config local development continues to work.
///
/// In production:
/// - `None` → [`SigningSecretError::MissingInProduction`]
/// - Shorter than [`MIN_SECRET_LEN`] bytes → [`SigningSecretError::TooShort`]
/// - Matches a known demo/template string → [`SigningSecretError::KnownWeakValue`]
///
/// # Errors
///
/// Returns [`SigningSecretError`] when production validation fails.
pub fn validate_signing_secret(
    secret: Option<&str>,
    is_production: bool,
) -> Result<(), SigningSecretError> {
    if !is_production {
        return Ok(());
    }
    let secret = secret.ok_or(SigningSecretError::MissingInProduction)?;
    // Demo-value check first: "changeme" is more informative than "too short".
    let lower = secret.to_ascii_lowercase();
    for &demo in DEMO_VALUES {
        if lower == demo {
            return Err(SigningSecretError::KnownWeakValue(secret.to_owned()));
        }
    }
    let byte_len = secret.len();
    if byte_len < MIN_SECRET_LEN {
        return Err(SigningSecretError::TooShort {
            actual: byte_len,
            required: MIN_SECRET_LEN,
        });
    }
    Ok(())
}

// ── Resolved signing key material ─────────────────────────────────────────

/// HMAC-SHA256 of `message` under `key`, returned as lowercase hex.
///
/// # Panics
///
/// This should not panic because HMAC accepts keys of any length. A panic would
/// indicate a broken crypto crate invariant.
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    let bytes = mac.finalize().into_bytes();
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Constant-time string comparison for HMAC verification.
fn ct_eq_str(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Generate a random 32-byte ephemeral key from two UUID v4 values.
fn generate_ephemeral_key() -> Vec<u8> {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut bytes = vec![0u8; 32];
    bytes[..16].copy_from_slice(a.as_bytes());
    bytes[16..].copy_from_slice(b.as_bytes());
    bytes
}

/// Resolved signing keys for a running Autumn instance.
///
/// Created once at startup from [`SigningSecretConfig`] via [`resolve_signing_keys`]
/// and shared via `Arc` across session, CSRF, and local storage signing.
///
/// - `current` signs new tokens.
/// - `previous` are accepted during a rotation grace window.
#[derive(Clone, Debug)]
pub struct ResolvedSigningKeys {
    /// Key used to sign new tokens.
    pub current: Arc<[u8]>,
    /// Former keys accepted during a rotation grace window. New signatures always
    /// use `current`; tokens carrying a `previous` HMAC continue to verify until
    /// removed (see docs/guide/signing-secrets.md).
    pub previous: Vec<Arc<[u8]>>,
}

impl ResolvedSigningKeys {
    /// Build from raw byte vectors.
    pub fn new(current: Vec<u8>, previous: Vec<Vec<u8>>) -> Self {
        Self {
            current: current.into(),
            previous: previous.into_iter().map(|v: Vec<u8>| v.into()).collect(),
        }
    }

    /// HMAC-SHA256 of `message` under the current key, hex-encoded.
    pub fn sign(&self, message: &[u8]) -> String {
        hmac_sha256_hex(&self.current, message)
    }

    /// Returns `true` when `hex_sig` is a valid HMAC-SHA256 of `message` under
    /// any key (current first, then previous). All comparisons are constant-time.
    pub fn verify(&self, message: &[u8], hex_sig: &str) -> bool {
        if ct_eq_str(&hmac_sha256_hex(&self.current, message), hex_sig) {
            return true;
        }
        for prev in &self.previous {
            if ct_eq_str(&hmac_sha256_hex(prev, message), hex_sig) {
                return true;
            }
        }
        false
    }
}

/// Resolve signing keys from a [`SigningSecretConfig`].
///
/// - When `secret` is set, its bytes become the current key.
/// - When `secret` is absent (dev/test), an ephemeral random key is generated.
///   This means signed tokens do not survive process restarts.
/// - `previous_secrets` are always included for rotation grace-window verification.
///
/// Production boot validation (requiring `secret` to be non-empty, long enough,
/// and not a demo value) is a separate step via [`validate_signing_secret`].
pub fn resolve_signing_keys(config: &SigningSecretConfig) -> ResolvedSigningKeys {
    let current = config
        .secret
        .as_deref()
        .map_or_else(generate_ephemeral_key, |s| s.as_bytes().to_vec());
    let previous = config
        .previous_secrets
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    ResolvedSigningKeys::new(current, previous)
}

/// Top-level security configuration section.
///
/// Groups security headers and CSRF protection under `[security]`
/// in `autumn.toml`.
///
/// # Examples
///
/// ```rust
/// use autumn_web::security::SecurityConfig;
///
/// let config = SecurityConfig::default();
/// assert_eq!(config.headers.x_frame_options, "DENY");
/// assert!(config.headers.x_content_type_options);
/// assert!(!config.csrf.enabled);
/// assert!(!config.rate_limit.enabled);
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecurityConfig {
    /// HTTP security headers applied to all responses.
    #[serde(default)]
    pub headers: HeadersConfig,

    /// CSRF (Cross-Site Request Forgery) protection.
    #[serde(default)]
    pub csrf: CsrfConfig,

    /// One-time submit tokens — at-most-once form submissions.
    #[serde(default)]
    pub submit_token: SubmitTokenConfig,

    /// Rate limiting (per-client-IP token bucket).
    #[serde(default)]
    pub rate_limit: RateLimitConfig,

    /// Multipart upload safeguards and validation policy.
    #[serde(default)]
    pub upload: UploadConfig,

    /// Signed webhook intake endpoints.
    #[serde(default)]
    pub webhooks: crate::webhook::WebhookConfig,

    /// Paths that must bypass CAPTCHA bot-protection independently of CSRF
    /// exemptions.  Framework-managed inbound-mail and webhook receiver paths
    /// are added here automatically; user-configured CSRF-exempt paths are
    /// deliberately NOT copied here so that a form route that skips CSRF for
    /// non-cookie auth still requires a CAPTCHA token.
    #[serde(default)]
    pub captcha_exempt_paths: Vec<String>,

    /// HTTP status returned when a [`Policy`](crate::authorization::Policy)
    /// denies a record-level action. Defaults to `"404"` to mirror the
    /// Rails / Phoenix posture of hiding existence from unauthorized
    /// clients.
    #[serde(default)]
    pub forbidden_response: crate::authorization::ForbiddenResponse,

    /// Allow `#[repository(api = "...")]` to mount auto-generated
    /// CRUD endpoints in `prod` builds without a paired `policy =`
    /// argument.
    ///
    /// Default: `false`. The framework refuses to start when an
    /// `api =` repository has no `policy =` because the auto-
    /// generated endpoints would be reachable by any authenticated
    /// user. Flip this to `true` only when the lack of authz is
    /// genuinely intended (e.g. a fully-public read-only API).
    #[serde(default)]
    pub allow_unauthorized_repository_api: bool,

    /// Signing-secret configuration for HMAC-signed framework surfaces.
    ///
    /// Covers sessions, CSRF tokens, flash/signed-cookie state, and
    /// local-storage signed URLs. In dev the framework generates an
    /// ephemeral per-process key; production MUST set a stable, private
    /// secret via `AUTUMN_SECURITY__SIGNING_SECRET`.
    #[serde(default)]
    pub signing_secret: SigningSecretConfig,

    /// Trusted Host header allow-list.
    #[serde(default)]
    pub trusted_hosts: TrustedHostsConfig,

    /// Top-level trusted-proxy policy for `X-Forwarded-*` headers.
    ///
    /// When configured, every forwarding-aware middleware (rate limiter, CSRF
    /// origin check, method-override, HSTS detection, tracing fields) honours
    /// this policy.  The old per-subsystem `security.rate_limit.trusted_proxies`
    /// and `security.rate_limit.trust_forwarded_headers` fields continue to work
    /// for one minor release but are deprecated; configure this block instead.
    #[serde(default)]
    pub trusted_proxies: TrustedProxiesConfig,
}

impl SecurityConfig {
    /// Check for conflicting configuration between the new top-level
    /// `[security.trusted_proxies]` and the deprecated rate-limit-scoped fields.
    ///
    /// Returns `Some(message)` when both are set with values that differ; the
    /// caller (e.g. `autumn doctor --strict`) should treat this as a failure.
    #[must_use]
    pub fn trusted_proxies_conflict(&self) -> Option<String> {
        let new_set =
            self.trusted_proxies.trust_forwarded_headers || !self.trusted_proxies.ranges.is_empty();
        let old_set =
            self.rate_limit.trust_forwarded_headers || !self.rate_limit.trusted_proxies.is_empty();

        if new_set && old_set {
            // Check for value-level conflicts.
            let new_ranges: std::collections::HashSet<&str> = self
                .trusted_proxies
                .ranges
                .iter()
                .map(String::as_str)
                .collect();
            let old_ranges: std::collections::HashSet<&str> = self
                .rate_limit
                .trusted_proxies
                .iter()
                .map(String::as_str)
                .collect();

            // The legacy rate-limit fields have no hop-count equivalent, so any
            // trusted_hops value in the new block is always a conflict.
            let hops_conflict = self.trusted_proxies.trusted_hops.is_some();

            if new_ranges != old_ranges
                || self.trusted_proxies.trust_forwarded_headers
                    != self.rate_limit.trust_forwarded_headers
                || hops_conflict
            {
                return Some(
                    "[security.trusted_proxies] and \
                     [security.rate_limit] trusted_proxies/trust_forwarded_headers \
                     are both set with conflicting values. Remove the deprecated \
                     rate_limit fields and keep only [security.trusted_proxies]."
                        .to_owned(),
                );
            }
        }

        None
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrustedHostsConfig {
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Top-level trusted-proxy policy applied by every forwarding-aware middleware.
///
/// Declare this once under `[security.trusted_proxies]` and every framework
/// middleware that reads `X-Forwarded-*` headers (rate limiter, CSRF origin
/// check, method-override, HSTS detection, tracing fields) will honour it
/// automatically.
///
/// # Examples
///
/// ```toml
/// # Behind Cloudflare (known IP ranges) + an ALB in 10.0.0.0/8
/// [security.trusted_proxies]
/// ranges = ["173.245.48.0/20", "103.21.244.0/22", "10.0.0.0/8"]
/// trust_forwarded_headers = true
///
/// # Behind exactly one ALB with dynamic IPs — trust the rightmost 1 hop
/// [security.trusted_proxies]
/// trusted_hops = 1
/// trust_forwarded_headers = true
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrustedProxiesConfig {
    /// Trusted proxy IP addresses or CIDR ranges.
    ///
    /// Walk the `X-Forwarded-For` chain from the right, skipping IPs in these
    /// ranges.  The first IP that falls outside the ranges is the real client.
    #[serde(default)]
    pub ranges: Vec<String>,

    /// Trust exactly this many proxy hops from the right of the
    /// `X-Forwarded-For` chain, regardless of their IPs.
    ///
    /// Use when proxy IPs are dynamic (e.g., AWS ALB).  Takes precedence over
    /// `ranges` when set.
    #[serde(default)]
    pub trusted_hops: Option<u32>,

    /// Whether to consult `X-Forwarded-*` headers at all.
    ///
    /// Defaults to `false` in `prod` (safe default — no forwarding trust until
    /// explicitly configured).  Set `true` when the application is behind a
    /// reverse proxy that sets these headers.
    #[serde(default)]
    pub trust_forwarded_headers: bool,
}

/// Security response headers configuration.
///
/// Controls which protective HTTP headers are added to every response.
/// Follows OWASP security header recommendations.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `x_frame_options` | `"DENY"` |
/// | `x_content_type_options` | `true` |
/// | `xss_protection` | `true` |
/// | `strict_transport_security` | `false` |
/// | `hsts_max_age_secs` | `31_536_000` (1 year) |
/// | `hsts_include_subdomains` | `true` |
/// | `content_security_policy` | htmx-compatible policy (see [`default_content_security_policy`]) |
/// | `referrer_policy` | `"strict-origin-when-cross-origin"` |
/// | `permissions_policy` | `""` (disabled) |
///
/// # Examples
///
/// ```toml
/// [security.headers]
/// x_frame_options = "SAMEORIGIN"
/// content_security_policy = "default-src 'self'; script-src 'self'"
/// strict_transport_security = true
/// ```
#[derive(Debug, Clone, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct HeadersConfig {
    /// `X-Frame-Options` header value. Default: `"DENY"`.
    ///
    /// Prevents the page from being loaded in an iframe. Common values:
    /// - `"DENY"` -- never allow framing
    /// - `"SAMEORIGIN"` -- allow framing by same origin
    /// - `""` -- do not send the header
    #[serde(default = "default_x_frame_options")]
    pub x_frame_options: String,

    /// Add `X-Content-Type-Options: nosniff`. Default: `true`.
    ///
    /// Prevents MIME-type sniffing attacks.
    #[serde(default = "default_true")]
    pub x_content_type_options: bool,

    /// Add `X-XSS-Protection: 1; mode=block`. Default: `true`.
    ///
    /// Enables the browser's built-in XSS filter (legacy but still useful).
    #[serde(default = "default_true")]
    pub xss_protection: bool,

    /// Add `Strict-Transport-Security` (HSTS) header. Default: `false`.
    ///
    /// When `true`, tells browsers to only connect via HTTPS. Enabled
    /// automatically for `prod` profile via smart defaults.
    #[serde(default)]
    pub strict_transport_security: bool,

    /// HSTS `max-age` in seconds. Default: `31_536_000` (1 year).
    ///
    /// Only used when `strict_transport_security` is `true`.
    #[serde(default = "default_hsts_max_age")]
    pub hsts_max_age_secs: u64,

    /// Include subdomains in HSTS policy. Default: `true`.
    #[serde(default = "default_true")]
    pub hsts_include_subdomains: bool,

    /// `Content-Security-Policy` header value.
    ///
    /// Defaults to an htmx-compatible, same-origin policy (see
    /// [`default_content_security_policy`]). When set to an empty string,
    /// the header is not emitted (explicit opt-out).
    ///
    /// The default allows htmx to function normally because htmx and Autumn's
    /// htmx CSRF helper are served from the same origin and operate via
    /// `addEventListener` rather than inline scripts.
    #[serde(default = "default_content_security_policy")]
    pub content_security_policy: String,

    /// `Referrer-Policy` header value. Default: `"strict-origin-when-cross-origin"`.
    #[serde(default = "default_referrer_policy")]
    pub referrer_policy: String,

    /// `Permissions-Policy` header value. Default: `""` (not sent).
    ///
    /// Controls which browser features and APIs can be used.
    /// Example: `"camera=(), microphone=(), geolocation=()"`.
    #[serde(default)]
    pub permissions_policy: String,

    /// Per-request CSP nonce configuration.
    ///
    /// When enabled, a fresh cryptographically-random nonce is generated for
    /// every request and injected into `script-src` and `style-src` of the
    /// default `Content-Security-Policy`. The nonce is also available via the
    /// [`CspNonce`] extractor for use in templates.
    ///
    /// Apps that set an explicit `content_security_policy` string opt out of
    /// automatic nonce injection automatically — their custom CSP is used
    /// verbatim, but the nonce is still generated and available via the
    /// extractor.
    ///
    /// [`CspNonce`]: crate::security::CspNonce
    #[serde(default)]
    pub csp_nonce: CspNonceConfig,
}

impl Default for HeadersConfig {
    fn default() -> Self {
        Self {
            x_frame_options: default_x_frame_options(),
            x_content_type_options: true,
            xss_protection: true,
            strict_transport_security: false,
            hsts_max_age_secs: default_hsts_max_age(),
            hsts_include_subdomains: true,
            content_security_policy: default_content_security_policy(),
            referrer_policy: default_referrer_policy(),
            permissions_policy: String::new(),
            csp_nonce: CspNonceConfig::default(),
        }
    }
}

/// CSRF (Cross-Site Request Forgery) protection configuration.
///
/// When enabled, mutating requests (POST, PUT, DELETE, PATCH) must include
/// a valid CSRF token either as:
///
/// - An HTTP header (default: `X-CSRF-Token`)
/// - A form field (default: `_csrf`)
///
/// The token is generated per-session and stored in a cookie.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `enabled` | `false` |
/// | `token_header` | `"X-CSRF-Token"` |
/// | `form_field` | `"_csrf"` |
/// | `cookie_name` | `"autumn-csrf"` |
/// | `safe_methods` | `["GET", "HEAD", "OPTIONS", "TRACE"]` |
/// | `exempt_paths` | `[]` |
///
/// # Examples
///
/// ```toml
/// [security.csrf]
/// enabled = true
/// token_header = "X-XSRF-Token"
/// cookie_name = "XSRF-TOKEN"
/// exempt_paths = ["/api/"]
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct CsrfConfig {
    /// Enable CSRF protection. Default: `false`.
    ///
    /// Enabled automatically for `prod` profile via smart defaults.
    #[serde(default)]
    pub enabled: bool,

    /// HTTP header name for the CSRF token. Default: `"X-CSRF-Token"`.
    #[serde(default = "default_csrf_header")]
    pub token_header: String,

    /// Form field name for the CSRF token. Default: `"_csrf"`.
    #[serde(default = "default_csrf_field")]
    pub form_field: String,

    /// Cookie name for storing the CSRF token. Default: `"autumn-csrf"`.
    #[serde(default = "default_csrf_cookie")]
    pub cookie_name: String,

    /// HTTP methods that do NOT require CSRF validation.
    /// Default: `["GET", "HEAD", "OPTIONS", "TRACE"]`.
    #[serde(default = "default_safe_methods")]
    pub safe_methods: Vec<String>,

    /// Request path prefixes that are exempt from CSRF validation.
    /// Default: `[]`.
    ///
    /// Use this to opt JSON API routes out of CSRF when they authenticate
    /// with bearer tokens or other non-cookie credentials. Matches are by
    /// prefix on the request path, e.g. `"/api/"` exempts all routes
    /// under `/api/`.
    #[serde(default)]
    pub exempt_paths: Vec<String>,
}

impl Default for CsrfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_header: default_csrf_header(),
            form_field: default_csrf_field(),
            cookie_name: default_csrf_cookie(),
            safe_methods: default_safe_methods(),
            exempt_paths: Vec::new(),
        }
    }
}

/// One-time submit-token protection settings.
///
/// When enabled (the default), a per-render random token is exposed via the
/// [`SubmitToken`](crate::security::SubmitToken) extractor and embedded as a
/// hidden `_submit_token` field in scaffolded create/update forms. On the
/// mutating POST the server consumes the token exactly once: a double-click,
/// Back→resubmit, or browser retry carrying an already-consumed token replays
/// the first response instead of re-running the handler, so no duplicate row is
/// created — with no client-side JavaScript.
///
/// Unlike [`IdempotencyConfig`](crate::config::IdempotencyConfig), the guard is
/// driven by a form field, not the `Idempotency-Key` header, so it protects
/// bare browser form submits.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `enabled` | `true` |
/// | `field_name` | `"_submit_token"` |
/// | `ttl_secs` | `600` (10 min) |
/// | `in_flight_ttl_secs` | `86_400` (24 h) |
/// | `backend` | *inherits `[idempotency].backend`* (in-memory in dev, Redis in prod) |
/// | `exempt_paths` | `[]` |
///
/// # Examples
///
/// ```toml
/// [security.submit_token]
/// enabled = true
/// ttl_secs = 900
/// backend = "redis"   # override; reuses the [idempotency.redis] connection settings
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct SubmitTokenConfig {
    /// Enable one-time submit-token protection. Default: `true`.
    #[serde(default = "default_submit_token_enabled")]
    pub enabled: bool,

    /// Hidden form field name carrying the token. Default: `"_submit_token"`.
    #[serde(default = "default_submit_token_field")]
    pub field_name: String,

    /// Time-to-live in seconds for a consumed token's stored response.
    /// Default: `600` (10 minutes).
    #[serde(default = "default_submit_token_ttl_secs")]
    pub ttl_secs: u64,

    /// Maximum stale lifetime in seconds for an in-flight submission lock.
    ///
    /// While a mutating request is running, its token is locked so a concurrent
    /// retry carrying the same token is excluded until the first request records
    /// its consumed response. The lock is released as soon as that record is
    /// stored, so this value is only the backend safety expiry for crashes or
    /// lost unlocks — it must be comfortably longer than any supported mutating
    /// request duration. Deliberately **independent of `ttl_secs`** (the replay
    /// window): lowering `ttl_secs` must never shorten how long an active
    /// submission is excluded from re-entry, which would let a slow request's
    /// retry acquire a fresh lock and double-execute. Default: `86_400`
    /// (24 hours), matching `[idempotency].in_flight_ttl_secs`.
    #[serde(default = "default_submit_token_in_flight_ttl_secs")]
    pub in_flight_ttl_secs: u64,

    /// Storage backend for consumed submit tokens.
    ///
    /// When unset (the default, `None`), the submit-token store **inherits the
    /// configured idempotency backend** (`[idempotency].backend`): a
    /// Redis-configured app automatically shares one consumed-token store across
    /// replicas, while a dev app on the default in-memory idempotency backend
    /// keeps an in-memory token store. This matches issue #1360: the token store
    /// is backed by the existing idempotency/session store backend (in-memory in
    /// dev, Redis in prod), so a double-click load-balanced to a different
    /// replica cannot re-run the mutation in production.
    ///
    /// Set explicitly to override the inherited backend for submit tokens only.
    /// When it resolves to `"redis"`, the store reuses the `[idempotency.redis]`
    /// connection settings so a multi-replica deployment shares one token store.
    ///
    /// Use [`Self::resolved_backend`] to obtain the effective backend.
    #[serde(default)]
    pub backend: Option<crate::config::IdempotencyBackend>,

    /// Request path prefixes that are exempt from submit-token guarding.
    /// Default: `[]`.
    #[serde(default)]
    pub exempt_paths: Vec<String>,
}

impl Default for SubmitTokenConfig {
    fn default() -> Self {
        Self {
            enabled: default_submit_token_enabled(),
            field_name: default_submit_token_field(),
            ttl_secs: default_submit_token_ttl_secs(),
            in_flight_ttl_secs: default_submit_token_in_flight_ttl_secs(),
            backend: None,
            exempt_paths: Vec::new(),
        }
    }
}

impl SubmitTokenConfig {
    /// Resolve the effective consumed-token storage backend.
    ///
    /// Returns the explicit `backend` override when one is configured;
    /// otherwise inherits `idempotency_backend` (the app's
    /// `[idempotency].backend`) so submit tokens share the idempotency store by
    /// default. This is the single source of truth for backend selection so the
    /// idempotency layer and the submit-token layer cannot drift apart.
    #[must_use]
    pub fn resolved_backend(
        &self,
        idempotency_backend: crate::config::IdempotencyBackend,
    ) -> crate::config::IdempotencyBackend {
        self.backend.unwrap_or(idempotency_backend)
    }
}

const fn default_submit_token_enabled() -> bool {
    true
}

fn default_submit_token_field() -> String {
    "_submit_token".to_owned()
}

const fn default_submit_token_ttl_secs() -> u64 {
    600
}

const fn default_submit_token_in_flight_ttl_secs() -> u64 {
    86_400
}

/// Strategy for identifying which client a rate-limit bucket belongs to.
///
/// Controls what value is used as the bucket key for incoming requests.
///
/// # `autumn.toml` example
///
/// ```toml
/// [security.rate_limit]
/// enabled = true
/// key_strategy = "authenticated_principal"
/// ```
///
/// | Value | Description |
/// |-------|-------------|
/// | `"ip"` | Connection peer address (default). Safe against header spoofing. |
/// | `"api_token"` | `Authorization: Bearer <token>` value. Falls back to IP when no token. |
/// | `"authenticated_principal"` | Principal ID set by auth middleware via `RateLimitPrincipal` extension. Falls back to IP for unauthenticated requests. |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStrategy {
    /// Key on client IP address (connection peer or trusted-proxy-resolved). **Default.**
    #[default]
    Ip,
    /// Key on the `Authorization: Bearer` token value. Falls back to IP when absent.
    ///
    /// The `token` alias matches the `#[throttle(key = "token")]` macro spelling so an
    /// operator can move an inline policy into `[security.rate_limit.named.*]` verbatim.
    #[serde(alias = "token")]
    ApiToken,
    /// Key on the authenticated principal ID from the `RateLimitPrincipal` request
    /// extension (set by the auth middleware). Falls back to IP for unauthenticated requests.
    ///
    /// The `principal` alias matches the `#[throttle(key = "principal")]` macro spelling so
    /// an operator can move an inline policy into `[security.rate_limit.named.*]` verbatim.
    #[serde(alias = "principal")]
    AuthenticatedPrincipal,
}

impl KeyStrategy {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ip" => Some(Self::Ip),
            "api_token" => Some(Self::ApiToken),
            "authenticated_principal" => Some(Self::AuthenticatedPrincipal),
            _ => None,
        }
    }
}

/// Per-tier rate limit parameters for tiered quota configuration.
///
/// Declare named tiers under `[security.rate_limit.tiers.<name>]` in `autumn.toml`.
/// Each tier gets its own token bucket with independent `requests_per_second` and
/// `burst` values. The app maps callers to a tier via a tier-assignment hook
/// (see [`crate::security::rate_limit::RateLimitLayer::with_tier_hook`]).
///
/// # `autumn.toml` example
///
/// ```toml
/// [security.rate_limit.tiers.free]
/// requests_per_second = 1.0
/// burst = 10
///
/// [security.rate_limit.tiers.pro]
/// requests_per_second = 10.0
/// burst = 100
///
/// [security.rate_limit.tiers.enterprise]
/// requests_per_second = 100.0
/// burst = 1000
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitTierConfig {
    /// Steady-state refill rate for this tier in requests per second.
    pub requests_per_second: f64,
    /// Maximum burst capacity (token bucket size) for this tier.
    pub burst: u32,
}

/// Named per-route rate limit referenced from `#[throttle("name")]`.
///
/// Declare one entry per named limiter under `[security.rate_limit.named.<name>]`
/// and reference it from a handler via `#[throttle("name")]` so the throttling
/// policy lives in config, not code.
///
/// ```toml
/// [security.rate_limit.named.login]
/// limit = 5
/// per = "1m"
/// key = "ip"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitNamedConfig {
    /// Maximum number of requests allowed in each `per` window. Also serves as
    /// the token-bucket burst capacity.
    pub limit: u32,
    /// Window duration parsed by [`crate::task::parse_duration`] (e.g. `"1m"`,
    /// `"30s"`, `"1h"`). The steady-state refill rate is `limit / per`.
    pub per: String,
    /// Optional keying strategy override. When `None`, the named limiter uses
    /// the same key strategy as the global limiter (`security.rate_limit.key_strategy`).
    #[serde(default)]
    pub key: Option<KeyStrategy>,
}

/// Rate limiting configuration.
///
/// Applies a token bucket to every request, keyed by client IP (default)
/// or by authenticated principal / API token. When a client exhausts their
/// bucket, the middleware returns `429 Too Many Requests` with `Retry-After`
/// and Problem Details (RFC 9457).
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `enabled` | `false` |
/// | `requests_per_second` | `10.0` |
/// | `burst` | `20` |
/// | `trust_forwarded_headers` | `false` |
/// | `trusted_proxies` | `[]` |
/// | `key_strategy` | `"ip"` |
/// | `tiers` | `{}` (no tiers; all callers share the default config) |
///
/// # Client IP resolution
///
/// By default the limiter keys on the **connection peer address**. This
/// prevents clients from bypassing throttling by rotating `X-Forwarded-For`
/// values. Set `trust_forwarded_headers = true` only when the server
/// sits behind a trusted reverse proxy that strips and rewrites
/// forwarding headers on every request.
///
/// If trusted upstream proxies append to `X-Forwarded-For`, configure
/// `trusted_proxies` with the trusted proxy IPs or CIDR ranges. Autumn
/// then walks the header from right to left, skips those trusted proxy
/// hops, and keys the bucket on the nearest untrusted client IP.
///
/// # Per-principal / API-token keying
///
/// Set `key_strategy = "authenticated_principal"` to key on the authenticated
/// user identity instead of IP. Auth middleware must insert a
/// `RateLimitPrincipal` extension on the request before the rate limiter runs.
/// Unauthenticated requests fall through to IP-based keying — never silently
/// unbounded.
///
/// Set `key_strategy = "api_token"` to key on the `Authorization: Bearer`
/// token value. Falls back to IP when no `Authorization` header is present.
///
/// # Tiered quotas
///
/// Declare named tiers and register a tier-assignment hook at startup:
///
/// ```toml
/// [security.rate_limit]
/// key_strategy = "authenticated_principal"
///
/// [security.rate_limit.tiers.free]
/// requests_per_second = 1.0
/// burst = 10
///
/// [security.rate_limit.tiers.pro]
/// requests_per_second = 10.0
/// burst = 100
/// ```
///
/// # Examples
///
/// ```toml
/// [security.rate_limit]
/// enabled = true
/// requests_per_second = 5.0
/// burst = 10
/// trust_forwarded_headers = false
/// trusted_proxies = ["10.0.0.10", "203.0.113.0/24"]
/// key_strategy = "authenticated_principal"
///
/// # Multi-replica: share the budget across all pods
/// backend = "redis"
/// on_backend_failure = "fail_open"
///
/// [security.rate_limit.redis]
/// url = "redis://redis:6379"
/// key_prefix = "myapp:rate_limit"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    /// Enable rate limiting. Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Steady-state refill rate in requests per second. Default: `10.0`.
    ///
    /// Used as the default when no tier matches. Configure per-tier values
    /// under `[security.rate_limit.tiers.<name>]`.
    #[serde(default = "default_rps")]
    pub requests_per_second: f64,

    /// Maximum burst capacity (number of tokens the bucket can hold).
    /// Default: `20`.
    ///
    /// Used as the default when no tier matches.
    #[serde(default = "default_burst")]
    pub burst: u32,

    /// **Deprecated** — use `[security.trusted_proxies]` instead.
    ///
    /// Consult `X-Forwarded-For` / `X-Real-IP` before the connection peer
    /// when identifying the client. Default: `false`.
    ///
    /// This field is honoured for one minor release and emits a startup
    /// warning.  Configure [`SecurityConfig::trusted_proxies`] to silence
    /// the warning and share the policy with all middleware.
    #[serde(default)]
    pub trust_forwarded_headers: bool,

    /// **Deprecated** — use `[security.trusted_proxies]` instead.
    ///
    /// Trusted proxy IP addresses or CIDR ranges to skip at the right
    /// side of an appended `X-Forwarded-For` chain.
    ///
    /// This field is honoured for one minor release and emits a startup
    /// warning.  Configure [`SecurityConfig::trusted_proxies`] to silence
    /// the warning and share the policy with all middleware.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    /// Key extraction strategy. Default: `"ip"`.
    ///
    /// Determines what value is used as the rate-limit bucket key.
    /// See [`KeyStrategy`] for the available options.
    #[serde(default)]
    pub key_strategy: KeyStrategy,

    /// Named tiers with per-tier `requests_per_second` and `burst` values.
    ///
    /// When a tier-assignment hook is registered and returns a tier name that
    /// matches a key here, that tier's config is used for the caller's bucket
    /// instead of the top-level defaults.
    #[serde(default)]
    pub tiers: HashMap<String, RateLimitTierConfig>,

    /// Named per-route limiters referenced from `#[throttle("name")]`.
    ///
    /// Each entry defines a `limit` (tokens per window) and `per` (window
    /// duration) so ops can tune per-route thresholds without a recompile.
    /// An optional `key` overrides the default keying strategy for that
    /// named limiter.
    ///
    /// ```toml
    /// [security.rate_limit.named.login]
    /// limit = 5
    /// per = "1m"
    /// key = "ip"
    /// ```
    #[serde(default)]
    pub named: HashMap<String, RateLimitNamedConfig>,

    /// Bucket store backend. Default: `"memory"` (in-process, single-replica).
    ///
    /// Set to `"redis"` in multi-replica deployments so the configured
    /// rate cap is enforced globally rather than per pod. Requires the
    /// `redis` cargo feature to take effect; without it, a startup warning
    /// is emitted and the memory backend is used.
    #[serde(default)]
    pub backend: RateLimitBackend,

    /// Redis backend options. Used when `backend = "redis"`.
    ///
    /// Requires the `redis` cargo feature.
    #[cfg(feature = "redis")]
    #[serde(default)]
    pub redis: RateLimitRedisConfig,

    /// Behavior when the backend is unavailable. Default: `"fail_open"`.
    ///
    /// `"fail_open"` lets requests through (matches single-replica posture).
    /// `"fail_closed"` returns `429` until the backend recovers.
    ///
    /// Requires the `redis` cargo feature.
    #[cfg(feature = "redis")]
    #[serde(default)]
    pub on_backend_failure: RateLimitBackendFailure,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: default_rps(),
            burst: default_burst(),
            trust_forwarded_headers: false,
            trusted_proxies: Vec::new(),
            key_strategy: KeyStrategy::default(),
            tiers: HashMap::new(),
            named: HashMap::new(),
            backend: RateLimitBackend::default(),
            #[cfg(feature = "redis")]
            redis: RateLimitRedisConfig::default(),
            #[cfg(feature = "redis")]
            on_backend_failure: RateLimitBackendFailure::default(),
        }
    }
}

/// Storage backend for per-IP token buckets.
///
/// Matches the pattern established by [`CacheBackend`](crate::config::CacheBackend)
/// (issue #535) and `SchedulerBackend` (issue #531): one `backend = "redis"` flip
/// per subsystem, identical failure semantics.
///
/// The enum is always available so misconfiguration is detectable even when the
/// `redis` cargo feature is disabled. Without the feature, selecting `Redis`
/// emits a startup warning and falls back to `Memory`.
///
/// [`CacheBackend`]: crate::config::CacheBackend
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitBackend {
    /// In-process LRU of token buckets (default). Each replica maintains its own
    /// store; a 3-replica deployment permits up to 3× the configured rate.
    #[default]
    Memory,
    /// Shared Redis store coordinated via an atomic Lua script. The configured
    /// rate is enforced globally across all replicas.
    ///
    /// Requires the `redis` cargo feature.
    Redis,
}

impl RateLimitBackend {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Behavior when the rate-limit backend becomes unreachable.
///
/// Configures the limiter's posture when the storage backend (Redis) is
/// unavailable. Matches the pattern used by the webhook replay store.
///
/// Requires the `redis` cargo feature.
#[cfg(feature = "redis")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitBackendFailure {
    /// Allow the request through. Matches the existing single-replica posture:
    /// a lost limiter is invisible to clients. **Default.**
    #[default]
    #[serde(alias = "open")]
    FailOpen,
    /// Deny the request with `429 Too Many Requests` until the backend recovers.
    #[serde(alias = "closed")]
    FailClosed,
}

#[cfg(feature = "redis")]
impl RateLimitBackendFailure {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fail_open" | "open" => Some(Self::FailOpen),
            "fail_closed" | "closed" => Some(Self::FailClosed),
            _ => None,
        }
    }
}

/// Redis-specific options for the rate-limit backend.
///
/// Used when `security.rate_limit.backend = "redis"`.
///
/// Requires the `redis` cargo feature.
#[cfg(feature = "redis")]
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitRedisConfig {
    /// Redis connection URL (e.g. `redis://127.0.0.1:6379`).
    /// Reuses the same Redis instance as sessions, cache, and the scheduler.
    #[serde(default)]
    pub url: Option<String>,

    /// Key prefix for all token-bucket hashes stored in Redis.
    #[serde(default = "default_rate_limit_redis_key_prefix")]
    pub key_prefix: String,
}

#[cfg(feature = "redis")]
impl Default for RateLimitRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_rate_limit_redis_key_prefix(),
        }
    }
}

#[cfg(feature = "redis")]
fn default_rate_limit_redis_key_prefix() -> String {
    "autumn:rate_limit".to_owned()
}

/// Multipart upload configuration.
///
/// Applies framework-level guardrails for `multipart/form-data` requests:
///
/// - `max_request_size_bytes`: global request body cap (enforced by middleware)
/// - `max_file_size_bytes`: per-file cap for `crate::extract::Multipart` helpers
/// - `allowed_mime_types`: optional MIME-type allow list for uploaded parts
/// - `reject_on_content_type_mismatch`: strict mode that rejects when the
///   client-declared `Content-Type` disagrees with the sniffed content
///
/// Leave `allowed_mime_types` empty to allow any content type.
///
/// # Content sniffing
///
/// The `crate::extract::Multipart` extractor validates uploaded file parts by
/// their actual content (magic bytes), **not** the spoofable client-declared
/// `Content-Type` header. See `allowed_mime_types` for the exact sniffed →
/// markup-guard → declared-fallback precedence used when a list is configured.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadConfig {
    /// Maximum total multipart request body size in bytes.
    #[serde(default = "default_max_request_size_bytes")]
    pub max_request_size_bytes: usize,
    /// Maximum individual uploaded file size in bytes.
    #[serde(default = "default_max_file_size_bytes")]
    pub max_file_size_bytes: usize,
    /// Optional allowed MIME types (e.g. `["image/png", "image/jpeg"]`).
    ///
    /// Enforced primarily against the **sniffed** (magic-byte) content type,
    /// never blindly against the client-declared header. Because `infer` only
    /// recognizes binary formats, the check applies this precedence for each
    /// file part when the list is non-empty:
    ///
    /// 1. **Sniffed type recognized** → it must appear in the list, else the
    ///    upload is rejected (`400`). The declared header is ignored.
    /// 2. **Unrecognized but looks like markup** (leading `<…` after a BOM /
    ///    whitespace — HTML, SVG, XML) → always rejected (`400`), so scripts
    ///    or `<svg onload=…>` can't ride in under a spoofed declared type.
    /// 3. **Unrecognized and not markup** → the declared content-type essence
    ///    (media type without parameters) is trusted **only** when it names a
    ///    signature-less TEXT type (`text/*`, `application/json`,
    ///    `application/csv`) that appears in the list. Binary/sniffable types
    ///    (`image/*`, `application/pdf`, …) are always enforced strictly by
    ///    magic bytes: unrecognizable bytes declaring such a type are rejected
    ///    (`400`), since a genuine file of that type would have sniffed
    ///    positively. This is the only case where the declared header is
    ///    trusted, and only to disambiguate among signature-less text formats.
    #[serde(default)]
    pub allowed_mime_types: Vec<String>,
    /// When `true`, reject an uploaded file part if the client-declared
    /// `Content-Type` header disagrees with the sniffed (magic-byte) content
    /// type. Default: `false`.
    ///
    /// Behavior when enabled (comparison uses the declared essence — the media
    /// type without parameters):
    /// - declared and sniffed both known but differ → reject (`400`)
    /// - declared known but content unrecognized (sniffed unknown) → reject
    ///   (`400`, the declared type cannot be verified)
    /// - no declared header → reject (`400`); omitting `Content-Type` must not
    ///   silently bypass the mismatch check
    ///
    /// This is independent of `allowed_mime_types`; both checks apply when set.
    #[serde(default)]
    pub reject_on_content_type_mismatch: bool,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_request_size_bytes: default_max_request_size_bytes(),
            max_file_size_bytes: default_max_file_size_bytes(),
            allowed_mime_types: Vec::new(),
            reject_on_content_type_mismatch: false,
        }
    }
}

/// Per-request Content Security Policy nonce configuration.
///
/// When `enabled = true`, the security-headers middleware generates a fresh
/// cryptographically-random nonce (≥128 bits, URL-safe base64) for every
/// request. The nonce is:
///
/// 1. Injected into `script-src` and `style-src` of the **default** CSP as
///    `'nonce-<value>'`, replacing `'unsafe-inline'`.
/// 2. Inserted into request extensions so handlers can extract it via
///    [`CspNonce`](crate::security::CspNonce).
///
/// Apps that override `content_security_policy` with an explicit string
/// automatically opt out of nonce injection for the header — the custom CSP
/// is used verbatim — but the nonce is still generated and available via the
/// extractor for template use.
///
/// # `autumn.toml` example
///
/// ```toml
/// [security.headers.csp_nonce]
/// enabled = true
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CspNonceConfig {
    /// Enable per-request CSP nonce generation. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
}

// ── Default value functions ────────────────────────────────────────

const fn default_true() -> bool {
    true
}

fn default_x_frame_options() -> String {
    "DENY".to_owned()
}

const fn default_hsts_max_age() -> u64 {
    31_536_000 // 1 year
}

fn default_referrer_policy() -> String {
    "strict-origin-when-cross-origin".to_owned()
}

/// Default `Content-Security-Policy` value.
///
/// Designed to be "sensible by default" while allowing htmx to function
/// normally when served from the same origin (as Autumn does for htmx and its
/// CSRF helper under `/static/js/`).
///
/// Directives:
/// - `default-src 'self'` -- everything defaults to same-origin
/// - `img-src 'self' data:` -- images from self and inline data URIs
/// - `style-src 'self' 'unsafe-inline'` -- same-origin stylesheets plus
///   inline `style` attributes (required by many UI libraries and
///   template engines)
/// - `script-src 'self'` -- only same-origin scripts; htmx and Autumn's htmx
///   CSRF helper work here because they are served from `/static/js/`
/// - `connect-src 'self'` -- `fetch`/`XHR`/htmx requests go to same origin
/// - `form-action 'self'` -- forms can only POST to same origin
/// - `frame-ancestors 'none'` -- matches the default `X-Frame-Options: DENY`
/// - `base-uri 'self'` -- prevents `<base>` hijacking
#[must_use]
pub fn default_content_security_policy() -> String {
    "default-src 'self'; \
     img-src 'self' data:; \
     style-src 'self' 'unsafe-inline'; \
     script-src 'self'; \
     connect-src 'self'; \
     form-action 'self'; \
     frame-ancestors 'none'; \
     base-uri 'self'"
        .to_owned()
}

fn default_csrf_header() -> String {
    "X-CSRF-Token".to_owned()
}

fn default_csrf_field() -> String {
    "_csrf".to_owned()
}

fn default_csrf_cookie() -> String {
    "autumn-csrf".to_owned()
}

fn default_safe_methods() -> Vec<String> {
    vec![
        "GET".to_owned(),
        "HEAD".to_owned(),
        "OPTIONS".to_owned(),
        "TRACE".to_owned(),
    ]
}

const fn default_rps() -> f64 {
    10.0
}

const fn default_burst() -> u32 {
    20
}

const fn default_max_request_size_bytes() -> usize {
    32 * 1024 * 1024
}

const fn default_max_file_size_bytes() -> usize {
    16 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IdempotencyBackend;

    // ── submit-token backend resolution (Finding D: inherit idempotency) ─────

    #[test]
    fn submit_token_backend_defaults_to_none_and_inherits_idempotency() {
        // Unset `[security.submit_token].backend` deserializes to `None`.
        let cfg: SubmitTokenConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.backend, None, "unset backend must deserialize to None");
        // With idempotency on Redis, the resolved submit-token backend follows
        // it — NOT the old hardcoded Memory default.
        assert_eq!(
            cfg.resolved_backend(IdempotencyBackend::Redis),
            IdempotencyBackend::Redis,
            "an unset submit-token backend must inherit the Redis idempotency backend"
        );
        // A dev app on the default Memory idempotency backend stays Memory.
        assert_eq!(
            cfg.resolved_backend(IdempotencyBackend::Memory),
            IdempotencyBackend::Memory,
            "an unset submit-token backend on a Memory idempotency app stays Memory"
        );
    }

    #[test]
    fn submit_token_explicit_backend_overrides_inherited_idempotency() {
        // An explicit `backend = "memory"` wins even when idempotency is Redis.
        let cfg: SubmitTokenConfig = toml::from_str("backend = \"memory\"").unwrap();
        assert_eq!(cfg.backend, Some(IdempotencyBackend::Memory));
        assert_eq!(
            cfg.resolved_backend(IdempotencyBackend::Redis),
            IdempotencyBackend::Memory,
            "an explicit submit-token backend override must win over the inherited backend"
        );

        // An explicit `backend = "redis"` wins even when idempotency is Memory.
        let cfg: SubmitTokenConfig = toml::from_str("backend = \"redis\"").unwrap();
        assert_eq!(cfg.backend, Some(IdempotencyBackend::Redis));
        assert_eq!(
            cfg.resolved_backend(IdempotencyBackend::Memory),
            IdempotencyBackend::Redis,
            "an explicit redis override must win over an inherited Memory backend"
        );
    }

    // ── validate_signing_secret (RED phase) ─────────────────────────────────

    #[test]
    fn signing_secret_dev_skips_validation_with_none() {
        assert!(validate_signing_secret(None, false).is_ok());
    }

    #[test]
    fn signing_secret_dev_skips_validation_with_weak_value() {
        assert!(validate_signing_secret(Some("changeme"), false).is_ok());
    }

    #[test]
    fn signing_secret_dev_skips_validation_with_short_value() {
        assert!(validate_signing_secret(Some("short"), false).is_ok());
    }

    #[test]
    fn signing_secret_prod_missing_is_error() {
        let err = validate_signing_secret(None, true).unwrap_err();
        assert!(matches!(err, SigningSecretError::MissingInProduction));
    }

    #[test]
    fn signing_secret_prod_too_short_is_error() {
        let short = "a".repeat(MIN_SECRET_LEN - 1);
        let err = validate_signing_secret(Some(&short), true).unwrap_err();
        assert!(matches!(err, SigningSecretError::TooShort { .. }));
    }

    #[test]
    fn signing_secret_prod_exact_min_length_passes() {
        let exactly_min = "a".repeat(MIN_SECRET_LEN);
        assert!(validate_signing_secret(Some(&exactly_min), true).is_ok());
    }

    #[test]
    fn signing_secret_prod_known_demo_value_is_error() {
        let err = validate_signing_secret(Some("changeme"), true).unwrap_err();
        assert!(matches!(err, SigningSecretError::KnownWeakValue(_)));
    }

    #[test]
    fn signing_secret_prod_demo_value_case_insensitive() {
        let err = validate_signing_secret(Some("CHANGEME"), true).unwrap_err();
        assert!(matches!(err, SigningSecretError::KnownWeakValue(_)));
    }

    #[test]
    fn signing_secret_prod_valid_64char_hex_passes() {
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert!(validate_signing_secret(Some(secret), true).is_ok());
    }

    #[test]
    fn signing_secret_config_defaults_to_none() {
        let config = SigningSecretConfig::default();
        assert!(config.secret.is_none());
        assert!(config.previous_secrets.is_empty());
    }

    #[test]
    fn signing_secret_error_missing_display_mentions_env_var() {
        let err = SigningSecretError::MissingInProduction;
        assert!(err.to_string().contains("AUTUMN_SECURITY__SIGNING_SECRET"));
    }

    #[test]
    fn signing_secret_error_too_short_display_shows_lengths() {
        let err = SigningSecretError::TooShort {
            actual: 8,
            required: 32,
        };
        let s = err.to_string();
        assert!(s.contains('8'));
        assert!(s.contains("32"));
    }

    #[test]
    fn signing_secret_error_weak_value_display_mentions_demo() {
        let err = SigningSecretError::KnownWeakValue("changeme".to_owned());
        assert!(err.to_string().contains("template/demo"));
    }

    #[test]
    fn signing_secret_prod_too_short_error_reports_actual_length() {
        let short = "tooshort"; // 8 bytes
        let err = validate_signing_secret(Some(short), true).unwrap_err();
        if let SigningSecretError::TooShort { actual, required } = err {
            assert_eq!(actual, 8);
            assert_eq!(required, MIN_SECRET_LEN);
        } else {
            panic!("expected TooShort error");
        }
    }

    #[test]
    fn signing_secret_prod_secret_key_demo_value_fails() {
        assert!(matches!(
            validate_signing_secret(Some("secret"), true),
            Err(SigningSecretError::KnownWeakValue(_))
        ));
    }

    #[test]
    fn signing_secret_prod_supersecret_demo_value_fails() {
        assert!(matches!(
            validate_signing_secret(Some("supersecret"), true),
            Err(SigningSecretError::KnownWeakValue(_))
        ));
    }

    #[test]
    fn signing_secret_config_deserialize_from_toml() {
        let toml_str = r#"
            secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
            previous_secrets = ["oldsecret01234567890123456789012"]
        "#;
        let config: SigningSecretConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.secret.as_deref(),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4")
        );
        assert_eq!(config.previous_secrets.len(), 1);
    }

    #[test]
    fn security_config_defaults() {
        let config = SecurityConfig::default();
        assert_eq!(config.headers.x_frame_options, "DENY");
        assert!(config.headers.x_content_type_options);
        assert!(config.headers.xss_protection);
        assert!(!config.headers.strict_transport_security);
        assert_eq!(config.headers.hsts_max_age_secs, 31_536_000);
        // Default CSP is non-empty and htmx-compatible.
        assert!(!config.headers.content_security_policy.is_empty());
        assert!(
            config
                .headers
                .content_security_policy
                .contains("default-src 'self'")
        );
        assert!(
            config
                .headers
                .content_security_policy
                .contains("script-src 'self'")
        );
        assert_eq!(
            config.headers.referrer_policy,
            "strict-origin-when-cross-origin"
        );
    }

    #[test]
    fn default_csp_does_not_allow_unsafe_eval() {
        // htmx works without unsafe-eval; only `hx-on` opts into it.
        // Keep the default tight so that the baseline policy passes
        // Mozilla Observatory and similar automated scanners.
        let csp = default_content_security_policy();
        assert!(!csp.contains("'unsafe-eval'"), "csp = {csp}");
        assert!(
            !csp.contains("'unsafe-inline' 'unsafe-eval'"),
            "csp = {csp}"
        );
    }

    #[test]
    fn csp_can_be_disabled_via_toml_empty_string() {
        let toml_str = r#"
            content_security_policy = ""
        "#;
        let config: HeadersConfig = toml::from_str(toml_str).unwrap();
        assert!(config.content_security_policy.is_empty());
    }

    #[test]
    fn csp_can_be_overridden_via_toml() {
        let toml_str = r#"
            content_security_policy = "default-src 'none'"
        "#;
        let config: HeadersConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.content_security_policy, "default-src 'none'");
    }

    #[test]
    fn csrf_config_defaults() {
        let config = CsrfConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.token_header, "X-CSRF-Token");
        assert_eq!(config.form_field, "_csrf");
        assert_eq!(config.cookie_name, "autumn-csrf");
        assert_eq!(config.safe_methods.len(), 4);
    }

    #[test]
    fn headers_config_deserialize() {
        let toml_str = r#"
            x_frame_options = "SAMEORIGIN"
            strict_transport_security = true
            content_security_policy = "default-src 'self'"
        "#;
        let config: HeadersConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.x_frame_options, "SAMEORIGIN");
        assert!(config.strict_transport_security);
        assert_eq!(config.content_security_policy, "default-src 'self'");
        // Defaults for unspecified fields
        assert!(config.x_content_type_options);
        assert!(config.xss_protection);
    }

    #[test]
    fn csrf_config_deserialize() {
        let toml_str = r#"
            enabled = true
            token_header = "X-XSRF-Token"
        "#;
        let config: CsrfConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.token_header, "X-XSRF-Token");
        assert_eq!(config.form_field, "_csrf"); // default preserved
    }

    #[test]
    fn rate_limit_config_defaults() {
        let config = RateLimitConfig::default();
        assert!(!config.enabled);
        assert!((config.requests_per_second - 10.0).abs() < f64::EPSILON);
        assert_eq!(config.burst, 20);
        assert!(!config.trust_forwarded_headers);
        assert!(config.trusted_proxies.is_empty());
        #[cfg(feature = "redis")]
        {
            assert_eq!(config.backend, RateLimitBackend::Memory);
            assert_eq!(config.on_backend_failure, RateLimitBackendFailure::FailOpen);
            assert_eq!(config.redis.key_prefix, "autumn:rate_limit");
        }
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_backend_deserializes_memory() {
        let config: RateLimitConfig = toml::from_str("backend = \"memory\"").unwrap();
        assert_eq!(config.backend, RateLimitBackend::Memory);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_backend_deserializes_redis() {
        let config: RateLimitConfig = toml::from_str("backend = \"redis\"").unwrap();
        assert_eq!(config.backend, RateLimitBackend::Redis);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_on_backend_failure_deserializes_fail_open() {
        let config: RateLimitConfig = toml::from_str("on_backend_failure = \"fail_open\"").unwrap();
        assert_eq!(config.on_backend_failure, RateLimitBackendFailure::FailOpen);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_on_backend_failure_deserializes_fail_closed() {
        let config: RateLimitConfig =
            toml::from_str("on_backend_failure = \"fail_closed\"").unwrap();
        assert_eq!(
            config.on_backend_failure,
            RateLimitBackendFailure::FailClosed
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_redis_config_deserializes() {
        let toml_str = r#"
            backend = "redis"
            [redis]
            url = "redis://localhost:6379"
            key_prefix = "myapp:rl"
        "#;
        let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.backend, RateLimitBackend::Redis);
        assert_eq!(config.redis.url.as_deref(), Some("redis://localhost:6379"));
        assert_eq!(config.redis.key_prefix, "myapp:rl");
    }

    #[cfg(feature = "redis")]
    #[test]
    fn rate_limit_redis_config_defaults_key_prefix() {
        let config: RateLimitConfig = toml::from_str("backend = \"redis\"").unwrap();
        assert_eq!(config.redis.key_prefix, "autumn:rate_limit");
        assert!(config.redis.url.is_none());
    }

    #[test]
    fn rate_limit_backend_from_env_value() {
        assert_eq!(
            RateLimitBackend::from_env_value("memory"),
            Some(RateLimitBackend::Memory)
        );
        assert_eq!(
            RateLimitBackend::from_env_value("redis"),
            Some(RateLimitBackend::Redis)
        );
        assert_eq!(
            RateLimitBackend::from_env_value("REDIS"),
            Some(RateLimitBackend::Redis)
        );
        assert_eq!(RateLimitBackend::from_env_value("postgres"), None);
        assert_eq!(RateLimitBackend::from_env_value(""), None);
    }

    #[cfg(feature = "redis")] // RateLimitBackendFailure is redis-gated
    #[test]
    fn rate_limit_backend_failure_from_env_value() {
        assert_eq!(
            RateLimitBackendFailure::from_env_value("fail_open"),
            Some(RateLimitBackendFailure::FailOpen)
        );
        assert_eq!(
            RateLimitBackendFailure::from_env_value("open"),
            Some(RateLimitBackendFailure::FailOpen)
        );
        assert_eq!(
            RateLimitBackendFailure::from_env_value("FAIL_OPEN"),
            Some(RateLimitBackendFailure::FailOpen)
        );
        assert_eq!(
            RateLimitBackendFailure::from_env_value("fail_closed"),
            Some(RateLimitBackendFailure::FailClosed)
        );
        assert_eq!(
            RateLimitBackendFailure::from_env_value("closed"),
            Some(RateLimitBackendFailure::FailClosed)
        );
        assert_eq!(RateLimitBackendFailure::from_env_value("panic"), None);
        assert_eq!(RateLimitBackendFailure::from_env_value(""), None);
    }

    #[test]
    fn rate_limit_config_deserialize() {
        let toml_str = r#"
            enabled = true
            requests_per_second = 5.0
            burst = 100
            trust_forwarded_headers = true
            trusted_proxies = ["10.0.0.10", "203.0.113.0/24"]
        "#;
        let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!((config.requests_per_second - 5.0).abs() < f64::EPSILON);
        assert_eq!(config.burst, 100);
        assert!(config.trust_forwarded_headers);
        assert_eq!(config.trusted_proxies, vec!["10.0.0.10", "203.0.113.0/24"]);
    }

    #[test]
    fn rate_limit_config_partial_deserialize_uses_defaults() {
        let toml_str = "enabled = true";
        let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!((config.requests_per_second - 10.0).abs() < f64::EPSILON);
        assert_eq!(config.burst, 20);
        assert!(!config.trust_forwarded_headers);
        assert!(config.trusted_proxies.is_empty());
    }

    #[test]
    fn rate_limit_named_key_accepts_short_macro_spellings() {
        // `#[throttle(key = "principal")]` / `key = "token"` use the SHORT macro
        // spellings; an operator moving an inline policy into config must be able
        // to use the same words. Serde aliases bridge macro and config vocabularies.
        let toml_str = r#"
            [named.login]
            limit = 5
            per = "1m"
            key = "principal"

            [named.api]
            limit = 10
            per = "1s"
            key = "token"
        "#;
        let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.named["login"].key,
            Some(KeyStrategy::AuthenticatedPrincipal),
            "short `principal` must deserialize to AuthenticatedPrincipal"
        );
        assert_eq!(
            config.named["api"].key,
            Some(KeyStrategy::ApiToken),
            "short `token` must deserialize to ApiToken"
        );
    }

    #[test]
    fn rate_limit_named_key_still_accepts_long_config_spellings() {
        // The canonical long spellings must keep working unchanged.
        let toml_str = r#"
            [named.login]
            limit = 5
            per = "1m"
            key = "authenticated_principal"

            [named.api]
            limit = 10
            per = "1s"
            key = "api_token"

            [named.byip]
            limit = 1
            per = "1s"
            key = "ip"
        "#;
        let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.named["login"].key,
            Some(KeyStrategy::AuthenticatedPrincipal)
        );
        assert_eq!(config.named["api"].key, Some(KeyStrategy::ApiToken));
        assert_eq!(config.named["byip"].key, Some(KeyStrategy::Ip));
    }

    #[test]
    fn global_key_strategy_still_accepts_existing_spellings() {
        // The aliases live on the shared `KeyStrategy` enum, so the global
        // `key_strategy` field also accepts them — but its existing canonical
        // spellings must remain valid.
        for (toml_str, expected) in [
            (
                "key_strategy = \"authenticated_principal\"",
                KeyStrategy::AuthenticatedPrincipal,
            ),
            ("key_strategy = \"api_token\"", KeyStrategy::ApiToken),
            ("key_strategy = \"ip\"", KeyStrategy::Ip),
            // Aliases also work here (a beneficial side effect).
            (
                "key_strategy = \"principal\"",
                KeyStrategy::AuthenticatedPrincipal,
            ),
            ("key_strategy = \"token\"", KeyStrategy::ApiToken),
        ] {
            let config: RateLimitConfig = toml::from_str(toml_str).unwrap();
            assert_eq!(config.key_strategy, expected, "for {toml_str}");
        }
    }

    #[test]
    fn upload_config_defaults() {
        let config = UploadConfig::default();
        assert_eq!(config.max_request_size_bytes, 32 * 1024 * 1024);
        assert_eq!(config.max_file_size_bytes, 16 * 1024 * 1024);
        assert!(config.allowed_mime_types.is_empty());
    }

    #[test]
    fn upload_config_deserialize() {
        let toml_str = r#"
            max_request_size_bytes = 1024
            max_file_size_bytes = 256
            allowed_mime_types = ["image/png", "image/jpeg"]
        "#;
        let config: UploadConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_size_bytes, 1024);
        assert_eq!(config.max_file_size_bytes, 256);
        assert_eq!(config.allowed_mime_types.len(), 2);
    }

    #[test]
    fn full_security_config_deserialize() {
        let toml_str = r#"
            [headers]
            x_frame_options = "DENY"
            strict_transport_security = true

            [csrf]
            enabled = true

            [rate_limit]
            enabled = true
            requests_per_second = 50.0
            burst = 100

            [upload]
            max_request_size_bytes = 4096
            max_file_size_bytes = 1024
            allowed_mime_types = ["text/plain"]
        "#;
        let config: SecurityConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.headers.x_frame_options, "DENY");
        assert!(config.headers.strict_transport_security);
        assert!(config.csrf.enabled);
        assert!(config.rate_limit.enabled);
        assert!((config.rate_limit.requests_per_second - 50.0).abs() < f64::EPSILON);
        assert_eq!(config.rate_limit.burst, 100);
        assert_eq!(config.upload.max_request_size_bytes, 4096);
        assert_eq!(config.upload.max_file_size_bytes, 1024);
        assert_eq!(config.upload.allowed_mime_types, vec!["text/plain"]);
    }

    // ── ResolvedSigningKeys + resolve_signing_keys (RED phase) ─────────────

    #[test]
    fn resolve_signing_keys_dev_generates_non_empty_ephemeral() {
        let config = SigningSecretConfig::default();
        let keys = resolve_signing_keys(&config);
        assert!(keys.current.len() >= MIN_SECRET_LEN);
    }

    #[test]
    fn resolve_signing_keys_prod_uses_secret_bytes() {
        let secret = "a".repeat(MIN_SECRET_LEN);
        let config = SigningSecretConfig {
            secret: Some(secret.clone()),
            previous_secrets: vec![],
        };
        let keys = resolve_signing_keys(&config);
        assert_eq!(keys.current.as_ref(), secret.as_bytes());
    }

    #[test]
    fn resolve_signing_keys_includes_previous_secrets() {
        let config = SigningSecretConfig {
            secret: Some("a".repeat(MIN_SECRET_LEN)),
            previous_secrets: vec!["b".repeat(MIN_SECRET_LEN)],
        };
        let keys = resolve_signing_keys(&config);
        assert_eq!(keys.previous.len(), 1);
        assert_eq!(
            keys.previous[0].as_ref(),
            "b".repeat(MIN_SECRET_LEN).as_bytes()
        );
    }

    #[test]
    fn resolved_keys_sign_and_verify_current() {
        let keys = ResolvedSigningKeys::new(b"current-key-32-bytes-xxxxxxxxxx".to_vec(), vec![]);
        let sig = keys.sign(b"test-message");
        assert!(keys.verify(b"test-message", &sig));
    }

    #[test]
    fn resolved_keys_verify_rejects_wrong_message() {
        let keys = ResolvedSigningKeys::new(b"current-key-32-bytes-xxxxxxxxxx".to_vec(), vec![]);
        let sig = keys.sign(b"message-a");
        assert!(!keys.verify(b"message-b", &sig));
    }

    #[test]
    fn resolved_keys_verify_previous_key_passes() {
        let old_key = b"old-key-32-bytes-xxxxxxxxxxxx!x".to_vec();
        let new_key = b"new-key-32-bytes-xxxxxxxxxxxx!x".to_vec();
        let old_keys = ResolvedSigningKeys::new(old_key.clone(), vec![]);
        let old_sig = old_keys.sign(b"session-id");
        let new_keys = ResolvedSigningKeys::new(new_key, vec![old_key]);
        assert!(new_keys.verify(b"session-id", &old_sig));
    }

    #[test]
    fn resolved_keys_verify_wrong_key_fails() {
        let keys_a = ResolvedSigningKeys::new(b"key-a-32-bytes-xxxxxxxxxxxxxxxx".to_vec(), vec![]);
        let keys_b = ResolvedSigningKeys::new(b"key-b-32-bytes-xxxxxxxxxxxxxxxx".to_vec(), vec![]);
        let sig = keys_a.sign(b"message");
        assert!(!keys_b.verify(b"message", &sig));
    }

    #[test]
    fn resolved_keys_sign_produces_64_char_hex() {
        let keys = ResolvedSigningKeys::new(b"key".to_vec(), vec![]);
        let sig = keys.sign(b"msg");
        assert_eq!(sig.len(), 64, "HMAC-SHA256 hex is 64 chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── CspNonceConfig (RED phase) ────────────────────────────────────────────

    #[test]
    fn csp_nonce_config_defaults_to_disabled() {
        let config = CspNonceConfig::default();
        assert!(!config.enabled);
    }

    #[test]
    fn headers_config_csp_nonce_defaults_to_disabled() {
        let config = HeadersConfig::default();
        assert!(!config.csp_nonce.enabled);
    }

    #[test]
    fn csp_nonce_config_can_be_enabled_via_toml() {
        let toml_str = r"
            [csp_nonce]
            enabled = true
        ";
        let config: HeadersConfig = toml::from_str(toml_str).unwrap();
        assert!(config.csp_nonce.enabled);
    }

    #[test]
    fn csp_nonce_config_deserialize_standalone() {
        let config: CspNonceConfig = toml::from_str("enabled = true").unwrap();
        assert!(config.enabled);
    }

    #[test]
    fn csp_nonce_config_disabled_by_default_in_standalone() {
        let config: CspNonceConfig = toml::from_str("").unwrap();
        assert!(!config.enabled);
    }

    // ── TrustedProxiesConfig & conflict detection ─────────────────────────────

    #[test]
    fn trusted_proxies_config_parses_from_toml() {
        let toml = r#"
[trusted_proxies]
ranges = ["10.0.0.0/8", "203.0.113.0/24"]
trusted_hops = 2
trust_forwarded_headers = true
"#;
        let config: SecurityConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.trusted_proxies.ranges.len(), 2);
        assert_eq!(config.trusted_proxies.trusted_hops, Some(2));
        assert!(config.trusted_proxies.trust_forwarded_headers);
    }

    #[test]
    fn trusted_proxies_config_defaults_to_no_trust() {
        let config: SecurityConfig = toml::from_str("").unwrap();
        assert!(config.trusted_proxies.ranges.is_empty());
        assert!(config.trusted_proxies.trusted_hops.is_none());
        assert!(!config.trusted_proxies.trust_forwarded_headers);
    }

    #[test]
    fn trusted_proxies_conflict_detected_when_both_set_with_different_values() {
        let toml = r#"
[trusted_proxies]
ranges = ["10.0.0.0/8"]
trust_forwarded_headers = true

[rate_limit]
trusted_proxies = ["192.168.0.0/16"]
trust_forwarded_headers = true
"#;
        let config: SecurityConfig = toml::from_str(toml).unwrap();
        assert!(
            config.trusted_proxies_conflict().is_some(),
            "conflicting proxy configs must be detected"
        );
    }

    #[test]
    fn trusted_proxies_no_conflict_when_only_new_set() {
        let toml = r#"
[trusted_proxies]
ranges = ["10.0.0.0/8"]
trust_forwarded_headers = true
"#;
        let config: SecurityConfig = toml::from_str(toml).unwrap();
        assert!(config.trusted_proxies_conflict().is_none());
    }

    #[test]
    fn trusted_proxies_no_conflict_when_only_old_set() {
        let toml = r#"
[rate_limit]
trusted_proxies = ["10.0.0.0/8"]
trust_forwarded_headers = true
"#;
        let config: SecurityConfig = toml::from_str(toml).unwrap();
        assert!(config.trusted_proxies_conflict().is_none());
    }

    #[test]
    fn trusted_proxies_no_conflict_when_same_values_in_both() {
        let toml = r#"
[trusted_proxies]
ranges = ["10.0.0.0/8"]
trust_forwarded_headers = true

[rate_limit]
trusted_proxies = ["10.0.0.0/8"]
trust_forwarded_headers = true
"#;
        let config: SecurityConfig = toml::from_str(toml).unwrap();
        // Same values — no conflict (though old fields still warn at startup).
        assert!(config.trusted_proxies_conflict().is_none());
    }
}
