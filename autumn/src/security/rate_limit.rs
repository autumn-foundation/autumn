//! Rate limiting middleware.
//!
//! Protects endpoints from abuse by applying a per-client token bucket.
//! The bucket key defaults to client IP and can be changed to
//! authenticated principal or API bearer token via [`KeyStrategy`].
//! Requests that exhaust their bucket receive `429 Too Many Requests`
//! in Problem Details format (RFC 9457) with `Retry-After` and
//! `X-RateLimit-*` headers.
//!
//! # Key strategies
//!
//! | Strategy | Key source | Fallback |
//! |----------|-----------|---------|
//! | `ip` (default) | Connection peer / `X-Forwarded-For` | — |
//! | `api_token` | `Authorization: Bearer <token>` | client IP |
//! | `authenticated_principal` | [`RateLimitPrincipal`] extension | client IP |
//!
//! # Tiered quotas
//!
//! Register named tiers in `autumn.toml` and map principals to tiers via
//! [`RateLimitLayer::with_tier_hook`]. Callers not assigned a tier use the
//! top-level `requests_per_second` / `burst` defaults.
//!
//! # Per-path overrides
//!
//! Call [`RateLimitLayer::with_path_override`] to apply stricter or laxer
//! limits on specific URL paths without disabling the global limiter.
//!
//! # Backends
//!
//! - `"memory"` (default): in-process LRU, zero-config for development.
//! - `"redis"`: shared atomic bucket across replicas (requires `redis` feature).
//!
//! # Configuration
//!
//! ```toml
//! [security.rate_limit]
//! enabled = true
//! requests_per_second = 10.0
//! burst = 20
//! key_strategy = "authenticated_principal"
//!
//! [security.rate_limit.tiers.free]
//! requests_per_second = 1.0
//! burst = 10
//!
//! [security.rate_limit.tiers.pro]
//! requests_per_second = 10.0
//! burst = 100
//!
//! # Multi-replica: share the budget across all pods
//! backend = "redis"
//! on_backend_failure = "fail_open"
//!
//! [security.rate_limit.redis]
//! url = "redis://redis:6379"
//! key_prefix = "myapp:rate_limit"
//! ```

use lru::LruCache;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{HeaderValue, Request, Response, StatusCode};
use http::header::{CONTENT_TYPE, HeaderName, RETRY_AFTER};
use tower::{Layer, Service};

#[cfg(feature = "redis")]
use super::config::RateLimitBackendFailure;
use super::config::{
    KeyStrategy, RateLimitBackend, RateLimitConfig, RateLimitTierConfig, TrustedProxiesConfig,
};
use super::trusted_proxies::ProxyResolver;

const X_RATELIMIT_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
const X_RATELIMIT_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
const X_RATELIMIT_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");

// ── Public extension types ────────────────────────────────────────────────────

/// Request extension inserted by auth middleware to identify the authenticated
/// principal for rate-limit keying.
///
/// Auth middleware that wants to participate in principal-keyed rate limiting
/// should insert this type into the request extensions before the rate limiter
/// runs. The value is typically the user ID or session principal ID.
///
/// ```rust,ignore
/// // In auth middleware:
/// req.extensions_mut().insert(RateLimitPrincipal(user_id.to_string()));
/// ```
///
/// When `key_strategy = "authenticated_principal"` and this extension is absent,
/// the limiter falls back to IP-based keying so unauthenticated callers are
/// never silently unbounded.
#[derive(Clone, Debug)]
pub struct RateLimitPrincipal(pub String);

/// Per-path rate limit override.
///
/// Apply a stricter or laxer limit on a specific URL prefix without disabling
/// the global rate limiter. Register via [`RateLimitLayer::with_path_override`].
///
/// `None` fields inherit from the global config.
///
/// ```rust,ignore
/// let layer = RateLimitLayer::from_config(&config)
///     .with_path_override("/api/free", RateLimitOverride { burst: Some(5), requests_per_second: Some(1.0) });
/// ```
#[derive(Clone, Debug)]
pub struct RateLimitOverride {
    /// Override `requests_per_second` for this path prefix. `None` uses the global value.
    pub requests_per_second: Option<f64>,
    /// Override `burst` for this path prefix. `None` uses the global value.
    pub burst: Option<u32>,
}

// ── Decision ──────────────────────────────────────────────────────────────────

/// Outcome of consuming one token from a bucket.
#[derive(Debug, Clone, Copy)]
enum Decision {
    Allowed {
        remaining: u32,
        /// Unix timestamp (seconds) of when the bucket will be available again.
        reset_at_unix: u64,
    },
    Denied {
        retry_after_secs: u64,
        /// Unix timestamp (seconds) of when the next token will be available.
        reset_at_unix: u64,
    },
}

// ── In-memory bucket store ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Per-key token bucket state stored in-process.
#[derive(Debug)]
struct MemoryStore {
    buckets: Arc<Mutex<LruCache<String, Bucket>>>,
}

impl Clone for MemoryStore {
    fn clone(&self) -> Self {
        Self {
            buckets: Arc::clone(&self.buckets),
        }
    }
}

impl MemoryStore {
    fn new() -> Self {
        Self {
            buckets: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(10_000).expect("10_000 is non-zero"),
            ))),
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn decide(&self, key: &str, now: Instant, burst: f64, refill_per_sec: f64) -> Decision {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let tokens_after = {
            let mut buckets = match self.buckets.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            let mut bucket = buckets.get(key).copied().unwrap_or(Bucket {
                tokens: burst,
                last_refill: now,
            });

            let elapsed = now
                .saturating_duration_since(bucket.last_refill)
                .as_secs_f64();
            bucket.tokens = elapsed.mul_add(refill_per_sec, bucket.tokens).min(burst);
            bucket.last_refill = now;

            let result = if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                Ok(bucket.tokens)
            } else {
                Err(bucket.tokens)
            };

            buckets.put(key.to_owned(), bucket);
            result
        };

        match tokens_after {
            Ok(remaining_tokens) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let remaining = remaining_tokens.floor() as u32;
                // When the bucket has fewer than one token left after consuming,
                // the next request will be denied. Compute the earliest future
                // time at which a new token will arrive so clients can back off.
                let reset_at_unix = if remaining_tokens < 1.0 {
                    let secs_to_next = ((1.0 - remaining_tokens) / refill_per_sec).ceil().max(1.0);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        now_unix + secs_to_next as u64
                    }
                } else {
                    now_unix
                };
                Decision::Allowed {
                    remaining,
                    reset_at_unix,
                }
            }
            Err(current_tokens) => {
                let deficit = 1.0 - current_tokens;
                let secs = (deficit / refill_per_sec).ceil().max(1.0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let retry_after_secs = secs as u64;
                Decision::Denied {
                    retry_after_secs,
                    reset_at_unix: now_unix + retry_after_secs,
                }
            }
        }
    }
}

// ── Redis bucket store ────────────────────────────────────────────────────────

#[cfg(feature = "redis")]
const RATE_LIMIT_LUA: &str = include_str!("rate_limit.lua");

#[cfg(feature = "redis")]
struct RedisStore {
    connection: redis::aio::ConnectionManager,
    key_prefix: String,
    failure_mode: RateLimitBackendFailure,
    /// Set to `true` once on the first Redis error; reset when it recovers.
    outage_logged: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "redis")]
