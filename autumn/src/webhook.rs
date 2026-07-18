//! Signed webhook intake for third-party callbacks.
//!
//! The [`SignedWebhook`] extractor verifies provider signatures against the
//! exact HTTP request bytes before handler code runs. Configure endpoints with
//! [`WebhookEndpointConfig`] under `security.webhooks.endpoints`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::FromRequest;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde::Deserialize;
use thiserror::Error;

pub use crate::security::config::hmac_sha256_hex;

const DEFAULT_TIMESTAMP_TOLERANCE_SECS: u64 = 300;
const DEFAULT_REPLAY_WINDOW_SECS: u64 = 24 * 60 * 60;
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
const IN_MEMORY_REPLAY_CLEANUP_INTERVAL: usize = 128;
const IN_MEMORY_REPLAY_CLEANUP_HIGH_WATER: usize = 16 * 1024;

/// Provider preset for signed webhook verification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookProvider {
    /// Stripe-style `Stripe-Signature: t=...,v1=...`.
    Stripe,
    /// GitHub-style `X-Hub-Signature-256: sha256=...`.
    Github,
    /// Slack-style `X-Slack-Signature: v0=...`.
    Slack,
    /// Generic HMAC-SHA256 over the raw request body.
    #[default]
    Generic,
}

impl WebhookProvider {
    /// Stable lower-case provider name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Github => "github",
            Self::Slack => "slack",
            Self::Generic => "generic",
        }
    }
}

impl std::fmt::Display for WebhookProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Signed webhook configuration section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebhookConfig {
    /// Replay protection backend used for all replay-protected endpoints.
    #[serde(default)]
    pub replay: WebhookReplayConfig,
    /// Configured signed webhook endpoints.
    #[serde(default)]
    pub endpoints: Vec<WebhookEndpointConfig>,
}

impl WebhookConfig {
    /// Apply environment-sourced webhook configuration.
    pub(crate) fn apply_env_overrides_with_env(&mut self, env: &dyn crate::config::Env) {
        self.replay.apply_env_overrides_with_env(env);
        self.resolve_secret_envs_with_env(env);
    }

    /// Resolve `secret_env` and `previous_secret_envs` entries from the
    /// configured environment.
    fn resolve_secret_envs_with_env(&mut self, env: &dyn crate::config::Env) {
        for endpoint in &mut self.endpoints {
            if endpoint.secret.is_none()
                && let Some(env_name) = endpoint.secret_env.as_deref()
                && let Ok(secret) = env.var(env_name)
            {
                endpoint.secret = Some(secret);
            }

            for env_name in &endpoint.previous_secret_envs {
                if let Ok(secret) = env.var(env_name)
                    && !secret.is_empty()
                {
                    endpoint.previous_secrets.push(secret);
                }
            }
        }
    }

    /// Validate configured signed webhook endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookConfigError`] when an endpoint has no usable secret,
    /// an invalid path, or a weak production secret.
    pub fn validate(&self, is_production: bool) -> Result<(), WebhookConfigError> {
        for endpoint in &self.endpoints {
            endpoint.validate(is_production)?;
        }
        validate_unique_endpoint_paths(&self.endpoints)?;
        if self
            .endpoints
            .iter()
            .any(|endpoint| endpoint.replay_protection)
        {
            self.replay.validate(is_production)?;
        }
        Ok(())
    }
}

/// Replay protection backend for signed webhooks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookReplayBackend {
    /// Process-local memory store. Suitable for tests, development, and
    /// explicitly acknowledged single-replica deployments.
    #[serde(alias = "local", alias = "in_memory")]
    #[default]
    Memory,
    /// Redis `SET NX EX` store shared by every application replica.
    Redis,
}

impl WebhookReplayBackend {
    fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" | "local" | "in_memory" | "in-memory" => Some(Self::Memory),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Replay protection storage configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookReplayConfig {
    /// Active replay backend.
    #[serde(default)]
    pub backend: WebhookReplayBackend,
    /// Explicit production escape hatch for single-replica deployments.
    #[serde(default)]
    pub allow_memory_in_production: bool,
    /// Redis backend options.
    #[serde(default)]
    pub redis: WebhookReplayRedisConfig,
}

impl Default for WebhookReplayConfig {
    fn default() -> Self {
        Self {
            backend: WebhookReplayBackend::Memory,
            allow_memory_in_production: false,
            redis: WebhookReplayRedisConfig::default(),
        }
    }
}

impl WebhookReplayConfig {
    fn apply_env_overrides_with_env(&mut self, env: &dyn crate::config::Env) {
        if let Ok(value) = env.var("AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND") {
            if let Some(backend) = WebhookReplayBackend::from_env_value(&value) {
                self.backend = backend;
            } else {
                eprintln!(
                    "Warning: AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND={value:?} is not valid \
                     (expected memory or redis), ignoring"
                );
            }
        }
        if let Ok(value) = env.var("AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION")
        {
            match value.trim().parse::<bool>() {
                Ok(value) => self.allow_memory_in_production = value,
                Err(error) => eprintln!(
                    "Warning: AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION \
                     could not be parsed as bool: {error}"
                ),
            }
        }
        if let Ok(value) = env.var("AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL") {
            let value = value.trim();
            self.redis.url = if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            };
        }
        if let Ok(value) = env.var("AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX")
            && !value.trim().is_empty()
        {
            self.redis.key_prefix = value;
        }
    }

    fn validate(&self, is_production: bool) -> Result<(), WebhookConfigError> {
        match self.backend {
            WebhookReplayBackend::Memory => {
                if is_production && !self.allow_memory_in_production {
                    return Err(WebhookConfigError::MemoryReplayInProduction);
                }
                Ok(())
            }
            WebhookReplayBackend::Redis => validate_redis_replay_config(&self.redis),
        }
    }
}