impl Clone for RedisStore {
    fn clone(&self) -> Self {
        Self {
            connection: self.connection.clone(),
            key_prefix: self.key_prefix.clone(),
            failure_mode: self.failure_mode,
            outage_logged: Arc::clone(&self.outage_logged),
        }
    }
}

#[cfg(feature = "redis")]
impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("key_prefix", &self.key_prefix)
            .field("failure_mode", &self.failure_mode)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "redis")]
impl RedisStore {
    fn new(
        connection: redis::aio::ConnectionManager,
        key_prefix: String,
        failure_mode: RateLimitBackendFailure,
    ) -> Self {
        Self {
            connection,
            key_prefix,
            failure_mode,
            outage_logged: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    async fn decide(&self, key: &str, burst: f64, refill_per_sec: f64) -> Option<Decision> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let redis_key = format!("{}:{}", self.key_prefix, key);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        let script = redis::Script::new(RATE_LIMIT_LUA);
        let result: redis::RedisResult<Vec<i64>> = {
            let mut conn = self.connection.clone();
            script
                .key(&redis_key)
                .arg(burst)
                .arg(refill_per_sec)
                .arg(i64::try_from(now_ms).unwrap_or(i64::MAX))
                .invoke_async(&mut conn)
                .await
        };

        match result {
            Ok(values) if values.len() == 3 => {
                // Redis recovered after an outage — reset the flag so we warn again next time.
                self.outage_logged
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let allowed = values[0] == 1;
                if allowed {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let remaining = values[1] as u32;
                    Some(Decision::Allowed {
                        remaining,
                        reset_at_unix: now_unix,
                    })
                } else {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let retry_after_secs = values[2].max(1) as u64;
                    Some(Decision::Denied {
                        retry_after_secs,
                        reset_at_unix: now_unix + retry_after_secs,
                    })
                }
            }
            Err(err) => {
                // Emit one warning per outage, not per request.
                if !self
                    .outage_logged
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        error = %err,
                        key_prefix = %self.key_prefix,
                        "rate-limit Redis backend unavailable; \
                         switching to on_backend_failure posture"
                    );
                }
                match self.failure_mode {
                    RateLimitBackendFailure::FailOpen => None,
                    RateLimitBackendFailure::FailClosed => Some(Decision::Denied {
                        retry_after_secs: 1,
                        reset_at_unix: now_unix + 1,
                    }),
                }
            }
            Ok(values) => {
                // Unexpected script return shape — treat as backend error.
                if !self
                    .outage_logged
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        ?values,
                        key_prefix = %self.key_prefix,
                        "rate-limit Redis backend: unexpected script return value; \
                         switching to on_backend_failure posture"
                    );
                }
                match self.failure_mode {
                    RateLimitBackendFailure::FailOpen => None,
                    RateLimitBackendFailure::FailClosed => Some(Decision::Denied {
                        retry_after_secs: 1,
                        reset_at_unix: now_unix + 1,
                    }),
                }
            }
        }
    }
}

// ── Backend enum ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum BucketBackend {
    Memory(MemoryStore),
    #[cfg(feature = "redis")]
    Redis(RedisStore),
}

// ── Limiter (shared state) ────────────────────────────────────────────────────

#[allow(clippy::type_complexity)]
struct TierHookFn(Arc<dyn Fn(&str) -> Option<String> + Send + Sync>);

impl TierHookFn {
    fn call(&self, key: &str) -> Option<String> {
        (self.0)(key)
    }
}

impl Clone for TierHookFn {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl std::fmt::Debug for TierHookFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TierHookFn")
    }
}

/// Shared rate limiter state.
#[derive(Debug)]
struct Limiter {
    refill_per_sec: f64,
    burst: f64,
    burst_header: HeaderValue,
    resolver: Arc<ProxyResolver>,
    key_strategy: KeyStrategy,
    tiers: Arc<std::collections::HashMap<String, RateLimitTierConfig>>,
    tier_hook: Option<TierHookFn>,
    path_overrides: Vec<(String, RateLimitOverride)>,
    backend: BucketBackend,
    /// Whether this limiter honors the [`RateLimitExempt`] request marker. Only
    /// the framework's own default limiter sets this: it shares a bucket with the
    /// MCP `/mcp` envelope limiter, so a `tools/call` it already counted there
    /// must not be charged twice in the dispatch pipeline. User-installed
    /// limiters (e.g. an `AppBuilder::layer(RateLimitLayer::with_path_override(..))`)
    /// leave it `false` so an MCP replay still consumes their route-specific
    /// bucket, exactly as the equivalent direct HTTP call would.
    honors_mcp_exempt: bool,
}

impl Limiter {
    fn from_config(config: &RateLimitConfig) -> Self {
        Self::from_config_with_resolver(config, None)
    }

    fn from_config_with_resolver(
        config: &RateLimitConfig,
        top_level_resolver: Option<ProxyResolver>,
    ) -> Self {
        let burst = f64::from(config.burst.max(1));
        let refill_per_sec = config.requests_per_second.max(f64::MIN_POSITIVE);
        let burst_header = HeaderValue::from(config.burst.max(1));

        // Emit deprecation warning when the rate-limit-scoped proxy fields are
        // in use but the caller didn't supply a top-level resolver.
        if top_level_resolver.is_none()
            && (config.trust_forwarded_headers || !config.trusted_proxies.is_empty())
        {
            tracing::warn!(
                "security.rate_limit.trust_forwarded_headers and \
                 security.rate_limit.trusted_proxies are deprecated. \
                 Configure [security.trusted_proxies] instead and the \
                 rate limiter will use the shared policy automatically."
            );
        }

        // Prefer the top-level resolver when provided; otherwise build one
        // from the legacy rate-limit-scoped config so existing deployments
        // continue to work without changes.
        let resolver = top_level_resolver.unwrap_or_else(|| {
            ProxyResolver::from_config(&TrustedProxiesConfig {
                ranges: config.trusted_proxies.clone(),
                trusted_hops: None,
                trust_forwarded_headers: config.trust_forwarded_headers,
            })
        });

        let backend = Self::build_backend(config);

        Self {
            refill_per_sec,
            burst,
            burst_header,
            resolver: Arc::new(resolver),
            key_strategy: config.key_strategy,
            tiers: Arc::new(config.tiers.clone()),
            tier_hook: None,
            path_overrides: Vec::new(),
            backend,
            honors_mcp_exempt: false,
        }
    }