/// Redis replay protection backend configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookReplayRedisConfig {
    /// Redis connection URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Prefix for all replay keys stored in Redis.
    #[serde(default = "default_replay_redis_key_prefix")]
    pub key_prefix: String,
}

impl Default for WebhookReplayRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_replay_redis_key_prefix(),
        }
    }
}

/// Configuration for one signed webhook endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookEndpointConfig {
    /// Unique endpoint name used in diagnostics and replay keys.
    pub name: String,
    /// Exact route path protected by this config.
    pub path: String,
    /// Provider verification preset.
    #[serde(default)]
    pub provider: WebhookProvider,
    /// Current webhook signing secret.
    #[serde(default)]
    pub secret: Option<String>,
    /// Environment variable that provides the current secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Previous secrets accepted during a rotation grace window.
    #[serde(default)]
    pub previous_secrets: Vec<String>,
    /// Environment variables that provide previous rotation secrets.
    #[serde(default)]
    pub previous_secret_envs: Vec<String>,
    /// Maximum timestamp skew accepted for timestamped providers.
    #[serde(default = "default_timestamp_tolerance_secs")]
    pub timestamp_tolerance_secs: u64,
    /// Replay rejection window for duplicate delivery IDs.
    #[serde(default = "default_replay_window_secs")]
    pub replay_window_secs: u64,
    /// Whether duplicate delivery IDs are rejected.
    #[serde(default = "default_true")]
    pub replay_protection: bool,
    /// Header carrying the signature. Provider defaults are applied by
    /// constructors and by `Default`.
    #[serde(default)]
    pub signature_header: Option<String>,
    /// Optional prefix stripped from header signatures before comparison.
    #[serde(default)]
    pub signature_prefix: Option<String>,
    /// Optional header carrying a Unix timestamp.
    #[serde(default)]
    pub timestamp_header: Option<String>,
    /// Optional header carrying provider delivery ID.
    #[serde(default)]
    pub delivery_id_header: Option<String>,
    /// Optional header carrying provider event type.
    #[serde(default)]
    pub event_type_header: Option<String>,
    /// Maximum raw body bytes read by the extractor.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl Default for WebhookEndpointConfig {
    fn default() -> Self {
        Self::provider_defaults(WebhookProvider::Generic)
    }
}

impl WebhookEndpointConfig {
    /// Create a provider-shaped endpoint config with a current secret.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        path: impl Into<String>,
        provider: WebhookProvider,
        secret: impl Into<String>,
    ) -> Self {
        let mut config = Self::provider_defaults(provider);
        config.name = name.into();
        config.path = path.into();
        config.secret = Some(secret.into());
        config
    }

    /// Create a Stripe-style endpoint.
    #[must_use]
    pub fn stripe(
        name: impl Into<String>,
        path: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self::new(name, path, WebhookProvider::Stripe, secret)
    }

    /// Create a GitHub-style endpoint.
    #[must_use]
    pub fn github(
        name: impl Into<String>,
        path: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self::new(name, path, WebhookProvider::Github, secret)
    }

    /// Create a Slack-style endpoint.
    #[must_use]
    pub fn slack(
        name: impl Into<String>,
        path: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self::new(name, path, WebhookProvider::Slack, secret)
    }

    /// Create a generic HMAC-SHA256 endpoint.
    #[must_use]
    pub fn generic(
        name: impl Into<String>,
        path: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self::new(name, path, WebhookProvider::Generic, secret)
    }

    /// Accept one previous secret during rotation.
    #[must_use]
    pub fn with_previous_secret(mut self, secret: impl Into<String>) -> Self {
        self.previous_secrets.push(secret.into());
        self
    }

    /// Override timestamp tolerance.
    #[must_use]
    pub const fn with_timestamp_tolerance_secs(mut self, secs: u64) -> Self {
        self.timestamp_tolerance_secs = secs;
        self
    }

    /// Override replay rejection window.
    #[must_use]
    pub const fn with_replay_window_secs(mut self, secs: u64) -> Self {
        self.replay_window_secs = secs;
        self
    }

    /// Disable duplicate delivery rejection for this endpoint.
    #[must_use]
    pub const fn without_replay_protection(mut self) -> Self {
        self.replay_protection = false;
        self
    }

    fn provider_defaults(provider: WebhookProvider) -> Self {
        let mut config = Self {
            name: String::new(),
            path: String::new(),
            provider,
            secret: None,
            secret_env: None,
            previous_secrets: Vec::new(),
            previous_secret_envs: Vec::new(),
            timestamp_tolerance_secs: DEFAULT_TIMESTAMP_TOLERANCE_SECS,
            replay_window_secs: DEFAULT_REPLAY_WINDOW_SECS,
            replay_protection: true,
            signature_header: None,
            signature_prefix: None,
            timestamp_header: None,
            delivery_id_header: None,
            event_type_header: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        };

        match provider {
            WebhookProvider::Stripe => {
                config.signature_header = Some("Stripe-Signature".to_owned());
            }
            WebhookProvider::Github => {
                config.signature_header = Some("X-Hub-Signature-256".to_owned());
                config.signature_prefix = Some("sha256=".to_owned());
                config.delivery_id_header = Some("X-GitHub-Delivery".to_owned());
                config.event_type_header = Some("X-GitHub-Event".to_owned());
            }
            WebhookProvider::Slack => {
                config.signature_header = Some("X-Slack-Signature".to_owned());
                config.signature_prefix = Some("v0=".to_owned());
                config.timestamp_header = Some("X-Slack-Request-Timestamp".to_owned());
            }
            WebhookProvider::Generic => {
                config.signature_header = Some("X-Webhook-Signature".to_owned());
                config.signature_prefix = Some("sha256=".to_owned());
                config.delivery_id_header = Some("X-Webhook-Delivery".to_owned());
                config.event_type_header = Some("X-Webhook-Event".to_owned());
            }
        }

        config
    }

    fn validate(&self, is_production: bool) -> Result<(), WebhookConfigError> {
        if self.name.trim().is_empty() {
            return Err(WebhookConfigError::InvalidEndpoint {
                name: self.name.clone(),
                message: "name must not be empty".to_owned(),
            });
        }
        if !self.path.starts_with('/') || self.path.trim() == "/" || self.path.trim().is_empty() {
            return Err(WebhookConfigError::InvalidEndpoint {
                name: self.name.clone(),
                message: format!("path {:?} must start with '/' and not be root", self.path),
            });
        }
        let Some(secret) = self.secret.as_deref().filter(|value| !value.is_empty()) else {
            return Err(WebhookConfigError::MissingSecret {
                name: self.name.clone(),
                path: self.path.clone(),
            });
        };

        if is_production {
            crate::security::config::validate_signing_secret(Some(secret), true).map_err(
                |reason| WebhookConfigError::InvalidSecret {
                    name: self.name.clone(),
                    reason,
                },
            )?;
            for (index, previous) in self.previous_secrets.iter().enumerate() {
                crate::security::config::validate_signing_secret(Some(previous), true).map_err(
                    |reason| WebhookConfigError::InvalidPreviousSecret {
                        name: self.name.clone(),
                        index,
                        reason,
                    },
                )?;
            }
        }

        Ok(())
    }

    fn apply_provider_defaults(&mut self) {
        let defaults = Self::provider_defaults(self.provider);
        if self.signature_header.is_none() {
            self.signature_header = defaults.signature_header;
        }
        if self.signature_prefix.is_none() {
            self.signature_prefix = defaults.signature_prefix;
        }
        if self.timestamp_header.is_none() {
            self.timestamp_header = defaults.timestamp_header;
        }
        if self.delivery_id_header.is_none() {
            self.delivery_id_header = defaults.delivery_id_header;
        }
        if self.event_type_header.is_none() {
            self.event_type_header = defaults.event_type_header;
        }
    }
}

/// Webhook configuration validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WebhookConfigError {
    /// A configured endpoint has no current secret.
    #[error("webhook endpoint {name:?} at {path:?} is missing a secret")]
    MissingSecret {
        /// Endpoint name.
        name: String,
        /// Endpoint path.
        path: String,
    },
    /// A configured endpoint is invalid.
    #[error("webhook endpoint {name:?} is invalid: {message}")]
    InvalidEndpoint {
        /// Endpoint name.
        name: String,
        /// Validation message.
        message: String,
    },
    /// Two endpoints declare the same route path.
    #[error(
        "duplicate webhook endpoint path {path:?}: endpoints {first_name:?} and \
         {duplicate_name:?} would shadow each other"
    )]
    DuplicatePath {
        /// Shared endpoint path.
        path: String,
        /// Name of the first endpoint using the path.
        first_name: String,
        /// Name of the later endpoint using the same path.
        duplicate_name: String,
    },
    /// Current production secret is weak or malformed.
    #[error("webhook endpoint {name:?} has invalid secret: {reason}")]
    InvalidSecret {
        /// Endpoint name.
        name: String,
        /// Signing secret validation error.
        reason: crate::security::config::SigningSecretError,
    },
    /// Previous production secret is weak or malformed.
    #[error("webhook endpoint {name:?} has invalid previous secret {index}: {reason}")]
    InvalidPreviousSecret {
        /// Endpoint name.
        name: String,
        /// Previous secret index.
        index: usize,
        /// Signing secret validation error.
        reason: crate::security::config::SigningSecretError,
    },
    /// Process-local replay storage was selected for production webhooks.
    #[error(
        "webhook replay backend memory is not allowed in production; set \
         security.webhooks.replay.backend = \"redis\" or explicitly set \
         security.webhooks.replay.allow_memory_in_production = true"
    )]
    MemoryReplayInProduction,
    /// Redis replay protection was selected without a URL.
    #[error("webhook redis replay backend requires security.webhooks.replay.redis.url")]
    RedisReplayMissingUrl,
    /// Redis replay URL is malformed.
    #[error("webhook redis replay backend URL is invalid: {0}")]
    RedisReplayInvalidUrl(String),
    /// Redis replay protection was selected without compiling the Redis feature.
    #[error("webhook redis replay backend requires the autumn-web redis feature")]
    RedisReplayFeatureDisabled,
}

#[derive(Debug)]
struct ResolvedWebhookEndpoint {
    config: WebhookEndpointConfig,
    keys: crate::security::config::ResolvedSigningKeys,
}

/// Runtime registry for signed webhook endpoints.
#[derive(Clone, Debug)]
pub struct WebhookRegistry {
    endpoints_by_path: Arc<HashMap<String, Arc<ResolvedWebhookEndpoint>>>,
    replay_store: Arc<dyn WebhookReplayStore>,
}