    fn build_backend(config: &RateLimitConfig) -> BucketBackend {
        if config.backend == RateLimitBackend::Redis {
            #[cfg(not(feature = "redis"))]
            {
                tracing::warn!(
                    "rate-limit backend = \"redis\" requires the `redis` cargo feature; \
                     falling back to memory. Enable the feature or set backend = \"memory\"."
                );
                return BucketBackend::Memory(MemoryStore::new());
            }
        }
        #[cfg(feature = "redis")]
        if config.backend == RateLimitBackend::Redis {
            if let Some(url) = config.redis.url.as_deref().filter(|u| !u.trim().is_empty()) {
                match redis::Client::open(url) {
                    Ok(client) => {
                        match redis::aio::ConnectionManager::new_lazy_with_config(
                            client,
                            redis::aio::ConnectionManagerConfig::new(),
                        ) {
                            Ok(conn) => {
                                return BucketBackend::Redis(RedisStore::new(
                                    conn,
                                    config.redis.key_prefix.clone(),
                                    config.on_backend_failure,
                                ));
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    url = %url,
                                    "rate-limit Redis backend: failed to create \
                                     connection manager; falling back to memory"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            url = %url,
                            "rate-limit Redis backend: invalid Redis URL; \
                             falling back to memory"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    "rate-limit backend = \"redis\" but no redis.url configured; \
                     falling back to memory"
                );
            }
        }
        BucketBackend::Memory(MemoryStore::new())
    }

    /// Resolve explicit path-override params for this request.
    ///
    /// Returns `(opt_burst, opt_rps, key_namespace)` where the `Option` values
    /// are `Some` only when the override explicitly sets that field. `key_namespace`
    /// is non-empty when a path override matched (used to isolate buckets from the
    /// global default pool).
    fn effective_params_with_ns<B>(&self, req: &Request<B>) -> (Option<f64>, Option<f64>, &str) {
        let path = req.uri().path();
        for (prefix, override_) in &self.path_overrides {
            if path.starts_with(prefix.as_str()) {
                let burst = override_.burst.map(|b| f64::from(b.max(1)));
                let rps = override_
                    .requests_per_second
                    .map(|r| r.max(f64::MIN_POSITIVE));
                return (burst, rps, prefix.as_str());
            }
        }
        (None, None, "")
    }

    /// Resolve the bucket key and effective tier params for a request.
    ///
    /// Returns `(bucket_key, burst, rps)` or `None` if rate limiting should
    /// be bypassed for this request (in-process caller with no identifiable peer).
    fn resolve_key_and_params<B>(&self, req: &Request<B>) -> Option<(String, f64, f64)> {
        let (opt_burst, opt_rps, key_ns) = self.effective_params_with_ns(req);

        let raw_key = self.extract_key(req)?;

        // Namespace the bucket key by the active path prefix so that different
        // path overrides get independent token buckets (avoids burst-value
        // collision when /strict and /normal share the same client IP).
        let key = if key_ns.is_empty() {
            raw_key.clone()
        } else {
            format!("{key_ns}\0{raw_key}")
        };

        let mut burst = opt_burst.unwrap_or(self.burst);
        let mut rps = opt_rps.unwrap_or(self.refill_per_sec);

        // Apply tier hook if configured. Pass the raw (un-prefixed) value so
        // hooks receive e.g. "user-42" rather than "principal:user-42".
        // Path overrides always take precedence over tier limits when explicitly set.
        if let Some(hook) = &self.tier_hook {
            let value = strip_key_prefix(&raw_key);
            if let Some(tier_name) = hook.call(value)
                && let Some(tier) = self.tiers.get(&tier_name)
            {
                if opt_burst.is_none() {
                    burst = f64::from(tier.burst.max(1));
                }
                if opt_rps.is_none() {
                    rps = tier.requests_per_second.max(f64::MIN_POSITIVE);
                }
            }
        }

        Some((key, burst, rps))
    }

    /// Extract the bucket key based on the configured strategy.
    fn extract_key<B>(&self, req: &Request<B>) -> Option<String> {
        match self.key_strategy {
            KeyStrategy::Ip => self.client_ip(req),
            KeyStrategy::ApiToken => {
                let token = extract_bearer_token(req);
                if token.is_some() {
                    token.map(|t| format!("token:{t}"))
                } else {
                    self.client_ip(req)
                }
            }
            KeyStrategy::AuthenticatedPrincipal => {
                let principal = req
                    .extensions()
                    .get::<RateLimitPrincipal>()
                    .map(|p| format!("principal:{}", p.0));
                if principal.is_some() {
                    principal
                } else {
                    self.client_ip(req)
                }
            }
        }
    }

    /// Consume one token for `key`. Returns `None` when throttling must be bypassed.
    #[allow(clippy::unused_async)]
    async fn decide(&self, key: &str, burst: f64, rps: f64) -> Option<Decision> {
        match &self.backend {
            BucketBackend::Memory(store) => Some(store.decide(key, Instant::now(), burst, rps)),
            #[cfg(feature = "redis")]
            BucketBackend::Redis(store) => store.decide(key, burst, rps).await,
        }
    }
}

/// Strip the known scheme prefix from an extracted bucket key before passing
/// to the tier hook, so hooks receive the raw value (e.g. "user-42" rather
/// than "principal:user-42"). Only removes `"token:"` and `"principal:"`
/// — bare IP keys (including IPv6 addresses containing colons) are passed through unchanged.
fn strip_key_prefix(key: &str) -> &str {
    key.strip_prefix("token:")
        .or_else(|| key.strip_prefix("principal:"))
        .unwrap_or(key)
}

/// Extract the key string from the `Authorization: Bearer <token>` header.
fn extract_bearer_token<B>(req: &Request<B>) -> Option<String> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let mut parts = s.splitn(2, ' ');
            let scheme = parts.next()?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return None;
            }
            parts.next().map(|t| t.trim().to_owned())
        })
        .filter(|t| !t.is_empty())
}

impl Limiter {
    fn client_ip<B>(&self, req: &Request<B>) -> Option<String> {
        self.resolver
            .resolve_client_addr(req)
            .map(|ip| ip.to_string())
    }
}

// ── Response building helpers ─────────────────────────────────────────────────

/// Build the `application/problem+json` 429 body per RFC 9457.
fn rate_limit_problem_json(key_class: &str) -> String {
    use crate::error::problem_details_json_string;
    use axum::http::StatusCode;

    problem_details_json_string(
        StatusCode::TOO_MANY_REQUESTS,
        format!("Rate limit exceeded for {key_class}"),
        None,
        Some("https://autumn.dev/problems/rate-limited"),
        None,
        None,
        true,
    )
}

/// Classify the bucket key for user-facing error messages without leaking the value.
fn key_class_label(key: &str) -> &'static str {
    if key.starts_with("token:") {
        "api token"
    } else if key.starts_with("principal:") {
        "authenticated principal"
    } else {
        "ip"
    }
}

// ── Tower layer & service ─────────────────────────────────────────────────────

/// Tower [`Layer`] that applies rate limiting.
///
/// Applied automatically when `security.rate_limit.enabled = true`.
///
/// # Per-principal / API-token keying
///
/// Configure `key_strategy` in `autumn.toml` or set it programmatically:
///
/// ```rust,ignore
/// let layer = RateLimitLayer::from_config(&config.security.rate_limit);
/// ```
///
/// # Tiered quotas
///
/// Register a tier-assignment hook after creating the layer:
///
/// ```rust,ignore
/// let layer = RateLimitLayer::from_config(&config.security.rate_limit)
///     .with_tier_hook(|principal_id| match db.get_plan(principal_id) {
///         "pro" => Some("pro".into()),
///         _ => None,
///     });
/// ```
///
/// # Per-path overrides
///
/// ```rust,ignore
/// let layer = RateLimitLayer::from_config(&config.security.rate_limit)
///     .with_path_override("/api/free/", RateLimitOverride { burst: Some(5), requests_per_second: Some(1.0) });
/// ```
/// Request-extension marker that exempts a request from rate limiting.
///
/// Set on requests that have already been counted by an upstream limiter so the
/// pipeline's limiter doesn't double-count them. The MCP endpoint uses this:
/// it counts a `tools/call` once at its `/mcp` envelope, then replays the
/// request through the full dispatch pipeline (which carries its own
/// [`RateLimitLayer`]); the marker keeps that replay from consuming a second
/// token. It is only ever set internally — external requests cannot carry it,
/// since extensions are not derived from headers.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitExempt;