impl WebhookRegistry {
    /// Build a registry from config using the built-in in-memory replay store.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookConfigError`] when any endpoint is missing a secret.
    pub fn from_config(config: &WebhookConfig) -> Result<Self, WebhookConfigError> {
        let replay_store = if config
            .endpoints
            .iter()
            .any(|endpoint| endpoint.replay_protection)
        {
            replay_store_from_config(&config.replay)?
        } else {
            Arc::new(InMemoryWebhookReplayStore::default())
        };
        Self::from_config_with_shared_replay_store(config, replay_store)
    }

    /// Build a registry from config with a custom replay store.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookConfigError`] when any endpoint is missing a secret.
    pub fn from_config_with_replay_store(
        config: &WebhookConfig,
        replay_store: impl WebhookReplayStore + 'static,
    ) -> Result<Self, WebhookConfigError> {
        Self::from_config_with_shared_replay_store(config, Arc::new(replay_store))
    }

    /// Build a registry from config with a shared replay store.
    ///
    /// This is useful when multiple test app instances should share replay
    /// state, or when an integration constructs its own durable backend.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookConfigError`] when any endpoint is missing a secret.
    pub fn from_config_with_shared_replay_store(
        config: &WebhookConfig,
        replay_store: Arc<dyn WebhookReplayStore>,
    ) -> Result<Self, WebhookConfigError> {
        validate_unique_endpoint_paths(&config.endpoints)?;
        let mut endpoints_by_path = HashMap::new();
        for endpoint in &config.endpoints {
            let mut endpoint = endpoint.clone();
            endpoint.apply_provider_defaults();
            endpoint.validate(false)?;
            let Some(secret) = endpoint.secret.as_ref() else {
                return Err(WebhookConfigError::MissingSecret {
                    name: endpoint.name.clone(),
                    path: endpoint.path.clone(),
                });
            };
            let current = secret.as_bytes().to_vec();
            let previous = endpoint
                .previous_secrets
                .iter()
                .map(|secret| secret.as_bytes().to_vec())
                .collect();
            endpoints_by_path.insert(
                endpoint.path.clone(),
                Arc::new(ResolvedWebhookEndpoint {
                    config: endpoint,
                    keys: crate::security::config::ResolvedSigningKeys::new(current, previous),
                }),
            );
        }

        Ok(Self {
            endpoints_by_path: Arc::new(endpoints_by_path),
            replay_store,
        })
    }

    fn endpoint_for_path(&self, path: &str) -> Option<Arc<ResolvedWebhookEndpoint>> {
        self.endpoints_by_path.get(path).cloned()
    }
}

fn validate_unique_endpoint_paths(
    endpoints: &[WebhookEndpointConfig],
) -> Result<(), WebhookConfigError> {
    let mut seen_paths = HashMap::new();
    for endpoint in endpoints {
        if let Some(first_name) = seen_paths.insert(endpoint.path.as_str(), endpoint.name.as_str())
        {
            return Err(WebhookConfigError::DuplicatePath {
                path: endpoint.path.clone(),
                first_name: first_name.to_owned(),
                duplicate_name: endpoint.name.clone(),
            });
        }
    }
    Ok(())
}

/// Boxed replay store operation future.
pub type WebhookReplayFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, WebhookReplayStoreError>> + Send + 'a>>;

/// Replay store used to reject duplicate provider delivery IDs.
pub trait WebhookReplayStore: Send + Sync + std::fmt::Debug {
    /// Insert a delivery key and return `true`; return `false` if it already
    /// exists inside the replay window.
    fn check_and_insert<'a>(
        &'a self,
        key: &'a str,
        received_at: SystemTime,
        window: Duration,
    ) -> WebhookReplayFuture<'a>;

    /// Remove a delivery key from the store.
    fn remove<'a>(&'a self, key: &'a str) -> WebhookReplayFuture<'a>;
}

impl<T> WebhookReplayStore for Arc<T>
where
    T: WebhookReplayStore + ?Sized,
{
    fn check_and_insert<'a>(
        &'a self,
        key: &'a str,
        received_at: SystemTime,
        window: Duration,
    ) -> WebhookReplayFuture<'a> {
        self.as_ref().check_and_insert(key, received_at, window)
    }

    fn remove<'a>(&'a self, key: &'a str) -> WebhookReplayFuture<'a> {
        self.as_ref().remove(key)
    }
}

/// Replay store operation failure.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct WebhookReplayStoreError {
    message: String,
}

impl WebhookReplayStoreError {
    /// Create a replay-store failure with a human-readable diagnostic.
    ///
    /// Custom [`WebhookReplayStore`] implementations should return this when
    /// their durable backend is unavailable or cannot complete the atomic
    /// delivery-ID claim. Autumn surfaces the failure as `503 Service
    /// Unavailable` before the webhook handler runs.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[cfg(feature = "redis")]
impl WebhookReplayStoreError {
    fn backend(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::new(format!("webhook replay store {operation} failed: {error}"))
    }
}

/// In-memory replay protection store.
///
/// This is suitable for tests, development, and single-process deployments. A
/// multi-replica production fleet should configure the Redis replay backend.
#[derive(Debug, Default)]
pub struct InMemoryWebhookReplayStore {
    state: Mutex<InMemoryWebhookReplayState>,
}

#[derive(Debug, Default)]
struct InMemoryWebhookReplayState {
    seen: HashMap<String, SystemTime>,
    checks_since_cleanup: usize,
}

impl InMemoryWebhookReplayStore {
    fn check_and_insert_sync(&self, key: &str, received_at: SystemTime, window: Duration) -> bool {
        {
            let mut state = self
                .state
                .lock()
                .expect("webhook replay store lock poisoned");
            state.checks_since_cleanup = state.checks_since_cleanup.saturating_add(1);

            if let Some(expires_at) = state.seen.get(key).copied() {
                if expires_at.duration_since(received_at).is_ok() {
                    Self::cleanup_if_due(&mut state, received_at);
                    drop(state);
                    return false;
                }
                state.seen.remove(key);
            }

            let expires_at = received_at.checked_add(window).unwrap_or(received_at);
            state.seen.insert(key.to_owned(), expires_at);
            Self::cleanup_if_due(&mut state, received_at);
            drop(state);
        }
        true
    }

    fn cleanup_if_due(state: &mut InMemoryWebhookReplayState, received_at: SystemTime) {
        if state.checks_since_cleanup < IN_MEMORY_REPLAY_CLEANUP_INTERVAL
            && state.seen.len() <= IN_MEMORY_REPLAY_CLEANUP_HIGH_WATER
        {
            return;
        }

        state.checks_since_cleanup = 0;
        state
            .seen
            .retain(|_, expires_at| expires_at.duration_since(received_at).is_ok());
    }
}

impl WebhookReplayStore for InMemoryWebhookReplayStore {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a str,
        received_at: SystemTime,
        window: Duration,
    ) -> WebhookReplayFuture<'a> {
        Box::pin(async move { Ok(self.check_and_insert_sync(key, received_at, window)) })
    }

    fn remove<'a>(&'a self, key: &'a str) -> WebhookReplayFuture<'a> {
        self.state
            .lock()
            .expect("webhook replay store lock poisoned")
            .seen
            .remove(key);
        Box::pin(async move { Ok(true) })
    }
}

/// Redis replay protection store.
#[cfg(feature = "redis")]
#[derive(Clone, Debug)]
pub struct RedisWebhookReplayStore {
    connection: redis::aio::ConnectionManager,
    key_prefix: String,
}

#[cfg(feature = "redis")]
impl RedisWebhookReplayStore {
    /// Build a Redis replay store from config.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookConfigError`] when the URL is absent or malformed.
    pub fn from_config(config: &WebhookReplayRedisConfig) -> Result<Self, WebhookConfigError> {
        let url = config
            .url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or(WebhookConfigError::RedisReplayMissingUrl)?;
        let client = redis::Client::open(url)
            .map_err(|error| WebhookConfigError::RedisReplayInvalidUrl(error.to_string()))?;
        let connection = redis::aio::ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .map_err(|error| WebhookConfigError::RedisReplayInvalidUrl(error.to_string()))?;
        Ok(Self {
            connection,
            key_prefix: config.key_prefix.clone(),
        })
    }

    fn key_for(&self, replay_key: &str) -> String {
        format!("{}:{replay_key}", self.key_prefix)
    }
}

#[cfg(feature = "redis")]
impl WebhookReplayStore for RedisWebhookReplayStore {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a str,
        received_at: SystemTime,
        window: Duration,
    ) -> WebhookReplayFuture<'a> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let key = self.key_for(key);
            let ttl_secs = window.as_secs().max(1);
            let received_unix = received_at
                .duration_since(UNIX_EPOCH)
                .map_err(|error| WebhookReplayStoreError::backend("timestamp", error))?
                .as_secs()
                .to_string();
            let inserted: Option<String> = redis::cmd("SET")
                .arg(&key)
                .arg(received_unix)
                .arg("NX")
                .arg("EX")
                .arg(ttl_secs)
                .query_async(&mut connection)
                .await
                .map_err(|error| WebhookReplayStoreError::backend("insert", error))?;
            Ok(inserted.is_some())
        })
    }

    fn remove<'a>(&'a self, key: &'a str) -> WebhookReplayFuture<'a> {
        Box::pin(async move {
            let mut connection = self.connection.clone();
            let key = self.key_for(key);
            let _: () = redis::cmd("DEL")
                .arg(&key)
                .query_async(&mut connection)
                .await
                .map_err(|error| WebhookReplayStoreError::backend("delete", error))?;
            Ok(true)
        })
    }
}

/// A request that has passed signed webhook verification.
#[derive(Debug, Clone)]
pub struct SignedWebhook {
    provider: WebhookProvider,
    endpoint: String,
    delivery_id: Option<String>,
    event_type: Option<String>,
    received_at: SystemTime,
    raw_body: Bytes,
}

impl SignedWebhook {
    /// Provider preset that verified this request.
    #[must_use]
    pub const fn provider(&self) -> &'static str {
        self.provider.as_str()
    }

    /// Configured endpoint name.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Provider delivery ID, when present.
    #[must_use]
    pub fn delivery_id(&self) -> Option<&str> {
        self.delivery_id.as_deref()
    }

    /// Provider event type, when present.
    #[must_use]
    pub fn event_type(&self) -> Option<&str> {
        self.event_type.as_deref()
    }

    /// Request receive time used for timestamp and replay checks.
    #[must_use]
    pub const fn received_at(&self) -> SystemTime {
        self.received_at
    }

    /// Exact verified request body bytes.
    #[must_use]
    pub fn raw_body(&self) -> &[u8] {
        &self.raw_body
    }

    /// Decode the verified body as JSON.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` when the verified body is not valid JSON for
    /// the requested type.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.raw_body)
    }
}

impl FromRequest<crate::AppState> for SignedWebhook {
    type Rejection = crate::AutumnError;