#[derive(Clone, Debug)]
pub struct RateLimitLayer {
    limiter: Arc<Limiter>,
}

impl RateLimitLayer {
    /// Create a new rate limit layer from configuration.
    #[must_use]
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self {
            limiter: Arc::new(Limiter::from_config(config)),
        }
    }

    /// Override the proxy resolver with a pre-built [`ProxyResolver`].
    ///
    /// Use this to wire the top-level `[security.trusted_proxies]` policy into
    /// the rate limiter so all middleware share a single trust boundary.
    ///
    /// ```rust,ignore
    /// let resolver = ProxyResolver::from_config(&security_config.trusted_proxies);
    /// let layer = RateLimitLayer::from_config(&security_config.rate_limit)
    ///     .with_proxy_resolver(resolver);
    /// ```
    #[must_use]
    pub fn with_proxy_resolver(self, resolver: ProxyResolver) -> Self {
        let mut limiter = Arc::try_unwrap(self.limiter).unwrap_or_else(|arc| (*arc).deep_clone());
        limiter.resolver = Arc::new(resolver);
        Self {
            limiter: Arc::new(limiter),
        }
    }

    /// Register an app-supplied tier-assignment hook.
    ///
    /// The hook receives the extracted bucket key (principal ID or token, without
    /// the scheme prefix) and returns a tier name that matches a key in
    /// `[security.rate_limit.tiers]`. Returning `None` uses the top-level
    /// `requests_per_second` / `burst` defaults.
    ///
    /// The hook is called synchronously on every request; keep it O(1).
    ///
    /// ```rust,ignore
    /// let layer = RateLimitLayer::from_config(&config)
    ///     .with_tier_hook(|key| {
    ///         if key.starts_with("pro_") { Some("pro".into()) } else { None }
    ///     });
    /// ```
    #[must_use]
    pub fn with_tier_hook<F>(self, hook: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        let mut limiter = Arc::try_unwrap(self.limiter).unwrap_or_else(|arc| (*arc).deep_clone());
        limiter.tier_hook = Some(TierHookFn(Arc::new(hook)));
        Self {
            limiter: Arc::new(limiter),
        }
    }

    /// Register a per-path rate limit override.
    ///
    /// Requests whose URL path starts with `path_prefix` use the override's
    /// `burst` / `requests_per_second` values instead of the global defaults.
    /// The first matching prefix wins (registration order matters).
    ///
    /// ```rust,ignore
    /// let layer = RateLimitLayer::from_config(&config)
    ///     .with_path_override("/api/strict/", RateLimitOverride {
    ///         burst: Some(1),
    ///         requests_per_second: Some(0.1),
    ///     });
    /// ```
    #[must_use]
    pub fn with_path_override(
        self,
        path_prefix: impl Into<String>,
        override_: RateLimitOverride,
    ) -> Self {
        let mut limiter = Arc::try_unwrap(self.limiter).unwrap_or_else(|arc| (*arc).deep_clone());
        limiter.path_overrides.push((path_prefix.into(), override_));
        Self {
            limiter: Arc::new(limiter),
        }
    }

    /// Mark this as the framework's default limiter, which shares a bucket with
    /// the MCP `/mcp` envelope limiter and therefore honors the
    /// [`RateLimitExempt`] marker (so an envelope-counted `tools/call` isn't
    /// charged a second time on replay). Framework-internal: user-built limiters
    /// must not set this, or MCP replays would skip their per-route buckets.
    #[must_use]
    pub(crate) fn honoring_mcp_exempt(self) -> Self {
        let mut limiter = Arc::try_unwrap(self.limiter).unwrap_or_else(|arc| (*arc).deep_clone());
        limiter.honors_mcp_exempt = true;
        Self {
            limiter: Arc::new(limiter),
        }
    }
}

impl Limiter {
    /// Deep-clone for builder mutation (used when `Arc::try_unwrap` fails).
    fn deep_clone(&self) -> Self {
        Self {
            refill_per_sec: self.refill_per_sec,
            burst: self.burst,
            burst_header: self.burst_header.clone(),
            resolver: Arc::clone(&self.resolver),
            key_strategy: self.key_strategy,
            tiers: Arc::clone(&self.tiers),
            tier_hook: self.tier_hook.clone(),
            path_overrides: self.path_overrides.clone(),
            backend: self.backend.clone(),
            honors_mcp_exempt: self.honors_mcp_exempt,
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limiter: Arc::clone(&self.limiter),
        }
    }
}

/// Tower [`Service`] produced by [`RateLimitLayer`].
#[derive(Clone, Debug)]
pub struct RateLimitService<S> {
    inner: S,
    limiter: Arc<Limiter>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RateLimitService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Default + From<String> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        // A request already counted upstream (e.g. an MCP `tools/call` charged
        // at the `/mcp` envelope and replayed through this pipeline) bypasses
        // the limiter so it isn't double-counted — but only this framework
        // default limiter, which shares the envelope's bucket, honors the
        // marker. A user-installed limiter (e.g. a per-path override added via
        // `AppBuilder::layer`) leaves `honors_mcp_exempt` false so the replay
        // still consumes its route-specific bucket, exactly as a direct call.
        if self.limiter.honors_mcp_exempt && req.extensions().get::<RateLimitExempt>().is_some() {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }
        // Resolve key and effective params synchronously (no I/O).
        let resolved = self.limiter.resolve_key_and_params(&req);

        let limiter = Arc::clone(&self.limiter);
        let mut inner = self.inner.clone();
        // Swap to ensure correct poll_ready semantics.
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            let decision = match resolved {
                Some((ref key, burst, rps)) => limiter.decide(key, burst, rps).await,
                None => None,
            };