    async fn from_request(
        req: axum::extract::Request,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let path = parts.uri.path().to_owned();
        let registry = state
            .extension::<WebhookRegistry>()
            .ok_or_else(|| WebhookVerifyError::RegistryMissing.into_autumn_error())?;
        let endpoint = registry
            .endpoint_for_path(&path)
            .ok_or_else(|| WebhookVerifyError::EndpointMissing(path.clone()).into_autumn_error())?;
        let body = axum::body::to_bytes(body, endpoint.config.max_body_bytes)
            .await
            .map_err(|err| {
                crate::AutumnError::bad_request_msg(format!(
                    "webhook body could not be read: {err}"
                ))
            })?;
        let received_at = SystemTime::now();
        verify_request(&registry, &endpoint, &parts.headers, body, received_at)
            .await
            .map_err(WebhookVerifyError::into_autumn_error)
    }
}

#[derive(Debug, Error)]
enum WebhookVerifyError {
    #[error("signed webhook registry is not installed")]
    RegistryMissing,
    #[error("no signed webhook endpoint is configured for path {0}")]
    EndpointMissing(String),
    #[error("missing required webhook header {0}")]
    MissingHeader(String),
    #[error("malformed webhook signature")]
    MalformedSignature,
    #[error("malformed webhook timestamp")]
    MalformedTimestamp,
    #[error("webhook timestamp is outside the accepted tolerance")]
    StaleTimestamp,
    #[error("webhook signature mismatch")]
    SignatureMismatch,
    #[error("missing webhook delivery ID")]
    MissingDeliveryId,
    #[error("duplicate webhook delivery")]
    DuplicateDelivery,
    #[error("webhook replay store unavailable: {0}")]
    ReplayStoreUnavailable(String),
}

impl WebhookVerifyError {
    const fn status(&self) -> StatusCode {
        match self {
            Self::RegistryMissing | Self::EndpointMissing(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::MissingHeader(_)
            | Self::MalformedSignature
            | Self::MalformedTimestamp
            | Self::MissingDeliveryId => StatusCode::BAD_REQUEST,
            Self::StaleTimestamp | Self::SignatureMismatch => StatusCode::UNAUTHORIZED,
            Self::DuplicateDelivery => StatusCode::CONFLICT,
            Self::ReplayStoreUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn into_autumn_error(self) -> crate::AutumnError {
        crate::AutumnError::bad_request_msg(self.to_string()).with_status(self.status())
    }
}

type ReplayStoreCell =
    std::sync::Arc<std::sync::Mutex<Option<(std::sync::Arc<dyn WebhookReplayStore>, Vec<String>)>>>;

tokio::task_local! {
    pub static WEBHOOK_REPLAY_KEY: ReplayStoreCell;
}

/// Guard that releases the in-flight webhook replay keys if the handler does not
/// complete normally.
///
/// Webhook delivery is at-least-once: providers retry on any failure, and
/// handlers are expected to be idempotent. So when a handler fails — whether it
/// panics, the request-timeout deadline cancels its future (this middleware is
/// applied *inner* to the timeout layer), or it returns a `5xx` (handled in
/// [`webhook_replay_cleanup_middleware`]) — the inserted replay keys are removed
/// so the provider's redelivery is accepted rather than rejected as a replay.
/// This is deliberate and consistent across all three failure modes; it does
/// mean a handler that committed its side effect *before* failing can be invoked
/// again on retry, which is the standard at-least-once contract handlers must
/// tolerate.
struct ReplayKeyGuard {
    cell: ReplayStoreCell,
    completed: bool,
}

impl Drop for ReplayKeyGuard {
    fn drop(&mut self) {
        if !self.completed {
            let to_remove = match self.cell.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some((store, keys)) = to_remove {
                tokio::spawn(async move {
                    for key in keys {
                        tracing::debug!(key = %key, "Releasing webhook replay key after handler panic or deadline cancellation");
                        let _ = store.remove(&key).await;
                    }
                });
            }
        }
    }
}

/// Middleware to clean up webhook replay keys on handler failure.
pub async fn webhook_replay_cleanup_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let cell = std::sync::Arc::new(std::sync::Mutex::new(None));
    let cell_cloned = cell.clone();

    let mut guard = ReplayKeyGuard {
        cell: cell_cloned.clone(),
        completed: false,
    };

    let response = WEBHOOK_REPLAY_KEY
        .scope(cell, async move { next.run(req).await })
        .await;

    guard.completed = true;

    if response.status().is_server_error() {
        let to_remove = match cell_cloned.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some((store, keys)) = to_remove {
            for key in keys {
                tracing::debug!(key = %key, "Releasing webhook replay key due to 5xx server error");
                let _ = store.remove(&key).await;
            }
        }
    }

    response
}

async fn verify_request(
    registry: &WebhookRegistry,
    endpoint: &ResolvedWebhookEndpoint,
    headers: &HeaderMap,
    body: Bytes,
    received_at: SystemTime,
) -> Result<SignedWebhook, WebhookVerifyError> {
    match endpoint.config.provider {
        WebhookProvider::Stripe => verify_stripe(endpoint, headers, &body, received_at)?,
        WebhookProvider::Github | WebhookProvider::Generic => {
            verify_body_hmac(endpoint, headers, &body, None, received_at)?;
        }
        WebhookProvider::Slack => verify_slack(endpoint, headers, &body, received_at)?,
    }

    let json_body = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let delivery_id = resolve_delivery_id(&endpoint.config, headers, json_body.as_ref());
    if endpoint.config.replay_protection {
        let delivery_id = delivery_id
            .as_deref()
            .ok_or(WebhookVerifyError::MissingDeliveryId)?;

        let mut keys_to_check = vec![format!(
            "{}:{}:id:{delivery_id}",
            endpoint.config.provider.as_str(),
            endpoint.config.name
        )];

        if matches!(
            endpoint.config.provider,
            WebhookProvider::Github | WebhookProvider::Generic
        ) {
            let sig_hdr = signature_header(endpoint);
            if let Some(sig_val) = headers.get(sig_hdr).and_then(|v| v.to_str().ok()) {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(sig_val.as_bytes());
                let sig_hash = hex::encode(hasher.finalize());
                keys_to_check.push(format!(
                    "{}:{}:sig:{sig_hash}",
                    endpoint.config.provider.as_str(),
                    endpoint.config.name
                ));
            }
        }

        let window = Duration::from_secs(endpoint.config.replay_window_secs);
        let mut inserted_keys = Vec::new();
        for key in keys_to_check {
            match registry
                .replay_store
                .check_and_insert(&key, received_at, window)
                .await
            {
                Ok(true) => {
                    inserted_keys.push(key);
                }
                Ok(false) => {
                    // Rollback already inserted keys for this request
                    for inserted in inserted_keys {
                        let _ = registry.replay_store.remove(&inserted).await;
                    }
                    return Err(WebhookVerifyError::DuplicateDelivery);
                }
                Err(error) => {
                    // Rollback already inserted keys for this request
                    for inserted in inserted_keys {
                        let _ = registry.replay_store.remove(&inserted).await;
                    }
                    return Err(WebhookVerifyError::ReplayStoreUnavailable(
                        error.to_string(),
                    ));
                }
            }
        }

        let _ = WEBHOOK_REPLAY_KEY.try_with(|cell| {
            let guard = match cell.lock() {
                Ok(g) => Some(g),
                Err(poisoned) => Some(poisoned.into_inner()),
            };
            if let Some(mut guard) = guard {
                *guard = Some((std::sync::Arc::clone(&registry.replay_store), inserted_keys));
            }
        });
    }

    Ok(SignedWebhook {
        provider: endpoint.config.provider,
        endpoint: endpoint.config.name.clone(),
        delivery_id,
        event_type: resolve_event_type(&endpoint.config, headers, json_body.as_ref()),
        received_at,
        raw_body: body,
    })
}

fn verify_stripe(
    endpoint: &ResolvedWebhookEndpoint,
    headers: &HeaderMap,
    body: &[u8],
    received_at: SystemTime,
) -> Result<(), WebhookVerifyError> {
    let header = required_header(headers, signature_header(endpoint))?;
    let (timestamp, signatures) = parse_stripe_signature(header)?;
    verify_timestamp(
        timestamp,
        received_at,
        endpoint.config.timestamp_tolerance_secs,
    )?;

    let timestamp = timestamp.to_string();
    let mut signed_payload = Vec::with_capacity(timestamp.len() + 1 + body.len());
    signed_payload.extend_from_slice(timestamp.as_bytes());
    signed_payload.push(b'.');
    signed_payload.extend_from_slice(body);

    if signatures
        .iter()
        .any(|signature| endpoint.keys.verify(&signed_payload, signature))
    {
        Ok(())
    } else {
        Err(WebhookVerifyError::SignatureMismatch)
    }
}

fn verify_slack(
    endpoint: &ResolvedWebhookEndpoint,
    headers: &HeaderMap,
    body: &[u8],
    received_at: SystemTime,
) -> Result<(), WebhookVerifyError> {
    let timestamp_header = endpoint
        .config
        .timestamp_header
        .as_deref()
        .ok_or(WebhookVerifyError::MalformedTimestamp)?;
    let timestamp = required_header(headers, timestamp_header)?
        .parse::<i64>()
        .map_err(|_| WebhookVerifyError::MalformedTimestamp)?;
    verify_timestamp(
        timestamp,
        received_at,
        endpoint.config.timestamp_tolerance_secs,
    )?;

    let timestamp = timestamp.to_string();
    let mut signed_payload = Vec::with_capacity(3 + timestamp.len() + 1 + body.len());
    signed_payload.extend_from_slice(b"v0:");
    signed_payload.extend_from_slice(timestamp.as_bytes());
    signed_payload.push(b':');
    signed_payload.extend_from_slice(body);
    verify_body_hmac(
        endpoint,
        headers,
        &signed_payload,
        endpoint.config.signature_prefix.as_deref(),
        received_at,
    )
}

fn verify_body_hmac(
    endpoint: &ResolvedWebhookEndpoint,
    headers: &HeaderMap,
    body_or_base: &[u8],
    explicit_prefix: Option<&str>,
    received_at: SystemTime,
) -> Result<(), WebhookVerifyError> {
    if let Some(timestamp_header) = endpoint.config.timestamp_header.as_deref()
        && endpoint.config.provider != WebhookProvider::Slack
    {
        let timestamp = required_header(headers, timestamp_header)?
            .parse::<i64>()
            .map_err(|_| WebhookVerifyError::MalformedTimestamp)?;
        verify_timestamp(
            timestamp,
            received_at,
            endpoint.config.timestamp_tolerance_secs,
        )?;
    }

    let mut signature = required_header(headers, signature_header(endpoint))?;
    let prefix = explicit_prefix.or(endpoint.config.signature_prefix.as_deref());
    if let Some(prefix) = prefix {
        signature = signature
            .strip_prefix(prefix)
            .ok_or(WebhookVerifyError::MalformedSignature)?;
    }

    if endpoint.keys.verify(body_or_base, signature) {
        Ok(())
    } else {
        Err(WebhookVerifyError::SignatureMismatch)
    }
}

fn signature_header(endpoint: &ResolvedWebhookEndpoint) -> &str {
    endpoint
        .config
        .signature_header
        .as_deref()
        .unwrap_or("X-Webhook-Signature")
}

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, WebhookVerifyError> {
    headers
        .get(name)
        .ok_or_else(|| WebhookVerifyError::MissingHeader(name.to_owned()))?
        .to_str()
        .map_err(|_| WebhookVerifyError::MalformedSignature)
}

fn parse_stripe_signature(header: &str) -> Result<(i64, Vec<&str>), WebhookVerifyError> {
    let mut timestamp = None;
    let mut signatures = Vec::new();

    for part in header.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            return Err(WebhookVerifyError::MalformedSignature);
        };
        match key.trim() {
            "t" => {
                timestamp = Some(
                    value
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| WebhookVerifyError::MalformedTimestamp)?,
                );
            }
            "v1" => signatures.push(value.trim()),
            _ => {}
        }
    }

    let timestamp = timestamp.ok_or(WebhookVerifyError::MalformedTimestamp)?;
    if signatures.is_empty() {
        return Err(WebhookVerifyError::MalformedSignature);
    }
    Ok((timestamp, signatures))
}