            let burst_for_header = resolved.as_ref().map_or_else(
                || limiter.burst_header.clone(),
                |(_, burst, _)| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let b = *burst as u32;
                    HeaderValue::from(b)
                },
            );

            match decision {
                Some(Decision::Denied {
                    retry_after_secs,
                    reset_at_unix,
                }) => {
                    let key_class = resolved
                        .as_ref()
                        .map_or("ip", |(k, _, _)| key_class_label(k));
                    let body_json = rate_limit_problem_json(key_class);

                    let mut response = Response::new(ResBody::from(body_json));
                    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
                    let headers = response.headers_mut();
                    headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
                    headers.insert(X_RATELIMIT_LIMIT, burst_for_header);
                    headers.insert(X_RATELIMIT_REMAINING, HeaderValue::from_static("0"));
                    headers.insert(X_RATELIMIT_RESET, HeaderValue::from(reset_at_unix));
                    headers.insert(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/problem+json"),
                    );
                    Ok(response)
                }
                Some(Decision::Allowed {
                    remaining,
                    reset_at_unix,
                }) => {
                    let mut response = inner.call(req).await?;
                    let headers = response.headers_mut();
                    headers.insert(X_RATELIMIT_LIMIT, burst_for_header);
                    headers.insert(X_RATELIMIT_REMAINING, HeaderValue::from(remaining));
                    headers.insert(X_RATELIMIT_RESET, HeaderValue::from(reset_at_unix));
                    Ok(response)
                }
                None => inner.call(req).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::routing::get;
    use std::net::{IpAddr, SocketAddr};
    use std::time::Duration;
    use tower::ServiceExt;

    fn cfg(enabled: bool, rps: f64, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            enabled,
            requests_per_second: rps,
            burst,
            // Tests exercise the key-by-IP path via X-Forwarded-For so
            // they don't need a real TCP listener.
            trust_forwarded_headers: true,
            trusted_proxies: Vec::new(),
            ..Default::default()
        }
    }

    fn app(config: &RateLimitConfig) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(RateLimitLayer::from_config(config))
    }

    fn req_with_ip(ip: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri("/")
            .header("X-Forwarded-For", ip)
            .body(Body::empty())
            .expect("infallible response builder")
    }

    fn limiter(trust: bool) -> Limiter {
        Limiter::from_config(&RateLimitConfig {
            enabled: true,
            requests_per_second: 10.0,
            burst: 5,
            trust_forwarded_headers: trust,
            trusted_proxies: Vec::new(),
            ..Default::default()
        })
    }

    fn limiter_with_trusted_proxies(proxies: &[&str]) -> Limiter {
        Limiter::from_config(&RateLimitConfig {
            enabled: true,
            requests_per_second: 10.0,
            burst: 5,
            trust_forwarded_headers: true,
            trusted_proxies: proxies.iter().map(|proxy| (*proxy).to_owned()).collect(),
            ..Default::default()
        })
    }

    fn req_with_connect_info(xff: &str, peer: &str) -> Request<()> {
        let mut req: Request<()> = Request::builder()
            .header("X-Forwarded-For", xff)
            .body(())
            .expect("infallible response builder");
        let addr: SocketAddr = peer.parse().expect("test peer socket address parses");
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn requests_under_limit_pass() {
        let app = app(&cfg(true, 1.0, 5));
        for _ in 0..5 {
            let response = app
                .clone()
                .oneshot(req_with_ip("1.1.1.1"))
                .await
                .expect("infallible response builder");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get("x-ratelimit-limit").is_some());
        }
    }

    #[tokio::test]
    async fn request_over_limit_returns_429_with_retry_after() {
        let app = app(&cfg(true, 1.0, 2));

        // Burn through the burst.
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(req_with_ip("2.2.2.2"))
                .await
                .expect("infallible response builder");
            assert_eq!(response.status(), StatusCode::OK);
        }

        // Next request is over the limit.
        let response = app
            .clone()
            .oneshot(req_with_ip("2.2.2.2"))
            .await
            .expect("infallible response builder");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .expect("Retry-After header present")
            .to_str()
            .expect("infallible response builder")
            .parse::<u64>()
            .expect("Retry-After parses as integer seconds");
        assert!(retry_after >= 1);

        assert_eq!(
            response
                .headers()
                .get("x-ratelimit-remaining")
                .expect("infallible response builder")
                .to_str()
                .expect("infallible response builder"),
            "0"
        );
    }

    #[tokio::test]
    async fn request_429_has_problem_details_body() {
        let app = app(&cfg(true, 1.0, 1));

        let _ = app.clone().oneshot(req_with_ip("9.9.9.9")).await.unwrap();
        let response = app.clone().oneshot(req_with_ip("9.9.9.9")).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/problem+json"),
            "content-type should be application/problem+json, got {ct}"
        );

        let reset = response.headers().get("x-ratelimit-reset");
        assert!(reset.is_some(), "x-ratelimit-reset must be present on 429");
    }

    #[tokio::test]
    async fn request_ok_has_ratelimit_reset_header() {
        let app = app(&cfg(true, 10.0, 5));
        let response = app.clone().oneshot(req_with_ip("8.8.8.8")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("x-ratelimit-reset").is_some(),
            "x-ratelimit-reset must be present on allowed responses"
        );
    }

    #[tokio::test]
    async fn different_ips_are_independent() {
        let app = app(&cfg(true, 0.1, 1));

        // Exhaust IP A.
        let ok_a = app
            .clone()
            .oneshot(req_with_ip("10.0.0.1"))
            .await
            .expect("infallible response builder");
        assert_eq!(ok_a.status(), StatusCode::OK);
        let blocked_a = app
            .clone()
            .oneshot(req_with_ip("10.0.0.1"))
            .await
            .expect("infallible response builder");
        assert_eq!(blocked_a.status(), StatusCode::TOO_MANY_REQUESTS);

        // IP B still has a full bucket.
        let ok_b = app
            .clone()
            .oneshot(req_with_ip("10.0.0.2"))
            .await
            .expect("infallible response builder");
        assert_eq!(ok_b.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tokens_refill_over_time() {
        let app = app(&cfg(true, 50.0, 1));

        let first = app
            .clone()
            .oneshot(req_with_ip("3.3.3.3"))
            .await
            .expect("infallible response builder");
        assert_eq!(first.status(), StatusCode::OK);
        let blocked = app
            .clone()
            .oneshot(req_with_ip("3.3.3.3"))
            .await
            .expect("infallible response builder");
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);

        // Wait long enough for one token to refill (50 rps -> 20ms per token).
        tokio::time::sleep(Duration::from_millis(80)).await;

        let after_refill = app
            .clone()
            .oneshot(req_with_ip("3.3.3.3"))
            .await
            .expect("infallible response builder");
        assert_eq!(after_refill.status(), StatusCode::OK);
    }

    #[test]
    fn build_backend_memory_config_returns_memory() {
        let config = RateLimitConfig {
            backend: RateLimitBackend::Memory,
            ..Default::default()
        };
        let backend = Limiter::build_backend(&config);
        assert!(matches!(backend, BucketBackend::Memory(_)));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn build_backend_redis_with_empty_url_falls_back_to_memory() {
        let config = RateLimitConfig {
            backend: RateLimitBackend::Redis,
            redis: super::super::config::RateLimitRedisConfig {
                url: Some("   ".to_string()),
                key_prefix: "test".to_string(),
            },
            ..Default::default()
        };
        let backend = Limiter::build_backend(&config);
        assert!(matches!(backend, BucketBackend::Memory(_)));
    }

    #[test]
    fn memory_store_retry_after_calculation() {
        let store = MemoryStore::new();
        let now = Instant::now();
        // Burst 1.0, Refill 0.1 tokens/sec (10 sec per token)
        let _ = store.decide("ip1", now, 1.0, 0.1); // Consumes 1.0, bucket.tokens = 0.0

        // Immediately after, bucket.tokens = 0.0. Deficit = 1.0. Secs = 1.0 / 0.1 = 10.0
        match store.decide("ip1", now, 1.0, 0.1) {
            Decision::Denied {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, 10),
            Decision::Allowed { .. } => panic!("Expected Denied"),
        }

        // 5 seconds later, bucket.tokens = 0.5. Deficit = 0.5. Secs = 0.5 / 0.1 = 5.0
        let later = now.checked_add(Duration::from_secs(5)).unwrap_or(now);
        match store.decide("ip1", later, 1.0, 0.1) {
            Decision::Denied {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, 5),
            Decision::Allowed { .. } => panic!("Expected Denied"),
        }

        // 9.5 seconds later, bucket.tokens = 0.95. Deficit = 0.05. Secs = 0.05 / 0.1 = 0.5 -> ceil -> 1.0
        let even_later = now.checked_add(Duration::from_millis(9500)).unwrap_or(now);
        match store.decide("ip1", even_later, 1.0, 0.1) {
            Decision::Denied {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, 1),
            Decision::Allowed { .. } => panic!("Expected Denied"),
        }
    }

    #[test]
    fn rate_limit_service_poll_ready() {
        use std::convert::Infallible;
        use tower::Service;
        let config = RateLimitConfig::default();
        let mut service = RateLimitLayer::from_config(&config).layer(tower::service_fn(
            |_req: Request<Body>| async { Ok::<_, Infallible>(Response::new(Body::empty())) },
        ));

        // Use a dummy context
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        let poll = service.poll_ready(&mut cx);
        assert!(poll.is_ready());
    }

    #[test]
    fn client_ip_uses_proxy_appended_entry_without_proxy_list() {
        let req = req_with_connect_info("attacker_spoofed_ip, 198.51.100.77", "203.0.113.10:4000");
        assert_eq!(
            limiter(true).client_ip(&req).as_deref(),
            Some("198.51.100.77")
        );
    }

    #[test]
    fn client_ip_prefers_x_forwarded_for_proxy_appended_entry_without_proxy_list() {
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "1.2.3.4, 5.6.7.8")
            .body(())
            .expect("infallible response builder");
        assert_eq!(limiter(true).client_ip(&req).as_deref(), Some("5.6.7.8"));
    }

    #[test]
    fn client_ip_uses_rightmost_forwarded_entry_without_configured_proxy_list() {
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "198.51.100.77, 203.0.113.10")
            .body(())
            .expect("infallible response builder");

        assert_eq!(
            limiter(true).client_ip(&req).as_deref(),
            Some("203.0.113.10")
        );
    }

    #[test]
    fn client_ip_skips_peer_self_append_without_configured_proxy_list() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10", "203.0.113.10:4000");

        assert_eq!(
            limiter(true).client_ip(&req).as_deref(),
            Some("198.51.100.77")
        );
    }

    #[test]
    fn client_ip_skips_configured_trusted_proxy_chain_entries() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10", "203.0.113.10:4000");
        assert_eq!(
            limiter_with_trusted_proxies(&["203.0.113.10"])
                .client_ip(&req)
                .as_deref(),
            Some("198.51.100.77")
        );
    }

    #[test]
    fn client_ip_skips_configured_trusted_proxy_cidr_entries() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10, 10.0.0.5", "10.0.0.5:4000");
        assert_eq!(
            limiter_with_trusted_proxies(&["203.0.113.0/24", "10.0.0.5"])
                .client_ip(&req)
                .as_deref(),
            Some("198.51.100.77")
        );
    }

    #[test]
    fn client_ip_accepts_forwarded_chain_from_configured_trusted_peer() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10", "10.0.0.5:4000");

        assert_eq!(
            limiter_with_trusted_proxies(&["10.0.0.5", "203.0.113.10"])
                .client_ip(&req)
                .as_deref(),
            Some("198.51.100.77")
        );
    }

    #[test]
    fn client_ip_ignores_forwarded_chain_from_untrusted_peer() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10", "192.0.2.44:4000");

        assert_eq!(
            limiter_with_trusted_proxies(&["203.0.113.10"])
                .client_ip(&req)
                .as_deref(),
            Some("192.0.2.44")
        );
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_without_peer_when_trusted_proxies_configured() {
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "198.51.100.77, 203.0.113.10")
            .header("X-Real-IP", "198.51.100.88")
            .body(())
            .expect("infallible response builder");

        assert!(
            limiter_with_trusted_proxies(&["203.0.113.10"])
                .client_ip(&req)
                .is_none()
        );
    }

    #[test]
    fn client_ip_falls_back_to_peer_when_all_trusted_proxies_are_invalid() {
        let req = req_with_connect_info("198.51.100.77, 203.0.113.10", "192.0.2.44:4000");

        assert_eq!(
            limiter_with_trusted_proxies(&["203.0.113.10:443", "198.51.100.0/999"])
                .client_ip(&req)
                .as_deref(),
            Some("192.0.2.44")
        );
    }

    #[test]
    fn client_ip_trims_whitespace_when_trusted() {
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "  9.9.9.9  ")
            .body(())
            .expect("infallible response builder");
        assert_eq!(limiter(true).client_ip(&req).as_deref(), Some("9.9.9.9"));
    }

    #[test]
    fn client_ip_falls_back_to_x_real_ip_when_trusted() {
        let req: Request<()> = Request::builder()
            .header("X-Real-IP", "7.7.7.7")
            .body(())
            .expect("infallible response builder");
        assert_eq!(limiter(true).client_ip(&req).as_deref(), Some("7.7.7.7"));
    }

    #[test]
    fn client_ip_falls_back_to_connect_info() {
        let mut req: Request<()> = Request::builder()
            .body(())
            .expect("infallible response builder");
        let addr: SocketAddr = "127.0.0.1:4242"
            .parse()
            .expect("infallible response builder");
        req.extensions_mut().insert(ConnectInfo(addr));
        assert_eq!(limiter(true).client_ip(&req).as_deref(), Some("127.0.0.1"));
        // Untrusted limiter also falls back, since headers are ignored.
        assert_eq!(limiter(false).client_ip(&req).as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn client_ip_none_when_no_source() {
        // In-process callers without ConnectInfo (SSG, tests) must be
        // bypassed, not collapsed onto a shared fallback bucket.
        let req: Request<()> = Request::builder()
            .body(())
            .expect("infallible response builder");
        assert!(limiter(true).client_ip(&req).is_none());
        assert!(limiter(false).client_ip(&req).is_none());
    }

    #[test]
    fn client_ip_empty_xff_falls_through_to_x_real_ip_when_trusted() {
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "  ")
            .header("X-Real-IP", "8.8.8.8")
            .body(())
            .expect("infallible response builder");
        // The XFF client entry is empty after trim, so we fall back to X-Real-IP.
        assert_eq!(limiter(true).client_ip(&req).as_deref(), Some("8.8.8.8"));
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_by_default() {
        // Attacker-supplied forwarding headers must not be trusted when
        // `trust_forwarded_headers = false`; the limiter keys on the
        // real peer address instead.
        let mut req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "1.2.3.4")
            .header("X-Real-IP", "5.6.7.8")
            .body(())
            .expect("infallible response builder");
        let addr: SocketAddr = "10.0.0.42:1111"
            .parse()
            .expect("infallible response builder");
        req.extensions_mut().insert(ConnectInfo(addr));
        assert_eq!(limiter(false).client_ip(&req).as_deref(), Some("10.0.0.42"));
    }

    #[tokio::test]
    async fn forwarded_headers_cannot_bypass_throttling_when_untrusted() {
        // `trust_forwarded_headers = false` (default). When ConnectInfo is
        // set (the production configuration), rotating XFF values must
        // NOT shard the throttle into separate buckets — the peer IP is
        // the sole key source.
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1,
            trust_forwarded_headers: false,
            trusted_proxies: Vec::new(),
            ..Default::default()
        };
        let app = app(&config);
        let peer: SocketAddr = "198.51.100.1:2000"
            .parse()
            .expect("infallible response builder");

        let make_req = |xff: &str| {
            let mut req = Request::builder()
                .method("GET")
                .uri("/")
                .header("X-Forwarded-For", xff)
                .body(Body::empty())
                .expect("infallible response builder");
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        };

        let first = app
            .clone()
            .oneshot(make_req("1.1.1.1"))
            .await
            .expect("infallible response builder");
        assert_eq!(first.status(), StatusCode::OK);
        // Different XFF value, but same peer → still throttled.
        let blocked = app
            .clone()
            .oneshot(make_req("2.2.2.2"))
            .await
            .expect("infallible response builder");
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn forwarded_header_chains_keep_independent_client_buckets() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1,
            trust_forwarded_headers: true,
            trusted_proxies: vec!["203.0.113.10".to_owned()],
            ..Default::default()
        };
        let app = app(&config);

        let req_with_chain = |client_ip: &str| {
            let mut req = Request::builder()
                .method("GET")
                .uri("/")
                .header("X-Forwarded-For", format!("{client_ip}, 203.0.113.10"))
                .body(Body::empty())
                .expect("infallible response builder");
            let peer: SocketAddr = "203.0.113.10:4000"
                .parse()
                .expect("test peer socket address parses");
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        };

        let first_a = app
            .clone()
            .oneshot(req_with_chain("198.51.100.77"))
            .await
            .expect("infallible response builder");
        assert_eq!(first_a.status(), StatusCode::OK);

        let blocked_a = app
            .clone()
            .oneshot(req_with_chain("198.51.100.77"))
            .await
            .expect("infallible response builder");
        assert_eq!(blocked_a.status(), StatusCode::TOO_MANY_REQUESTS);

        let first_b = app
            .clone()
            .oneshot(req_with_chain("198.51.100.88"))
            .await
            .expect("infallible response builder");
        assert_eq!(first_b.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn requests_without_connect_info_bypass_rate_limit() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.001,
            burst: 1,
            trust_forwarded_headers: false,
            trusted_proxies: Vec::new(),
            ..Default::default()
        };
        let app = app(&config);

        for _ in 0..10 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/")
                        .body(Body::empty())
                        .expect("infallible response builder"),
                )
                .await
                .expect("infallible response builder");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers().get("x-ratelimit-limit").is_none(),
                "bypassed requests should not carry rate-limit headers"
            );
        }
    }

    #[tokio::test]
    async fn requests_without_connect_info_bypass_when_trusted_proxies_configured() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.001,
            burst: 1,
            trust_forwarded_headers: true,
            trusted_proxies: vec!["203.0.113.10".to_owned()],
            ..Default::default()
        };
        let app = app(&config);

        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/")
                        .header("X-Forwarded-For", "198.51.100.77, 203.0.113.10")
                        .header("X-Real-IP", "198.51.100.88")
                        .body(Body::empty())
                        .expect("infallible response builder"),
                )
                .await
                .expect("infallible response builder");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers().get("x-ratelimit-limit").is_none(),
                "requests with configured trusted proxies but no peer must not trust forwarded headers"
            );
        }
    }

    #[tokio::test]
    async fn invalid_trusted_proxies_do_not_reopen_forwarded_header_trust() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.001,
            burst: 1,
            trust_forwarded_headers: true,
            trusted_proxies: vec!["203.0.113.10:443".to_owned(), "198.51.100.0/999".to_owned()],
            ..Default::default()
        };
        let app = app(&config);
        let peer: SocketAddr = "192.0.2.44:4000"
            .parse()
            .expect("test peer socket address parses");

        let make_req = |xff: &str| {
            let mut req = Request::builder()
                .method("GET")
                .uri("/")
                .header("X-Forwarded-For", xff)
                .body(Body::empty())
                .expect("infallible response builder");
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        };

        let first = app
            .clone()
            .oneshot(make_req("198.51.100.77"))
            .await
            .expect("infallible response builder");
        assert_eq!(first.status(), StatusCode::OK);

        let blocked = app
            .clone()
            .oneshot(make_req("198.51.100.88"))
            .await
            .expect("infallible response builder");
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    // ── Key strategy unit tests ───────────────────────────────────────────────

    #[test]
    fn extract_bearer_token_parses_authorization_header() {
        let req = Request::builder()
            .header("authorization", "Bearer my-secret-token")
            .body(())
            .unwrap();
        assert_eq!(
            extract_bearer_token(&req).as_deref(),
            Some("my-secret-token")
        );
    }

    #[test]
    fn extract_bearer_token_case_insensitive_scheme() {
        let req = Request::builder()
            .header("authorization", "BEARER token123")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer_token(&req).as_deref(), Some("token123"));
    }

    #[test]
    fn extract_bearer_token_returns_none_for_non_bearer() {
        let req = Request::builder()
            .header("authorization", "Basic dXNlcjpwYXNz")
            .body(())
            .unwrap();
        assert!(extract_bearer_token(&req).is_none());
    }

    #[test]
    fn extract_bearer_token_returns_none_when_absent() {
        let req = Request::builder().body(()).unwrap();
        assert!(extract_bearer_token(&req).is_none());
    }

    #[test]
    fn key_class_label_ip() {
        assert_eq!(key_class_label("1.2.3.4"), "ip");
    }

    #[test]
    fn key_class_label_token() {
        assert_eq!(key_class_label("token:abc123"), "api token");
    }

    #[test]
    fn key_class_label_principal() {
        assert_eq!(
            key_class_label("principal:user-42"),
            "authenticated principal"
        );
    }

    #[test]
    fn key_strategy_extract_api_token_with_header() {
        let config = RateLimitConfig {
            key_strategy: KeyStrategy::ApiToken,
            trust_forwarded_headers: true,
            ..Default::default()
        };
        let limiter = Limiter::from_config(&config);
        let req = Request::builder()
            .header("authorization", "Bearer tok-abc")
            .header("X-Forwarded-For", "1.2.3.4")
            .body(())
            .unwrap();
        let key = limiter.extract_key(&req).unwrap();
        assert_eq!(key, "token:tok-abc");
    }

    #[test]
    fn key_strategy_principal_uses_extension() {
        let config = RateLimitConfig {
            key_strategy: KeyStrategy::AuthenticatedPrincipal,
            trust_forwarded_headers: true,
            ..Default::default()
        };
        let limiter = Limiter::from_config(&config);
        let mut req: Request<()> = Request::builder().body(()).unwrap();
        req.extensions_mut()
            .insert(RateLimitPrincipal("user-99".to_owned()));
        let key = limiter.extract_key(&req).unwrap();
        assert_eq!(key, "principal:user-99");
    }

    #[test]
    fn key_strategy_principal_falls_back_to_ip() {
        let config = RateLimitConfig {
            key_strategy: KeyStrategy::AuthenticatedPrincipal,
            trust_forwarded_headers: true,
            ..Default::default()
        };
        let limiter = Limiter::from_config(&config);
        let req: Request<()> = Request::builder()
            .header("X-Forwarded-For", "5.5.5.5")
            .body(())
            .unwrap();
        // No RateLimitPrincipal extension → falls back to IP.
        let key = limiter.extract_key(&req).unwrap();
        assert_eq!(key, "5.5.5.5");
    }

    // ── Redis backend build_backend fallback tests ────────────────────────────

    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn redis_store_debug_format() {
        use super::super::config::RateLimitBackendFailure;
        let client = redis::Client::open("redis://127.0.0.1/").unwrap();
        let connection = redis::aio::ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .unwrap();
        let store = RedisStore::new(
            connection,
            "test_prefix".to_string(),
            RateLimitBackendFailure::FailOpen,
        );
        let dbg = format!("{store:?}");
        assert!(dbg.contains("RedisStore"));
        assert!(dbg.contains("key_prefix"));
        assert!(dbg.contains("test_prefix"));
        assert!(dbg.contains("failure_mode"));
        assert!(dbg.contains("FailOpen"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn build_backend_falls_back_to_memory_when_redis_url_missing() {
        use super::super::config::{
            RateLimitBackend, RateLimitBackendFailure, RateLimitRedisConfig,
        };
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10.0,
            burst: 5,
            trust_forwarded_headers: false,
            trusted_proxies: Vec::new(),
            backend: RateLimitBackend::Redis,
            redis: RateLimitRedisConfig {
                url: None,
                key_prefix: "test:rl".to_owned(),
            },
            on_backend_failure: RateLimitBackendFailure::FailOpen,
            ..Default::default()
        };
        let limiter = Limiter::from_config(&config);
        assert!(matches!(limiter.backend, BucketBackend::Memory(_)));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn build_backend_falls_back_to_memory_for_invalid_redis_url() {
        use super::super::config::{
            RateLimitBackend, RateLimitBackendFailure, RateLimitRedisConfig,
        };
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10.0,
            burst: 5,
            trust_forwarded_headers: false,
            trusted_proxies: Vec::new(),
            backend: RateLimitBackend::Redis,
            redis: RateLimitRedisConfig {
                url: Some("not_a_valid_redis_url://???".to_owned()),
                key_prefix: "test:rl".to_owned(),
            },
            on_backend_failure: RateLimitBackendFailure::FailClosed,
            ..Default::default()
        };
        let limiter = Limiter::from_config(&config);
        assert!(matches!(limiter.backend, BucketBackend::Memory(_)));
    }

    #[test]
    fn is_trusted_proxy_returns_false_for_untrusted_ip() {
        use crate::security::config::TrustedProxiesConfig;
        use crate::security::trusted_proxies::ProxyResolver;

        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["10.0.0.0/8".to_string()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        let untrusted_ip: IpAddr = "192.168.1.1".parse().unwrap();
        // Build a request with this peer IP and verify XFF is NOT trusted.
        let peer_addr: std::net::SocketAddr = format!("{untrusted_ip}:1234").parse().unwrap();
        let mut req: axum::http::Request<()> = axum::http::Request::builder()
            .header("x-forwarded-for", "10.0.0.1")
            .body(())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(peer_addr));
        // Untrusted peer: should return peer IP, not XFF value.
        let client_ip = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(client_ip, untrusted_ip);
    }

    // ── Path override tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn path_override_applies_stricter_burst() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 5, // global: 5
            trust_forwarded_headers: true,
            ..Default::default()
        };

        let layer = RateLimitLayer::from_config(&config).with_path_override(
            "/strict",
            RateLimitOverride {
                burst: Some(1), // override: 1
                requests_per_second: None,
            },
        );

        let app = Router::new()
            .route("/strict", get(|| async { "strict" }))
            .route("/normal", get(|| async { "normal" }))
            .layer(layer);

        let strict_req = || {
            Request::builder()
                .method("GET")
                .uri("/strict")
                .header("X-Forwarded-For", "2.2.2.2")
                .body(Body::empty())
                .unwrap()
        };
        let normal_req = || {
            Request::builder()
                .method("GET")
                .uri("/normal")
                .header("X-Forwarded-For", "2.2.2.2")
                .body(Body::empty())
                .unwrap()
        };

        // /strict: 1 allowed, then denied.
        let r = app.clone().oneshot(strict_req()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app.clone().oneshot(strict_req()).await.unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        // /normal: still uses global burst=5, should pass.
        for _ in 0..3 {
            let r = app.clone().oneshot(normal_req()).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    }

    // ── RateLimitExempt scoping (MCP replay) tests ─────────────────────────────

    fn exempt_req(uri: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("X-Forwarded-For", "2.2.2.2")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(RateLimitExempt);
        req
    }

    #[tokio::test]
    async fn exempt_marker_honored_only_by_framework_default_limiter() {
        // The framework's default limiter shares the MCP envelope's bucket, so a
        // `RateLimitExempt` replay must bypass it (no double-count).
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1,
            trust_forwarded_headers: true,
            ..Default::default()
        };
        let layer = RateLimitLayer::from_config(&config).honoring_mcp_exempt();
        let app = Router::new()
            .route("/api/strict", get(|| async { "ok" }))
            .layer(layer);

        // Even past the burst of 1, every exempt request passes.
        for _ in 0..3 {
            let r = app
                .clone()
                .oneshot(exempt_req("/api/strict"))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn exempt_marker_ignored_by_user_path_override_limiter() {
        // A user-installed per-path override limiter does NOT share the envelope's
        // bucket, so an MCP `tools/call` replay must still consume its
        // route-specific bucket — the marker is ignored.
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 5,
            trust_forwarded_headers: true,
            ..Default::default()
        };
        let layer = RateLimitLayer::from_config(&config).with_path_override(
            "/api/strict",
            RateLimitOverride {
                burst: Some(1),
                requests_per_second: None,
            },
        );
        let app = Router::new()
            .route("/api/strict", get(|| async { "ok" }))
            .layer(layer);

        // Override burst is 1: the first exempt replay passes, the second is
        // denied despite carrying `RateLimitExempt`.
        let r = app
            .clone()
            .oneshot(exempt_req("/api/strict"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .clone()
            .oneshot(exempt_req("/api/strict"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    // ── Tier hook tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn tier_hook_assigns_correct_burst_to_tier() {
        use super::super::config::RateLimitTierConfig;
        use std::collections::HashMap;

        let mut tiers = HashMap::new();
        tiers.insert(
            "premium".to_owned(),
            RateLimitTierConfig {
                requests_per_second: 0.1,
                burst: 10,
            },
        );

        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 0.1,
            burst: 1, // default
            key_strategy: KeyStrategy::AuthenticatedPrincipal,
            tiers,
            ..Default::default()
        };

        let layer = RateLimitLayer::from_config(&config).with_tier_hook(|key| {
            // key is the raw value after stripping the scheme prefix.
            if key.starts_with("vip_") {
                Some("premium".to_owned())
            } else {
                None
            }
        });

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer);

        let make_req = |principal: &str| {
            let mut req = Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(RateLimitPrincipal(principal.to_owned()));
            req
        };

        // Premium user: burst=10.
        for i in 0..10 {
            let r = app.clone().oneshot(make_req("vip_user")).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK, "premium request {i} failed");
        }
        let r = app.clone().oneshot(make_req("vip_user")).await.unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        // Regular user: burst=1.
        let r = app.clone().oneshot(make_req("regular_user")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app.clone().oneshot(make_req("regular_user")).await.unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