fn verify_timestamp(
    timestamp: i64,
    received_at: SystemTime,
    tolerance_secs: u64,
) -> Result<(), WebhookVerifyError> {
    let now = i64::try_from(
        received_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WebhookVerifyError::MalformedTimestamp)?
            .as_secs(),
    )
    .map_err(|_| WebhookVerifyError::MalformedTimestamp)?;
    let skew = now.abs_diff(timestamp);
    if skew > tolerance_secs {
        return Err(WebhookVerifyError::StaleTimestamp);
    }
    Ok(())
}

fn resolve_delivery_id(
    config: &WebhookEndpointConfig,
    headers: &HeaderMap,
    json_body: Option<&serde_json::Value>,
) -> Option<String> {
    let header = config
        .delivery_id_header
        .as_deref()
        .and_then(|header| optional_header(headers, header));

    match config.provider {
        WebhookProvider::Slack => header
            .or_else(|| slack_delivery_id(json_body))
            .or_else(|| json_string_field(json_body, "id")),
        _ => header.or_else(|| json_string_field(json_body, "id")),
    }
}

fn resolve_event_type(
    config: &WebhookEndpointConfig,
    headers: &HeaderMap,
    json_body: Option<&serde_json::Value>,
) -> Option<String> {
    config
        .event_type_header
        .as_deref()
        .and_then(|header| optional_header(headers, header))
        .or_else(|| json_string_field(json_body, "type"))
        .or_else(|| nested_json_string_field(json_body, "event", "type"))
}

fn optional_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn slack_delivery_id(json_body: Option<&serde_json::Value>) -> Option<String> {
    json_string_field(json_body, "event_id").or_else(|| {
        let value = json_body?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some("url_verification") {
            value
                .get("challenge")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        } else {
            None
        }
    })
}

fn json_string_field(value: Option<&serde_json::Value>, field: &str) -> Option<String> {
    let value = value?;
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn nested_json_string_field(
    value: Option<&serde_json::Value>,
    parent: &str,
    field: &str,
) -> Option<String> {
    let value = value?;
    value
        .get(parent)
        .and_then(|parent_value| parent_value.get(field))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

const fn default_timestamp_tolerance_secs() -> u64 {
    DEFAULT_TIMESTAMP_TOLERANCE_SECS
}

const fn default_replay_window_secs() -> u64 {
    DEFAULT_REPLAY_WINDOW_SECS
}

const fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

const fn default_true() -> bool {
    true
}

fn default_replay_redis_key_prefix() -> String {
    "autumn:webhooks:replay".to_owned()
}

fn validate_redis_replay_config(
    config: &WebhookReplayRedisConfig,
) -> Result<(), WebhookConfigError> {
    let url = config
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .ok_or(WebhookConfigError::RedisReplayMissingUrl)?;

    #[cfg(feature = "redis")]
    {
        redis::Client::open(url)
            .map_err(|error| WebhookConfigError::RedisReplayInvalidUrl(error.to_string()))?;
        Ok(())
    }

    #[cfg(not(feature = "redis"))]
    {
        let _ = url;
        Err(WebhookConfigError::RedisReplayFeatureDisabled)
    }
}

fn replay_store_from_config(
    config: &WebhookReplayConfig,
) -> Result<Arc<dyn WebhookReplayStore>, WebhookConfigError> {
    match config.backend {
        WebhookReplayBackend::Memory => Ok(Arc::new(InMemoryWebhookReplayStore::default())),
        WebhookReplayBackend::Redis => {
            #[cfg(feature = "redis")]
            {
                Ok(Arc::new(RedisWebhookReplayStore::from_config(
                    &config.redis,
                )?))
            }

            #[cfg(not(feature = "redis"))]
            {
                Err(WebhookConfigError::RedisReplayFeatureDisabled)
            }
        }
    }
}

pub(crate) fn install_registry_from_config(
    state: &crate::AppState,
    config: &WebhookConfig,
) -> Result<(), WebhookConfigError> {
    if config.endpoints.is_empty() {
        return Ok(());
    }
    let registry = WebhookRegistry::from_config(config)?;
    state.insert_extension(registry);
    Ok(())
}
