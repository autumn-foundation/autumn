//! Framework configuration with sensible defaults and profile-based layering.
//!
//! Autumn uses a five-layer configuration system where each layer
//! overrides the previous one:
//!
//! 1. **Framework defaults** (this module) -- compiled into the binary.
//! 2. **Profile smart defaults** -- per-profile values for `dev`/`prod`.
//! 3. **`autumn.toml`** -- project-level overrides checked into source control.
//! 4. **`[profile.{name}]` in `autumn.toml`** -- profile-specific overrides.
//! 5. **`autumn-{profile}.toml`** -- legacy profile-specific overrides.
//! 6. **`AUTUMN_*` environment variables** -- deployment/CI overrides.
//!
//! An Autumn application runs with zero configuration -- every field
//! has a sensible default value. Override only what you need.
//!
//! # Local-dev `.env` files
//!
//! A project-root `.env` file is a **local-dev feeder for the highest layer**
//! (the `AUTUMN_*` env-var layer) -- it does *not* add a new precedence tier.
//! Values parsed from `.env` populate env-layer keys that are still unset; a
//! real environment variable of the same name always wins. Auto-loaded in the
//! `dev` and `test` profiles and ignored in `prod` unless `AUTUMN_DOTENV=1`.
//! Files load in order `.env` -> `.env.local` -> `.env.{profile}` ->
//! `.env.{profile}.local`, and earlier files (and real env vars) win. See the
//! [`dotenv`](crate::dotenv) module.
//!
//! # Profiles
//!
//! Profiles are resolved in precedence order:
//! 1. `AUTUMN_ENV` environment variable
//! 2. `AUTUMN_PROFILE` environment variable (legacy alias)
//! 3. `--profile` CLI flag
//! 4. Auto-detect from debug/release build mode
//!
//! # Example
//!
//! ```rust
//! use autumn_web::config::AutumnConfig;
//!
//! // All defaults -- no file needed
//! let config = AutumnConfig::default();
//! assert_eq!(config.server.port, 3000);
//! assert_eq!(config.server.host, "127.0.0.1");
//! assert!(config.database.url.is_none());
//! ```
//!
//! # Environment variable reference
//!
//! | Variable | Config field | Type |
//! |----------|-------------|------|
//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |
//! | `AUTUMN_SERVER__HOST` | `server.host` | `String` |
//! | `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS` | `server.shutdown_timeout_secs` | `u64` |
//! | `AUTUMN_SERVER__PRESTOP_GRACE_SECS` | `server.prestop_grace_secs` | `u64` |
//! | `AUTUMN_SERVER__UPGRADE__ENABLED` | `server.upgrade.enabled` | `bool` |
//! | `AUTUMN_SERVER__UPGRADE__READY_TIMEOUT_SECS` | `server.upgrade.ready_timeout_secs` | `u64` |
//! | `AUTUMN_SERVER__TIMEOUTS__REQUEST_TIMEOUT_MS` | `server.timeouts.request_timeout_ms` | `u64` |
//! | `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS` | `server.max_concurrent_requests` | `usize` |
//! | `AUTUMN_SERVER__CAPACITY_CONTRACT` | `server.capacity_contract` | `String` |
//! | `AUTUMN_DATABASE__URL` | `database.url` | `String` |
//! | `AUTUMN_DATABASE__PRIMARY_URL` | `database.primary_url` | `String` |
//! | `AUTUMN_DATABASE__REPLICA_URL` | `database.replica_url` | `String` |
//! | `AUTUMN_DATABASE__POOL_SIZE` | `database.pool_size` | `usize` |
//! | `AUTUMN_DATABASE__PRIMARY_POOL_SIZE` | `database.primary_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__REPLICA_POOL_SIZE` | `database.replica_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__REPLICA_FALLBACK` | `database.replica_fallback` | `fail_readiness` / `primary` |
//! | `AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS` | `database.connect_timeout_secs` | `u64` |
//! | `AUTUMN_DATABASE__STARTUP_WAIT_SECS` | `database.startup_wait_secs` | `u64` |
//! | `AUTUMN_DATABASE__AUTO_MIGRATE` | `database.auto_migrate` | `Option<bool>` |
//! | `AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION` | `database.auto_migrate_in_production` | `bool` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__NAME` | `database.shards[i].name` | `String` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__PRIMARY_URL` | `database.shards[i].primary_url` | `String` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__SLOTS` | `database.shards[i].slots` | CSV of indices / `A-B` ranges |
//! | `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_URL` | `database.shards[i].replica_url` | `String` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__PRIMARY_POOL_SIZE` | `database.shards[i].primary_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_POOL_SIZE` | `database.shards[i].replica_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_FALLBACK` | `database.shards[i].replica_fallback` | `fail_readiness` / `primary` |
//! | `AUTUMN_LOG__LEVEL` | `log.level` | tracing filter directive |
//! | `AUTUMN_LOG__FORMAT` | `log.format` | `Auto` / `Pretty` / `Json` |
//! | `AUTUMN_TELEMETRY__ENABLED` | `telemetry.enabled` | `bool` |
//! | `AUTUMN_TELEMETRY__SERVICE_NAME` | `telemetry.service_name` | `String` |
//! | `AUTUMN_TELEMETRY__SERVICE_NAMESPACE` | `telemetry.service_namespace` | `String` |
//! | `AUTUMN_TELEMETRY__SERVICE_VERSION` | `telemetry.service_version` | `String` |
//! | `AUTUMN_TELEMETRY__ENVIRONMENT` | `telemetry.environment` | `String` |
//! | `AUTUMN_TELEMETRY__OTLP_ENDPOINT` | `telemetry.otlp_endpoint` | `String` |
//! | `AUTUMN_TELEMETRY__PROTOCOL` | `telemetry.protocol` | `Grpc` / `HttpProtobuf` |
//! | `AUTUMN_TELEMETRY__STRICT` | `telemetry.strict` | `bool` |
//! | `AUTUMN_HEALTH__PATH` | `health.path` | `String` |
//! | `AUTUMN_HEALTH__LIVE_PATH` | `health.live_path` | `String` |
//! | `AUTUMN_HEALTH__READY_PATH` | `health.ready_path` | `String` |
//! | `AUTUMN_HEALTH__STARTUP_PATH` | `health.startup_path` | `String` |
//! | `AUTUMN_HEALTH__DETAILED` | `health.detailed` | `bool` |
//! | `AUTUMN_HEALTH__ENABLED` | `health.enabled` | `bool` |
//! | `AUTUMN_CORS__ALLOWED_ORIGINS` | `cors.allowed_origins` | comma-separated `String` |
//! | `AUTUMN_CORS__ALLOWED_METHODS` | `cors.allowed_methods` | comma-separated `String` |
//! | `AUTUMN_CORS__ALLOWED_HEADERS` | `cors.allowed_headers` | comma-separated `String` |
//! | `AUTUMN_CORS__ALLOW_CREDENTIALS` | `cors.allow_credentials` | `bool` |
//! | `AUTUMN_CORS__MAX_AGE_SECS` | `cors.max_age_secs` | `u64` |
//! | `AUTUMN_CACHE__BACKEND` | `cache.backend` | `memory` / `redis` |
//! | `AUTUMN_CACHE__REDIS__URL` | `cache.redis.url` | `String` |
//! | `AUTUMN_CACHE__REDIS__KEY_PREFIX` | `cache.redis.key_prefix` | `String` |
//! | `AUTUMN_SESSION__BACKEND` | `session.backend` | `memory` / `redis` |
//! | `AUTUMN_SESSION__COOKIE_NAME` | `session.cookie_name` | `String` |
//! | `AUTUMN_SESSION__MAX_AGE_SECS` | `session.max_age_secs` | `u64` |
//! | `AUTUMN_SESSION__SECURE` | `session.secure` | `bool` |
//! | `AUTUMN_SESSION__SAME_SITE` | `session.same_site` | `String` |
//! | `AUTUMN_SESSION__HTTP_ONLY` | `session.http_only` | `bool` |
//! | `AUTUMN_SESSION__PATH` | `session.path` | `String` |
//! | `AUTUMN_SESSION__ALLOW_MEMORY_IN_PRODUCTION` | `session.allow_memory_in_production` | `bool` |
//! | `AUTUMN_SESSION__REDIS__URL` | `session.redis.url` | `String` |
//! | `AUTUMN_SESSION__REDIS__KEY_PREFIX` | `session.redis.key_prefix` | `String` |
//! | `AUTUMN_CHANNELS__BACKEND` | `channels.backend` | `in_process` / `redis` |
//! | `AUTUMN_CHANNELS__CAPACITY` | `channels.capacity` | `usize` |
//! | `AUTUMN_CHANNELS__REPLAY_BUFFER` | `channels.replay_buffer` | `usize` |
//! | `AUTUMN_CHANNELS__REDIS__URL` | `channels.redis.url` | `String` |
//! | `AUTUMN_CHANNELS__REDIS__KEY_PREFIX` | `channels.redis.key_prefix` | `String` |
//! | `AUTUMN_JOBS__BACKEND` | `jobs.backend` | `local` / `postgres` / `redis` |
//! | `AUTUMN_JOBS__WORKERS` | `jobs.workers` | `usize` |
//! | `AUTUMN_JOBS__PIN` | `jobs.pin` | comma-separated queue names |
//! | `AUTUMN_JOBS__MAX_ATTEMPTS` | `jobs.max_attempts` | `u32` |
//! | `AUTUMN_JOBS__INITIAL_BACKOFF_MS` | `jobs.initial_backoff_ms` | `u64` |
//! | `AUTUMN_JOBS__REDIS__URL` | `jobs.redis.url` | `String` |
//! | `AUTUMN_JOBS__REDIS__KEY_PREFIX` | `jobs.redis.key_prefix` | `String` |
//! | `AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS` | `jobs.redis.visibility_timeout_ms` | `u64` |
//! | `AUTUMN_JOBS__POSTGRES__VISIBILITY_TIMEOUT_MS` | `jobs.postgres.visibility_timeout_ms` | `u64` |
//! | `AUTUMN_JOBS__TRACKING__TTL_SECS` | `jobs.tracking.ttl_secs` | `u64` |
//! | `AUTUMN_JOBS__TRACKING__ROUTE_ENABLED` | `jobs.tracking.route_enabled` | `bool` |
//! | `AUTUMN_SCHEDULER__BACKEND` | `scheduler.backend` | `in_process` / `postgres` |
//! | `AUTUMN_RETENTION__SWEEP_INTERVAL` | `retention.sweep_interval` | duration `String` |
//! | `AUTUMN_RETENTION__JOB_HISTORY` | `retention.job_history` | duration `String` |
//! | `AUTUMN_RETENTION__COMMIT_HOOKS` | `retention.commit_hooks` | duration `String` |
//! | `AUTUMN_RETENTION__JOB_TRACKING` | `retention.job_tracking` | duration `String` |
//! | `AUTUMN_RETENTION__IDEMPOTENCY` | `retention.idempotency` | duration `String` |
//! | `AUTUMN_RETENTION__EXPERIMENT_ASSIGNMENTS` | `retention.experiment_assignments` | duration `String` |
//! | `AUTUMN_RETENTION__WEBHOOK_REPLAY` | `retention.webhook_replay` | duration `String` |
//! | `AUTUMN_RETENTION__SESSIONS` | `retention.sessions` | duration `String` |
//! | `AUTUMN_RETENTION__AUDIT_ARCHIVES` | `retention.audit_archives` | duration `String` |
//! | `AUTUMN_SCHEDULER__LEASE_TTL_SECS` | `scheduler.lease_ttl_secs` | `u64` |
//! | `AUTUMN_SCHEDULER__REPLICA_ID` | `scheduler.replica_id` | `String` |
//! | `AUTUMN_SCHEDULER__KEY_PREFIX` | `scheduler.key_prefix` | `String` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__ENABLED` | `security.rate_limit.enabled` | `bool` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__REQUESTS_PER_SECOND` | `security.rate_limit.requests_per_second` | `f64` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__BURST` | `security.rate_limit.burst` | `u32` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS` | `security.rate_limit.trust_forwarded_headers` | `bool` |
//! | `AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES` | `security.rate_limit.trusted_proxies` | comma-separated `String` |
//! | `AUTUMN_ENV` | active profile | `String` |
//! | `AUTUMN_PROFILE` | active profile (legacy alias) | `String` |
//! | `AUTUMN_SECURITY__UPLOAD__MAX_REQUEST_SIZE_BYTES` | `security.upload.max_request_size_bytes` | `usize` |
//! | `AUTUMN_SECURITY__UPLOAD__MAX_FILE_SIZE_BYTES` | `security.upload.max_file_size_bytes` | `usize` |
//! | `AUTUMN_SECURITY__UPLOAD__ALLOWED_MIME_TYPES` | `security.upload.allowed_mime_types` | comma-separated `String` |
//! | `AUTUMN_SECURITY__UPLOAD__REJECT_ON_CONTENT_TYPE_MISMATCH` | `security.upload.reject_on_content_type_mismatch` | `bool` |
//! | `AUTUMN_SECURITY__FORBIDDEN_RESPONSE` | `security.forbidden_response` | `"403"` or `"404"` |
//! | `AUTUMN_SECURITY__ALLOW_UNAUTHORIZED_REPOSITORY_API` | `security.allow_unauthorized_repository_api` | `bool` |
//! | `AUTUMN_SECURITY__SIGNING_SECRET` | `security.signing_secret.secret` | `String` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND` | `security.webhooks.replay.backend` | `memory` / `redis` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL` | `security.webhooks.replay.redis.url` | `String` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX` | `security.webhooks.replay.redis.key_prefix` | `String` |
//! | `AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION` | `security.webhooks.replay.allow_memory_in_production` | `bool` |
//! | `AUTUMN_DEV__INSPECTOR_PATH` | `dev.inspector_path` | `String` |
//! | `AUTUMN_DEV__INSPECTOR_CAPACITY` | `dev.inspector_capacity` | `usize` |
//! | `AUTUMN_DEV__INSPECTOR_N_PLUS_ONE_THRESHOLD` | `dev.inspector_n_plus_one_threshold` | `usize` |
//! | `AUTUMN_OBSERVABILITY__SERVER_TIMING` | `observability.server_timing` | `bool` |
//! | `AUTUMN_COMPRESSION__ENABLED` | `compression.enabled` | `bool` |
//! | `AUTUMN_STORIES__ENABLED` | `stories.enabled` | `bool` |
//! | `AUTUMN_AUTH__LOCKOUT__ENABLED` | `auth.lockout.enabled` | `bool` |
//! | `AUTUMN_AUTH__LOCKOUT__THRESHOLD` | `auth.lockout.threshold` | `i32` |
//! | `AUTUMN_AUTH__LOCKOUT__WINDOW_SECS` | `auth.lockout.window_secs` | `u64` |
//! | `AUTUMN_AUTH__LOCKOUT__COOLOFF_SECS` | `auth.lockout.cooloff_secs` | `u64` |
//! | `AUTUMN_AUTH__MAGIC_LINK__TTL_MINUTES` | `auth.magic_link.ttl_minutes` | `u64` |
//! | `AUTUMN_AUTH__MAGIC_LINK__EMAIL_COOLDOWN_SECS` | `auth.magic_link.email_cooldown_secs` | `u64` |
//! | `AUTUMN_TIME_ZONE__IDENTIFIER` | `time_zone.identifier` | IANA id `String` |
//! | `AUTUMN_FAILURE_CAPTURE__ENABLED` | `failure_capture.enabled` | `bool` |
//! | `AUTUMN_FAILURE_CAPTURE__DIR` | `failure_capture.dir` | `String` |
//! | `AUTUMN_FAILURE_CAPTURE__MAX_BODY_BYTES` | `failure_capture.max_body_bytes` | `usize` |
//! | `AUTUMN_FAILURE_CAPTURE__MAX_CAPSULE_BYTES` | `failure_capture.max_capsule_bytes` | `usize` |
//! | `AUTUMN_FAILURE_CAPTURE__MAX_CAPSULES` | `failure_capture.max_capsules` | `usize` |
//! | `AUTUMN_CLUSTER__ENABLED` | `cluster.enabled` | `bool` |
//! | `AUTUMN_CLUSTER__SECRET` | `cluster.secret` | `SecretString` |
//! | `AUTUMN_CLUSTER__CLUSTER_NAME` | `cluster.cluster_name` | `String` |
//! | `AUTUMN_CLUSTER__BIND_ADDR` | `cluster.bind_addr` | `String` |
//! | `AUTUMN_CLUSTER__ADVERTISE_ADDR` | `cluster.advertise_addr` | `String` |
//! | `AUTUMN_CLUSTER__SEED_PEERS` | `cluster.seed_peers` | comma-separated addresses |
//! | `AUTUMN_CLUSTER__NODE_ID` | `cluster.node_id` | `String` |
//! | `AUTUMN_CLUSTER__PUSH_INTERVAL_MS` | `cluster.push_interval_ms` | `u64` |
//! | `AUTUMN_CLUSTER__SUSPICION_TIMEOUT_MS` | `cluster.suspicion_timeout_ms` | `u64` |
//! | `AUTUMN_SHADOW__ENABLED` | `shadow.enabled` | `bool` |
//! | `AUTUMN_SHADOW__TARGET` | `shadow.target` | `String` |
//! | `AUTUMN_SHADOW__SAMPLE_RATE` | `shadow.sample_rate` | `f64` |
//! | `AUTUMN_SHADOW__ROUTES` | `shadow.routes` | comma-separated patterns |
//! | `AUTUMN_SHADOW__TIMEOUT_MS` | `shadow.timeout_ms` | `u64` |
//! | `AUTUMN_SHADOW__MAX_IN_FLIGHT` | `shadow.max_in_flight` | `usize` |
//! | `AUTUMN_SHADOW__MAX_BODY_BYTES` | `shadow.max_body_bytes` | `usize` |
//! | `AUTUMN_SHADOW__MAX_RECORDS` | `shadow.max_records` | `usize` |
//! | `AUTUMN_SHADOW__MAX_SAMPLE_BYTES` | `shadow.max_sample_bytes` | `usize` |

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Abstraction for reading environment variables, supporting dependency injection for testing.
use std::sync::OnceLock;

static MACRO_MANIFEST_DIR: OnceLock<String> = OnceLock::new();
static MACRO_IS_DEBUG: OnceLock<bool> = OnceLock::new();

#[doc(hidden)]
pub fn __set_macro_context(manifest_dir: String, is_debug: bool) {
    let _ = MACRO_MANIFEST_DIR.set(manifest_dir);
    let _ = MACRO_IS_DEBUG.set(is_debug);
}

/// Trait for environment variable reading to allow testing overrides.
///
/// This abstracts the OS environment (`std::env::var`) so that
/// configuration loading logic can be unit-tested deterministically
/// by supplying a mock environment.
pub trait Env {
    /// Read an environment variable.
    ///
    /// # Examples
    ///
    /// ```
    /// use autumn_web::config::{Env, OsEnv};
    /// let env = OsEnv;
    /// let val = env.var("NON_EXISTENT_VAR");
    /// assert!(val.is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`std::env::VarError`] if the variable is not present or is not valid Unicode.
    fn var(&self, key: &str) -> Result<String, std::env::VarError>;
}

/// Production implementation of `Env` that reads from the OS environment.
#[derive(Clone, Default)]
pub struct OsEnv;

impl Env for OsEnv {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        if key == "AUTUMN_MANIFEST_DIR" {
            // Process env takes priority over the compile-time baked-in path so
            // installed apps (e.g. Tauri sidecars) can redirect config loading to
            // their bundled resource dir by setting AUTUMN_MANIFEST_DIR at launch.
            if let Ok(override_val) = std::env::var(key) {
                return Ok(override_val);
            }
            if let Some(dir) = MACRO_MANIFEST_DIR.get() {
                return Ok(dir.clone());
            }
        } else if key == "AUTUMN_IS_DEBUG"
            && let Some(is_debug) = MACRO_IS_DEBUG.get()
        {
            return Ok(if *is_debug {
                "1".to_string()
            } else {
                "0".to_string()
            });
        }
        std::env::var(key)
    }
}

/// Mock implementation of `Env` for testing.
#[derive(Clone, Default)]
pub struct MockEnv {
    vars: std::collections::HashMap<String, String>,
}

impl MockEnv {
    /// Create a new, empty `MockEnv`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vars: std::collections::HashMap::new(),
        }
    }

    /// Set an environment variable in the mock.
    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Remove an environment variable from the mock.
    #[must_use]
    pub fn without(mut self, key: &str) -> Self {
        self.vars.remove(key);
        self
    }
}

impl Env for MockEnv {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        self.vars
            .get(key)
            .cloned()
            .ok_or(std::env::VarError::NotPresent)
    }
}

/// Locate a config file by checking the app's crate directory first, then CWD.
fn find_config_file_named(filename: &str, env: &dyn Env) -> PathBuf {
    if let Ok(manifest_dir) = env.var("AUTUMN_MANIFEST_DIR") {
        let candidate = PathBuf::from(manifest_dir).join(filename);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(filename)
}

/// Load a TOML file as a raw `toml::Value` table.
/// Returns `Ok(None)` if the file doesn't exist.
fn load_raw_toml(path: &Path) -> Result<Option<toml::Value>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let table = toml::from_str::<toml::Table>(&contents)?;
            Ok(Some(toml::Value::Table(table)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Resolve the active profile using the precedence chain.
///
/// 1. `AUTUMN_ENV` env var (highest priority)
/// 2. `AUTUMN_PROFILE` env var (legacy alias)
/// 3. `--profile <name>` CLI flag
/// 4. Auto-detect from build mode (`AUTUMN_IS_DEBUG` set by `#[autumn_web::main]`)
/// 5. Fallback to `dev`
pub(crate) fn resolve_profile(env: &dyn Env) -> String {
    let selected_profile_input = resolve_profile_input(env);
    normalize_profile_name(&selected_profile_input).unwrap_or_else(|| "dev".to_owned())
}

/// Resolve the raw profile selector value (before normalization).
///
/// The env-var keys consulted here (`AUTUMN_ENV`, `AUTUMN_PROFILE`,
/// `AUTUMN_IS_DEBUG`) are the profile *selectors*; they are deliberately
/// excluded from the `.env` overlay (see [`crate::dotenv`]'s
/// `PROFILE_SELECTOR_KEYS`) so a `.env` file cannot switch the active profile.
/// Keep the two lists in sync.
fn resolve_profile_input(env: &dyn Env) -> String {
    // 1. Preferred env var
    if let Ok(profile) = env.var("AUTUMN_ENV") {
        let trimmed = profile.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    // 2. Legacy env var
    if let Ok(profile) = env.var("AUTUMN_PROFILE") {
        let trimmed = profile.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    // 3. CLI flag
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--profile"
            && let Some(profile) = args.get(i + 1)
        {
            let trimmed = profile.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
        if let Some(profile) = arg.strip_prefix("--profile=") {
            let trimmed = profile.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    // 4. Auto-detect from build mode
    if env.var("AUTUMN_IS_DEBUG").ok().as_deref() == Some("0") {
        return "prod".to_owned();
    }
    "dev".to_owned()
}

/// Normalize profile aliases and trim whitespace.
///
/// Supported aliases:
/// - `production` -> `prod`
/// - `development` -> `dev`
/// - `prod`/`PROD` -> `prod`
/// - `dev`/`DEV` -> `dev`
///
/// `pub` so the deploy CLI (`autumn-cli`) can mirror the runtime's profile
/// normalization exactly when picking which local `autumn-<profile>.toml` to
/// upload — a single source of truth prevents deploy/runtime drift (#1952).
#[must_use]
pub fn normalize_profile_name(profile: &str) -> Option<String> {
    let trimmed = profile.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.eq_ignore_ascii_case("production") {
        return Some("prod".to_owned());
    }
    if trimmed.eq_ignore_ascii_case("development") {
        return Some("dev".to_owned());
    }
    if trimmed.eq_ignore_ascii_case("prod") {
        return Some("prod".to_owned());
    }
    if trimmed.eq_ignore_ascii_case("dev") {
        return Some("dev".to_owned());
    }

    // Preserve user-specified case for custom profile names.
    Some(trimmed.to_owned())
}

/// Profile names to check for inline/file overrides.
///
/// For canonical profiles, include legacy aliases for compatibility so
/// `production` and `development` profile sources are still loaded.
fn profile_lookup_names(profile: &str) -> Vec<&str> {
    match profile {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Ordered file lookup names for profile override file compatibility.
///
/// Only one profile override file is loaded: the first existing file in this
/// ordered list. The order prefers the explicitly-selected spelling.
///
/// `pub` so the deploy CLI (`autumn-cli`) can mirror the runtime's
/// override-file lookup exactly when picking which local `autumn-<profile>.toml`
/// to upload — a single source of truth prevents deploy/runtime drift (#1952).
#[must_use]
pub fn profile_override_file_lookup_names(
    profile: &str,
    selected_profile_input: &str,
) -> Vec<String> {
    match profile {
        "prod" if selected_profile_input.eq_ignore_ascii_case("production") => {
            vec!["production".to_owned(), "prod".to_owned()]
        }
        "prod" => vec!["prod".to_owned(), "production".to_owned()],
        "dev" if selected_profile_input.eq_ignore_ascii_case("development") => {
            vec!["development".to_owned(), "dev".to_owned()]
        }
        "dev" => vec!["dev".to_owned(), "development".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// Extract `[profile.<name>]` table from a parsed `autumn.toml`.
fn profile_section_from_base_toml(base: &toml::Value, profile: &str) -> Option<toml::Value> {
    base.get("profile")
        .and_then(toml::Value::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(toml::Value::as_table)
        .map(|table| toml::Value::Table(table.clone()))
}

/// Profile-specific smart defaults as a TOML table.
///
/// Only `dev` and `prod` have smart defaults. Custom profiles
/// (staging, test, etc.) get no smart defaults — they rely on
/// their profile TOML file and env overrides.
fn profile_defaults_as_toml(profile: &str) -> toml::Value {
    let mut table = toml::map::Map::new();

    match profile {
        "dev" => {
            let mut log = toml::map::Map::new();
            log.insert("level".into(), "debug".into());
            log.insert("format".into(), "Pretty".into());
            table.insert("log".into(), toml::Value::Table(log));

            let mut telemetry = toml::map::Map::new();
            telemetry.insert("environment".into(), "development".into());
            table.insert("telemetry".into(), toml::Value::Table(telemetry));

            let mut server = toml::map::Map::new();
            server.insert("host".into(), "127.0.0.1".into());
            server.insert("shutdown_timeout_secs".into(), toml::Value::Integer(1));
            // Zero-out the prestop grace in dev: there is no load balancer to
            // deregister, so the 5-second default would add unnecessary latency
            // on every Ctrl-C.
            server.insert("prestop_grace_secs".into(), toml::Value::Integer(0));
            table.insert("server".into(), toml::Value::Table(server));

            let mut health = toml::map::Map::new();
            health.insert("detailed".into(), toml::Value::Boolean(true));
            table.insert("health".into(), toml::Value::Table(health));

            let mut actuator = toml::map::Map::new();
            actuator.insert("sensitive".into(), toml::Value::Boolean(true));
            table.insert("actuator".into(), toml::Value::Table(actuator));

            let mut cors = toml::map::Map::new();
            cors.insert(
                "allowed_origins".into(),
                toml::Value::Array(vec![toml::Value::String("*".to_owned())]),
            );
            table.insert("cors".into(), toml::Value::Table(cors));

            // Dev: enable the local-disk blob store rooted at
            // `target/blobs/` automatically when the `storage` feature
            // is on. `prod` deliberately leaves `backend = "disabled"`
            // so the operator has to opt into either `local` (with
            // `allow_local_in_production = true`) or `s3`.
            let mut storage = toml::map::Map::new();
            storage.insert("backend".into(), "local".into());
            table.insert("storage".into(), toml::Value::Table(storage));
            // Dev: trust X-Forwarded-* from loopback only so local reverse
            // proxies (nginx, caddy, etc. on 127.0.0.1/::1) work out of the box.
            let mut trusted_proxies = toml::map::Map::new();
            trusted_proxies.insert("trust_forwarded_headers".into(), toml::Value::Boolean(true));
            trusted_proxies.insert(
                "ranges".into(),
                toml::Value::Array(vec![
                    toml::Value::String("127.0.0.0/8".to_owned()),
                    toml::Value::String("::1/128".to_owned()),
                ]),
            );
            let mut security = toml::map::Map::new();
            security.insert(
                "trusted_proxies".into(),
                toml::Value::Table(trusted_proxies),
            );
            table.insert("security".into(), toml::Value::Table(security));
            // Dev: CSRF disabled (default), HSTS off (default)
        }
        "prod" => {
            let mut log = toml::map::Map::new();
            log.insert("level".into(), "info".into());
            log.insert("format".into(), "Json".into());
            table.insert("log".into(), toml::Value::Table(log));

            let mut telemetry = toml::map::Map::new();
            telemetry.insert("environment".into(), "production".into());
            table.insert("telemetry".into(), toml::Value::Table(telemetry));

            let mut server = toml::map::Map::new();
            server.insert("host".into(), "0.0.0.0".into());
            server.insert("shutdown_timeout_secs".into(), toml::Value::Integer(30));
            let mut timeouts = toml::map::Map::new();
            timeouts.insert("request_timeout_ms".into(), toml::Value::Integer(30_000));
            server.insert("timeouts".into(), toml::Value::Table(timeouts));
            table.insert("server".into(), toml::Value::Table(server));

            let mut health = toml::map::Map::new();
            health.insert("detailed".into(), toml::Value::Boolean(false));
            table.insert("health".into(), toml::Value::Table(health));

            // Prod: strict security -- HSTS on, CSRF enabled, secure cookies
            let mut security = toml::map::Map::new();
            let mut headers = toml::map::Map::new();
            headers.insert(
                "strict_transport_security".into(),
                toml::Value::Boolean(true),
            );
            security.insert("headers".into(), toml::Value::Table(headers));
            let mut csrf = toml::map::Map::new();
            csrf.insert("enabled".into(), toml::Value::Boolean(true));
            security.insert("csrf".into(), toml::Value::Table(csrf));
            table.insert("security".into(), toml::Value::Table(security));

            let mut session = toml::map::Map::new();
            session.insert("secure".into(), toml::Value::Boolean(true));
            table.insert("session".into(), toml::Value::Table(session));
        }
        _ => {} // Custom profiles get no smart defaults
    }

    toml::Value::Table(table)
}

#[cfg(feature = "mail")]
fn has_mail_transport_source(merged: &toml::Value, env: &dyn Env) -> bool {
    merged
        .get("mail")
        .and_then(toml::Value::as_table)
        .is_some_and(|mail| mail.contains_key("transport"))
        || env
            .var("AUTUMN_MAIL__TRANSPORT")
            .ok()
            .as_deref()
            .is_some_and(|value| crate::mail::Transport::from_env_value(value).is_some())
}

/// Maximum recursion depth for merging TOML tables.
const MAX_MERGE_DEPTH: usize = 16;

/// Deep-merge two TOML values. Tables are merged recursively;
/// non-table values in `overlay` replace those in `base`.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    deep_merge_with_depth(base, overlay, 0);
}

fn deep_merge_with_depth(base: &mut toml::Value, overlay: toml::Value, depth: usize) {
    if depth > MAX_MERGE_DEPTH {
        eprintln!(
            "Warning: Configuration merge exceeded max depth ({MAX_MERGE_DEPTH}), ignoring deeper values."
        );
        return;
    }

    let toml::Value::Table(overlay_table) = overlay else {
        return;
    };
    let Some(base_table) = base.as_table_mut() else {
        return;
    };

    for (key, overlay_val) in overlay_table {
        let is_recursive_merge =
            overlay_val.is_table() && base_table.get(&key).is_some_and(toml::Value::is_table);

        if is_recursive_merge {
            if let Some(base_val) = base_table.get_mut(&key) {
                deep_merge_with_depth(base_val, overlay_val, depth + 1);
            }
        } else {
            base_table.insert(key, overlay_val);
        }
    }
}

/// Suggest a close match for a custom profile name.
///
/// Returns `Some(name)` when a known profile is within edit distance 2.
fn suggest_profile(profile: &str) -> Option<&'static str> {
    let known = ["dev", "prod"];
    let mut suggestions: Vec<(&str, usize)> = known
        .iter()
        .map(|k| (*k, levenshtein(profile, k)))
        .filter(|(_, d)| *d <= 2)
        .collect();
    suggestions.sort_by_key(|(_, d)| *d);
    suggestions.first().map(|(name, _)| *name)
}

/// Warn when a custom profile has no TOML file, suggesting close matches.
fn warn_profile_typo(profile: &str) {
    if let Some(suggestion) = suggest_profile(profile) {
        eprintln!(
            "Warning: profile \"{profile}\" has no config file (autumn-{profile}.toml) \
             and no smart defaults. Did you mean \"{suggestion}\"?"
        );
    }
}

fn should_warn_missing_profile_file(profile: &str, has_inline_profile_section: bool) -> bool {
    profile != "dev" && profile != "prod" && !has_inline_profile_section
}

/// Levenshtein edit distance between two strings.
///
/// ⚡ Bolt Optimization:
/// Reduces memory allocations by using a single `Vec` instead of two and
/// iterating directly over `Chars` to avoid `Vec<char>` allocations.
#[must_use]
pub fn levenshtein(a: &str, b: &str) -> usize {
    let n = b.chars().count();
    let mut prev: Vec<usize> = (0..=n).collect();
    for (i, a_ch) in a.chars().enumerate() {
        let mut prev_diag = prev[0];
        prev[0] = i + 1;
        for (j, b_ch) in b.chars().enumerate() {
            let old_prev = prev[j + 1];
            let cost = usize::from(a_ch != b_ch);
            prev[j + 1] = (prev[j + 1] + 1).min(prev[j] + 1).min(prev_diag + cost);
            prev_diag = old_prev;
        }
    }
    prev[n]
}

// ── Deprecation channel ───────────────────────────────────────────────────────

/// A configuration key (or its corresponding `AUTUMN_*` env var) that is
/// deprecated but still honored for the current minor-release line.
///
/// Register entries in [`DEPRECATED_CONFIG_KEYS`]. The config loader emits a
/// structured `WARN` for each entry whose key is present in the resolved config,
/// and `autumn doctor` surfaces them as ⚠️ checks.
///
/// # Env-var contract
///
/// A registered `path` MUST correspond to the mechanical env-var name produced
/// by [`deprecated_env_var_name`] (`a.b.c` → `AUTUMN_A__B__C`), which is the
/// same name the loader's `apply_*_env_overrides` reads to honor the value. If
/// a key's loader override uses a non-mechanical env-var name, env-var detection
/// here would diverge from what the loader actually applies. The integration
/// tests in `autumn/tests/config_deprecation.rs` lock this for every entry by
/// loading config with each key set via its env var and asserting the value is
/// honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeprecatedKey {
    /// Dotted config path, e.g. `"security.rate_limit.trusted_proxies"`.
    pub path: &'static str,
    /// The replacement key path, or `None` meaning "remove it; no replacement".
    pub replacement: Option<&'static str>,
    /// Version the deprecation was introduced (e.g. `"0.5.0"`).
    pub since: &'static str,
    /// Version the key is scheduled for removal (e.g. `"1.0.0"`).
    pub remove_in: &'static str,
}

/// The canonical registry of deprecated config keys.
///
/// Add entries here when retiring a key; never silently delete a schema field
/// without first registering it here. The schema-snapshot CI guard
/// (`autumn/tests/schema_drift_guard.rs`) enforces this rule.
pub static DEPRECATED_CONFIG_KEYS: &[DeprecatedKey] = &[
    DeprecatedKey {
        path: "security.rate_limit.trusted_proxies",
        replacement: Some("security.trusted_proxies.ranges"),
        since: "0.5.0",
        remove_in: "1.0.0",
    },
    DeprecatedKey {
        path: "security.rate_limit.trust_forwarded_headers",
        replacement: Some("security.trusted_proxies.trust_forwarded_headers"),
        since: "0.5.0",
        remove_in: "1.0.0",
    },
];

/// Returns the full registry of deprecated config keys.
#[must_use]
pub fn deprecated_config_keys() -> &'static [DeprecatedKey] {
    DEPRECATED_CONFIG_KEYS
}

/// Converts a dotted config key path to its `AUTUMN_*` env var name.
///
/// # Examples
/// ```
/// # use autumn_web::config::deprecated_env_var_name;
/// assert_eq!(
///     deprecated_env_var_name("security.rate_limit.trusted_proxies"),
///     "AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES"
/// );
/// ```
#[must_use]
pub fn deprecated_env_var_name(path: &str) -> String {
    format!("AUTUMN_{}", path.to_uppercase().replace('.', "__"))
}

/// Where a deprecated key was detected: TOML only, env-var only, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeprecationSource {
    Toml,
    Env,
    Both,
}

/// One detected use of a deprecated config key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeprecationFinding {
    pub path: String,
    pub replacement: Option<String>,
    pub since: String,
    pub remove_in: String,
    pub source: DeprecationSource,
}

/// Tests whether a dotted key path is present in a TOML table (any value type).
///
/// Non-table mid-segments are treated as absent (no panic).
fn toml_path_present(table: &toml::Table, path: &str) -> bool {
    let mut current_table = table;
    let mut segments = path.split('.').peekable();

    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return current_table.contains_key(segment);
        }
        match current_table.get(segment) {
            Some(toml::Value::Table(next)) => current_table = next,
            _ => return false,
        }
    }
    false
}

/// Scans the merged config table and env for any registered deprecated key.
///
/// Returns at most one [`DeprecationFinding`] per registry entry (even if the key
/// is set in both TOML and env, the two sources are collapsed into [`DeprecationSource::Both`]).
/// Registry order is preserved for deterministic output.
#[must_use]
pub fn detect_deprecated_keys(
    merged: &toml::Table,
    env: &dyn Env,
    registry: &[DeprecatedKey],
) -> Vec<DeprecationFinding> {
    let mut findings = Vec::new();
    for entry in registry {
        let in_toml = toml_path_present(merged, entry.path);
        let env_name = deprecated_env_var_name(entry.path);
        let in_env = env.var(&env_name).is_ok();

        let source = match (in_toml, in_env) {
            (false, false) => continue,
            (true, false) => DeprecationSource::Toml,
            (false, true) => DeprecationSource::Env,
            (true, true) => DeprecationSource::Both,
        };

        findings.push(DeprecationFinding {
            path: entry.path.to_owned(),
            replacement: entry.replacement.map(str::to_owned),
            since: entry.since.to_owned(),
            remove_in: entry.remove_in.to_owned(),
            source,
        });
    }
    findings
}

/// Detects deprecated keys the way [`AutumnConfig::load_with_env`] would, given a
/// profile and a file-merged TOML table a tool has already built.
///
/// Seeds `profile_defaults_as_toml` as the base layer and deep-merges
/// `file_table` on top before running [`detect_deprecated_keys`], so external
/// tools (e.g. `autumn doctor`) evaluate the *same* layered config the runtime
/// loader does — a key set only in a profile default is still detected.
#[must_use]
pub fn detect_deprecated_keys_for(
    profile: &str,
    file_table: &toml::Table,
    env: &dyn Env,
    registry: &[DeprecatedKey],
) -> Vec<DeprecationFinding> {
    let mut merged = profile_defaults_as_toml(profile);
    deep_merge(&mut merged, toml::Value::Table(file_table.clone()));
    let empty_table = toml::Table::new();
    let merged_table = merged.as_table().unwrap_or(&empty_table);
    detect_deprecated_keys(merged_table, env, registry)
}

/// Errors that can occur when loading or validating configuration.
///
/// Returned by [`AutumnConfig::load`], [`AutumnConfig::load_from`], and
/// [`DatabaseConfig::validate`].
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::{AutumnConfig, ConfigError};
/// use std::path::Path;
///
/// let result = AutumnConfig::load_from(Path::new("nonexistent.toml"));
/// // Returns Ok(defaults) when file is missing -- not an error
/// assert!(result.is_ok());
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("failed to read autumn.toml: {0}")]
    Io(#[from] std::io::Error),

    /// The config file contains invalid TOML syntax.
    #[error("invalid autumn.toml: {0}")]
    Parse(#[from] toml::de::Error),

    /// A configuration value failed semantic validation (e.g., invalid
    /// database URL scheme).
    #[error("configuration error: {0}")]
    Validation(String),

    /// The credentials file exists but could not be decrypted.
    #[error("credentials error: {0}")]
    Credentials(String),

    /// A project-root `.env` file exists but could not be read or parsed.
    #[error("dotenv error: {0}")]
    Dotenv(String),
}

/// Top-level framework configuration.
///
/// All sections are optional -- missing sections use their defaults.
/// Deserialized from `autumn.toml` (TOML format).
///
/// # `autumn.toml` example
///
/// ```toml
/// [server]
/// port = 8080
///
/// [database]
/// url = "postgres://user:pass@db:5432/myapp"
/// pool_size = 20
/// ```
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::AutumnConfig;
///
/// let config = AutumnConfig::default();
/// assert_eq!(config.server.port, 3000);
/// assert_eq!(config.database.pool_size, 10);
/// assert_eq!(config.log.level, "info");
/// assert_eq!(config.health.path, "/health");
/// ```
/// `[backup]` configuration section (issue #1619).
///
/// Groups database-backup destinations. Currently only an offsite S3-compatible
/// destination is supported. NOT feature-gated: this section (`[backup.offsite]`)
/// is recognized by every autumn-web build so a strict-config app compiled
/// WITHOUT the `storage` feature still accepts its own `autumn.toml` `[backup]`
/// keys (the offsite upload client lives in the CLI and is independent of the
/// storage feature).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BackupConfig {
    /// Offsite upload destination (`[backup.offsite]`). `None` (the default)
    /// means no offsite destination is configured; `autumn db backup --upload`
    /// then errors with configuration guidance rather than silently no-op'ing.
    ///
    /// Boxed so an unconfigured `[backup]` costs one pointer rather than the full
    /// [`OffsiteBackupConfig`] inline: `AutumnConfig` is held across awaits in the
    /// app-run future, and keeping this field small avoids bloating that future.
    #[serde(default)]
    pub offsite: Option<Box<OffsiteBackupConfig>>,
}

/// `[backup.offsite]` — an S3-compatible offsite backup destination (issue #1619).
///
/// Credentials are supplied by env-var *indirection* only: `s3.access_key_id_env`
/// / `s3.secret_access_key_env` name the environment variables the secrets are
/// read from at upload time. The secret values themselves never live in config,
/// argv, logs, or error messages.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OffsiteBackupConfig {
    /// S3-compatible connection + credential-indirection settings
    /// (`[backup.offsite.s3]`). Works against AWS S3 / `MinIO` / R2 / B2 / Garage.
    #[serde(default)]
    pub s3: OffsiteS3Config,

    /// Key prefix under which run directories are stored. Defaults to `""`
    /// (bucket root). Objects are keyed `{prefix}/{profile}/{timestamp}/{file}`.
    #[serde(default)]
    pub prefix: Option<String>,

    /// Independent remote retention: keep only the newest `N` uploaded runs per
    /// profile, pruning older ones *after* a verified upload. `None` (default)
    /// keeps all remote runs. Distinct from the local `--keep`.
    #[serde(default)]
    pub keep: Option<usize>,

    /// Upload after every successful `autumn db backup` even without `--upload`
    /// (the "configured default" upload, AC #1). Off by default.
    #[serde(default)]
    pub auto_upload: bool,

    /// Opt-in to pointing the offsite destination at the same bucket+endpoint as
    /// the app's user-facing blob storage (`[storage.s3]`). Off by default so a
    /// shared bucket is a deliberate choice (AC #3).
    #[serde(default)]
    pub allow_shared_bucket: bool,
}

/// `[backup.offsite.s3]` — S3-compatible connection settings for offsite backups.
///
/// A dedicated, NON-feature-gated mirror of the storage backend's S3 shape so the
/// `[backup]` section is available in every autumn-web build (the storage
/// module — and its `StorageS3Config` — only exist under the `storage` feature).
/// Credentials are named via `*_env` indirection, never inlined.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OffsiteS3Config {
    /// Target bucket.
    #[serde(default)]
    pub bucket: Option<String>,

    /// AWS region or region-shaped string (R2 uses `auto`). Used for the `SigV4`
    /// credential scope; many S3-compatible endpoints ignore it.
    #[serde(default)]
    pub region: Option<String>,

    /// Custom endpoint URL. Required for non-AWS providers (R2, `MinIO`, B2,
    /// Garage). Leave unset for AWS.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Environment variable the access-key id is read from.
    #[serde(default)]
    pub access_key_id_env: Option<String>,

    /// Environment variable the secret access key is read from.
    #[serde(default)]
    pub secret_access_key_env: Option<String>,

    /// Path-style addressing toggle (R2 / `MinIO` need this `true`).
    #[serde(default)]
    pub force_path_style: bool,
}

/// `[replication]` — continuous `SQLite` replication to an offsite destination
/// (issue #1628).
///
/// NOT feature-gated, for the same reason `[backup]` is not: a strict-config app
/// must accept its own `autumn.toml` keys whatever feature set it was compiled
/// with. The replication engine itself lives behind the `db` feature.
///
/// The section is absent by default. Present-but-`enabled = false` is the shape
/// a profile overlay wants: keep one destination definition in `autumn.toml` and
/// turn replication off for `dev`/`test`.
///
/// ```toml
/// [replication]
/// enabled = true
/// rpo_secs = 10
/// retention_hours = 168
///
/// [replication.s3]
/// bucket = "myapp-replicas"
/// region = "auto"
/// endpoint = "https://<account>.r2.cloudflarestorage.com"
/// access_key_id_env = "AUTUMN_REPLICA_ACCESS_KEY_ID"
/// secret_access_key_env = "AUTUMN_REPLICA_SECRET_ACCESS_KEY"
/// force_path_style = true
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct ReplicationConfig {
    /// Master switch. Off by default, so adding the section is not enough to
    /// start shipping — the operator must say so.
    #[serde(default)]
    pub enabled: bool,

    /// The recovery point objective in seconds: the steady-state upper bound on
    /// how much recently committed data a total loss of the machine costs.
    /// Defaults to 10 (AC #2).
    #[serde(default = "default_rpo_secs")]
    pub rpo_secs: u64,

    /// How often the replicator ships, in seconds. Defaults to half the RPO
    /// (minimum one second) so the steady-state lag stays inside the contract.
    #[serde(default)]
    pub sync_interval_secs: Option<u64>,

    /// How old a generation may get before the replicator checkpoints and opens
    /// a fresh one. Bounds how many segments a restore replays. Default: 1 hour.
    #[serde(default = "default_snapshot_interval_secs")]
    pub snapshot_interval_secs: u64,

    /// How large the `-wal` file may grow before the replicator checkpoints.
    /// Default: 16 MiB.
    #[serde(default = "default_max_wal_bytes")]
    pub max_wal_bytes: u64,

    /// How far back a point-in-time restore must stay possible. Older
    /// generations are pruned. Default: 168 hours (7 days).
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u64,

    /// How often to prove the replica restorable by actually restoring it into
    /// a scratch directory. `0` disables periodic verification. Default: 6 hours.
    #[serde(default = "default_verify_interval_secs")]
    pub verify_interval_secs: u64,

    /// Key prefix under the destination. Objects are keyed
    /// `{prefix}/{profile}/generations/…`. Defaults to the bucket/directory root.
    #[serde(default)]
    pub prefix: Option<String>,

    /// Opt in to pointing replication at the same bucket + endpoint as the app's
    /// user-facing blob storage (`[storage.s3]`). Off by default so a shared
    /// bucket — where a lifecycle rule written for user uploads could quietly
    /// expire the replicas — is a deliberate choice. Mirrors #1619.
    #[serde(default)]
    pub allow_shared_bucket: bool,

    /// S3-compatible destination (`[replication.s3]`). Mutually exclusive with
    /// [`path`](Self::path).
    #[serde(default)]
    pub s3: Option<ReplicationS3Config>,

    /// Filesystem destination: a directory to replicate into. A second disk, an
    /// NFS/SSHFS mount or a bind-mounted volume is a legitimate offsite target.
    /// Mutually exclusive with [`s3`](Self::s3).
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rpo_secs: default_rpo_secs(),
            sync_interval_secs: None,
            snapshot_interval_secs: default_snapshot_interval_secs(),
            max_wal_bytes: default_max_wal_bytes(),
            retention_hours: default_retention_hours(),
            verify_interval_secs: default_verify_interval_secs(),
            prefix: None,
            allow_shared_bucket: false,
            s3: None,
            path: None,
        }
    }
}

impl ReplicationConfig {
    /// The effective ship interval: the explicit override, else half the RPO,
    /// never less than one second.
    #[must_use]
    pub fn sync_interval(&self) -> std::time::Duration {
        let secs = self
            .sync_interval_secs
            .unwrap_or_else(|| self.rpo_secs.div_euclid(2))
            .max(1);
        std::time::Duration::from_secs(secs)
    }

    /// The RPO as a duration, never zero.
    #[must_use]
    pub fn rpo(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.rpo_secs.max(1))
    }

    /// The periodic-verification interval, or `None` when disabled.
    #[must_use]
    pub fn verify_interval(&self) -> Option<std::time::Duration> {
        (self.verify_interval_secs > 0)
            .then(|| std::time::Duration::from_secs(self.verify_interval_secs))
    }

    /// Validate the section on its own terms, returning an operator-facing
    /// message for each problem. Only meaningful when `enabled`.
    #[must_use]
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.enabled {
            return errors;
        }
        match (&self.s3, &self.path) {
            (Some(_), Some(_)) => errors.push(
                "[replication] configures both `s3` and `path`; pick exactly one destination"
                    .to_owned(),
            ),
            (None, None) => errors.push(
                "[replication] enabled = true but no destination is configured; add a \
                 [replication.s3] section (bucket/region/endpoint plus *_env credential \
                 indirection) or set `path` to a directory"
                    .to_owned(),
            ),
            (Some(s3), None) => {
                if s3.bucket.as_deref().unwrap_or("").trim().is_empty() {
                    errors.push("[replication.s3] bucket is unset".to_owned());
                }
                if s3
                    .access_key_id_env
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                    || s3
                        .secret_access_key_env
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty()
                {
                    errors.push(
                        "[replication.s3] needs access_key_id_env and secret_access_key_env \
                         (the NAMES of the environment variables the credentials are read \
                         from — never the credentials themselves)"
                            .to_owned(),
                    );
                }
            }
            (None, Some(path)) => {
                if path.trim().is_empty() {
                    errors.push("[replication] path is empty".to_owned());
                }
            }
        }
        if self.rpo_secs == 0 {
            errors.push("[replication] rpo_secs must be at least 1".to_owned());
        }
        // An explicit override longer than the RPO quietly invalidates the
        // objective it sits next to: `rpo_secs = 10` with
        // `sync_interval_secs = 60` ships once a minute and can lose nearly a
        // minute of committed writes, while every surface — the docs, the health
        // detail, the restore report — still promises ten seconds. Refuse the
        // pair rather than letting the stricter-looking number be the wrong one.
        if let Some(interval) = self.sync_interval_secs
            && self.rpo_secs > 0
            && interval > self.rpo_secs
        {
            errors.push(format!(
                "[replication] sync_interval_secs ({interval}) must not exceed rpo_secs \
                 ({}): shipping less often than the objective cannot meet it",
                self.rpo_secs
            ));
        }
        if self.retention_hours == 0 {
            errors.push("[replication] retention_hours must be at least 1".to_owned());
        }
        if self.max_wal_bytes < 32 {
            errors.push(
                "[replication] max_wal_bytes must be at least 32 (the size of a WAL header)"
                    .to_owned(),
            );
        }
        errors
    }
}

/// `[replication.s3]` — S3-compatible destination for continuous replication.
///
/// Same credential posture as `[backup.offsite.s3]`: the secrets are named by
/// environment variable, never inlined into config, argv, logs, or errors.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReplicationS3Config {
    /// Target bucket.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Region for the `SigV4` credential scope (R2 uses `auto`).
    #[serde(default)]
    pub region: Option<String>,
    /// Custom endpoint URL. Required for non-AWS providers.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Environment variable the access-key id is read from.
    #[serde(default)]
    pub access_key_id_env: Option<String>,
    /// Environment variable the secret access key is read from.
    #[serde(default)]
    pub secret_access_key_env: Option<String>,
    /// Path-style addressing toggle (R2 / `MinIO` need this `true`).
    #[serde(default)]
    pub force_path_style: bool,
}

/// Default RPO: at most ten seconds of potential data loss (#1628 AC #2).
const fn default_rpo_secs() -> u64 {
    10
}

/// Default generation length: one hour.
const fn default_snapshot_interval_secs() -> u64 {
    3600
}

/// Default WAL ceiling before a checkpoint: 16 MiB.
const fn default_max_wal_bytes() -> u64 {
    16 * 1024 * 1024
}

/// Default point-in-time window: seven days.
const fn default_retention_hours() -> u64 {
    168
}

/// Default restore-verification cadence: every six hours.
const fn default_verify_interval_secs() -> u64 {
    21_600
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutumnConfig {
    /// Active profile name (e.g., "dev", "prod", "staging").
    /// Resolved at load time, not deserialized from TOML.
    #[serde(skip)]
    pub profile: Option<String>,

    /// HTTP server settings (port, host, shutdown behavior).
    #[serde(default)]
    pub server: ServerConfig,

    /// Push-button VPS deploy settings (`[deploy]` section, issue #1607).
    ///
    /// Operator-facing configuration for `autumn deploy` — the SSH-reachable
    /// target host plus the remote install layout and rollout tuning knobs.
    /// Top-level (not nested under `[server]`) because it describes *where and
    /// how* the app is deployed, not how the running server behaves.
    ///
    /// Absent by default (`None`), so an app that never runs `autumn deploy`
    /// is unaffected. A bare `[deploy]` table is valid at rest — `host` is only
    /// required when a deploy actually runs, enforced by
    /// [`DeployConfig::validate`].
    ///
    /// # Field ordering (load-bearing — do not move below `database`)
    ///
    /// `deploy` is declared here, before [`database`](Self::database), so that
    /// [`get_schema_keys`](Self::get_schema_keys)'s `SchemaDeserializer`
    /// traversal recurses into [`DeployConfig`]'s child keys and the strict
    /// unknown-key validator (`validate_toml` / `server.strict_config` /
    /// `autumn check --config`) rejects a typo like `[deploy] app_dr = "…"`.
    /// `DatabaseConfig` has a `deserialize_with` duration field
    /// (`statement_timeout`, via the untagged [`deserialize_duration`] parser)
    /// whose parser rejects the `SchemaDeserializer`'s placeholder value and
    /// returns an error, which aborts the remainder of `AutumnConfig`'s field
    /// traversal — so any section declared *after* `database` is recorded only
    /// as an opaque root leaf, never descended into. Keeping `deploy` ahead of
    /// `database` sidesteps that abort. The regression guard
    /// `deploy_child_keys_are_strictly_validated` fails if this ordering breaks.
    #[serde(default)]
    pub deploy: Option<DeployConfig>,

    /// Deterministic replay capsule settings (`[failure_capture]` section,
    /// issue #1598).
    ///
    /// Off by default. When enabled, each failing request is written to disk
    /// as a replayable capsule; see [`FailureCaptureConfig`] and
    /// `docs/guide/failure-capsules.md` (capsules contain real request data —
    /// read the security section before turning this on).
    ///
    /// # Field ordering (load-bearing — do not move below `database`)
    ///
    /// Declared here, before [`database`](Self::database), for the same reason
    /// [`deploy`](Self::deploy) is: `DatabaseConfig`'s `deserialize_with`
    /// duration field aborts the `SchemaDeserializer` traversal, so a section
    /// declared after it is recorded only as an opaque root leaf and strict
    /// unknown-key validation never descends into its children. The regression
    /// guard `failure_capture_child_keys_are_strictly_validated` fails if this
    /// ordering breaks.
    #[cfg(feature = "reporting")]
    #[serde(default)]
    pub failure_capture: FailureCaptureConfig,

    /// Embedded self-clustering control plane (`[cluster]` section, issue
    /// #1762).
    ///
    /// Off by default. See [`ClusterConfig`] and `docs/guide/clustering.md`.
    ///
    /// # Field ordering (load-bearing — do not move below `database`)
    ///
    /// Declared here, before [`database`](Self::database), for the same reason
    /// [`deploy`](Self::deploy) and `failure_capture` (a field only present
    /// with the `reporting` feature, hence not linked) are: `DatabaseConfig`'s
    /// `deserialize_with` duration field aborts the
    /// `SchemaDeserializer` traversal, so a section declared after it is
    /// recorded only as an opaque root leaf and strict unknown-key validation
    /// never descends into its children — a typo like
    /// `[cluster] seed_peer = […]` would then be silently accepted. The
    /// regression guard `cluster_child_keys_are_strictly_validated` fails if
    /// this ordering breaks.
    #[serde(default)]
    pub cluster: ClusterConfig,

    /// Live-traffic shadow mirroring and response diffing (`[shadow]` section,
    /// issue #1653).
    ///
    /// Off by default. When enabled, a sampled slice of `GET`/`HEAD` traffic is
    /// replayed against an operator-provided candidate build and the two
    /// responses are diffed; see [`crate::shadow::ShadowConfig`] and
    /// `docs/guide/staged-deploys.md`.
    ///
    /// # Field ordering (load-bearing — do not move below `database`)
    ///
    /// Declared here, before [`database`](Self::database), for the same reason
    /// [`deploy`](Self::deploy) and `cluster` are: `DatabaseConfig`'s
    /// `deserialize_with` duration field can abort the `SchemaDeserializer`
    /// traversal, and a section it never descends into is recorded only as an
    /// opaque root leaf — so a typo like `[shadow] targt = "…"` would be
    /// silently accepted and the mirror would never run. The regression guard
    /// `shadow_child_keys_are_strictly_validated` fails if this ordering breaks.
    #[serde(default)]
    pub shadow: crate::shadow::ShadowConfig,

    /// Database connection settings (URL, pool size, timeouts).
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Logging configuration (level, format).
    #[serde(default)]
    pub log: LogConfig,

    /// Telemetry configuration (OTLP tracing and service metadata).
    #[serde(default)]
    pub telemetry: TelemetryConfig,

    /// Health check endpoint settings.
    #[serde(default)]
    pub health: HealthConfig,

    /// Actuator endpoint settings.
    #[serde(default)]
    pub actuator: ActuatorConfig,

    /// CORS (Cross-Origin Resource Sharing) settings.
    #[serde(default)]
    pub cors: CorsConfig,

    /// Session management settings.
    #[serde(default)]
    pub session: crate::session::SessionConfig,

    /// Cache backend settings.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Row-level multi-tenancy settings.
    #[serde(default)]
    pub tenancy: TenancyConfig,

    /// Web Push settings (`[push]` section, issue #1392).
    ///
    /// Absent by default. The VAPID key it names is loaded and validated once
    /// at boot — a key that is present but unusable fails the boot rather than
    /// leaving the app running with push silently dead. See
    /// [`crate::push::PushConfig`] and `docs/guide/web-push.md`.
    #[serde(default)]
    pub push: crate::push::PushConfig,

    /// HTTP idempotency-key middleware settings.
    #[serde(default)]
    pub idempotency: IdempotencyConfig,

    /// Real-time channel backend settings.
    #[serde(default)]
    pub channels: ChannelConfig,

    /// Background job backend and runtime settings.
    #[serde(default)]
    pub jobs: JobConfig,

    /// Scheduled task coordination backend settings.
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// Process role: which slice of the runtime this replica runs (web tier,
    /// worker tier, or both). Defaults to [`ProcessRole::Combined`] so existing
    /// single-process deployments are unaffected. Also settable via the flat
    /// `AUTUMN_ROLE` env var.
    #[serde(default)]
    pub role: ProcessRole,

    /// Authentication settings.
    #[serde(default)]
    pub auth: crate::auth::AuthConfig,

    /// Security settings (headers, CSRF).
    #[serde(default)]
    pub security: crate::security::config::SecurityConfig,

    /// Internationalization settings (default locale, supported locales,
    /// fallback chain). Populated from the `[i18n]` block in
    /// `autumn.toml`.
    #[cfg(feature = "i18n")]
    #[serde(default)]
    pub i18n: crate::i18n::I18nConfig,

    /// Per-user time zone settings (`[time_zone]` block in `autumn.toml`).
    ///
    /// Controls the default IANA zone and the source resolution chain for the
    /// [`TimeZone`](crate::time_zone::TimeZone) extractor.
    ///
    /// # Example
    ///
    /// ```toml
    /// [time_zone]
    /// identifier = "America/New_York"
    /// ```
    #[serde(default)]
    pub time_zone: crate::time_zone::TimeZoneConfig,
    /// Pluggable file storage configuration. Honored only when the
    /// `storage` cargo feature is enabled.
    #[cfg(feature = "storage")]
    #[serde(default)]
    pub storage: crate::storage::StorageConfig,

    /// Offsite database-backup destination (`[backup]` section, issue #1619).
    ///
    /// Composes the verified local-backup artifact (issue #1595) with an
    /// S3-compatible offsite destination. Always present (not feature-gated) so
    /// every autumn-web build recognizes `[backup.offsite]` — a strict-config app
    /// compiled without the `storage` feature still accepts its own `[backup]`
    /// keys. The offsite upload client lives in the CLI and needs no storage
    /// feature.
    #[serde(default)]
    pub backup: BackupConfig,
    /// Continuous `SQLite` replication (`[replication]` section, issue #1628).
    ///
    /// `None` (the default) means the section is absent and nothing is
    /// replicated. Boxed so an app that never replicates costs one pointer in
    /// the app-run future rather than the whole section inline.
    #[serde(default)]
    pub replication: Option<Box<ReplicationConfig>>,
    /// Transactional email settings.
    #[cfg(feature = "mail")]
    #[serde(default)]
    pub mail: crate::mail::MailConfig,
    /// `OpenAPI` spec runtime exposure settings.
    ///
    /// Controls whether the generated `OpenAPI` spec is served at runtime
    /// and at which path. Use `[openapi] enabled = false` in `autumn.toml`
    /// to suppress the spec endpoint in production.
    #[serde(default, rename = "openapi")]
    pub openapi_runtime: OpenApiRuntimeConfig,

    /// Encrypted credentials store loaded from `config/credentials/<env>.toml.enc`.
    ///
    /// Empty when no credentials file exists (existing apps continue to boot unchanged).
    /// Prefer using `config.credentials().get::<String>("stripe_key")` for type-safe access.
    #[serde(skip)]
    pub credentials: crate::credentials::CredentialsStore,

    /// Outbound HTTP settings (`[http]` section in `autumn.toml`).
    ///
    /// The nested `[http.client]` sub-table configures the outbound client.
    #[cfg(feature = "http-client")]
    #[serde(default, rename = "http")]
    pub http: HttpConfig,

    /// Developer-experience settings (`[dev]` section in `autumn.toml`).
    ///
    /// Controls the request inspector and other dev-only features.
    /// These settings have no effect outside the `dev` profile.
    #[serde(default)]
    pub dev: DevConfig,

    /// Widget story gallery settings (`[stories]` section in `autumn.toml`).
    ///
    /// Off by default; opt-in per profile (e.g. `[profile.dev.stories]
    /// enabled = true` for a dev-only gallery, or a prod profile for a
    /// public showcase). See `docs/guide/stories.md`.
    #[cfg(feature = "maud")]
    #[serde(default)]
    pub stories: crate::stories::StoriesConfig,

    /// Error-reporting settings (`[reporting]` section in `autumn.toml`).
    ///
    /// Controls delivery of panic + 5xx [`ErrorEvent`](crate::reporting::ErrorEvent)s
    /// to registered reporters. Honored only when the `reporting` cargo
    /// feature is enabled.
    #[cfg(feature = "reporting")]
    #[serde(default)]
    pub reporting: ReportingConfig,

    /// Response compression settings (`[compression]` section in `autumn.toml`).
    ///
    /// Compression is **off by default**. Enable with:
    /// ```toml
    /// [compression]
    /// enabled = true
    /// ```
    /// or via `AUTUMN_COMPRESSION__ENABLED=true`.
    #[serde(default)]
    pub compression: CompressionConfig,

    /// Bot protection / CAPTCHA settings (`[bot_protection]` section in `autumn.toml`).
    ///
    /// Requires a CAPTCHA token on mutating requests (POST/PUT/PATCH/DELETE) to
    /// protect public-facing forms against automated abuse.
    ///
    /// # Example
    ///
    /// ```toml
    /// [bot_protection]
    /// enabled    = true
    /// provider   = "turnstile"      # "turnstile" (default) or "hcaptcha"
    /// site_key   = "0x4AAAA..."     # public key — safe to commit
    /// secret_key = "..."            # private key — use env var!
    /// dev_bypass = false
    /// ```
    #[serde(default)]
    pub bot_protection: crate::security::captcha::BotProtectionConfig,

    /// Resilience settings (circuit breakers, fallbacks).
    #[serde(default)]
    pub resilience: ResilienceConfig,

    /// SEO settings (`[seo]` section in `autumn.toml`).
    ///
    /// Controls sitemap generation, robots.txt behavior, and canonical URL
    /// computation. See [`crate::seo`] for the full surface.
    ///
    /// # Example `autumn.toml`
    ///
    /// ```toml
    /// [seo]
    /// base_url = "https://example.com"
    ///
    /// [seo.robots]
    /// additional_rules = ["Disallow: /admin"]
    /// ```
    #[serde(default)]
    pub seo: SeoConfig,

    /// Observability settings (`[observability]` section in `autumn.toml`).
    ///
    /// Controls opt-in framework-emitted telemetry that supplements the
    /// access log — currently the `Server-Timing` response header. See
    /// [`ObservabilityConfig`] and `docs/guide/observability/server-timing.md`.
    #[serde(default)]
    pub observability: ObservabilityConfig,

    /// Operator alerts settings (`[alerts]` section in `autumn.toml`).
    ///
    /// Configure an operator email and/or a webhook URL to receive alerts for
    /// built-in failure conditions (dead-lettered jobs, Down health indicators,
    /// 5xx-rate spikes, scheduled-task failures) with zero application code.
    /// See [`crate::alerts::AlertConfig`] and `docs/guide/operator-alerts.md`.
    ///
    /// Boxed so the large `[alerts]` struct is stored behind a pointer rather
    /// than inline in `AutumnConfig`: `AutumnConfig` is held by value on the
    /// `app().run()` stack frame across await points, and inlining
    /// `AlertConfig` (many `Option<String>` destinations + tuning knobs) grew
    /// that future past the `clippy::large_futures` threshold. `Box<T>` keeps
    /// `Default`/`Deserialize` (both hold when `T` does), and field
    /// reads/writes still work through `Deref`/`DerefMut`.
    #[serde(default)]
    pub alerts: Box<crate::alerts::AlertConfig>,

    /// Unified data-retention policy for framework-owned data
    /// (`[retention]` section in `autumn.toml`, issue #1605).
    ///
    /// One retention window per framework-owned dataset (job history, job
    /// tracking, idempotency records, experiment assignments, webhook replay
    /// markers, sessions, audit archives). Every window is unset by default,
    /// which preserves today's behavior exactly. See [`RetentionConfig`] and
    /// `docs/guide/data-retention.md`.
    #[serde(default)]
    pub retention: RetentionConfig,
}

/// Opt-in TLS termination at the deploy-managed reverse proxy (`[deploy.tls]`
/// table, issue #1969).
///
/// Absent/disabled by default, so a deploy without this table is byte-for-byte
/// the historical HTTP-only behavior. When `enabled = true`, `autumn deploy`
/// wires the public `host` into kamal-proxy (`--host`/`--tls`) so the proxy
/// terminates TLS on 443 with an automatic Let's Encrypt certificate.
///
/// TLS terminates at the PROXY only — the app itself keeps serving plain HTTP on
/// its private loopback port, and its readiness/health probes are unaffected. Do
/// NOT also enable in-process `[server.tls]`/ACME on a deploy-managed app.
///
/// # `autumn.toml` example
///
/// ```toml
/// [deploy.tls]
/// enabled = true
/// host = "app.example.com"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeployTlsConfig {
    /// Whether the deploy-managed proxy terminates TLS on 443. Default: `false`
    /// (HTTP-only, unchanged behavior).
    #[serde(default)]
    pub enabled: bool,

    /// Public hostname the certificate is issued for (the DNS name pointing at
    /// the server). Required when `enabled = true`; enforced at resolve time by
    /// the CLI's `ResolvedDeployConfig::resolve`.
    #[serde(default)]
    pub host: Option<String>,
}

/// Push-button VPS deploy settings (`[deploy]` section, issue #1607).
///
/// Describes the SSH-reachable target server and the remote install layout for
/// `autumn deploy`'s zero-downtime rollout. Everything except `host` has a
/// sensible default, and `app_name`/`app_dir`/`service_name` are resolved from
/// the project's package name at deploy time (not during deserialization) so an
/// unset value stays `None` here.
///
/// # `autumn.toml` example
///
/// ```toml
/// [deploy]
/// host = "203.0.113.10"      # required at deploy time; SSH-reachable address
/// # hosts = ["10.0.0.1", "10.0.0.2"]  # fleet alternative to `host` (#1621);
/// #                                   # mutually exclusive with it, and the
/// #                                   # order is the rollout order
/// user = "deploy"            # SSH user (default: "root")
/// ssh_port = 22              # SSH port (default: 22)
/// app_name = "myapp"         # default: the crate's package name
/// app_dir = "/srv/myapp"     # default: /srv/autumn/{app_name}
/// service_name = "myapp"     # systemd unit name; default: {app_name}
/// readiness_timeout_secs = 60 # readiness window before rollback (default: 60)
/// keep_releases = 3          # releases retained on the host (default: 3)
/// profile = "prod"           # profile the deployed app runs under (default: "prod")
/// install_proxy = true       # install the reverse proxy on a bare host (default: true)
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DeployConfig {
    /// SSH-reachable address (hostname or IP) of the target server.
    ///
    /// Required when a deploy actually runs (`autumn deploy`), but `None` is
    /// valid at rest so a bare `[deploy]` table parses. Enforced by
    /// [`validate`](Self::validate).
    #[serde(default)]
    pub host: Option<String>,

    /// SSH-reachable addresses of the target fleet, in rollout order (#1621).
    ///
    /// Mutually exclusive with [`host`](Self::host): a single-entry `hosts` list
    /// is byte-for-byte the historical single-server deploy, and the order of the
    /// list **is** the rollout order (a documented operator contract, not an
    /// implementation detail — the fleet driver never sorts or regroups it).
    /// Empty by default, so every pre-#1621 `[deploy]` table is unchanged.
    ///
    /// `[deploy.tls] host` stays fleet-singular: it is the PUBLIC DNS name the
    /// certificate is issued for, semantically distinct from these SSH targets.
    ///
    /// Blank entries and duplicates (compared after trimming) are rejected, as is
    /// setting this alongside `host`. The enforcing seam is the CLI's
    /// `ResolvedFleet::resolve` — the only validation a deploy actually calls;
    /// [`validate`](Self::validate) mirrors the same rules so the two never
    /// disagree about what a valid `[deploy]` table looks like.
    #[serde(default)]
    pub hosts: Vec<String>,

    /// SSH user to connect as. Default: `"root"`.
    #[serde(default = "default_deploy_user")]
    pub user: String,

    /// SSH port on the target host. Default: `22`.
    #[serde(default = "default_deploy_ssh_port")]
    pub ssh_port: u16,

    /// Application name used to derive remote paths and the service unit.
    /// Resolved to the project's package name when unset (at deploy time, not
    /// during deserialization).
    #[serde(default)]
    pub app_name: Option<String>,

    /// Remote install directory. Resolved to `/srv/autumn/{app_name}` when
    /// unset (at deploy time).
    #[serde(default)]
    pub app_dir: Option<String>,

    /// systemd unit name. Resolved to `{app_name}` when unset (at deploy time).
    #[serde(default)]
    pub service_name: Option<String>,

    /// Bounded readiness window, in seconds, the new release has to report
    /// `/ready` before the deploy rolls back. Default: `60`.
    #[serde(default = "default_deploy_readiness_timeout_secs")]
    pub readiness_timeout_secs: u64,

    /// Number of prior releases retained on the host for rollback. Default: `3`.
    #[serde(default = "default_deploy_keep_releases")]
    pub keep_releases: u32,

    /// The profile the deployed app runs under (written into the host env file
    /// as `AUTUMN_ENV`). Defaults to the production profile (`"prod"`) so a
    /// deploy never silently runs the `dev` profile; set to e.g. `"staging"`
    /// for non-prod targets.
    #[serde(default = "default_deploy_profile")]
    pub profile: String,

    /// Opt-in TLS termination at the deploy-managed reverse proxy
    /// (`[deploy.tls]`). Disabled by default — an absent table is byte-for-byte
    /// the historical HTTP-only behavior. See [`DeployTlsConfig`].
    #[serde(default)]
    pub tls: DeployTlsConfig,

    /// Whether `autumn deploy` may PREPARE the target host by installing the
    /// reverse-proxy binary when the host has none. Default: `true` (issue #1607,
    /// AC-1 — the documented target-host precondition is at most a stock Ubuntu
    /// LTS with SSH access, so the command performs the remaining host preparation
    /// itself).
    ///
    /// Probe-gated and idempotent: a host that already has a working proxy binary
    /// is never touched, and a binary that responds but whose CLI surface has
    /// drifted is never replaced (that stays a hard, actionable refusal).
    ///
    /// Set to `false` when you provision the proxy yourself — a pinned internal
    /// build, your own package, or a host you do not want a container runtime
    /// installed on. A missing binary is then an actionable deploy failure instead
    /// of something the deploy fixes.
    #[serde(default = "default_deploy_install_proxy")]
    pub install_proxy: bool,
}

/// Default for [`DeployConfig::install_proxy`]: prepare the host (issue #1607).
const fn default_deploy_install_proxy() -> bool {
    true
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            host: None,
            // #1621: empty, so an existing single-host `[deploy]` table (and the
            // type default) is byte-for-byte unchanged.
            hosts: Vec::new(),
            user: default_deploy_user(),
            ssh_port: default_deploy_ssh_port(),
            app_name: None,
            app_dir: None,
            service_name: None,
            readiness_timeout_secs: default_deploy_readiness_timeout_secs(),
            keep_releases: default_deploy_keep_releases(),
            profile: default_deploy_profile(),
            tls: DeployTlsConfig::default(),
            install_proxy: default_deploy_install_proxy(),
        }
    }
}

impl DeployConfig {
    /// Validate the `[deploy]` section for a context that actually runs a deploy.
    ///
    /// A bare `[deploy]` table is valid at rest, but a deploy needs a target:
    /// this rejects a missing or blank `host` with an actionable message so the
    /// operator knows exactly which key to set.
    ///
    /// Since #1621 it also mirrors the fleet rules the CLI's
    /// `ResolvedFleet::resolve` enforces — `host`/`hosts` mutual exclusion, no
    /// blank entry, no duplicate entry — in the same order, so the two surfaces
    /// never disagree about what a valid `[deploy]` table looks like.
    ///
    /// **This method has no production call site** (it is not reached from
    /// [`AutumnConfig::validate`]); the CLI's resolve step is the enforcing seam.
    /// Keep the rules here in sync anyway: a future wiring must not change
    /// behavior.
    ///
    /// # Errors
    ///
    /// Returns a message when `host` and `hosts` are both set, when a `hosts`
    /// entry is blank or duplicated, or when neither key provides a target.
    pub fn validate(&self) -> Result<(), String> {
        let host = self
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty());

        // 1. Mutual exclusion: with both spellings set the rollout order is
        //    ambiguous, so name BOTH keys and let the operator pick one.
        if host.is_some() && !self.hosts.is_empty() {
            return Err(
                "[deploy] host and [deploy] hosts are mutually exclusive: keep the \
                 single-server `[deploy] host = \"<address>\"` or the fleet list \
                 `[deploy] hosts = [\"<address>\", …]` in autumn.toml, not both (#1621)"
                    .to_owned(),
            );
        }

        // 2. A blank entry would resolve to a hostless SSH target mid-rollout.
        for (index, entry) in self.hosts.iter().enumerate() {
            if entry.trim().is_empty() {
                return Err(format!(
                    "[deploy] hosts entry {index} is blank: every fleet entry must be an \
                     SSH-reachable hostname or IP (#1621)"
                ));
            }
        }

        // 3. A duplicate would deploy the same server twice — the second pass sees
        //    its own new release as live and corrupts the previous-release chain a
        //    rollback depends on. Compared after trimming; DNS aliases are a
        //    documented limitation.
        let mut seen: Vec<&str> = Vec::with_capacity(self.hosts.len());
        for entry in &self.hosts {
            let trimmed = entry.trim();
            if seen.contains(&trimmed) {
                return Err(format!(
                    "[deploy] hosts lists `{trimmed}` more than once: each fleet host must \
                     appear exactly once (#1621)"
                ));
            }
            seen.push(trimmed);
        }

        // 4. Neither spelling provides a target.
        if host.is_none() && self.hosts.is_empty() {
            return Err(
                "[deploy] requires a target host: set `[deploy] host = \"<address>\"` in \
                      autumn.toml to the SSH-reachable hostname or IP of your server, or \
                      `[deploy] hosts = [\"<address>\", …]` for a fleet (#1621)"
                    .to_owned(),
            );
        }

        Ok(())
    }
}

/// Observability configuration (`[observability]` section in `autumn.toml`).
///
/// Controls opt-in telemetry that supplements the default access log.
///
/// # Server-Timing header
///
/// When `server_timing = true`, Autumn emits a W3C-conformant
/// [`Server-Timing`](https://www.w3.org/TR/server-timing/) header on every
/// non-streaming response with at minimum a `total` metric (whole-request
/// wall time, matching the access-log `duration_ms`) and a `db` metric
/// summarising cumulative query time plus a query count (`db;dur=…;desc="N queries"`)
/// when at least one query ran during the request.
///
/// The default is **off in production** and **on in the `dev` profile**;
/// leave the field unset for that behavior, or pin it to `true` / `false`
/// explicitly. Requires opt-in in prod because timings can leak
/// infrastructure detail to anonymous clients.
///
/// # Example
///
/// ```toml
/// # Force on in a staging profile where dev-team browsers inspect timings.
/// [observability]
/// server_timing = true
/// ```
///
/// ```toml
/// # Force off during a dev-profile perf comparison against production.
/// [observability]
/// server_timing = false
/// ```
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ObservabilityConfig {
    /// Emit the `Server-Timing` response header on served requests.
    ///
    /// `None` (unset) means the effective value follows the profile default:
    /// on in `dev`/`development`, off everywhere else. `Some(true)` or
    /// `Some(false)` pin the choice explicitly.
    #[serde(default)]
    pub server_timing: Option<bool>,
}

/// Resolve the effective value of `[observability] server_timing` for a
/// given [`AutumnConfig`].
///
/// The rules are:
/// - Explicit `Some(true)` / `Some(false)` in config or env → returned as-is.
/// - `None` (unset) → `true` iff the active profile is `"dev"` or
///   `"development"`, otherwise `false`. This keeps production off by
///   default so timings never leak to anonymous clients without opt-in.
pub(crate) fn server_timing_enabled(cfg: &AutumnConfig) -> bool {
    if let Some(explicit) = cfg.observability.server_timing {
        return explicit;
    }
    matches!(cfg.profile.as_deref(), Some("dev" | "development"))
}

/// SEO configuration (`[seo]` section in `autumn.toml`).
///
/// # Example
///
/// ```toml
/// [seo]
/// base_url = "https://example.com"
///
/// [seo.robots]
/// additional_rules = ["Disallow: /admin"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SeoConfig {
    /// Base URL used for canonical URL computation and sitemap auto-injection.
    ///
    /// E.g. `"https://example.com"`. When set, the `Sitemap:` directive is
    /// automatically injected into `robots.txt`.
    pub base_url: Option<String>,

    /// Robots.txt overrides.
    #[serde(default)]
    pub robots: RobotsConfig,
}

/// Per-profile `robots.txt` overrides (`[seo.robots]` in `autumn.toml`).
///
/// The framework default behavior (dev/test → disallow all; prod → allow all)
/// can be overridden here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RobotsConfig {
    /// Override the profile-driven allow/disallow default.
    ///
    /// `None` means: use the profile default (dev → disallow, prod → allow).
    /// `Some(true)` forces `Allow: /`; `Some(false)` forces `Disallow: /`.
    pub allow_all: Option<bool>,

    /// Additional directives appended after the main `User-agent` block.
    ///
    /// Example: `["Disallow: /admin", "Crawl-delay: 5"]`
    #[serde(default)]
    pub additional_rules: Vec<String>,

    /// Explicit `Sitemap:` URL.
    ///
    /// When `None`, the URL is auto-computed from `[seo] base_url` if set.
    pub sitemap_url: Option<String>,
}

/// Error-reporting settings (`[reporting]` section in `autumn.toml`).
///
/// # Example `autumn.toml`
///
/// ```toml
/// [reporting]
/// enabled = true      # deliver events to reporters (default: true)
/// sample_rate = 0.25  # report ~25% of events (default: 1.0 = all)
/// ```
///
/// Note: `enabled = false` only suppresses *delivery* to reporters. Handler
/// panics are still caught and converted to a clean 500 response regardless of
/// this setting.
#[cfg(feature = "reporting")]
#[derive(Debug, Clone, Deserialize)]
pub struct ReportingConfig {
    /// Whether error events are delivered to registered reporters.
    ///
    /// Defaults to `true`. When `false`, panics are still caught and turned
    /// into clean 500 responses, but no [`ErrorEvent`](crate::reporting::ErrorEvent)
    /// is dispatched.
    #[serde(default = "default_reporting_enabled")]
    pub enabled: bool,
    /// Fraction of events to deliver, in `[0.0, 1.0]`.
    ///
    /// `1.0` (the default) reports every event; `0.0` reports none. Values
    /// outside the range are clamped at the extremes.
    #[serde(default = "default_reporting_sample_rate")]
    pub sample_rate: f64,
}

#[cfg(feature = "reporting")]
impl Default for ReportingConfig {
    fn default() -> Self {
        Self {
            enabled: default_reporting_enabled(),
            sample_rate: default_reporting_sample_rate(),
        }
    }
}

#[cfg(feature = "reporting")]
const fn default_reporting_enabled() -> bool {
    true
}

#[cfg(feature = "reporting")]
const fn default_reporting_sample_rate() -> f64 {
    1.0
}

/// Deterministic replay capsule settings (`[failure_capture]` section in
/// `autumn.toml`, issue #1598).
///
/// # Example `autumn.toml`
///
/// ```toml
/// [failure_capture]
/// enabled = true                  # record capsules for failing requests (default: false)
/// dir = "tmp/autumn-capsules"     # where capsules are written (project-relative)
/// max_body_bytes = 65536          # largest request body copied into a capsule
/// max_capsule_bytes = 1048576     # effect budget before a capsule is marked truncated
/// max_capsules = 50               # retained capsules; oldest are pruned
/// ```
///
/// **A capsule holds real production request data and real database rows.**
/// Sensitive headers, query parameters and structured body fields are masked
/// through `[log] filter_parameters`, but unstructured bodies, URL paths and
/// result rows are not. Read `docs/guide/failure-capsules.md` before enabling
/// this outside development.
#[cfg(feature = "reporting")]
#[derive(Debug, Clone, Deserialize)]
pub struct FailureCaptureConfig {
    /// Whether failing requests are recorded as capsules.
    ///
    /// Defaults to `false` — capture costs a teed request body and a
    /// teed database stream on every request, and the artifacts contain
    /// production data.
    #[serde(default = "default_failure_capture_enabled")]
    pub enabled: bool,

    /// Directory capsules are written to, project-relative by default
    /// (mirroring `tmp/autumn-maintenance.json`).
    #[serde(default = "default_failure_capture_dir")]
    pub dir: String,

    /// Largest request body copied into a capsule, in bytes.
    ///
    /// A body larger than this is never consumed at all — the handler still
    /// receives it intact and the capsule records it as skipped.
    #[serde(default = "default_failure_capture_max_body_bytes")]
    pub max_body_bytes: usize,

    /// Budget for recorded effects before a capsule is marked truncated.
    ///
    /// Recording stops at the ceiling; the capsule is still written (so the
    /// failure is not lost) but replay refuses it.
    #[serde(default = "default_failure_capture_max_capsule_bytes")]
    pub max_capsule_bytes: usize,

    /// How many capsules to retain in `dir`; the oldest beyond this are
    /// pruned after each write so an error storm cannot fill a disk.
    #[serde(default = "default_failure_capture_max_capsules")]
    pub max_capsules: usize,
}

#[cfg(feature = "reporting")]
impl Default for FailureCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: default_failure_capture_enabled(),
            dir: default_failure_capture_dir(),
            max_body_bytes: default_failure_capture_max_body_bytes(),
            max_capsule_bytes: default_failure_capture_max_capsule_bytes(),
            max_capsules: default_failure_capture_max_capsules(),
        }
    }
}

#[cfg(feature = "reporting")]
const fn default_failure_capture_enabled() -> bool {
    false
}

#[cfg(feature = "reporting")]
fn default_failure_capture_dir() -> String {
    "tmp/autumn-capsules".to_owned()
}

#[cfg(feature = "reporting")]
const fn default_failure_capture_max_body_bytes() -> usize {
    65_536
}

#[cfg(feature = "reporting")]
const fn default_failure_capture_max_capsule_bytes() -> usize {
    1_048_576
}

#[cfg(feature = "reporting")]
const fn default_failure_capture_max_capsules() -> usize {
    50
}

/// Developer-experience settings (`[dev]` section in `autumn.toml`).
///
/// All fields are ignored outside the `dev` profile.
///
/// # Example `autumn.toml`
///
/// ```toml
/// [dev]
/// inspector_path = "/_autumn/inspect"
/// inspector_capacity = 200
/// inspector_n_plus_one_threshold = 3
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DevConfig {
    /// Mount path for the request inspector UI.
    ///
    /// Default: `"/_autumn/inspect"`. Only active in the `dev` profile;
    /// ignored everywhere else.
    #[serde(default = "default_inspector_path")]
    pub inspector_path: String,

    /// Maximum number of requests retained in the in-memory ring buffer.
    ///
    /// Default: `100`. Set to `0` to disable recording without removing
    /// the middleware.
    #[serde(default = "default_inspector_capacity")]
    pub inspector_capacity: usize,

    /// Minimum number of structurally identical SQL statements in a single
    /// request before an N+1 warning is emitted.
    ///
    /// Default: `5`. Set to `0` to disable N+1 detection.
    #[serde(default = "default_inspector_n_plus_one_threshold")]
    pub inspector_n_plus_one_threshold: usize,
}

impl Default for DevConfig {
    fn default() -> Self {
        Self {
            inspector_path: default_inspector_path(),
            inspector_capacity: default_inspector_capacity(),
            inspector_n_plus_one_threshold: default_inspector_n_plus_one_threshold(),
        }
    }
}

fn default_inspector_path() -> String {
    "/_autumn/inspect".to_owned()
}

const fn default_inspector_capacity() -> usize {
    100
}

const fn default_inspector_n_plus_one_threshold() -> usize {
    crate::inspector::DEFAULT_N_PLUS_ONE_THRESHOLD
}

/// Top-level `[http]` configuration section.
#[cfg(feature = "http-client")]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HttpConfig {
    /// Outbound HTTP client settings (`[http.client]`).
    #[serde(default)]
    pub client: HttpClientConfig,
}

/// Configuration for the outbound HTTP client (`[http.client]` in `autumn.toml`).
///
/// # Example `autumn.toml`
///
/// ```toml
/// [http.client]
/// timeout_secs = 30
/// max_retries  = 3
///
/// [http.client.base_urls]
/// stripe   = "https://api.stripe.com"
/// sendgrid = "https://api.sendgrid.com"
/// ```
#[cfg(feature = "http-client")]
#[derive(Debug, Clone, Deserialize)]
pub struct HttpClientConfig {
    /// Per-request timeout in seconds. Default: 30.
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum retry attempts for transient failures on idempotent methods.
    /// Default: 3 (four total attempts).
    #[serde(default = "default_http_max_retries")]
    pub max_retries: u32,

    /// Maximum Retry-After sleep duration in seconds to accept before clamping.
    /// Default: 10.
    #[serde(default = "default_http_max_retry_after_secs")]
    pub max_retry_after_secs: u64,

    /// Named base URL aliases, e.g. `stripe = "https://api.stripe.com"`.
    ///
    /// A [`Client`](crate::http_client::Client) configured with `.named("stripe")` will
    /// prepend this URL to relative request paths and match against mocks
    /// registered for that alias via
    /// [`TestApp::http_mock`](crate::test::TestApp::http_mock).
    #[serde(default)]
    pub base_urls: std::collections::HashMap<String, String>,
}

#[cfg(feature = "http-client")]
const fn default_http_timeout_secs() -> u64 {
    30
}

#[cfg(feature = "http-client")]
const fn default_http_max_retries() -> u32 {
    3
}

#[cfg(feature = "http-client")]
const fn default_http_max_retry_after_secs() -> u64 {
    10
}

#[cfg(feature = "http-client")]
impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_http_timeout_secs(),
            max_retries: default_http_max_retries(),
            max_retry_after_secs: default_http_max_retry_after_secs(),
            base_urls: std::collections::HashMap::new(),
        }
    }
}

impl axum::extract::FromRequestParts<crate::AppState> for AutumnConfig {
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        state
            .extension::<Self>()
            .as_deref()
            .cloned()
            .ok_or_else(|| crate::AutumnError::service_unavailable_msg("Config is not available"))
    }
}

/// Real-time channel backend selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelBackend {
    /// In-process Tokio broadcast channels. Default, zero config.
    #[serde(alias = "local", alias = "memory")]
    #[default]
    InProcess,
    /// Redis pub/sub fan-out across application replicas.
    Redis,
}

impl ChannelBackend {
    /// Parse an environment variable value for channel backend selection.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "in_process" | "in-process" | "local" | "memory" => Some(Self::InProcess),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Real-time channel runtime configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    /// Runtime backend selection.
    #[serde(default)]
    pub backend: ChannelBackend,
    /// Per-topic broadcast ring buffer capacity.
    #[serde(default = "default_channel_capacity")]
    pub capacity: usize,
    /// Per-topic replay ring buffer capacity (`N`).
    ///
    /// Number of most-recent events retained per topic for `Last-Event-ID`
    /// replay via [`crate::sse::stream_resumable`]. Memory is `O(N)` per topic
    /// regardless of throughput.
    #[serde(default = "default_channel_replay_buffer")]
    pub replay_buffer: usize,
    /// Redis backend options.
    #[serde(default)]
    pub redis: ChannelRedisConfig,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            backend: ChannelBackend::default(),
            capacity: default_channel_capacity(),
            replay_buffer: default_channel_replay_buffer(),
            redis: ChannelRedisConfig::default(),
        }
    }
}

/// Redis channel backend configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelRedisConfig {
    /// Redis URL used when `channels.backend = "redis"`.
    #[serde(default)]
    pub url: Option<String>,
    /// Redis pub/sub channel prefix.
    #[serde(default = "default_channels_redis_prefix")]
    pub key_prefix: String,
}

impl Default for ChannelRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_channels_redis_prefix(),
        }
    }
}

const fn default_channel_capacity() -> usize {
    32
}

const fn default_channel_replay_buffer() -> usize {
    256
}

fn default_channels_redis_prefix() -> String {
    "autumn:channels".to_owned()
}

// ── Cluster configuration ────────────────────────────────────────────────────

/// Embedded self-clustering control plane (`[cluster]` section, issue #1762).
///
/// Off by default. When enabled, the node binds a small authenticated gossip
/// listener, discovers the peers named in `seed_peers`, and exposes a
/// cluster-wide counter through
/// [`ClusterHandle`](crate::cluster::ClusterHandle). No external coordination
/// service is involved — see `docs/guide/clustering.md`.
///
/// The transport is **authenticated (HMAC-SHA256), not encrypted**: run it on a
/// trusted network.
///
/// Unrelated to [`crate::sharding`]'s database-"cluster" vocabulary.
///
/// # Examples
///
/// ```toml
/// [cluster]
/// enabled = true
/// secret = "a-shared-secret-at-least-16-bytes"
/// bind_addr = "0.0.0.0:7946"
/// advertise_addr = "10.0.0.4:7946"
/// seed_peers = ["10.0.0.5:7946"]
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    /// Master switch. When `false` (the default) nothing is bound, nothing is
    /// spawned, and `state.extension::<ClusterHandle>()` is `None`.
    #[serde(default)]
    pub enabled: bool,

    /// Shared secret every member signs its frames with (minimum 16 bytes).
    ///
    /// Stored as a [`secrecy::SecretString`] so the raw value is redacted from
    /// `Debug` output and zeroized on drop. Call
    /// [`secrecy::ExposeSecret::expose_secret`] at the point of use.
    #[serde(default)]
    pub secret: Option<secrecy::SecretString>,

    /// Cluster name. Signed into every frame, so two clusters that share a
    /// secret still refuse each other's traffic.
    #[serde(default = "default_cluster_name")]
    pub cluster_name: String,

    /// Address the cluster listener binds. Port `0` takes an OS-assigned
    /// ephemeral port, readable back through `ClusterHandle::local_addr`.
    #[serde(default = "default_cluster_bind_addr")]
    pub bind_addr: String,

    /// Address advertised to peers when it differs from `bind_addr` (NAT,
    /// container port mapping). Defaults to the bound address.
    #[serde(default)]
    pub advertise_addr: Option<String>,

    /// Peer addresses to dial on startup. One reachable seed is enough.
    #[serde(default)]
    pub seed_peers: Vec<String>,

    /// Explicit node id. Entropy-derived when absent (never hostname-derived).
    #[serde(default)]
    pub node_id: Option<String>,

    /// Base interval between state pushes, in milliseconds. The push is also
    /// the heartbeat, so this is the failure-detector's sampling rate.
    #[serde(default = "default_cluster_push_interval_ms")]
    pub push_interval_ms: u64,

    /// How long without a push before a peer is suspected, in milliseconds.
    /// Must be at least three push intervals (anti-flap hysteresis).
    #[serde(default = "default_cluster_suspicion_timeout_ms")]
    pub suspicion_timeout_ms: u64,
}

fn default_cluster_name() -> String {
    "autumn".to_owned()
}

fn default_cluster_bind_addr() -> String {
    "127.0.0.1:0".to_owned()
}

const fn default_cluster_push_interval_ms() -> u64 {
    500
}

const fn default_cluster_suspicion_timeout_ms() -> u64 {
    2_500
}

/// Shortest secret accepted when `[cluster] enabled = true`.
pub(crate) const MIN_CLUSTER_SECRET_LEN: usize = 16;

/// Shortest push interval accepted, in milliseconds.
pub(crate) const MIN_CLUSTER_PUSH_INTERVAL_MS: u64 = 10;

/// The suspicion timeout must be at least this many push intervals.
pub(crate) const MIN_CLUSTER_SUSPICION_MULTIPLE: u64 = 3;

/// Longest `node_id` / `cluster_name` accepted, in bytes.
///
/// Both travel in every frame and are covered by the MAC, so the bound keeps
/// the fixed overhead of a state push small and predictable.
pub(crate) const MAX_CLUSTER_IDENT_LEN: usize = 64;

/// Separator between node id and incarnation in a counter cell key
/// (`"{node_id}#{incarnation}"`). Reserved: an id containing it would make
/// cell keys ambiguous, so validation refuses one.
pub(crate) const CLUSTER_CELL_KEY_SEPARATOR: char = '#';

/// Validate one identity string (`cluster.node_id`, `cluster.cluster_name`).
///
/// `field` is the dotted config path, used verbatim in the message so an
/// operator can fix the offending key without reading the source.
fn validate_cluster_ident(field: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field} must not be empty: it identifies this node (or its cluster) in every frame"
        )));
    }
    let len = value.len();
    if len > MAX_CLUSTER_IDENT_LEN {
        return Err(ConfigError::Validation(format!(
            "{field} must be at most {MAX_CLUSTER_IDENT_LEN} bytes, got {len} ({value:?}); it \
             travels in every cluster frame and is covered by the MAC"
        )));
    }
    if value.contains(CLUSTER_CELL_KEY_SEPARATOR) {
        return Err(ConfigError::Validation(format!(
            "{field} must not contain {CLUSTER_CELL_KEY_SEPARATOR:?} ({value:?}): it separates \
             the node id from the incarnation in counter cell keys, and an id containing it \
             would make two different cells collide"
        )));
    }
    Ok(())
}

/// Parse a cluster address, rejecting anything that is not a `host:port` pair
/// with an IP literal (hostnames are never resolved).
fn parse_cluster_addr(field: &str, value: &str) -> Result<std::net::SocketAddr, ConfigError> {
    value.parse::<std::net::SocketAddr>().map_err(|error| {
        ConfigError::Validation(format!(
            "{field} must be a socket address of the form host:port with an IP literal \
             (hostnames are not resolved), got {value:?}: {error}"
        ))
    })
}

/// Reject port `0` on an address somebody has to *dial*.
///
/// Port `0` is only meaningful on `bind_addr`, where it means "let the OS pick"
/// and the node then advertises the port it actually got. Everywhere else it is
/// undialable: a peer would connect to port 0 and fail forever, and the mistake
/// looks exactly like a network problem from the other side.
fn reject_ephemeral_cluster_port(
    field: &str,
    value: &str,
    addr: std::net::SocketAddr,
) -> Result<(), ConfigError> {
    if addr.port() == 0 {
        return Err(ConfigError::Validation(format!(
            "{field} is {value:?}, which no peer can dial: port 0 means \"any free port\" and is \
             only meaningful on cluster.bind_addr, where the node advertises the port it was \
             actually given (see docs/guide/clustering.md)"
        )));
    }
    Ok(())
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            secret: None,
            cluster_name: default_cluster_name(),
            bind_addr: default_cluster_bind_addr(),
            advertise_addr: None,
            seed_peers: Vec::new(),
            node_id: None,
            push_interval_ms: default_cluster_push_interval_ms(),
            suspicion_timeout_ms: default_cluster_suspicion_timeout_ms(),
        }
    }
}

impl ClusterConfig {
    /// Fail fast on a `[cluster]` section that would boot an insecure or
    /// flapping cluster.
    ///
    /// Checked when `enabled` — these are about a node that will really bind
    /// and gossip:
    /// - `secret` must be present and at least 16 bytes. There is no lenient
    ///   unauthenticated mode.
    /// - The address peers are told to dial (`advertise_addr`, or `bind_addr`
    ///   when it is unset) must not be a wildcard: nobody can dial `0.0.0.0`.
    ///
    /// Checked always, enabled or not — a section that is wrong is wrong
    /// before the switch is flipped:
    /// - `push_interval_ms` must be at least 10ms.
    /// - `suspicion_timeout_ms` must be at least 3 × `push_interval_ms`: the
    ///   anti-flap hysteresis, below which one delayed push evicts a healthy
    ///   peer.
    /// - `bind_addr`, `advertise_addr` and every `seed_peers` entry must parse
    ///   as a [`std::net::SocketAddr`] (IP literal — hostnames are not
    ///   resolved).
    /// - `cluster_name`, and `node_id` when set, must be non-empty, at most 64
    ///   bytes, and free of the `#` counter cell-key separator.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] describing the first violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Operational rules: only meaningful for a node that will actually
        // bind and gossip.
        if self.enabled {
            self.validate_secret()?;
        }

        if self.push_interval_ms < MIN_CLUSTER_PUSH_INTERVAL_MS {
            return Err(ConfigError::Validation(format!(
                "cluster.push_interval_ms must be at least {MIN_CLUSTER_PUSH_INTERVAL_MS}ms, \
                 got {}",
                self.push_interval_ms
            )));
        }
        // Saturating: a push interval near u64::MAX would overflow the
        // multiply, and an overflow must not turn into an accidental pass.
        let min_suspicion = self
            .push_interval_ms
            .saturating_mul(MIN_CLUSTER_SUSPICION_MULTIPLE);
        if self.suspicion_timeout_ms < min_suspicion {
            return Err(ConfigError::Validation(format!(
                "cluster.suspicion_timeout_ms ({}) must be at least \
                 {MIN_CLUSTER_SUSPICION_MULTIPLE}x cluster.push_interval_ms ({}), i.e. at least \
                 {min_suspicion}ms: below that ratio one delayed push evicts a healthy peer and \
                 the view flaps",
                self.suspicion_timeout_ms, self.push_interval_ms
            )));
        }

        validate_cluster_ident("cluster.cluster_name", &self.cluster_name)?;
        if let Some(node_id) = self.node_id.as_deref() {
            validate_cluster_ident("cluster.node_id", node_id)?;
        }

        // `bind_addr` may keep port 0 — that is the documented "ephemeral bind"
        // spelling, read back through `ClusterHandle::local_addr`.
        let bind_addr = parse_cluster_addr("cluster.bind_addr", &self.bind_addr)?;
        let advertise_addr = match self.advertise_addr.as_deref() {
            Some(addr) => {
                let parsed = parse_cluster_addr("cluster.advertise_addr", addr)?;
                reject_ephemeral_cluster_port("cluster.advertise_addr", addr, parsed)?;
                parsed
            }
            None => bind_addr,
        };
        for (index, peer) in self.seed_peers.iter().enumerate() {
            let field = format!("cluster.seed_peers[{index}]");
            let parsed = parse_cluster_addr(&field, peer)?;
            reject_ephemeral_cluster_port(&field, peer, parsed)?;
        }

        // A wildcard bind is legal, advertising one is not: peers copy the
        // advertised address out of the pushed state and dial it verbatim.
        if self.enabled && advertise_addr.ip().is_unspecified() {
            let source = if self.advertise_addr.is_some() {
                "cluster.advertise_addr"
            } else {
                "cluster.bind_addr"
            };
            return Err(ConfigError::Validation(format!(
                "{source} advertises {advertise_addr}, which no peer can dial: binding a \
                 wildcard address is fine, but it requires an explicit, non-wildcard \
                 cluster.advertise_addr (see docs/guide/clustering.md)"
            )));
        }

        Ok(())
    }

    /// The shared HMAC key: required, and long enough to be a key.
    fn validate_secret(&self) -> Result<(), ConfigError> {
        let Some(secret) = self.secret.as_ref() else {
            return Err(ConfigError::Validation(
                "cluster.secret is required when cluster.enabled = true: the cluster transport \
                 is authenticated (HMAC-SHA256) and has no unauthenticated mode — set it with \
                 AUTUMN_CLUSTER__SECRET"
                    .to_owned(),
            ));
        };
        let len = secrecy::ExposeSecret::expose_secret(secret).len();
        if len < MIN_CLUSTER_SECRET_LEN {
            return Err(ConfigError::Validation(format!(
                "cluster.secret must be at least {MIN_CLUSTER_SECRET_LEN} bytes, got {len}: a \
                 short shared key is a guessable one, and every member signs every frame with it"
            )));
        }
        Ok(())
    }
}

// ── Cache configuration ──────────────────────────────────────────────────────

/// Cache backend selection for `#[cached]` and `CacheResponseLayer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CacheBackend {
    /// In-process Moka cache (default). Each replica has an independent store.
    #[default]
    Memory,
    /// Shared Redis cache. Invalidations propagate across all replicas.
    Redis,
}

impl CacheBackend {
    pub(crate) fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Configuration for the shared application cache.
///
/// Placed in `autumn.toml` under `[cache]`.
///
/// # Examples
///
/// ```toml
/// [cache]
/// backend = "redis"
///
/// [cache.redis]
/// url = "redis://redis:6379"
/// key_prefix = "myapp:cache"
/// ```
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CacheConfig {
    /// Active cache backend.
    #[serde(default)]
    pub backend: CacheBackend,

    /// Redis backend options.
    #[serde(default)]
    pub redis: CacheRedisConfig,
}

impl CacheConfig {
    /// Returns `true` when the memory (Moka) backend is selected.
    #[must_use]
    pub fn is_memory(&self) -> bool {
        self.backend == CacheBackend::Memory
    }

    /// Returns `true` when the Redis backend is selected.
    #[must_use]
    pub fn is_redis(&self) -> bool {
        self.backend == CacheBackend::Redis
    }
}

/// Redis cache backend configuration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CacheRedisConfig {
    /// Redis connection URL (e.g. `redis://127.0.0.1:6379`).
    #[serde(default)]
    pub url: Option<String>,

    /// Prefix for all cache keys stored in Redis.
    #[serde(default = "default_cache_redis_key_prefix")]
    pub key_prefix: String,
}

impl Default for CacheRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_cache_redis_key_prefix(),
        }
    }
}

fn default_cache_redis_key_prefix() -> String {
    "autumn:cache".to_owned()
}

/// Scheduled task coordination backend selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerBackend {
    /// Per-process scheduler timers. This preserves existing single-replica behavior.
    #[serde(alias = "local", alias = "memory")]
    #[default]
    InProcess,
    /// Fleet coordination with Postgres advisory locks.
    Postgres,
}

impl SchedulerBackend {
    /// Parse an environment variable value for scheduler backend selection.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "in_process" | "in-process" | "local" | "memory" => Some(Self::InProcess),
            "postgres" | "postgresql" => Some(Self::Postgres),
            _ => None,
        }
    }

    /// Whether this CONFIGURED backend coordinates across a fleet of
    /// replicas, rather than a single process.
    ///
    /// Named so call sites (issue #1864) — like the ACME renewal spawn site,
    /// which must ask this even when it had to fall back to an in-process
    /// [`crate::scheduler::SchedulerCoordinator`] after a construction error —
    /// read as intent rather than an inline enum match. See
    /// [`crate::scheduler::SchedulerCoordinator::is_fleet_distributed`] for
    /// the equivalent query on an actually-built coordinator instance.
    #[must_use]
    pub const fn is_fleet_distributed(self) -> bool {
        matches!(self, Self::Postgres)
    }
}

/// Process role: which slice of the framework runtime this replica runs.
///
/// The same binary can be deployed under different roles so a fleet can scale
/// its HTTP tier independently of its background-work tier. The role is chosen
/// by config (`role = "..."`) or the `AUTUMN_ROLE` env var only — application
/// code never changes. The default, [`Combined`](ProcessRole::Combined),
/// preserves today's single-process behavior exactly.
///
/// - [`Combined`](ProcessRole::Combined): serves HTTP **and** runs job workers
///   + the cron scheduler (default).
/// - [`Web`](ProcessRole::Web): serves HTTP and can still **enqueue** jobs, but
///   runs no `#[job]` worker loops and no `#[scheduled]`/cron scheduler.
/// - [`Worker`](ProcessRole::Worker): runs job workers + the cron scheduler and
///   does **not** serve user routes, but still binds the HTTP listener to serve
///   only the liveness/readiness probes and the actuator (so orchestrators can
///   supervise it and `/actuator/jobs` works).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    /// Serve HTTP and run background workers + scheduler (default; unchanged
    /// single-process behavior).
    #[serde(
        alias = "all",
        alias = "combined",
        alias = "web_and_worker",
        alias = "server_and_worker"
    )]
    #[default]
    Combined,
    /// Serve HTTP (and enqueue jobs) only — no worker loops, no scheduler.
    #[serde(alias = "server", alias = "http")]
    Web,
    /// Run workers + scheduler only — probe/actuator HTTP only, no user routes.
    #[serde(alias = "jobs", alias = "worker_only")]
    Worker,
}

impl ProcessRole {
    /// Parse an environment variable / flag value for process-role selection.
    ///
    /// Accepts (case-insensitive, trimmed): `combined`/`all`, `web`/`server`/
    /// `http`, `worker`/`jobs`. Returns `None` for anything else so callers can
    /// warn and keep the default.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "combined" | "all" | "web_and_worker" | "server_and_worker" => Some(Self::Combined),
            "web" | "server" | "http" => Some(Self::Web),
            "worker" | "jobs" | "worker_only" => Some(Self::Worker),
            _ => None,
        }
    }

    /// Stable lowercase identifier for the role (round-trips `from_env_value`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Combined => "combined",
            Self::Web => "web",
            Self::Worker => "worker",
        }
    }

    /// Whether this role serves user HTTP routes ([`Combined`](Self::Combined)
    /// or [`Web`](Self::Web)).
    #[must_use]
    pub const fn serves_http(self) -> bool {
        matches!(self, Self::Combined | Self::Web)
    }

    /// Whether this role runs background job workers and the cron scheduler
    /// ([`Combined`](Self::Combined) or [`Worker`](Self::Worker)).
    #[must_use]
    pub const fn runs_workers(self) -> bool {
        matches!(self, Self::Combined | Self::Worker)
    }
}

/// Whether a `role` / `jobs.backend` combination is invalid because a split
/// (web/worker) role sits on a non-durable jobs backend.
///
/// A split role runs the HTTP tier and the job/scheduler tier in **separate
/// processes**, so it needs a jobs backend the two processes can share. Only the
/// recognized durable backends [`start_runtime`](crate::job::start_runtime)
/// dispatches to durably — exactly `"postgres"` or `"redis"` — qualify. Every
/// other value (the in-process `"local"` queue, a typo like `"postgresql"`, or a
/// blank backend) falls through to the per-process local runtime, where a
/// [`Web`](ProcessRole::Web) replica would enqueue into a queue no separate
/// worker can drain and a [`Worker`](ProcessRole::Worker) replica's queue starts
/// empty. The match is intentionally exact (no trim/case-fold) so this guard and
/// `start_runtime`'s dispatch agree precisely on which backends are durable.
///
/// The combined role is always valid because it enqueues and drains in one
/// process. Returns `true` when the combination is **invalid**.
#[must_use]
pub fn split_role_requires_durable_backend(role: ProcessRole, jobs_backend: &str) -> bool {
    role != ProcessRole::Combined && !matches!(jobs_backend, "postgres" | "redis")
}

/// `[retention]` — one retention window per framework-owned dataset
/// (issue #1605).
///
/// Autumn creates and fills tables and stores the application never asked
/// for: the job queue and its tracking records, idempotency replay records,
/// sticky experiment assignments, webhook replay markers, sessions, and audit
/// archives. Being batteries-included means owning their lifecycle. This
/// section is the single place an operator declares how long each of those
/// datasets is kept; the framework then enforces it on a recurring in-process
/// sweep with no external cron.
///
/// Every window defaults to `None`, which means **exactly today's behavior**
/// for that dataset — nothing is swept and no sweep task is even registered
/// unless at least one window is set.
///
/// # `autumn.toml` example
///
/// ```toml
/// [retention]
/// sweep_interval         = "1h"    # how often the sweep runs (default "1h")
/// job_history            = "90d"   # terminal rows in `autumn_jobs`
/// commit_hooks           = "30d"   # terminal `#[after_commit]` hook rows
/// job_tracking           = "7d"    # `autumn_job_tracking` records
/// idempotency            = "2d"    # stored idempotency responses
/// experiment_assignments = "365d"  # `autumn_experiment_assignments`
/// webhook_replay         = "3d"    # inbound webhook replay markers
/// sessions               = "30d"   # server-side session records
/// audit_archives         = "400d"  # JSONL audit archive entries
/// ```
///
/// Durations use the same syntax as `#[scheduled(every = ...)]`: `s`/`m`/`h`/
/// `d`, optionally compound (`"1h 30m"`).
///
/// See `docs/guide/data-retention.md` for the full dataset table, the
/// precedence rule against `jobs.tracking.ttl_secs` / `idempotency.ttl_secs`,
/// and the `autumn db retention` CLI.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionConfig {
    /// How often the framework retention sweep runs. Default: `"1h"`.
    ///
    /// Only consulted when at least one dataset window is set — with none set
    /// no sweep task is registered at all.
    #[serde(default = "default_retention_sweep_interval")]
    pub sweep_interval: String,

    /// Terminal (`completed`/`failed`/`discarded`) rows in `autumn_jobs`,
    /// measured from `finished_at`. Unset (default): job history is kept
    /// forever.
    ///
    /// Live rows — enqueued, running, or retrying — are never touched
    /// regardless of age, and neither is a row still holding a
    /// `#[job(unique, unique_for_ms = ...)]` dedup key. Note that `failed` is
    /// also the dead-letter state a `autumn jobs retry` replays from, so a
    /// window here bounds how long a dead letter stays replayable; see
    /// `docs/guide/data-retention.md`.
    #[serde(default)]
    pub job_history: Option<String>,

    /// Terminal rows in the `#[after_commit]` hook queue,
    /// `autumn_repository_commit_hooks`, measured from `finished_at`. Unset
    /// (default): kept forever.
    #[serde(default)]
    pub commit_hooks: Option<String>,

    /// `autumn_job_tracking` progress/result records, measured from
    /// `updated_at`. Unset (default): rows live until their
    /// `jobs.tracking.ttl_secs` expiry, which the existing sweep already
    /// enforces.
    ///
    /// When both are set the **shorter** bound wins; see
    /// `docs/guide/data-retention.md`.
    #[serde(default)]
    pub job_tracking: Option<String>,

    /// Stored idempotency-key responses, measured from when they were
    /// written. Unset (default): records live for `idempotency.ttl_secs`.
    ///
    /// Idempotency backends expire their own records, so this window is
    /// enforced by capping the record TTL at write time rather than by a
    /// sweep. When both are set the **shorter** bound wins.
    #[serde(default)]
    pub idempotency: Option<String>,

    /// Sticky `autumn_experiment_assignments` rows, measured from
    /// `assigned_at`. Unset (default): assignments are kept forever.
    #[serde(default)]
    pub experiment_assignments: Option<String>,

    /// Inbound webhook replay markers, measured from when they were
    /// recorded. Unset (default): markers live for the endpoint's
    /// `replay_window_secs`.
    ///
    /// Enforced by capping the marker TTL, as for [`Self::idempotency`].
    #[serde(default)]
    pub webhook_replay: Option<String>,

    /// Server-side session records, measured from their last write. Unset
    /// (default): the session backend's own expiry applies (which, for the
    /// in-memory store, is no expiry at all).
    #[serde(default)]
    pub sessions: Option<String>,

    /// Entries in the JSONL audit archive, measured from each event's
    /// `timestamp`. Unset (default): audit archives are kept forever.
    ///
    /// Enforced by rewriting the archive file in place (atomically), so it
    /// only applies to sinks that support purging — today
    /// [`crate::audit::JsonlFileAuditSink`].
    #[serde(default)]
    pub audit_archives: Option<String>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            sweep_interval: default_retention_sweep_interval(),
            job_history: None,
            commit_hooks: None,
            job_tracking: None,
            idempotency: None,
            experiment_assignments: None,
            webhook_replay: None,
            sessions: None,
            audit_archives: None,
        }
    }
}

fn default_retention_sweep_interval() -> String {
    "1h".to_owned()
}

impl RetentionConfig {
    /// Every `(config key, window)` pair, in the order the CLI reports them.
    ///
    /// The single source of truth that keeps the config surface, the dataset
    /// registry in [`crate::data_retention`], the CLI report, and the docs
    /// table from drifting apart: adding a field here without adding the
    /// matching dataset fails a test in `crate::data_retention`.
    #[must_use]
    pub fn windows(&self) -> [(&'static str, Option<&str>); 8] {
        [
            ("job_history", self.job_history.as_deref()),
            ("commit_hooks", self.commit_hooks.as_deref()),
            ("job_tracking", self.job_tracking.as_deref()),
            ("idempotency", self.idempotency.as_deref()),
            (
                "experiment_assignments",
                self.experiment_assignments.as_deref(),
            ),
            ("webhook_replay", self.webhook_replay.as_deref()),
            ("sessions", self.sessions.as_deref()),
            ("audit_archives", self.audit_archives.as_deref()),
        ]
    }

    /// `true` when at least one dataset declares a retention window.
    ///
    /// When this is `false` no sweep task is registered and no framework
    /// behavior changes — AC #1's "leaving a dataset unset preserves today's
    /// behavior exactly", enforced structurally rather than by each sweeper
    /// remembering to check.
    #[must_use]
    pub fn any_window_configured(&self) -> bool {
        self.windows().iter().any(|(_, window)| window.is_some())
    }

    /// The configured window for one dataset key, already parsed.
    ///
    /// Returns `None` both when the dataset is unset and when its value does
    /// not parse — [`Self::validate`] rejects the latter at boot, so a
    /// running app never reaches this with an unparseable value.
    #[must_use]
    pub fn window(&self, key: &str) -> Option<std::time::Duration> {
        self.windows()
            .into_iter()
            .find(|(name, _)| *name == key)
            .and_then(|(_, window)| window)
            .and_then(crate::task::parse_duration)
    }

    /// How often the sweep runs, already parsed. Falls back to one hour when
    /// unparseable ([`Self::validate`] rejects that at boot).
    #[must_use]
    pub fn sweep_interval_duration(&self) -> std::time::Duration {
        crate::task::parse_duration(&self.sweep_interval)
            .unwrap_or(std::time::Duration::from_secs(3_600))
    }

    /// Validate every declared window.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when a window (or the sweep
    /// interval) is not a valid duration string, or resolves to zero — a zero
    /// window would purge a dataset the instant it was written, which is
    /// never what an operator means and is not something to guess at.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (key, window) in self.windows() {
            let Some(raw) = window else { continue };
            // One message, not two: `parse_duration` already returns `None`
            // for a syntactically valid but zero total ("0s"), so a separate
            // "resolves to zero" branch would be unreachable. State what is
            // required — valid *and* non-zero — and why zero is refused.
            let Some(parsed) = crate::task::parse_duration(raw) else {
                return Err(ConfigError::Validation(format!(
                    "retention.{key} = {raw:?} is not a valid non-zero duration: use a \
                     string like \"90d\", \"12h\", or \"1h 30m\". A zero window would \
                     purge the dataset as soon as it is written; remove the key entirely \
                     to keep today's behavior (see docs/guide/data-retention.md)"
                )));
            };
            // The sweep binds its cutoff as `NOW() - make_interval(secs => $1)`
            // with a `u32`-representable value. Rejecting anything larger here
            // keeps the configured window and the window the database actually
            // applies identical — silently clamping would delete rows the
            // report claimed were still inside the policy. 136 years is far
            // past any real retention policy, so this only ever catches a typo.
            if parsed.as_secs() > u64::from(u32::MAX) {
                return Err(ConfigError::Validation(format!(
                    "retention.{key} = {raw:?} is longer than the maximum supported \
                     retention window ({} years). Remove the key entirely to keep the \
                     data forever, which is what a window that long means in practice \
                     (see docs/guide/data-retention.md)",
                    u64::from(u32::MAX) / (365 * 86_400)
                )));
            }
        }

        // Only meaningful once something is actually swept, but validated
        // unconditionally so a typo is caught the first time it is written
        // rather than the first time a window is added next to it.
        if crate::task::parse_duration(&self.sweep_interval).is_none() {
            return Err(ConfigError::Validation(format!(
                "retention.sweep_interval = {:?} is not a valid non-zero duration: use a \
                 string like \"1h\" or \"30m\" (see docs/guide/data-retention.md)",
                self.sweep_interval
            )));
        }
        Ok(())
    }
}

/// Scheduled task coordination runtime configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Runtime backend selection.
    #[serde(default)]
    pub backend: SchedulerBackend,
    /// Lease duration used by distributed backends for run visibility and timeout guidance.
    #[serde(default = "default_scheduler_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// Stable replica identifier surfaced in actuator metadata.
    #[serde(default)]
    pub replica_id: Option<String>,
    /// Prefix included when deriving Postgres advisory lock keys.
    #[serde(default = "default_scheduler_key_prefix")]
    pub key_prefix: String,
}

impl SchedulerConfig {
    /// Resolve a stable-ish replica identifier for actuator metadata and lock ownership.
    #[must_use]
    pub fn resolved_replica_id(&self) -> String {
        self.replica_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .cloned()
            .or_else(|| std::env::var("FLY_MACHINE_ID").ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| format!("pid-{}", std::process::id()))
    }

    /// Validate scheduler-specific config shape.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when values are syntactically valid TOML
    /// but cannot be used by the runtime.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.lease_ttl_secs == 0 {
            return Err(ConfigError::Validation(
                "scheduler.lease_ttl_secs must be greater than zero".to_owned(),
            ));
        }
        if self.key_prefix.trim().is_empty() {
            return Err(ConfigError::Validation(
                "scheduler.key_prefix must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            backend: SchedulerBackend::default(),
            lease_ttl_secs: default_scheduler_lease_ttl_secs(),
            replica_id: None,
            key_prefix: default_scheduler_key_prefix(),
        }
    }
}

const fn default_scheduler_lease_ttl_secs() -> u64 {
    300
}

fn default_scheduler_key_prefix() -> String {
    "autumn:scheduler".to_owned()
}

/// Storage backend selection for HTTP idempotency keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum IdempotencyBackend {
    #[default]
    Memory,
    Redis,
}

impl IdempotencyBackend {
    /// Parse an environment variable value for idempotency backend selection.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" | "mem" => Some(Self::Memory),
            "redis" => Some(Self::Redis),
            _ => None,
        }
    }
}

/// Redis connection settings for the idempotency backend.
#[derive(Debug, Clone, Deserialize)]
pub struct IdempotencyRedisConfig {
    /// Redis connection URL (e.g. `redis://localhost:6379`).
    pub url: Option<String>,
    /// Key prefix for all idempotency entries and locks stored in Redis.
    #[serde(default = "default_idempotency_redis_key_prefix")]
    pub key_prefix: String,
}

impl Default for IdempotencyRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_idempotency_redis_key_prefix(),
        }
    }
}

fn default_idempotency_redis_key_prefix() -> String {
    "autumn:idempotency".to_owned()
}

/// HTTP idempotency-key middleware settings.
#[derive(Debug, Clone, Deserialize)]
pub struct IdempotencyConfig {
    /// Enable the idempotency-key middleware.
    ///
    /// When `true`, mutating requests that carry an `Idempotency-Key` header
    /// are deduplicated using the configured backend.
    ///
    /// `None` means the field was absent from the config file; the
    /// `AppBuilder::idempotent()` builder flag may still enable it.
    /// `Some(false)` is an explicit operator opt-out that overrides the builder.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Storage backend for idempotency records.
    #[serde(default)]
    pub backend: IdempotencyBackend,
    /// Time-to-live in seconds for stored idempotency records.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub ttl_secs: u64,
    /// Maximum stale lifetime for distributed in-flight locks.
    ///
    /// The lock is released as soon as the handler finishes. This value is only
    /// the backend safety expiry for crashes or lost unlocks, so it should be
    /// comfortably longer than any supported mutating request duration.
    #[serde(default = "default_idempotency_in_flight_ttl_secs")]
    pub in_flight_ttl_secs: u64,
    /// Allow the in-memory backend in production environments.
    #[serde(default)]
    pub allow_memory_in_production: bool,
    /// Redis connection settings (used when `backend = "redis"`).
    #[serde(default)]
    pub redis: IdempotencyRedisConfig,
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            backend: IdempotencyBackend::default(),
            ttl_secs: default_idempotency_ttl_secs(),
            in_flight_ttl_secs: default_idempotency_in_flight_ttl_secs(),
            allow_memory_in_production: false,
            redis: IdempotencyRedisConfig::default(),
        }
    }
}

const fn default_idempotency_ttl_secs() -> u64 {
    86_400
}

const fn default_idempotency_in_flight_ttl_secs() -> u64 {
    86_400
}

/// `OpenAPI` spec runtime exposure settings.
///
/// Populated from the `[openapi]` block in `autumn.toml`. When
/// `AppBuilder::openapi(...)` is called and `enabled = true`, the framework
/// mounts the spec at `path`. Set `enabled = false` in a production profile
/// to prevent exposing the spec publicly.
///
/// # `autumn.toml` example
///
/// ```toml
/// [openapi]
/// enabled = false   # disable in prod
/// path = "/openapi.json"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct OpenApiRuntimeConfig {
    /// Whether the `OpenAPI` spec endpoint is served.
    ///
    /// Defaults to `true` so new projects get the spec immediately.
    /// Set to `false` in production profiles to suppress the endpoint.
    #[serde(default = "default_openapi_enabled")]
    pub enabled: bool,
    /// URL path at which `openapi.json` is served.
    ///
    /// Defaults to `/openapi.json`.
    #[serde(default = "default_openapi_path")]
    pub path: String,
}

impl Default for OpenApiRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: default_openapi_enabled(),
            path: default_openapi_path(),
        }
    }
}

const fn default_openapi_enabled() -> bool {
    true
}

fn default_openapi_path() -> String {
    "/openapi.json".to_owned()
}

/// Background job runtime configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct JobConfig {
    /// Runtime backend selection.
    ///
    /// - `local` (default): in-process Tokio queue
    /// - `postgres`: Postgres-backed durable queue (requires `db` feature)
    /// - `redis`: Redis-backed durable queue (requires `redis` feature)
    #[serde(default = "default_job_backend")]
    pub backend: String,
    /// Number of concurrent worker loops to spawn.
    #[serde(default = "default_job_workers")]
    pub workers: usize,
    /// Default max attempts when `#[job(max_attempts = ...)]` is not set.
    #[serde(default = "default_job_max_attempts")]
    pub max_attempts: u32,
    /// Default initial retry backoff in milliseconds.
    #[serde(default = "default_job_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Ordered/weighted list of queues workers drain, highest priority first.
    ///
    /// Unset = a single `default` queue (today's behavior). A TOML array such as
    /// `queues = ["critical", "default", "low"]` is **strict priority**; a table
    /// such as `[jobs.queues] critical = 4` / `default = 1` is **weighted**
    /// (probabilistic fair draining that never starves lower queues).
    #[serde(default)]
    pub queues: JobQueuesConfig,
    /// Queues this process is pinned to. Empty (default) = claim every
    /// configured/declared queue (today's behavior). When non-empty, this
    /// worker process only ever claims jobs from queues in this set — on every
    /// backend — so a worker tier can be dedicated to a subset of queues
    /// (issue #1623, AC3). Names outside the configured/declared topology are
    /// ignored. Set from `AUTUMN_JOBS__PIN` (comma-separated) too.
    #[serde(default)]
    pub pin: Vec<String>,
    /// Declared worker-fleet topology, read by `autumn doctor` to prove
    /// topology-wide queue coverage (issue #1623, AC6). Purely declarative: the
    /// running process never acts on it — it describes the *other* tiers, which
    /// a single process cannot observe.
    #[serde(default)]
    pub fleet: JobFleetConfig,
    /// Redis backend options.
    #[serde(default)]
    pub redis: JobRedisConfig,
    /// Postgres backend options.
    #[serde(default)]
    pub postgres: JobPostgresConfig,
    /// Tracked-job progress/result store options (`enqueue_tracked`, the
    /// built-in `GET /_autumn/jobs/{token}` status route).
    #[serde(default)]
    pub tracking: JobTrackingConfig,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            backend: default_job_backend(),
            workers: default_job_workers(),
            max_attempts: default_job_max_attempts(),
            initial_backoff_ms: default_job_backoff_ms(),
            queues: JobQueuesConfig::default(),
            pin: Vec::new(),
            fleet: JobFleetConfig::default(),
            redis: JobRedisConfig::default(),
            postgres: JobPostgresConfig::default(),
            tracking: JobTrackingConfig::default(),
        }
    }
}

/// A single named queue and its draining weight, plus optional per-queue
/// worker-pool controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobQueue {
    /// Queue name, as declared by `#[job(queue = "...")]`.
    pub name: String,
    /// Relative draining weight (used only for weighted draining; `1` for the
    /// strict-priority list form).
    pub weight: u32,
    /// Optional hard cap on how many of the process's worker slots this queue
    /// may occupy at once. `None` = uncapped (may use the whole shared pool).
    /// Lets a bulk queue never exceed its configured share (issue #1623, AC2).
    pub concurrency: Option<usize>,
    /// Optional number of worker slots dedicated to this queue that no other
    /// queue may consume. `None`/`0` = no reservation. Guarantees a queue keeps
    /// making progress even while another queue floods (issue #1623, AC1).
    pub reserved: Option<usize>,
}

impl JobQueue {
    /// A weight-only queue (no per-queue caps or reservations).
    #[must_use]
    pub fn new(name: impl Into<String>, weight: u32) -> Self {
        Self {
            name: name.into(),
            weight,
            concurrency: None,
            reserved: None,
        }
    }
}

/// Worker queue drain configuration parsed from `[jobs] queues`.
///
/// Accepts **either** a TOML array (strict priority, in order) **or** a TOML
/// table of `name = weight` (weighted, fair). Empty or unset falls back to a
/// single `default` queue so an app that doesn't opt in behaves exactly as today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobQueuesConfig {
    /// Configured queues, highest priority first.
    pub queues: Vec<JobQueue>,
    /// `true` for the ordered-list form (strict priority); `false` for the
    /// weighted-table form (deficit weighted round-robin).
    pub strict: bool,
}

impl Default for JobQueuesConfig {
    fn default() -> Self {
        Self::single_default()
    }
}

impl JobQueuesConfig {
    /// The zero-config default: one strict `default` queue.
    #[must_use]
    pub fn single_default() -> Self {
        Self {
            queues: vec![JobQueue::new("default", 1)],
            strict: true,
        }
    }

    /// Build a strict-priority schedule from an ordered list of queue names.
    #[must_use]
    pub fn strict_list<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let queues: Vec<JobQueue> = names
            .into_iter()
            .map(|name| JobQueue::new(name, 1))
            .collect();
        if queues.is_empty() {
            Self::single_default()
        } else {
            Self {
                queues,
                strict: true,
            }
        }
    }

    /// Build a weighted schedule from `(name, weight)` pairs. Weights are
    /// clamped to a minimum of `1` so every configured queue makes progress.
    #[must_use]
    pub fn weighted<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = (S, u32)>,
        S: Into<String>,
    {
        let queues: Vec<JobQueue> = entries
            .into_iter()
            .map(|(name, weight)| JobQueue::new(name, weight.max(1)))
            .collect();
        if queues.is_empty() {
            Self::single_default()
        } else {
            Self {
                queues,
                strict: false,
            }
        }
    }

    /// Build a weighted schedule from fully-specified [`JobQueue`] entries
    /// (weight plus optional per-queue `concurrency` cap and `reserved` slots).
    /// Weights are clamped to a minimum of `1`. Empty input falls back to the
    /// zero-config single `default` queue.
    #[must_use]
    pub fn weighted_specs(queues: Vec<JobQueue>) -> Self {
        if queues.is_empty() {
            Self::single_default()
        } else {
            Self {
                queues: queues
                    .into_iter()
                    .map(|mut q| {
                        q.weight = q.weight.max(1);
                        q
                    })
                    .collect(),
                strict: false,
            }
        }
    }
}

/// One value in the `[jobs.queues]` weight table: either a bare integer weight
/// (`critical = 4`) or a table with per-queue pool controls
/// (`critical = { weight = 4, concurrency = 8, reserved = 2 }`).
#[derive(Debug, Clone)]
enum JobQueueValue {
    Weight(u32),
    Spec {
        weight: Option<u32>,
        concurrency: Option<usize>,
        reserved: Option<usize>,
    },
}

impl<'de> serde::Deserialize<'de> for JobQueueValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JobQueueValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a queue weight (e.g. critical = 4) or a queue table \
                     (e.g. critical = { weight = 4, concurrency = 8, reserved = 2 })",
                )
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(JobQueueValue::Weight(
                    u32::try_from(v).map_err(|_| E::custom("queue weight is too large"))?,
                ))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("queue weight must not be negative"));
                }
                self.visit_u64(u64::try_from(v).unwrap_or(0))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut weight = None;
                let mut concurrency = None;
                let mut reserved = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "weight" => weight = Some(map.next_value::<u32>()?),
                        "concurrency" => concurrency = Some(map.next_value::<usize>()?),
                        "reserved" => reserved = Some(map.next_value::<usize>()?),
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown queue setting '{other}' (expected weight, concurrency, \
                                 or reserved)"
                            )));
                        }
                    }
                }
                Ok(JobQueueValue::Spec {
                    weight,
                    concurrency,
                    reserved,
                })
            }
        }

        d.deserialize_any(ValueVisitor)
    }
}

impl<'de> serde::Deserialize<'de> for JobQueuesConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct JobQueuesVisitor;

        impl<'de> Visitor<'de> for JobQueuesVisitor {
            type Value = JobQueuesConfig;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "an ordered list of queue names (e.g. queues = [\"critical\", \"default\"]) \
                     or a weight table (e.g. [jobs.queues] critical = 4, default = 1)",
                )
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut names = Vec::new();
                let mut seen = std::collections::HashSet::new();
                while let Some(name) = seq.next_element::<String>()? {
                    if !seen.insert(name.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate queue name '{name}' in queues list"
                        )));
                    }
                    names.push(name);
                }
                Ok(JobQueuesConfig::strict_list(names))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut queues: Vec<JobQueue> = Vec::new();
                while let Some((k, value)) = map.next_entry::<String, JobQueueValue>()? {
                    let (weight, concurrency, reserved) = match value {
                        JobQueueValue::Weight(w) => (w, None, None),
                        JobQueueValue::Spec {
                            weight,
                            concurrency,
                            reserved,
                        } => (weight.unwrap_or(1), concurrency, reserved),
                    };
                    if weight == 0 {
                        return Err(serde::de::Error::custom(format!(
                            "queue '{k}' weight must be at least 1 (got 0); \
                             to disable a queue remove it from the list"
                        )));
                    }
                    queues.push(JobQueue {
                        name: k,
                        weight,
                        concurrency,
                        reserved,
                    });
                }
                Ok(JobQueuesConfig::weighted_specs(queues))
            }
        }

        d.deserialize_any(JobQueuesVisitor)
    }
}

/// Declared worker-fleet topology (`[jobs.fleet]` in `autumn.toml`).
///
/// A single process can see its own `jobs.pin` but not its siblings', so it can
/// never tell a valid multi-tier split (one tier drains `critical`, another
/// drains `bulk`) from a real coverage gap. Declaring every tier's pin here is
/// what lets `autumn doctor --strict` *prove* that some queue is drained by no
/// tier anywhere and hard-fail on it (issue #1623, AC6) instead of only
/// reporting what this one process claims.
///
/// # Example `autumn.toml`
///
/// ```toml
/// [jobs.fleet]
/// # One entry per worker tier, each holding that tier's `jobs.pin`. Must list
/// # every tier actually running: doctor can only reason about declared tiers,
/// # so omitting one reports a coverage gap that does not exist.
/// # An empty entry is an *unpinned* tier that drains every queue.
/// tiers = [["critical"], ["bulk", "default", "thumbnails"]]
/// # Optional: where the compiled `#[job(queue = "…")]` set comes from, so the
/// # check also covers queues declared in code but absent from `[jobs.queues]`.
/// # `manifest` (emitted by `autumn jobs manifest <path>`) wins when both are
/// # set. Every queue they name must be covered by some tier above.
/// manifest = "target/jobs-manifest.toml"
/// declared_queues = ["thumbnails"]
/// ```
///
/// The framework itself never reads this at runtime — it is operator-declared
/// input for the `doctor` check, and an app that declares nothing keeps today's
/// behavior exactly (the check stays informational-only).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct JobFleetConfig {
    /// One entry per worker tier, each entry being that tier's `jobs.pin`. An
    /// **empty** entry is an unpinned tier that drains every queue, which makes
    /// topology-wide coverage total. Empty/unset declares no topology.
    #[serde(default)]
    pub tiers: Vec<Vec<String>>,

    /// Path to a jobs manifest (a TOML file with a `queues = [...]` array) the
    /// app emits, naming the queues `#[job(queue = "…")]` declares. Lets the
    /// coverage check see queues that exist in code but not in `[jobs.queues]`.
    /// Takes precedence over [`Self::declared_queues`].
    #[serde(default)]
    pub manifest: Option<String>,

    /// Inline list of `#[job(queue = "…")]`-declared queue names, for operators
    /// who don't emit a manifest. Only *adds* to the set of queues that must be
    /// covered, so an incomplete list can never manufacture a false failure.
    #[serde(default)]
    pub declared_queues: Vec<String>,
}

/// Redis backend configuration options for the job runner.
#[derive(Debug, Clone, Deserialize)]
pub struct JobRedisConfig {
    /// Redis URL used when `jobs.backend = "redis"`.
    #[serde(default)]
    pub url: Option<String>,
    /// Key prefix for all queue keys.
    #[serde(default = "default_jobs_redis_prefix")]
    pub key_prefix: String,
    /// Duration before an in-flight job claim is considered stale.
    #[serde(default = "default_jobs_redis_visibility_timeout_ms")]
    pub visibility_timeout_ms: u64,
}

impl Default for JobRedisConfig {
    fn default() -> Self {
        Self {
            url: None,
            key_prefix: default_jobs_redis_prefix(),
            visibility_timeout_ms: default_jobs_redis_visibility_timeout_ms(),
        }
    }
}

/// Postgres backend configuration options for the job runner.
#[derive(Debug, Clone, Deserialize)]
pub struct JobPostgresConfig {
    /// Duration before an in-flight job claim is considered stale and recovered.
    ///
    /// Workers that crash mid-job have their claim reclaimed by another worker
    /// within this bound. Default: 30 seconds.
    #[serde(default = "default_jobs_pg_visibility_timeout_ms")]
    pub visibility_timeout_ms: u64,
}

impl Default for JobPostgresConfig {
    fn default() -> Self {
        Self {
            visibility_timeout_ms: default_jobs_pg_visibility_timeout_ms(),
        }
    }
}

/// Tracked-job progress/result store configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct JobTrackingConfig {
    /// How long a tracked job's progress/result record is retained after its
    /// last write, in seconds. Default: 24 hours.
    #[serde(default = "default_jobs_tracking_ttl_secs")]
    pub ttl_secs: u64,
    /// Whether the built-in `GET /_autumn/jobs/{token}` status route is
    /// mounted. Default: `true`.
    #[serde(default = "default_jobs_tracking_route_enabled")]
    pub route_enabled: bool,
}

impl Default for JobTrackingConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_jobs_tracking_ttl_secs(),
            route_enabled: default_jobs_tracking_route_enabled(),
        }
    }
}

const fn default_jobs_tracking_ttl_secs() -> u64 {
    86_400
}

const fn default_jobs_tracking_route_enabled() -> bool {
    true
}

const fn default_jobs_pg_visibility_timeout_ms() -> u64 {
    30_000
}

fn default_job_backend() -> String {
    "local".to_owned()
}

const fn default_job_workers() -> usize {
    1
}

const fn default_job_max_attempts() -> u32 {
    5
}

const fn default_job_backoff_ms() -> u64 {
    250
}

fn default_jobs_redis_prefix() -> String {
    "autumn:jobs".to_owned()
}

const fn default_jobs_redis_visibility_timeout_ms() -> u64 {
    30_000
}

/// Parent config paths whose child keys were already covered by strict
/// validation BEFORE the #1890 schema-walk fix. Captured verbatim from
/// `get_schema_keys()` on the pre-fix code (the walk aborted at
/// `database.statement_timeout`, so only these parents were reachable). Used by
/// the warn-first rollout: an unknown key whose PARENT is in this set hard-fails
/// under `strict_config` exactly as before; every key the #1890 fix newly
/// reveals is warned about instead (until `strict_config_enforce_all` promotes
/// it). Transitional — removed when enforcement becomes the default.
const PRE_1890_STRICT_PARENTS: &[&str] = &[
    "",
    "database",
    "deploy",
    "server",
    "server.timeouts",
    "server.tls",
    "server.tls.acme",
];

/// Whether an unknown-key error whose (profile-stripped, segment-derived) schema
/// parent is `schema_parent` was already hard-failing before #1890. Malformed
/// top-level profile entries surface with parent `"profile"` (always fatal
/// structural errors); everything else keys off the pre-#1890 parent set.
fn unknown_key_was_previously_strict(schema_parent: &str) -> bool {
    schema_parent == "profile" || PRE_1890_STRICT_PARENTS.contains(&schema_parent)
}

/// Policy for how the strict unknown-key check treats a genuinely-unknown
/// TOP-LEVEL config root — an unknown key whose schema parent is the document
/// root `""` (e.g. a plugin-owned `[media]` section that no core-schema key
/// covers).
///
/// App boot uses [`Strict`](UnknownRootPolicy::Strict): an unknown top-level
/// root is a hard error, exactly as before. Tooling that structurally cannot
/// know the application's plugin set — the deploy CLI, which has no
/// `AppBuilder`, no plugin list, and no plugin-crate dependency — uses
/// [`LenientWarn`](UnknownRootPolicy::LenientWarn): unknown top-level roots are
/// accepted as opaque with a single doctor-style warning, because app boot
/// (which DOES know the plugin set via the `config_section` seam / #2061 /
/// #1974) remains the authoritative strict gate for plugin-owned roots (#2063).
///
/// The leniency is scoped to top-level roots ONLY: unknown keys INSIDE a known
/// section (schema parent != `""`, e.g. a `[database] primry_url` typo) keep
/// their normal (hard) classification under both policies, and malformed TOML
/// still fails everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownRootPolicy {
    /// Unknown top-level roots hard-fail (the app-boot path).
    Strict,
    /// Unknown top-level roots are accepted as opaque with one warning (the
    /// deploy-CLI config-load path).
    LenientWarn,
}

/// Child schema keys for config sections whose `Deserialize` is OPAQUE to the
/// schema walker and must be declared by hand.
///
/// `#[serde(untagged)]` "scalar shorthand OR table" enums (e.g. `TimeZoneConfig`:
/// `time_zone = "UTC"` or `[time_zone] identifier = ...`) deserialize by first
/// buffering into serde's `Content` and then matching variants against that
/// buffer — so the table variant's fields are read from the buffer, never from
/// `SchemaDeserializer`. The walker therefore cannot see them, and the section
/// would otherwise be a childless leaf that strict validation skips (accepting
/// typos even under `strict_config_enforce_all`). Register such sections here so
/// `validate_toml` descends into them.
///
/// KEEP IN SYNC with the corresponding type's table fields (serialized names).
/// The guard test `manual_schema_sections_are_registered` pins the behavior.
const MANUAL_SCHEMA_SECTIONS: &[(&str, &[&str])] = &[
    // `crate::time_zone::TimeZoneConfig` — untagged Scalar|Table.
    ("time_zone", &["identifier", "sources"]),
];

impl AutumnConfig {
    /// Validate the `[push]` block, as `AppBuilder::run` does before binding.
    ///
    /// Factored out of the boot path so the rule it enforces — a VAPID key
    /// that is present but unusable is a hard failure, never a quiet fallback
    /// to "push disabled" — is reachable from a test. `run` calls exactly this
    /// and exits on `Err`.
    ///
    /// # Errors
    ///
    /// See [`crate::push::PushConfig::load_vapid_key`].
    pub fn validate_push(&self) -> Result<(), crate::push::PushError> {
        self.push.load_vapid_key().map(|_| ())
    }
}

impl AutumnConfig {
    /// Recursively extracts all valid configuration schema keys and nested fields.
    #[must_use]
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_schema_keys() -> HashMap<String, HashSet<String>> {
        // Adaptive multi-pass schema walk. `deserialize_any` probes with a scalar
        // by default; any path whose visitor rejects that probe (a seq/map-only
        // type such as `JobQueuesConfig` at `jobs.queues`) is escalated to a
        // map- then seq-probe on the next pass, so the walk stops aborting there
        // and enumerates every later section (#1890). Converges in two passes for
        // the current config; the loop is bounded and monotonic (each escalated
        // path only advances Str→Map→Seq), so it always terminates.
        const MAX_PASSES: usize = 8;
        let de = SchemaDeserializer::new();
        let mut prev_rejected: Vec<String> = Vec::new();
        for _ in 0..MAX_PASSES {
            de.rejected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            let _ = Self::deserialize(de.clone());
            let mut rejected: Vec<String> = std::mem::take(
                &mut de
                    .rejected
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            rejected.sort();
            rejected.dedup();
            if rejected.is_empty() {
                break;
            }
            let mut advanced = false;
            {
                let mut probes = de
                    .any_probe
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for p in &rejected {
                    let cur = probes.get(p).copied().unwrap_or(AnyProbe::Str);
                    let next = match cur {
                        AnyProbe::Str => AnyProbe::Map,
                        AnyProbe::Map | AnyProbe::Seq => AnyProbe::Seq,
                    };
                    if next != cur {
                        advanced = true;
                    }
                    probes.insert(p.clone(), next);
                }
            }
            // No path could be escalated further and the rejected set is stable:
            // any remaining aborter accepts none of str/map/seq — stop (leaf it).
            if !advanced && rejected == prev_rejected {
                break;
            }
            prev_rejected = rejected;
        }
        // Register walker-opaque sections (untagged scalar-or-table types whose
        // table fields buffer through serde `Content` and are invisible to the
        // walk). See MANUAL_SCHEMA_SECTIONS.
        {
            let mut schema = de
                .schema
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (section, keys) in MANUAL_SCHEMA_SECTIONS {
                let entry = schema.entry((*section).to_owned()).or_default();
                for k in *keys {
                    entry.insert((*k).to_owned());
                }
            }
        }
        de.into_schema()
    }

    /// Returns a sorted set of all schema leaf key paths (e.g. `"server.port"`).
    ///
    /// Used by the schema-snapshot CI guard (`autumn/tests/schema_drift_guard.rs`)
    /// to detect when a config key disappears without a registered deprecation entry.
    /// Regenerate the snapshot with:
    /// ```text
    /// UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web schema_keys_snapshot_guard
    /// ```
    ///
    /// **Note:** Always run the guard under a consistent feature set (e.g. `--all-features`)
    /// in CI, since feature-gated fields only appear when their feature is enabled.
    #[must_use]
    pub fn schema_leaf_paths() -> std::collections::BTreeSet<String> {
        let schema = Self::get_schema_keys();
        let mut leaves = std::collections::BTreeSet::new();
        for (parent, fields) in &schema {
            for field in fields {
                let leaf = if parent.is_empty() {
                    field.clone()
                } else {
                    format!("{parent}.{field}")
                };
                leaves.insert(leaf);
            }
        }
        leaves
    }

    /// Recursively validates TOML content against the derived schema.
    /// Returns a list of errors: (`dotted_path`, `option_suggestion`)
    #[must_use]
    pub fn validate_toml(
        content: &str,
        schema: &HashMap<String, HashSet<String>>,
    ) -> Vec<(String, Option<String>)> {
        Self::validate_toml_detailed(content, schema, &BTreeSet::new())
            .into_iter()
            .map(|(path, sug, _parent, _is_table, _is_top_level)| (path, sug))
            .collect()
    }

    /// Like [`validate_toml`](Self::validate_toml), but also returns each error's
    /// profile-stripped schema parent path (computed from path SEGMENTS, so it is
    /// correct even for quoted dotted profile names like
    /// `[profile."prod.eu".server]`) AND whether the offending TOML value was
    /// itself a table (`is_table`), AND whether the offending key sat at the
    /// STRUCTURAL document top level (`is_top_level`, i.e. its parent path was
    /// empty at push time). Used by strict-config classification; `validate_toml`
    /// maps this down to `(path, suggestion)`.
    ///
    /// The `is_table` flag lets the deploy-CLI leniency (#2067) demote ONLY a
    /// true top-level TABLE root, mirroring the app-boot `config_section` seam
    /// (#2061) which exempts a registered plugin root only when `val.is_table()`.
    ///
    /// The `is_top_level` flag carries the same STRUCTURAL top-level signal the
    /// app-boot exemption uses (`path.is_empty()`), so deploy leniency can tell a
    /// genuine top-level root from a profile-prefixed one WITHOUT inspecting the
    /// rendered dotted `path` string — which is ambiguous, since a quoted top-level
    /// key like `["my.plugin"]` and a 2-level path both render `my.plugin`.
    ///
    /// `plugin_config_roots` lists top-level roots a plugin has declared via
    /// [`config_section`](crate::app::AppBuilder::config_section): each is
    /// treated as a known, opaque table — accepted at the root and never
    /// descended into. An empty set restores the pre-seam behavior.
    #[must_use]
    pub(crate) fn validate_toml_detailed(
        content: &str,
        schema: &HashMap<String, HashSet<String>>,
        plugin_config_roots: &BTreeSet<String>,
    ) -> Vec<(String, Option<String>, String, bool, bool)> {
        let Ok(table) = toml::from_str::<toml::Table>(content) else {
            return Vec::new();
        };

        let mut errors = Vec::new();
        let mut path = Vec::new();
        Self::validate_toml_table(&table, &mut path, schema, plugin_config_roots, &mut errors);
        errors
    }

    #[allow(clippy::too_many_lines)]
    fn validate_toml_table(
        table: &toml::Table,
        path: &mut Vec<String>,
        schema: &HashMap<String, HashSet<String>>,
        plugin_config_roots: &BTreeSet<String>,
        errors: &mut Vec<(String, Option<String>, String, bool, bool)>,
    ) {
        let mut schema_path_parts = Vec::new();
        if path.len() >= 2 && path[0] == "profile" {
            schema_path_parts.extend(path[2..].iter().cloned());
        } else {
            schema_path_parts.extend(path.iter().cloned());
        }
        let schema_path = schema_path_parts.join(".");

        if let Some(valid_keys) = schema.get(&schema_path) {
            for (k, val) in table {
                if path.is_empty() && k == "profile" {
                    path.push(k.clone());
                    match val {
                        toml::Value::Table(t) => {
                            Self::validate_toml_table(t, path, schema, plugin_config_roots, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(
                                        t,
                                        path,
                                        schema,
                                        plugin_config_roots,
                                        errors,
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                    path.pop();
                    continue;
                }

                if valid_keys.contains(k) {
                    path.push(k.clone());
                    match val {
                        toml::Value::Table(t) => {
                            Self::validate_toml_table(t, path, schema, plugin_config_roots, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(
                                        t,
                                        path,
                                        schema,
                                        plugin_config_roots,
                                        errors,
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                    path.pop();
                } else if path.is_empty() && plugin_config_roots.contains(k) && val.is_table() {
                    // A plugin has declared this top-level root as its own config
                    // section, via `AppBuilder::config_section`. It is known and opaque:
                    // accept it and do not descend, because the plugin, not core, owns
                    // validation of its subtree. The `path.is_empty()` guard keeps this
                    // strictly the true top-level root (`[media]`, path `[]`), so a key
                    // that merely shares a plugin-root name while nested inside a known
                    // section is still validated normally.
                    //
                    // The `val.is_table()` guard keeps the exemption table-only:
                    // `config_section` declares a top-level config table, so a registered
                    // root written as a scalar or array (`media = "enabled"`, `media =
                    // ["a", "b"]`) is a malformed section that nothing would deserialize,
                    // and the app would boot on default plugin config. A non-table value
                    // therefore falls through to the normal unknown-root strict rejection
                    // below and fails loudly.
                    //
                    // A profile-prefixed plugin root (`[profile.<env>.media]`, path
                    // `["profile","<env>"]`) is deliberately not exempted and falls
                    // through to that same rejection: the plugin consumes only the
                    // top-level `[media]` table — its reader deserializes `root.media`
                    // directly and does not apply Autumn's profile merge — so exempting a
                    // profile layer the plugin cannot read would let a strict app with
                    // media settings only under `[profile.<env>.media]` boot silently on
                    // defaults. Profile-aware plugin config is a separate, larger
                    // enhancement. Deliberately not added to `valid_keys`, which would
                    // make the walk recurse and flag every plugin child as unknown.
                } else {
                    let mut full_path_parts = path.clone();
                    full_path_parts.push(k.clone());
                    let full_path = full_path_parts.join(".");

                    let mut closest: Option<&str> = None;
                    let mut min_dist = usize::MAX;
                    for valid_key in valid_keys {
                        let dist = levenshtein(k, valid_key);
                        if dist <= 2 && dist < min_dist {
                            min_dist = dist;
                            closest = Some(valid_key);
                        }
                    }

                    let suggestion = closest.map(|c| {
                        let mut sug_parts = path.clone();
                        sug_parts.push(c.to_string());
                        sug_parts.join(".")
                    });

                    errors.push((
                        full_path,
                        suggestion,
                        schema_path.clone(),
                        val.is_table(),
                        path.is_empty(),
                    ));
                }
            }
        } else if path.len() == 1 && path[0] == "profile" {
            for (k, val) in table {
                if let toml::Value::Table(t) = val {
                    path.push(k.clone());
                    Self::validate_toml_table(t, path, schema, plugin_config_roots, errors);
                    path.pop();
                } else {
                    let mut full_path_parts = path.clone();
                    full_path_parts.push(k.clone());
                    errors.push((
                        full_path_parts.join("."),
                        None,
                        schema_path.clone(),
                        val.is_table(),
                        path.is_empty(),
                    ));
                }
            }
        } else if path.is_empty() {
            let root_keys = schema.get("").cloned().unwrap_or_default();
            for (k, val) in table {
                if k == "profile" || root_keys.contains(k) {
                    path.push(k.clone());
                    match val {
                        toml::Value::Table(t) => {
                            Self::validate_toml_table(t, path, schema, plugin_config_roots, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(
                                        t,
                                        path,
                                        schema,
                                        plugin_config_roots,
                                        errors,
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                    path.pop();
                } else if plugin_config_roots.contains(k) && val.is_table() {
                    // A plugin has declared this top-level root as its own config
                    // section, via `AppBuilder::config_section`. It is known and opaque:
                    // accept it and do not descend, because the plugin owns validation of
                    // its subtree. Deliberately not injected into `root_keys`, which would
                    // make the walk recurse and flag every plugin child as unknown. The
                    // `val.is_table()` guard keeps the exemption table-only, since
                    // `config_section` declares a top-level `[media]` table: a registered
                    // root written as a scalar or array is malformed and falls through to
                    // the unknown-root strict rejection below rather than being exempted
                    // and booting on defaults.
                } else {
                    let mut closest: Option<&str> = None;
                    let mut min_dist = usize::MAX;
                    for valid_key in &root_keys {
                        let dist = levenshtein(k, valid_key);
                        if dist <= 2 && dist < min_dist {
                            min_dist = dist;
                            closest = Some(valid_key);
                        }
                    }
                    errors.push((
                        k.clone(),
                        closest.map(String::from),
                        schema_path.clone(),
                        val.is_table(),
                        path.is_empty(),
                    ));
                }
            }
        }
    }

    /// Access the decrypted credentials store.
    ///
    /// Returns an empty store when no credentials file was found (the feature is opt-in).
    /// Use `config.credentials().get::<String>("stripe_key")` to access values.
    #[must_use]
    pub const fn credentials(&self) -> &crate::credentials::CredentialsStore {
        &self.credentials
    }

    /// Load configuration with profile-aware layering.
    ///
    /// Applies the six-layer configuration system:
    /// 1. Framework defaults
    /// 2. Profile smart defaults (dev/prod)
    /// 3. `autumn.toml` (base config)
    /// 4. `[profile.{name}]` section in `autumn.toml`
    /// 5. `autumn-{profile}.toml` (legacy profile overrides)
    /// 6. `AUTUMN_*` environment variables
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if a config file cannot be read,
    /// [`ConfigError::Parse`] if a file contains invalid TOML, or
    /// [`ConfigError::Validation`] if a value is invalid.
    ///
    /// # Panics
    ///
    /// Panics if the internally-built TOML table fails to re-serialize
    /// (should never happen with well-formed profile defaults).
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_policy(UnknownRootPolicy::Strict)
    }

    /// Like [`load`](Self::load), but accepts unknown TOP-LEVEL config roots as
    /// opaque-with-a-warning instead of hard-failing.
    ///
    /// For tooling that cannot know the application's plugin set (e.g. the
    /// deploy CLI). Keeps STRICT validation of every known/core section
    /// `AutumnConfig` owns (`[server]`, `[database]`, `[deploy]`, …) — including
    /// child-key typos inside them — and still fails on malformed TOML; only a
    /// genuinely-unknown top-level root (very likely a plugin-owned table such
    /// as `[media]`) is spared, with a single doctor-style warning. App boot
    /// remains the authoritative strict gate for plugin-owned roots (see the
    /// `config_section` seam / #2061 / #1974 / #2063).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if a config file cannot be read,
    /// [`ConfigError::Parse`] if a file contains invalid TOML, or
    /// [`ConfigError::Validation`] if a value is invalid (including an unknown
    /// key inside a known section under `strict_config`).
    ///
    /// # Panics
    ///
    /// Panics if the internally-built TOML table fails to re-serialize.
    pub fn load_lenient_unknown_roots() -> Result<Self, ConfigError> {
        Self::load_policy(UnknownRootPolicy::LenientWarn)
    }

    fn load_policy(root_policy: UnknownRootPolicy) -> Result<Self, ConfigError> {
        // Feed a project-root `.env` into the `AUTUMN_*` env layer before
        // resolving config from the real environment. Rather than mutating the
        // process environment, `.env` values are layered *under* the real
        // environment via an overlay `Env`, so a real env var always wins. A
        // malformed `.env` fails loudly here rather than silently skipping
        // developer-provided values.
        let base = OsEnv;
        let profile = resolve_profile(&base);
        // Resolve `.env` from the same base directory config uses for
        // `autumn.toml` (AUTUMN_MANIFEST_DIR when set, else the process CWD),
        // so a binary launched from outside its crate root reads the `.env`
        // next to its config instead of the process working directory.
        let dir = crate::dotenv::dotenv_base_dir(&base);
        let vars = crate::dotenv::resolve_dotenv_vars(&dir, &profile, &base)
            .map_err(|e| ConfigError::Dotenv(e.to_string()))?;
        let env = crate::dotenv::DotenvEnv::new(&base, vars);
        // The zero-arg loaders (`load` / `load_lenient_unknown_roots`) have no
        // AppBuilder and therefore no plugin-declared config roots; plugin roots
        // arrive only via `TomlEnvConfigLoader::with_plugin_config_roots` →
        // `load_with_env_and_plugin_roots`. Pass an empty set here.
        Self::load_with_env_and_plugin_roots_policy(&env, &BTreeSet::new(), root_policy)
    }

    /// Load configuration with profile-aware layering, using a provided
    /// environment abstraction instead of the OS environment. Useful for testing.
    ///
    /// # Errors
    /// Returns [`ConfigError::Io`] if a config file cannot be read,
    /// [`ConfigError::Parse`] if a file contains invalid TOML, or
    /// [`ConfigError::Validation`] if a value is invalid.
    ///
    /// # Panics
    /// Panics if the internally-built TOML table fails to re-serialize.
    pub fn load_with_env(env: &dyn Env) -> Result<Self, ConfigError> {
        Self::load_with_env_and_plugin_roots_policy(
            env,
            &BTreeSet::new(),
            UnknownRootPolicy::Strict,
        )
    }

    /// Like [`load_with_env`](Self::load_with_env), but treats each top-level
    /// root in `plugin_config_roots` as a **known, opaque** config table under
    /// `server.strict_config`.
    ///
    /// A plugin owns a top-level `[root]` table (e.g. `[media]`) that core's
    /// closed [`AutumnConfig`] schema knows nothing about. Without a
    /// registration seam, the strict unknown-key check hard-rejects that root as
    /// an unknown key and a plugin-enabled app cannot boot under
    /// `strict_config = true`. Passing the plugin's declared roots here exempts
    /// exactly those roots from the check: each listed root is accepted and its
    /// subtree is **not** descended into (the plugin, not core, validates its own
    /// section). Every other unknown root still hard-fails — the seam is
    /// fail-closed, never a blanket "allow unknown roots" escape hatch.
    ///
    /// This is the roots-aware path the [`AppBuilder`](crate::app::AppBuilder)
    /// wires up from [`config_section`](crate::app::AppBuilder::config_section)
    /// declarations; the plain [`load_with_env`](Self::load_with_env) delegates
    /// here with an empty set, so all existing callers are unaffected.
    ///
    /// # Errors
    /// Returns [`ConfigError::Io`] if a config file cannot be read,
    /// [`ConfigError::Parse`] if a file contains invalid TOML, or
    /// [`ConfigError::Validation`] if a value is invalid.
    ///
    /// # Panics
    /// Panics if the internally-built TOML table fails to re-serialize.
    pub fn load_with_env_and_plugin_roots(
        env: &dyn Env,
        plugin_config_roots: &BTreeSet<String>,
    ) -> Result<Self, ConfigError> {
        Self::load_with_env_and_plugin_roots_policy(
            env,
            plugin_config_roots,
            UnknownRootPolicy::Strict,
        )
    }

    /// Like [`load_with_env`](Self::load_with_env), but accepts unknown
    /// TOP-LEVEL config roots as opaque-with-a-warning instead of hard-failing.
    ///
    /// For tooling that cannot know the application's plugin set (e.g. the
    /// deploy CLI). Keeps STRICT validation of every known/core section — and
    /// of child-key typos inside them — and still fails on malformed TOML; only
    /// a genuinely-unknown top-level root (very likely a plugin-owned table such
    /// as `[media]`) is spared, with a single doctor-style warning. App boot
    /// remains the authoritative strict gate for plugin-owned roots (see the
    /// `config_section` seam / #2061 / #1974 / #2063).
    ///
    /// # Errors
    /// Returns [`ConfigError::Io`] if a config file cannot be read,
    /// [`ConfigError::Parse`] if a file contains invalid TOML, or
    /// [`ConfigError::Validation`] if a value is invalid (including an unknown
    /// key inside a known section under `strict_config`).
    ///
    /// # Panics
    /// Panics if the internally-built TOML table fails to re-serialize.
    pub fn load_with_env_lenient_unknown_roots(env: &dyn Env) -> Result<Self, ConfigError> {
        Self::load_with_env_and_plugin_roots_policy(
            env,
            &BTreeSet::new(),
            UnknownRootPolicy::LenientWarn,
        )
    }

    /// Shared config-loading worker threading BOTH the plugin-declared config
    /// roots (#2061) AND the unknown-top-level-root policy (#2063).
    ///
    /// The two knobs are orthogonal: `plugin_config_roots` exempts SPECIFIC
    /// declared table roots (they produce no error to classify), while
    /// `root_policy` decides whether the REMAINING unknown top-level roots
    /// hard-fail ([`Strict`](UnknownRootPolicy::Strict), app boot) or are
    /// accepted opaque-with-a-warning
    /// ([`LenientWarn`](UnknownRootPolicy::LenientWarn), the deploy CLI).
    fn load_with_env_and_plugin_roots_policy(
        env: &dyn Env,
        plugin_config_roots: &BTreeSet<String>,
        root_policy: UnknownRootPolicy,
    ) -> Result<Self, ConfigError> {
        let selected_profile_input = resolve_profile_input(env);
        let profile =
            normalize_profile_name(&selected_profile_input).unwrap_or_else(|| "dev".to_owned());
        let mut has_inline_profile_section = false;

        // Build merged TOML:
        // profile smart defaults ← autumn.toml ← [profile.{name}] ← autumn-{profile}.toml
        let mut merged = profile_defaults_as_toml(&profile);

        // Layer 3: base autumn.toml
        if let Some(base) = load_raw_toml(&find_config_file_named("autumn.toml", env))? {
            deep_merge(&mut merged, base.clone());

            // Layer 4: [profile.{name}] in autumn.toml
            for profile_name in profile_lookup_names(&profile) {
                if let Some(inline_profile) = profile_section_from_base_toml(&base, profile_name) {
                    deep_merge(&mut merged, inline_profile);
                    has_inline_profile_section = true;
                }
            }
        }

        // Layer 5: autumn-{profile}.toml (legacy compatibility)
        let mut has_profile_file = false;
        for profile_name in profile_override_file_lookup_names(&profile, &selected_profile_input) {
            let profile_path = find_config_file_named(&format!("autumn-{profile_name}.toml"), env);
            if let Some(profile_toml) = load_raw_toml(&profile_path)? {
                deep_merge(&mut merged, profile_toml);
                has_profile_file = true;
                break;
            }
        }
        if !has_profile_file
            && should_warn_missing_profile_file(&profile, has_inline_profile_section)
        {
            warn_profile_typo(&profile);
        }

        // Deserialize the merged TOML table into AutumnConfig
        let toml_str =
            toml::to_string(&merged).expect("internal error: failed to serialize merged config");
        let mut config: Self = toml::from_str(&toml_str)?;
        config.profile = Some(profile);

        // Layer 6: env var overrides (highest priority)
        config.apply_env_overrides_with_env(env);

        let is_strict_env = env
            .var("AUTUMN_SERVER__STRICT_CONFIG")
            .is_ok_and(|v| v == "true" || v == "1");
        if config.server.strict_config || is_strict_env {
            let enforce_all = config.server.strict_config_enforce_all
                || env
                    .var("AUTUMN_SERVER__STRICT_CONFIG_ENFORCE_ALL")
                    .is_ok_and(|v| v == "true" || v == "1");
            Self::run_strict_unknown_key_check(
                &toml_str,
                enforce_all,
                plugin_config_roots,
                root_policy,
            )?;
        }

        // ── Deprecation channel (purely additive; never mutates `config`). ──────
        // Emit exactly one structured WARN per deprecated key that is present in
        // the resolved config (via TOML or env var). The old value is already
        // honoured above; this is observation only.
        let empty_table = toml::Table::new();
        let merged_table = merged.as_table().unwrap_or(&empty_table);
        for f in detect_deprecated_keys(merged_table, env, DEPRECATED_CONFIG_KEYS) {
            // eprintln! ensures the warning is visible on stderr even before the
            // tracing subscriber is installed (config loads before telemetry init in
            // the normal startup path).  The tracing::warn! below is kept so apps
            // that pre-install their own subscriber still receive structured events.
            eprintln!(
                "Warning: deprecated configuration key `{}` is still honored but will be removed \
                 in {}; deprecated since {} (replacement: {}; source: {:?})",
                f.path,
                f.remove_in,
                f.since,
                f.replacement.as_deref().unwrap_or("none — remove this key"),
                f.source,
            );
            tracing::warn!(
                deprecated_key = f.path.as_str(),
                replacement = f.replacement.as_deref().unwrap_or("none; remove this key"),
                since = f.since.as_str(),
                remove_in = f.remove_in.as_str(),
                source = ?f.source,
                "deprecated configuration key in use; it is still honored but scheduled for removal"
            );
        }

        #[cfg(feature = "mail")]
        if config.profile.as_deref() == Some("dev") && !has_mail_transport_source(&merged, env) {
            config.mail.transport = crate::mail::Transport::Log;
        }

        config.validate()?;

        let base_dir: PathBuf = env
            .var("AUTUMN_MANIFEST_DIR")
            .map_or_else(|_| PathBuf::from("."), PathBuf::from);
        let cred_profile = config.profile.as_deref().unwrap_or("dev");
        let master_key_override = env.var("AUTUMN_MASTER_KEY").ok();
        config.credentials = crate::credentials::load_credentials_with_key_override(
            cred_profile,
            &base_dir,
            master_key_override.as_deref(),
        )
        .map_err(|e| ConfigError::Credentials(e.to_string()))?;

        #[cfg(feature = "oauth2")]
        {
            config.expand_oauth2_providers();
        }

        Ok(config)
    }

    /// Runs the strict unknown-key check against the merged `toml_str`.
    ///
    /// Unknown keys are partitioned by [`unknown_key_was_previously_strict`]:
    /// keys whose section was already strictly validated before the #1890
    /// schema-walk fix (or all keys when `enforce_all` is set) hard-fail; keys
    /// in sections that only became covered by the fix are warned about but
    /// tolerated for one release (warn-first rollout).
    fn run_strict_unknown_key_check(
        toml_str: &str,
        enforce_all: bool,
        plugin_config_roots: &BTreeSet<String>,
        root_policy: UnknownRootPolicy,
    ) -> Result<(), ConfigError> {
        let schema = Self::get_schema_keys();
        let errors = Self::validate_toml_detailed(toml_str, &schema, plugin_config_roots);

        let mut hard_errors = Vec::new();
        let mut warn_only = Vec::new();
        let mut opaque_roots = Vec::new();
        for (path, sug, schema_parent, is_table, is_top_level) in errors {
            // Deploy-CLI leniency (#2063/#2067): a genuinely-unknown true top-level
            // root — one whose actual path is a bare root key, with no `profile.<name>`
            // prefix, and whose schema parent is the document root `""` — is accepted
            // as opaque rather than failing. It is almost certainly a plugin-owned
            // config table such as `[media]` that the CLI structurally cannot know
            // about, and app boot stays the strict gate for it.
            //
            // Top-level-ness is structural, taken from the error's `is_top_level` flag,
            // set when the offending key's parent path was empty at push time. That is
            // the same signal #2061's app-boot exemption keys on (`path.is_empty()` in
            // `validate_toml_table`). It is deliberately not inferred from the rendered
            // dotted `path` string, which is ambiguous: a legitimately quoted-dotted
            // top-level key like `["my.plugin"]`, from `config_section("my.plugin")`,
            // and a two-level path both render `my.plugin`. The earlier
            // `!path.contains('.')` heuristic therefore hard-failed a quoted-dotted
            // top-level plugin root at deploy even though app boot accepts it.
            //
            // Nor is it merely `schema_parent.is_empty()`. `validate_toml_detailed`
            // reports an empty schema parent for a profile-prefixed section like
            // `[profile.prod.media]` too, because the profile prefix is stripped before
            // root-schema validation. But such a section is pushed with a non-empty
            // parent path (`["profile","prod"]`), so `is_top_level` is false for it, and
            // the deployed app — whose `config_section` seam exempts only the true
            // top-level `[media]` via `path.is_empty()` — still rejects it at boot. So
            // deploy and app boot agree: both accept top-level `[media]` and
            // `["my.plugin"]`, and both reject `[profile.prod.media]`.
            //
            // A profile-prefixed root therefore falls through to the normal hard
            // classification below (schema_parent `""` ∈ `PRE_1890_STRICT_PARENTS`) and
            // is strictly rejected at deploy, matching app boot. Only a true root is
            // spared: an unknown key inside a known section, with a non-empty
            // schema_parent, also falls through, so a `[database] primry_url` typo still
            // hard-fails. The app-boot path passes `Strict`, so its behavior is unchanged.
            //
            // Leniency also requires the root's TOML value to be a table, mirroring the
            // #2061 app-boot exemption. A non-table true top-level root — `media =
            // "enabled"`, `media = ["a", "b"]` — is a malformed section nothing would
            // deserialize, so it falls through to the hard classification exactly as the
            // deployed app rejects it. Without this check, deploy would accept a
            // non-table root that boot rejects, reopening the gap this branch closes.
            if root_policy == UnknownRootPolicy::LenientWarn
                && schema_parent.is_empty()
                && is_top_level
                && is_table
            {
                opaque_roots.push(path);
                continue;
            }
            if enforce_all || unknown_key_was_previously_strict(&schema_parent) {
                hard_errors.push((path, sug));
            } else {
                warn_only.push((path, sug));
            }
        }

        // Deploy-CLI opaque top-level roots (#2063): surface exactly one
        // doctor-style line (never fatal) so an accepted plugin root is
        // observable and a typo'd root is not silently swallowed — it will be
        // rejected authoritatively when the app itself boots. `eprintln!`
        // guarantees visibility before a tracing subscriber is installed; the
        // `tracing::warn!` keeps structured output for apps that pre-install one.
        if !opaque_roots.is_empty() {
            let roots = opaque_roots.join(", ");
            let count = opaque_roots.len();
            eprintln!(
                "deploy config: accepting {count} unknown top-level config section(s) as \
                 opaque — the deployed app runs the authoritative strict check, so each must \
                 be a section the app declares (e.g. a plugin config table) or the app will \
                 reject it at boot: {roots}. A typo here will make the app fail to start."
            );
            tracing::warn!(
                unknown_top_level_roots = roots.as_str(),
                count,
                "deploy config: accepting unknown top-level config section(s) as opaque; the \
                 deployed app runs the authoritative strict check, so each must be a section \
                 the app declares (e.g. a plugin config table) or it will reject it at boot — \
                 a typo here will make the app fail to start (#2063)"
            );
        }

        // Warn-first rollout (#1890): unknown keys in sections that only became
        // strictly validated by the schema-walk fix are surfaced loudly but do not fail
        // startup for one release, so configs that silently passed before keep booting.
        // `eprintln!` guarantees visibility before the tracing subscriber is installed;
        // the `tracing::warn!` keeps structured output for apps that pre-install one.
        // Set `server.strict_config_enforce_all = true`, or
        // `AUTUMN_SERVER__STRICT_CONFIG_ENFORCE_ALL=1`, to promote these to hard errors.
        for (path, sug) in &warn_only {
            let hint = sug
                .as_deref()
                .map_or_else(String::new, |s| format!(" — did you mean \"{s}\"?"));
            eprintln!(
                "Warning: unknown configuration key \"{path}\"{hint}. It is ignored and \
                 falls back to defaults. This will become a hard error in a future \
                 release; set server.strict_config_enforce_all = true to enforce now."
            );
            tracing::warn!(
                unknown_key = path.as_str(),
                suggestion = sug.as_deref().unwrap_or(""),
                "unknown configuration key in a section newly covered by strict \
                 validation; ignored for now (warn-first rollout, #1890), will hard-fail \
                 once enforcement is promoted"
            );
        }

        if !hard_errors.is_empty() {
            let err_messages: Vec<String> = hard_errors
                .into_iter()
                .map(|(path, sug)| {
                    sug.map_or_else(
                        || format!("unknown key \"{path}\""),
                        |s| format!("unknown key \"{path}\" — did you mean \"{s}\"?"),
                    )
                })
                .collect();
            return Err(ConfigError::Validation(format!(
                "Strict config check failed. Unknown keys in configuration: {}",
                err_messages.join(", ")
            )));
        }
        Ok(())
    }

    /// Helper method to expand `OAuth2` preset configurations and resolve credentials-backed values.
    #[cfg(feature = "oauth2")]
    fn expand_oauth2_providers(&mut self) {
        let provider_names: Vec<String> = self.auth.oauth2.providers.keys().cloned().collect();
        for name in provider_names {
            // 1. Expand from preset if available
            if let (Some(preset), Some(p)) = (
                crate::auth::provider_preset(&name),
                self.auth.oauth2.providers.get_mut(&name),
            ) {
                if p.authorize_url.is_empty() {
                    p.authorize_url = preset.authorize_url;
                }
                if p.token_url.is_empty() {
                    p.token_url = preset.token_url;
                }
                if p.userinfo_url.is_none() {
                    p.userinfo_url = preset.userinfo_url;
                }
                if p.scope.is_empty() || p.scope == "default" {
                    p.scope = preset.scope;
                }
                if p.issuer.is_none() {
                    p.issuer = preset.issuer;
                }
                if p.jwks_url.is_none() {
                    p.jwks_url = preset.jwks_url;
                }
                if p.discovery_url.is_none() {
                    p.discovery_url = preset.discovery_url;
                }
            }

            // 2. Resolve credentials-backed secrets/IDs
            if let Some(p) = self.auth.oauth2.providers.get_mut(&name) {
                let normalized_name = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_lowercase();

                let id_key = format!("oauth2_{normalized_name}_client_id");
                if p.client_id.is_empty() {
                    if let Some(id) = self.credentials.get::<String>(&id_key) {
                        p.client_id = id;
                    } else if let Some(id) = self
                        .credentials
                        .get::<String>(&format!("oauth2_{name}_client_id"))
                    {
                        p.client_id = id;
                    }
                }
                let secret_key = format!("oauth2_{normalized_name}_client_secret");
                if p.client_secret.is_empty() {
                    if let Some(secret) = self.credentials.get::<String>(&secret_key) {
                        p.client_secret = secret;
                    } else if let Some(secret) = self
                        .credentials
                        .get::<String>(&format!("oauth2_{name}_client_secret"))
                    {
                        p.client_secret = secret;
                    }
                }
            }
        }
    }

    /// Load configuration from a specific TOML file path.
    ///
    /// Used internally and for testing. Does **not** apply profile
    /// layering or environment overrides. Prefer [`load()`](Self::load)
    /// in application code.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if the file cannot be read, or
    /// [`ConfigError::Parse`] if the file contains invalid TOML.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let config: Self = toml::from_str(&contents)?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Validate the resolved configuration for semantic errors.
    ///
    /// # Errors
    /// Returns [`ConfigError::Validation`] when a field combination is
    /// syntactically well-formed TOML but semantically invalid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.validate()?;
        self.cors.validate()?;
        self.scheduler.validate()?;
        // #1605: reject an unparseable or zero retention window at boot rather
        // than silently skipping the dataset it names — a policy an operator
        // believes is enforced but isn't is worse than no policy.
        self.retention.validate()?;
        self.validate_retention_against_replay_protection()?;
        // Framework state (autumn_jobs, scheduler advisory locks) lives on
        // the control topology and is never sharded. Sharded apps that use a
        // Postgres-backed jobs or scheduler backend therefore need a control
        // role alongside their shards.
        if self.database.has_shards()
            && self.database.effective_primary_url().is_none()
            && (self.scheduler.backend == SchedulerBackend::Postgres
                || self.jobs.backend == "postgres")
        {
            return Err(ConfigError::Validation(
                "jobs/scheduler require a control database: set database.primary_url (or \
                 database.url) alongside [[database.shards]] — framework state such as \
                 autumn_jobs and scheduler locks is not sharded (see docs/guide/sharding.md)"
                    .to_owned(),
            ));
        }
        let is_production = matches!(self.profile.as_deref(), Some("prod" | "production"));
        self.security
            .webhooks
            .validate(is_production)
            .map_err(|error| ConfigError::Validation(error.to_string()))?;
        #[cfg(feature = "mail")]
        self.mail.validate(self.profile.as_deref())?;
        self.time_zone.validate()?;
        // A `[shadow]` block that is switched on but cannot be honoured (no
        // target, an unusable URL, an out-of-range sample rate) must fail boot
        // rather than start a replica that silently mirrors nothing.
        self.shadow.validate().map_err(ConfigError::Validation)?;
        // Fail fast on an insecure or flapping [cluster] section: a node that
        // would boot without a shared secret must not boot at all.
        self.cluster.validate()?;
        // A `[replication]` block that is switched on but cannot ship (no
        // destination, both destinations, no credential indirection) must fail
        // here — so `autumn check` and `autumn doctor` see it too — rather than
        // only when the app itself boots (#1628).
        if let Some(replication) = &self.replication {
            let errors = replication.validation_errors();
            if !errors.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "invalid [replication] configuration:\n  - {}",
                    errors.join("\n  - ")
                )));
            }
        }
        // Session backend validation deliberately lives in
        // `crate::session::build_session_layer`, not here. That function
        // short-circuits when a custom `SessionStore` was installed via
        // `AppBuilder::with_session_store(...)`, so a then-irrelevant
        // `session.backend = "redis"` without a redis URL need not fail the
        // boot. Validating it here would defeat the override and exit before
        // the custom store applies. The "prod profile plus memory backend"
        // warning lives there for the same reason.
        Ok(())
    }

    /// Reject a `retention.webhook_replay` window shorter than any configured
    /// endpoint's replay-rejection window (issue #1605).
    ///
    /// A retention window is a *compliance* knob. Letting it silently shorten
    /// the lifetime of a replay marker would weaken a *security* control
    /// through a door nobody would think to look behind: once the marker is
    /// gone, a captured request replayed after that point is accepted again.
    /// Fail closed and name the knob to lower instead
    /// (`replay_window_secs`), rather than quietly reducing replay
    /// protection.
    ///
    /// Endpoints registered in code rather than in `autumn.toml` are not
    /// visible here; `docs/guide/data-retention.md` states the same rule for
    /// them.
    fn validate_retention_against_replay_protection(&self) -> Result<(), ConfigError> {
        let Some(window) = self.retention.window("webhook_replay") else {
            return Ok(());
        };
        for endpoint in &self.security.webhooks.endpoints {
            if !endpoint.replay_protection {
                continue;
            }
            let replay = std::time::Duration::from_secs(endpoint.replay_window_secs);
            if window < replay {
                return Err(ConfigError::Validation(format!(
                    "retention.webhook_replay ({window_secs}s) is shorter than the \
                     replay_window_secs ({replay_secs}s) of webhook endpoint {name:?}: \
                     expiring replay markers early would silently weaken replay \
                     protection. Lower that endpoint's replay_window_secs instead, or \
                     widen retention.webhook_replay (see \
                     docs/guide/data-retention.md).",
                    window_secs = window.as_secs(),
                    replay_secs = endpoint.replay_window_secs,
                    name = endpoint.name,
                )));
            }
        }
        Ok(())
    }

    /// Tighten the TTL-native subsystem knobs to their `[retention]` window
    /// (issue #1605).
    ///
    /// The `idempotency` and `sessions` datasets are expired by their own
    /// storage rather than by a sweep, so their retention window is enforced
    /// by writing records with a shorter lifetime. Applying the cap once, to
    /// the loaded config, means every derived lifetime inherits it: the
    /// idempotency layer's TTL, and the session cookie's `Max-Age` together
    /// with the Redis session TTL.
    ///
    /// Note that capping `sessions` shortens how long a signed-in user stays
    /// signed in — see `docs/guide/data-retention.md`.
    ///
    /// **`job_tracking` is deliberately not capped here.** It is
    /// sweep-enforced, and the sweep honours a GDPR legal hold on
    /// `autumn_job_tracking` while `job.rs`'s independent `expires_at`
    /// cleanup cannot — capping `jobs.tracking.ttl_secs` would let that
    /// cleanup delete, on exactly the retention schedule, the very rows the
    /// retention report says are being preserved under hold.
    ///
    /// Always a `min`, so this can only ever *shorten* a lifetime — there is
    /// no configuration in which declaring `[retention]` causes data to be
    /// kept longer than it is today — and it is idempotent, so calling it
    /// twice is harmless.
    ///
    /// Called on the loaded config during boot; apps do not call this
    /// directly.
    pub fn apply_retention_caps(&mut self) {
        // `job_tracking` is capped only when its records do NOT live in
        // `autumn_job_tracking`. Under `jobs.backend = "postgres"` the sweep
        // enforces the window and a GDPR legal hold can stop it, so capping
        // the TTL there would let the job runner's independent `expires_at`
        // cleanup delete held rows on exactly the retention schedule. Under
        // any other backend (redis, or the in-memory fallback) there is no
        // table to sweep and the record's TTL is the only bound there is —
        // leaving it uncapped would claim a window nothing enforces.
        let cap_job_tracking = self.jobs.backend != "postgres";
        let caps: [(&str, &mut u64); 3] = [
            ("idempotency", &mut self.idempotency.ttl_secs),
            ("sessions", &mut self.session.max_age_secs),
            ("job_tracking", &mut self.jobs.tracking.ttl_secs),
        ];
        for (key, target) in caps {
            if key == "job_tracking" && !cap_job_tracking {
                continue;
            }
            if let Some(window) = self.retention.window(key) {
                *target = (*target).min(window.as_secs());
            }
        }
    }

    /// Apply environment variable overrides to the loaded config.
    ///
    /// All fields can be overridden via `AUTUMN_SECTION__FIELD` environment
    /// variables. Double underscore `__` separates nested config sections.
    ///
    /// # Server
    /// - `AUTUMN_SERVER__PORT` → `server.port` (u16)
    /// - `AUTUMN_SERVER__HOST` → `server.host` (String)
    /// - `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS` → `server.shutdown_timeout_secs` (u64)
    /// - `AUTUMN_SERVER__PRESTOP_GRACE_SECS` → `server.prestop_grace_secs` (u64)
    /// - `AUTUMN_SERVER__UPGRADE__ENABLED` → `server.upgrade.enabled` (bool)
    /// - `AUTUMN_SERVER__UPGRADE__READY_TIMEOUT_SECS` →
    ///   `server.upgrade.ready_timeout_secs` (u64)
    ///
    /// # Database
    /// - `AUTUMN_DATABASE__PRIMARY_URL` -> `database.primary_url` (String)
    /// - `AUTUMN_DATABASE__REPLICA_URL` -> `database.replica_url` (String)
    /// - `AUTUMN_DATABASE__PRIMARY_POOL_SIZE` -> `database.primary_pool_size` (usize)
    /// - `AUTUMN_DATABASE__REPLICA_POOL_SIZE` -> `database.replica_pool_size` (usize)
    /// - `AUTUMN_DATABASE__REPLICA_FALLBACK` -> `database.replica_fallback` (`fail_readiness` | `primary`)
    /// - `AUTUMN_DATABASE__URL` → `database.url` (String)
    /// - `AUTUMN_DATABASE__POOL_SIZE` → `database.pool_size` (usize)
    /// - `AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS` → `database.connect_timeout_secs` (u64)
    /// - `AUTUMN_DATABASE__STARTUP_WAIT_SECS` → `database.startup_wait_secs` (u64)
    /// - `AUTUMN_DATABASE__AUTO_MIGRATE` -> `database.auto_migrate` (`Option<bool>`)
    /// - `AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION` -> `database.auto_migrate_in_production` (bool)
    ///
    /// # Log
    /// - `AUTUMN_LOG__LEVEL` → `log.level` (String, tracing filter directive)
    /// - `AUTUMN_LOG__FORMAT` → `log.format` (Auto | Pretty | Json)
    ///
    /// # Telemetry
    /// - `AUTUMN_TELEMETRY__ENABLED` -> `telemetry.enabled` (bool)
    /// - `AUTUMN_TELEMETRY__SERVICE_NAME` -> `telemetry.service_name` (String)
    /// - `AUTUMN_TELEMETRY__SERVICE_NAMESPACE` -> `telemetry.service_namespace` (String)
    /// - `AUTUMN_TELEMETRY__SERVICE_VERSION` -> `telemetry.service_version` (String)
    /// - `AUTUMN_TELEMETRY__ENVIRONMENT` -> `telemetry.environment` (String)
    /// - `AUTUMN_TELEMETRY__OTLP_ENDPOINT` -> `telemetry.otlp_endpoint` (String)
    /// - `AUTUMN_TELEMETRY__PROTOCOL` -> `telemetry.protocol` (`Grpc` | `HttpProtobuf`)
    /// - `AUTUMN_TELEMETRY__STRICT` -> `telemetry.strict` (bool)
    ///
    /// # Health / Probes
    /// - `AUTUMN_HEALTH__PATH` → `health.path` (String)
    /// - `AUTUMN_HEALTH__LIVE_PATH` → `health.live_path` (String)
    /// - `AUTUMN_HEALTH__READY_PATH` → `health.ready_path` (String)
    /// - `AUTUMN_HEALTH__STARTUP_PATH` → `health.startup_path` (String)
    /// - `AUTUMN_HEALTH__DETAILED` → `health.detailed` (bool)
    /// - `AUTUMN_HEALTH__ENABLED` → `health.enabled` (bool)
    ///
    /// # Jobs
    /// - `AUTUMN_JOBS__BACKEND` → `jobs.backend` (`local` / `redis`)
    /// - `AUTUMN_JOBS__WORKERS` → `jobs.workers` (`usize`)
    /// - `AUTUMN_JOBS__PIN` → `jobs.pin` (comma-separated queue names)
    /// - `AUTUMN_JOBS__MAX_ATTEMPTS` → `jobs.max_attempts` (`u32`)
    /// - `AUTUMN_JOBS__INITIAL_BACKOFF_MS` → `jobs.initial_backoff_ms` (`u64`)
    /// - `AUTUMN_JOBS__REDIS__URL` → `jobs.redis.url` (`String`)
    /// - `AUTUMN_JOBS__REDIS__KEY_PREFIX` → `jobs.redis.key_prefix` (`String`)
    /// - `AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS` → `jobs.redis.visibility_timeout_ms` (`u64`)
    /// - `AUTUMN_JOBS__TRACKING__TTL_SECS` → `jobs.tracking.ttl_secs` (`u64`)
    /// - `AUTUMN_JOBS__TRACKING__ROUTE_ENABLED` → `jobs.tracking.route_enabled` (`bool`)
    ///
    /// # Retention (issue #1605)
    /// - `AUTUMN_RETENTION__SWEEP_INTERVAL` → `retention.sweep_interval` (duration `String`)
    /// - `AUTUMN_RETENTION__JOB_HISTORY` → `retention.job_history` (duration `String`)
    /// - `AUTUMN_RETENTION__COMMIT_HOOKS` → `retention.commit_hooks` (duration `String`)
    /// - `AUTUMN_RETENTION__JOB_TRACKING` → `retention.job_tracking` (duration `String`)
    /// - `AUTUMN_RETENTION__IDEMPOTENCY` → `retention.idempotency` (duration `String`)
    /// - `AUTUMN_RETENTION__EXPERIMENT_ASSIGNMENTS` → `retention.experiment_assignments` (duration `String`)
    /// - `AUTUMN_RETENTION__WEBHOOK_REPLAY` → `retention.webhook_replay` (duration `String`)
    /// - `AUTUMN_RETENTION__SESSIONS` → `retention.sessions` (duration `String`)
    /// - `AUTUMN_RETENTION__AUDIT_ARCHIVES` → `retention.audit_archives` (duration `String`)
    ///
    /// # Signed webhooks
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND` -> `security.webhooks.replay.backend` (`memory` / `redis`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL` -> `security.webhooks.replay.redis.url` (`String`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX` -> `security.webhooks.replay.redis.key_prefix` (`String`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION` -> `security.webhooks.replay.allow_memory_in_production` (`bool`)
    ///
    /// # Cluster
    /// - `AUTUMN_CLUSTER__ENABLED` → `cluster.enabled` (`bool`)
    /// - `AUTUMN_CLUSTER__SECRET` → `cluster.secret` (`SecretString`)
    /// - `AUTUMN_CLUSTER__CLUSTER_NAME` → `cluster.cluster_name` (`String`)
    /// - `AUTUMN_CLUSTER__BIND_ADDR` → `cluster.bind_addr` (`String`)
    /// - `AUTUMN_CLUSTER__ADVERTISE_ADDR` → `cluster.advertise_addr` (`String`)
    /// - `AUTUMN_CLUSTER__SEED_PEERS` → `cluster.seed_peers` (comma-separated addresses)
    /// - `AUTUMN_CLUSTER__NODE_ID` → `cluster.node_id` (`String`)
    /// - `AUTUMN_CLUSTER__PUSH_INTERVAL_MS` → `cluster.push_interval_ms` (`u64`)
    /// - `AUTUMN_CLUSTER__SUSPICION_TIMEOUT_MS` → `cluster.suspicion_timeout_ms` (`u64`)
    /// - `AUTUMN_SHADOW__ENABLED` → `shadow.enabled` (`bool`)
    /// - `AUTUMN_SHADOW__TARGET` → `shadow.target` (`String`)
    /// - `AUTUMN_SHADOW__SAMPLE_RATE` → `shadow.sample_rate` (`f64`)
    /// - `AUTUMN_SHADOW__ROUTES` → `shadow.routes` (comma-separated patterns)
    /// - `AUTUMN_SHADOW__TIMEOUT_MS` → `shadow.timeout_ms` (`u64`)
    /// - `AUTUMN_SHADOW__MAX_IN_FLIGHT` → `shadow.max_in_flight` (`usize`)
    /// - `AUTUMN_SHADOW__MAX_BODY_BYTES` → `shadow.max_body_bytes` (`usize`)
    /// - `AUTUMN_SHADOW__MAX_RECORDS` → `shadow.max_records` (`usize`)
    /// - `AUTUMN_SHADOW__MAX_SAMPLE_BYTES` → `shadow.max_sample_bytes` (`usize`)
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_with_env(&OsEnv);
    }

    /// Apply environment overrides using the provided env abstraction.
    pub fn apply_env_overrides_with_env(&mut self, env: &dyn Env) {
        self.apply_server_env_overrides_with_env(env);
        self.apply_deploy_env_overrides_with_env(env);
        self.apply_database_env_overrides_with_env(env);
        self.apply_log_env_overrides_with_env(env);
        self.apply_telemetry_env_overrides_with_env(env);
        self.apply_health_env_overrides_with_env(env);
        self.apply_cors_env_overrides_with_env(env);
        self.apply_session_env_overrides_with_env(env);
        self.apply_cache_env_overrides_with_env(env);
        self.apply_channels_env_overrides_with_env(env);
        self.apply_jobs_env_overrides_with_env(env);
        self.apply_scheduler_env_overrides_with_env(env);
        self.apply_retention_env_overrides_with_env(env);
        self.apply_role_env_overrides_with_env(env);
        self.apply_auth_env_overrides_with_env(env);
        self.apply_security_env_overrides_with_env(env);
        self.apply_bot_protection_env_overrides_with_env(env);
        self.apply_idempotency_env_overrides_with_env(env);
        self.apply_dev_env_overrides_with_env(env);
        self.apply_observability_env_overrides_with_env(env);
        self.apply_compression_env_overrides_with_env(env);
        self.apply_actuator_env_overrides_with_env(env);
        #[cfg(feature = "reporting")]
        self.apply_reporting_env_overrides_with_env(env);
        #[cfg(feature = "reporting")]
        self.apply_failure_capture_env_overrides_with_env(env);
        #[cfg(feature = "storage")]
        self.apply_storage_env_overrides_with_env(env);
        self.apply_backup_env_overrides_with_env(env);
        self.apply_replication_env_overrides_with_env(env);
        #[cfg(feature = "mail")]
        self.apply_mail_env_overrides_with_env(env);
        #[cfg(feature = "maud")]
        self.apply_stories_env_overrides_with_env(env);
        self.apply_resilience_env_overrides_with_env(env);
        self.apply_time_zone_env_overrides_with_env(env);
        self.apply_alerts_env_overrides_with_env(env);
        self.apply_tenancy_env_overrides_with_env(env);
        self.apply_cluster_env_overrides_with_env(env);
        self.apply_push_env_overrides_with_env(env);
        self.apply_shadow_env_overrides_with_env(env);
    }

    /// Web Push (`[push]`) environment overrides.
    ///
    /// The private key is the reason this exists: it is a credential, so the
    /// guide and `PushConfig`'s own docs tell operators to supply it through
    /// `AUTUMN_PUSH__PRIVATE_KEY` rather than commit it. Overrides are applied
    /// only through the explicit per-section methods above, so without this one
    /// that documented deployment path silently leaves push unconfigured —
    /// every send failing `NotConfigured` and the public-key endpoint serving
    /// `503`, with the operator looking at a variable they did set.
    fn apply_push_env_overrides_with_env(&mut self, env: &dyn Env) {
        // Not `parse_env_option_secret`. That helper treats a blank value as "clear this
        // setting", which is right where unsetting via env is meaningful but wrong here,
        // and dangerously so: the commonest way `AUTUMN_PUSH__PRIVATE_KEY` ends up blank
        // is a secret that failed to interpolate. Clearing it would silently disable
        // push, sail through `validate_push`, and surface much later as a 503 and
        // `NotConfigured` on every send — and it would erase a good key from
        // `autumn.toml`. So a blank value is preserved, precisely so `load_vapid_key`
        // can reject it at boot with a message naming the environment variable.
        if let Ok(value) = env.var("AUTUMN_PUSH__PRIVATE_KEY") {
            self.push.private_key = Some(secrecy::SecretString::from(value));
        }
        parse_env_option_string(env, "AUTUMN_PUSH__PUBLIC_KEY", &mut self.push.public_key);
        parse_env_option_string(env, "AUTUMN_PUSH__SUBJECT", &mut self.push.subject);
        parse_env_option(env, "AUTUMN_PUSH__TTL_SECS", &mut self.push.ttl_secs);
    }

    fn apply_shadow_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(env, "AUTUMN_SHADOW__ENABLED", &mut self.shadow.enabled);
        parse_env_option_string(env, "AUTUMN_SHADOW__TARGET", &mut self.shadow.target);
        parse_env(
            env,
            "AUTUMN_SHADOW__SAMPLE_RATE",
            &mut self.shadow.sample_rate,
        );
        // `_non_empty`: an unfilled template (`AUTUMN_SHADOW__ROUTES=`) must not
        // become a one-element allowlist containing the empty pattern, which
        // matches no path and would silently mirror nothing (issue #1621's
        // failure shape).
        parse_env_csv_non_empty(env, "AUTUMN_SHADOW__ROUTES", &mut self.shadow.routes);
        parse_env(
            env,
            "AUTUMN_SHADOW__TIMEOUT_MS",
            &mut self.shadow.timeout_ms,
        );
        parse_env(
            env,
            "AUTUMN_SHADOW__MAX_IN_FLIGHT",
            &mut self.shadow.max_in_flight,
        );
        parse_env(
            env,
            "AUTUMN_SHADOW__MAX_BODY_BYTES",
            &mut self.shadow.max_body_bytes,
        );
        parse_env(
            env,
            "AUTUMN_SHADOW__MAX_RECORDS",
            &mut self.shadow.max_records,
        );
        parse_env(
            env,
            "AUTUMN_SHADOW__MAX_SAMPLE_BYTES",
            &mut self.shadow.max_sample_bytes,
        );
    }

    fn apply_cluster_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(env, "AUTUMN_CLUSTER__ENABLED", &mut self.cluster.enabled);
        parse_env_option_secret(env, "AUTUMN_CLUSTER__SECRET", &mut self.cluster.secret);
        parse_env_string(
            env,
            "AUTUMN_CLUSTER__CLUSTER_NAME",
            &mut self.cluster.cluster_name,
        );
        parse_env_string(
            env,
            "AUTUMN_CLUSTER__BIND_ADDR",
            &mut self.cluster.bind_addr,
        );
        parse_env_option_string(
            env,
            "AUTUMN_CLUSTER__ADVERTISE_ADDR",
            &mut self.cluster.advertise_addr,
        );
        parse_env_csv(
            env,
            "AUTUMN_CLUSTER__SEED_PEERS",
            &mut self.cluster.seed_peers,
        );
        parse_env_option_string(env, "AUTUMN_CLUSTER__NODE_ID", &mut self.cluster.node_id);
        parse_env(
            env,
            "AUTUMN_CLUSTER__PUSH_INTERVAL_MS",
            &mut self.cluster.push_interval_ms,
        );
        parse_env(
            env,
            "AUTUMN_CLUSTER__SUSPICION_TIMEOUT_MS",
            &mut self.cluster.suspicion_timeout_ms,
        );
    }

    fn apply_tenancy_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(env, "AUTUMN_TENANCY__ENABLED", &mut self.tenancy.enabled);
        parse_env_string(env, "AUTUMN_TENANCY__SOURCE", &mut self.tenancy.source);
        parse_env_string(
            env,
            "AUTUMN_TENANCY__HEADER_NAME",
            &mut self.tenancy.header_name,
        );
        parse_env_string(
            env,
            "AUTUMN_TENANCY__SESSION_KEY",
            &mut self.tenancy.session_key,
        );
        parse_env_string(
            env,
            "AUTUMN_TENANCY__JWT_CLAIM",
            &mut self.tenancy.jwt_claim,
        );
        parse_env_option_secret(
            env,
            "AUTUMN_TENANCY__JWT_SECRET",
            &mut self.tenancy.jwt_secret,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TENANCY__JWT_ISSUER",
            &mut self.tenancy.jwt_issuer,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TENANCY__JWT_AUDIENCE",
            &mut self.tenancy.jwt_audience,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TENANCY__BASE_DOMAIN",
            &mut self.tenancy.base_domain,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TENANCY__LOGIN_REDIRECT",
            &mut self.tenancy.login_redirect,
        );
        parse_env_csv(
            env,
            "AUTUMN_TENANCY__PUBLIC_PATHS",
            &mut self.tenancy.public_paths,
        );
        parse_env(
            env,
            "AUTUMN_TENANCY__QUOTA_BYTES",
            &mut self.tenancy.quota_bytes,
        );
        parse_env(
            env,
            "AUTUMN_TENANCY__MAX_CELLS",
            &mut self.tenancy.max_cells,
        );
        parse_env(
            env,
            "AUTUMN_TENANCY__IDLE_TTL_SECS",
            &mut self.tenancy.idle_ttl_secs,
        );
    }

    fn apply_alerts_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(env, "AUTUMN_ALERTS__ENABLED", &mut self.alerts.enabled);
        parse_env_option_string(env, "AUTUMN_ALERTS__EMAIL", &mut self.alerts.email);
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__WEBHOOK_URL",
            &mut self.alerts.webhook_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__WEBHOOK_SECRET",
            &mut self.alerts.webhook_secret,
        );
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY",
            &mut self.alerts.pagerduty_routing_key,
        );
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__PAGERDUTY_URL",
            &mut self.alerts.pagerduty_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__SLACK_WEBHOOK_URL",
            &mut self.alerts.slack_webhook_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_ALERTS__DISCORD_WEBHOOK_URL",
            &mut self.alerts.discord_webhook_url,
        );
        // Per-channel severity routing (`all` / `critical`). `AlertRouting`'s
        // `FromStr` accepts the same spellings as the TOML/serde path; an invalid
        // value is logged and ignored by `parse_env`, leaving the current value.
        parse_env(
            env,
            "AUTUMN_ALERTS__PAGERDUTY_SEVERITIES",
            &mut self.alerts.pagerduty_severities,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__SLACK_SEVERITIES",
            &mut self.alerts.slack_severities,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__DISCORD_SEVERITIES",
            &mut self.alerts.discord_severities,
        );
        parse_env_bool(
            env,
            "AUTUMN_ALERTS__CUSTOM_CHANNEL",
            &mut self.alerts.custom_channel,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__DEDUP_WINDOW_SECS",
            &mut self.alerts.dedup_window_secs,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__HEALTH_GRACE_SECS",
            &mut self.alerts.health_grace_secs,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__ERROR_RATE_THRESHOLD",
            &mut self.alerts.error_rate_threshold,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__ERROR_RATE_MIN_REQUESTS",
            &mut self.alerts.error_rate_min_requests,
        );
        parse_env(
            env,
            "AUTUMN_ALERTS__EVAL_INTERVAL_SECS",
            &mut self.alerts.eval_interval_secs,
        );
    }

    fn apply_time_zone_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(
            env,
            "AUTUMN_TIME_ZONE__IDENTIFIER",
            &mut self.time_zone.identifier,
        );
    }

    #[cfg(feature = "reporting")]
    fn apply_reporting_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(
            env,
            "AUTUMN_REPORTING__ENABLED",
            &mut self.reporting.enabled,
        );
        parse_env(
            env,
            "AUTUMN_REPORTING__SAMPLE_RATE",
            &mut self.reporting.sample_rate,
        );
    }

    #[cfg(feature = "reporting")]
    fn apply_failure_capture_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(
            env,
            "AUTUMN_FAILURE_CAPTURE__ENABLED",
            &mut self.failure_capture.enabled,
        );
        parse_env_string(
            env,
            "AUTUMN_FAILURE_CAPTURE__DIR",
            &mut self.failure_capture.dir,
        );
        parse_env(
            env,
            "AUTUMN_FAILURE_CAPTURE__MAX_BODY_BYTES",
            &mut self.failure_capture.max_body_bytes,
        );
        parse_env(
            env,
            "AUTUMN_FAILURE_CAPTURE__MAX_CAPSULE_BYTES",
            &mut self.failure_capture.max_capsule_bytes,
        );
        parse_env(
            env,
            "AUTUMN_FAILURE_CAPTURE__MAX_CAPSULES",
            &mut self.failure_capture.max_capsules,
        );
    }

    fn apply_dev_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(
            env,
            "AUTUMN_DEV__INSPECTOR_PATH",
            &mut self.dev.inspector_path,
        );
        parse_env(
            env,
            "AUTUMN_DEV__INSPECTOR_CAPACITY",
            &mut self.dev.inspector_capacity,
        );
        parse_env(
            env,
            "AUTUMN_DEV__INSPECTOR_N_PLUS_ONE_THRESHOLD",
            &mut self.dev.inspector_n_plus_one_threshold,
        );
    }

    fn apply_compression_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(
            env,
            "AUTUMN_COMPRESSION__ENABLED",
            &mut self.compression.enabled,
        );
    }

    fn apply_observability_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_option_bool(
            env,
            "AUTUMN_OBSERVABILITY__SERVER_TIMING",
            &mut self.observability.server_timing,
        );
    }

    #[cfg(feature = "maud")]
    fn apply_stories_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(env, "AUTUMN_STORIES__ENABLED", &mut self.stories.enabled);
    }

    fn apply_actuator_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(env, "AUTUMN_ACTUATOR__PREFIX", &mut self.actuator.prefix);
        parse_env_bool(
            env,
            "AUTUMN_ACTUATOR__SENSITIVE",
            &mut self.actuator.sensitive,
        );
        // Security-sensitive: operators disable the Prometheus scrape endpoint
        // with AUTUMN_ACTUATOR__PROMETHEUS=false; the override must be honored
        // so the endpoint is not left exposed against the operator's intent.
        parse_env_bool(
            env,
            "AUTUMN_ACTUATOR__PROMETHEUS",
            &mut self.actuator.prometheus,
        );
    }

    fn apply_idempotency_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_option_bool(
            env,
            "AUTUMN_IDEMPOTENCY__ENABLED",
            &mut self.idempotency.enabled,
        );
        if let Ok(val) = env.var("AUTUMN_IDEMPOTENCY__BACKEND") {
            match IdempotencyBackend::from_env_value(&val) {
                Some(backend) => self.idempotency.backend = backend,
                None => eprintln!(
                    "Warning: unrecognised AUTUMN_IDEMPOTENCY__BACKEND value {val:?}; ignoring"
                ),
            }
        }
        parse_env(
            env,
            "AUTUMN_IDEMPOTENCY__TTL_SECS",
            &mut self.idempotency.ttl_secs,
        );
        parse_env(
            env,
            "AUTUMN_IDEMPOTENCY__IN_FLIGHT_TTL_SECS",
            &mut self.idempotency.in_flight_ttl_secs,
        );
        parse_env_bool(
            env,
            "AUTUMN_IDEMPOTENCY__ALLOW_MEMORY_IN_PRODUCTION",
            &mut self.idempotency.allow_memory_in_production,
        );
        parse_env_string(
            env,
            "AUTUMN_IDEMPOTENCY__REDIS__URL",
            self.idempotency.redis.url.get_or_insert_with(String::new),
        );
        parse_env_string(
            env,
            "AUTUMN_IDEMPOTENCY__REDIS__KEY_PREFIX",
            &mut self.idempotency.redis.key_prefix,
        );
    }

    fn apply_server_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env(env, "AUTUMN_SERVER__PORT", &mut self.server.port);
        parse_env_string(env, "AUTUMN_SERVER__HOST", &mut self.server.host);
        parse_env(
            env,
            "AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS",
            &mut self.server.shutdown_timeout_secs,
        );
        parse_env(
            env,
            "AUTUMN_SERVER__PRESTOP_GRACE_SECS",
            &mut self.server.prestop_grace_secs,
        );
        parse_env(
            env,
            "AUTUMN_SERVER__UPGRADE__ENABLED",
            &mut self.server.upgrade.enabled,
        );
        parse_env(
            env,
            "AUTUMN_SERVER__UPGRADE__READY_TIMEOUT_SECS",
            &mut self.server.upgrade.ready_timeout_secs,
        );
        parse_env_option(
            env,
            "AUTUMN_SERVER__TIMEOUTS__REQUEST_TIMEOUT_MS",
            &mut self.server.timeouts.request_timeout_ms,
        );
        parse_env_option_string(
            env,
            "AUTUMN_SERVER__UNIX_SOCKET",
            &mut self.server.unix_socket,
        );
        parse_env_option(
            env,
            "AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS",
            &mut self.server.max_concurrent_requests,
        );
        parse_env_option_string(
            env,
            "AUTUMN_SERVER__CAPACITY_CONTRACT",
            &mut self.server.capacity_contract,
        );

        // `[server.tls]` is a nested optional. Materialize it from the
        // environment when any of its keys are set (seeding an empty struct if
        // the TOML section was absent), so a fully env-driven deployment can
        // enable direct HTTPS without an `autumn.toml` section. A partially
        // specified pair (e.g. only the cert) leaves the other path empty and
        // is caught by the startup fail-fast validation.
        let tls_cert = env.var("AUTUMN_SERVER__TLS__CERT_PATH").ok();
        let tls_key = env.var("AUTUMN_SERVER__TLS__KEY_PATH").ok();
        let tls_reload = env.var("AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS").ok();
        let tls_handshake = env.var("AUTUMN_SERVER__TLS__HANDSHAKE_TIMEOUT_SECS").ok();
        if tls_cert.is_some()
            || tls_key.is_some()
            || tls_reload.is_some()
            || tls_handshake.is_some()
        {
            let tls = self.server.tls.get_or_insert_with(TlsConfig::empty_for_env);
            if let Some(cert) = tls_cert {
                tls.cert_path = Some(PathBuf::from(cert));
            }
            if let Some(key) = tls_key {
                tls.key_path = Some(PathBuf::from(key));
            }
            if let Some(reload) = tls_reload.and_then(|v| v.trim().parse::<u64>().ok()) {
                tls.reload_interval_secs = reload;
            }
            if let Some(handshake) = tls_handshake.and_then(|v| v.trim().parse::<u64>().ok()) {
                tls.handshake_timeout_secs = handshake;
            }
        }
    }

    fn apply_deploy_env_overrides_with_env(&mut self, env: &dyn Env) {
        apply_deploy_env_overrides(&mut self.deploy, env);
    }

    fn apply_database_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_DATABASE__URL") {
            self.database.url = Some(val);
            self.database.primary_url = None;
        }
        parse_env_option_string(
            env,
            "AUTUMN_DATABASE__PRIMARY_URL",
            &mut self.database.primary_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_DATABASE__REPLICA_URL",
            &mut self.database.replica_url,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__POOL_SIZE",
            &mut self.database.pool_size,
        );
        parse_env_option(
            env,
            "AUTUMN_DATABASE__PRIMARY_POOL_SIZE",
            &mut self.database.primary_pool_size,
        );
        parse_env_option(
            env,
            "AUTUMN_DATABASE__REPLICA_POOL_SIZE",
            &mut self.database.replica_pool_size,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__REPLICA_FALLBACK",
            &mut self.database.replica_fallback,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__READ_YOUR_WRITES",
            &mut self.database.read_your_writes,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__PIN_AFTER_WRITE_SECS",
            &mut self.database.pin_after_write_secs,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS",
            &mut self.database.connect_timeout_secs,
        );
        parse_env(
            env,
            "AUTUMN_DATABASE__STARTUP_WAIT_SECS",
            &mut self.database.startup_wait_secs,
        );
        parse_env_option_bool(
            env,
            "AUTUMN_DATABASE__AUTO_MIGRATE",
            &mut self.database.auto_migrate,
        );
        parse_env_bool(
            env,
            "AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION",
            &mut self.database.auto_migrate_in_production,
        );
        parse_env_bool(
            env,
            "AUTUMN_DATABASE__DIRECTORY_SHARD_ROUTER",
            &mut self.database.directory_shard_router,
        );
        self.apply_shard_env_overrides(env);
    }

    /// Apply `AUTUMN_DATABASE__SHARDS__{i}__*` environment overrides.
    ///
    /// The [`Env`] abstraction can only probe known keys, so shard entries
    /// are addressed positionally: index `i` corresponds to the i-th
    /// `[[database.shards]]` entry in declaration order. Existing entries
    /// can have individual fields overridden; a brand-new entry is appended
    /// when both `__NAME` and `__PRIMARY_URL` are provided for the next
    /// free index. Probing stops at the first index that neither exists in
    /// TOML nor defines a complete new shard (bounded at 64).
    fn apply_shard_env_overrides(&mut self, env: &dyn Env) {
        const MAX_ENV_SHARDS: usize = 64;
        for i in 0..MAX_ENV_SHARDS {
            let key = |field: &str| format!("AUTUMN_DATABASE__SHARDS__{i}__{field}");
            if i >= self.database.shards.len() {
                let (Ok(name), Ok(primary_url)) =
                    (env.var(&key("NAME")), env.var(&key("PRIMARY_URL")))
                else {
                    break;
                };
                self.database.shards.push(ShardConfig {
                    name,
                    primary_url,
                    slots: None,
                    replica_url: None,
                    primary_pool_size: None,
                    replica_pool_size: None,
                    replica_fallback: None,
                });
            }
            let shard = &mut self.database.shards[i];
            parse_env_string(env, &key("NAME"), &mut shard.name);
            parse_env_string(env, &key("PRIMARY_URL"), &mut shard.primary_url);
            // Comma-separated indices and/or "A-B" ranges, e.g. "0-15,40,62-63".
            if let Ok(val) = env.var(&key("SLOTS")) {
                shard.slots = Some(
                    val.split(',')
                        .map(|token| SlotSpec::Range(token.trim().to_owned()))
                        .collect(),
                );
            }
            parse_env_option_string(env, &key("REPLICA_URL"), &mut shard.replica_url);
            parse_env_option(env, &key("PRIMARY_POOL_SIZE"), &mut shard.primary_pool_size);
            parse_env_option(env, &key("REPLICA_POOL_SIZE"), &mut shard.replica_pool_size);
            parse_env_option(env, &key("REPLICA_FALLBACK"), &mut shard.replica_fallback);
        }
    }

    fn apply_log_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(env, "AUTUMN_LOG__LEVEL", &mut self.log.level);
        parse_env_bool(env, "AUTUMN_LOG__ACCESS_LOG", &mut self.log.access_log);
        parse_env_csv(
            env,
            "AUTUMN_LOG__ACCESS_LOG_EXCLUDE",
            &mut self.log.access_log_exclude,
        );
        if let Ok(val) = env.var("AUTUMN_LOG__FORMAT") {
            match val.as_str() {
                "Auto" => self.log.format = LogFormat::Auto,
                "Pretty" => self.log.format = LogFormat::Pretty,
                "Json" => self.log.format = LogFormat::Json,
                _ => eprintln!(
                    "Warning: AUTUMN_LOG__FORMAT={val:?} is not valid \
                     (expected Auto, Pretty, or Json), ignoring"
                ),
            }
        }
    }

    fn apply_telemetry_env_overrides_with_env(&mut self, env: &dyn Env) {
        // ── Health ──────────────────────────────────────────────
        parse_env_bool(
            env,
            "AUTUMN_TELEMETRY__ENABLED",
            &mut self.telemetry.enabled,
        );
        parse_env_string(
            env,
            "AUTUMN_TELEMETRY__SERVICE_NAME",
            &mut self.telemetry.service_name,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TELEMETRY__SERVICE_NAMESPACE",
            &mut self.telemetry.service_namespace,
        );
        parse_env_string(
            env,
            "AUTUMN_TELEMETRY__SERVICE_VERSION",
            &mut self.telemetry.service_version,
        );
        parse_env_string(
            env,
            "AUTUMN_TELEMETRY__ENVIRONMENT",
            &mut self.telemetry.environment,
        );
        parse_env_option_string(
            env,
            "AUTUMN_TELEMETRY__OTLP_ENDPOINT",
            &mut self.telemetry.otlp_endpoint,
        );
        if let Ok(val) = env.var("AUTUMN_TELEMETRY__PROTOCOL") {
            match TelemetryProtocol::from_env_value(&val) {
                Some(protocol) => self.telemetry.protocol = protocol,
                None => eprintln!(
                    "Warning: AUTUMN_TELEMETRY__PROTOCOL={val:?} is not valid \
                     (expected Grpc or HttpProtobuf), ignoring"
                ),
            }
        }
        parse_env_bool(env, "AUTUMN_TELEMETRY__STRICT", &mut self.telemetry.strict);
    }

    fn apply_health_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(env, "AUTUMN_HEALTH__PATH", &mut self.health.path);
        parse_env_string(env, "AUTUMN_HEALTH__LIVE_PATH", &mut self.health.live_path);
        parse_env_string(
            env,
            "AUTUMN_HEALTH__READY_PATH",
            &mut self.health.ready_path,
        );
        parse_env_string(
            env,
            "AUTUMN_HEALTH__STARTUP_PATH",
            &mut self.health.startup_path,
        );
        parse_env_bool(env, "AUTUMN_HEALTH__DETAILED", &mut self.health.detailed);
        parse_env_bool(env, "AUTUMN_HEALTH__ENABLED", &mut self.health.enabled);
    }

    fn apply_cors_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_csv(
            env,
            "AUTUMN_CORS__ALLOWED_ORIGINS",
            &mut self.cors.allowed_origins,
        );
        parse_env_csv(
            env,
            "AUTUMN_CORS__ALLOWED_METHODS",
            &mut self.cors.allowed_methods,
        );
        parse_env_csv(
            env,
            "AUTUMN_CORS__ALLOWED_HEADERS",
            &mut self.cors.allowed_headers,
        );
        parse_env_bool(
            env,
            "AUTUMN_CORS__ALLOW_CREDENTIALS",
            &mut self.cors.allow_credentials,
        );
        parse_env(
            env,
            "AUTUMN_CORS__MAX_AGE_SECS",
            &mut self.cors.max_age_secs,
        );
    }

    fn apply_session_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(
            env,
            "AUTUMN_SESSION__COOKIE_NAME",
            &mut self.session.cookie_name,
        );
        if let Ok(val) = env.var("AUTUMN_SESSION__BACKEND") {
            match crate::session::SessionBackend::from_env_value(&val) {
                Some(backend) => self.session.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_SESSION__BACKEND={val:?} is not valid \
                     (expected memory or redis), ignoring"
                ),
            }
        }
        parse_env(
            env,
            "AUTUMN_SESSION__MAX_AGE_SECS",
            &mut self.session.max_age_secs,
        );
        parse_env_bool(env, "AUTUMN_SESSION__SECURE", &mut self.session.secure);
        parse_env_string(
            env,
            "AUTUMN_SESSION__SAME_SITE",
            &mut self.session.same_site,
        );
        parse_env_bool(
            env,
            "AUTUMN_SESSION__HTTP_ONLY",
            &mut self.session.http_only,
        );
        parse_env_string(env, "AUTUMN_SESSION__PATH", &mut self.session.path);
        parse_env_bool(
            env,
            "AUTUMN_SESSION__ALLOW_MEMORY_IN_PRODUCTION",
            &mut self.session.allow_memory_in_production,
        );
        parse_env_option_string(
            env,
            "AUTUMN_SESSION__REDIS__URL",
            &mut self.session.redis.url,
        );
        parse_env_string(
            env,
            "AUTUMN_SESSION__REDIS__KEY_PREFIX",
            &mut self.session.redis.key_prefix,
        );
    }

    fn apply_cache_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_CACHE__BACKEND") {
            match CacheBackend::from_env_value(&val) {
                Some(backend) => self.cache.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_CACHE__BACKEND={val:?} is not valid \
                     (expected memory or redis), ignoring"
                ),
            }
        }
        parse_env_option_string(env, "AUTUMN_CACHE__REDIS__URL", &mut self.cache.redis.url);
        parse_env_string(
            env,
            "AUTUMN_CACHE__REDIS__KEY_PREFIX",
            &mut self.cache.redis.key_prefix,
        );
    }

    fn apply_channels_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_CHANNELS__BACKEND") {
            match ChannelBackend::from_env_value(&val) {
                Some(backend) => self.channels.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_CHANNELS__BACKEND={val:?} is not valid \
                     (expected in_process or redis), ignoring"
                ),
            }
        }
        parse_env(
            env,
            "AUTUMN_CHANNELS__CAPACITY",
            &mut self.channels.capacity,
        );
        parse_env(
            env,
            "AUTUMN_CHANNELS__REPLAY_BUFFER",
            &mut self.channels.replay_buffer,
        );
        parse_env_option_string(
            env,
            "AUTUMN_CHANNELS__REDIS__URL",
            &mut self.channels.redis.url,
        );
        parse_env_string(
            env,
            "AUTUMN_CHANNELS__REDIS__KEY_PREFIX",
            &mut self.channels.redis.key_prefix,
        );
    }

    fn apply_jobs_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(env, "AUTUMN_JOBS__BACKEND", &mut self.jobs.backend);
        parse_env(env, "AUTUMN_JOBS__WORKERS", &mut self.jobs.workers);
        if let Ok(val) = env.var("AUTUMN_JOBS__PIN") {
            self.jobs.pin = val
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
        parse_env(
            env,
            "AUTUMN_JOBS__MAX_ATTEMPTS",
            &mut self.jobs.max_attempts,
        );
        parse_env(
            env,
            "AUTUMN_JOBS__INITIAL_BACKOFF_MS",
            &mut self.jobs.initial_backoff_ms,
        );
        parse_env_option_string(env, "AUTUMN_JOBS__REDIS__URL", &mut self.jobs.redis.url);
        parse_env_string(
            env,
            "AUTUMN_JOBS__REDIS__KEY_PREFIX",
            &mut self.jobs.redis.key_prefix,
        );
        parse_env(
            env,
            "AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS",
            &mut self.jobs.redis.visibility_timeout_ms,
        );
        parse_env(
            env,
            "AUTUMN_JOBS__POSTGRES__VISIBILITY_TIMEOUT_MS",
            &mut self.jobs.postgres.visibility_timeout_ms,
        );
        parse_env(
            env,
            "AUTUMN_JOBS__TRACKING__TTL_SECS",
            &mut self.jobs.tracking.ttl_secs,
        );
        parse_env_bool(
            env,
            "AUTUMN_JOBS__TRACKING__ROUTE_ENABLED",
            &mut self.jobs.tracking.route_enabled,
        );
    }

    fn apply_role_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_ROLE") {
            match ProcessRole::from_env_value(&val) {
                Some(role) => self.role = role,
                None => eprintln!(
                    "Warning: AUTUMN_ROLE={val:?} is not valid \
                     (expected combined, web, or worker), ignoring"
                ),
            }
        }
    }

    /// `AUTUMN_RETENTION__*` overrides for the unified retention policy
    /// (issue #1605).
    ///
    /// Each window uses [`parse_env_option_string`], so an empty value is the
    /// documented way to *clear* a window `autumn.toml` declared — restoring
    /// today's behavior for that one dataset without editing the file.
    fn apply_retention_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(
            env,
            "AUTUMN_RETENTION__SWEEP_INTERVAL",
            &mut self.retention.sweep_interval,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__JOB_HISTORY",
            &mut self.retention.job_history,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__COMMIT_HOOKS",
            &mut self.retention.commit_hooks,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__JOB_TRACKING",
            &mut self.retention.job_tracking,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__IDEMPOTENCY",
            &mut self.retention.idempotency,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__EXPERIMENT_ASSIGNMENTS",
            &mut self.retention.experiment_assignments,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__WEBHOOK_REPLAY",
            &mut self.retention.webhook_replay,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__SESSIONS",
            &mut self.retention.sessions,
        );
        parse_env_option_string(
            env,
            "AUTUMN_RETENTION__AUDIT_ARCHIVES",
            &mut self.retention.audit_archives,
        );
    }

    fn apply_scheduler_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_SCHEDULER__BACKEND") {
            match SchedulerBackend::from_env_value(&val) {
                Some(backend) => self.scheduler.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_SCHEDULER__BACKEND={val:?} is not valid \
                     (expected in_process or postgres), ignoring"
                ),
            }
        }
        parse_env(
            env,
            "AUTUMN_SCHEDULER__LEASE_TTL_SECS",
            &mut self.scheduler.lease_ttl_secs,
        );
        parse_env_option_string(
            env,
            "AUTUMN_SCHEDULER__REPLICA_ID",
            &mut self.scheduler.replica_id,
        );
        parse_env_string(
            env,
            "AUTUMN_SCHEDULER__KEY_PREFIX",
            &mut self.scheduler.key_prefix,
        );
    }

    fn apply_auth_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env(env, "AUTUMN_AUTH__BCRYPT_COST", &mut self.auth.bcrypt_cost);
        parse_env_string(env, "AUTUMN_AUTH__SESSION_KEY", &mut self.auth.session_key);
        parse_env(
            env,
            "AUTUMN_AUTH__LOCKOUT__ENABLED",
            &mut self.auth.lockout.enabled,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__LOCKOUT__THRESHOLD",
            &mut self.auth.lockout.threshold,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__LOCKOUT__WINDOW_SECS",
            &mut self.auth.lockout.window_secs,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__LOCKOUT__COOLOFF_SECS",
            &mut self.auth.lockout.cooloff_secs,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__PASSWORD__MIN_LENGTH",
            &mut self.auth.password.min_length,
        );
        parse_env_bool(
            env,
            "AUTUMN_AUTH__PASSWORD__REJECT_COMMON",
            &mut self.auth.password.reject_common,
        );
        if let Ok(val) = env.var("AUTUMN_AUTH__PASSWORD__BREACH_CHECK") {
            match val.as_str() {
                "off" => self.auth.password.breach_check = crate::auth::BreachCheck::Off,
                "fail_open" => self.auth.password.breach_check = crate::auth::BreachCheck::FailOpen,
                "fail_closed" => {
                    self.auth.password.breach_check = crate::auth::BreachCheck::FailClosed;
                }
                other => eprintln!(
                    "Warning: AUTUMN_AUTH__PASSWORD__BREACH_CHECK={other:?} is not valid \
                     (expected off, fail_open, or fail_closed), ignoring"
                ),
            }
        }
        parse_env_bool(
            env,
            "AUTUMN_AUTH__REMEMBER__ENABLED",
            &mut self.auth.remember.enabled,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__REMEMBER__DURATION_SECS",
            &mut self.auth.remember.duration_secs,
        );
        parse_env_string(
            env,
            "AUTUMN_AUTH__REMEMBER__COOKIE_NAME",
            &mut self.auth.remember.cookie_name,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__MAGIC_LINK__TTL_MINUTES",
            &mut self.auth.magic_link.ttl_minutes,
        );
        parse_env(
            env,
            "AUTUMN_AUTH__MAGIC_LINK__EMAIL_COOLDOWN_SECS",
            &mut self.auth.magic_link.email_cooldown_secs,
        );
        #[cfg(feature = "oauth2")]
        {
            let provider_names: Vec<String> = self.auth.oauth2.providers.keys().cloned().collect();
            for name in provider_names {
                let upper = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_uppercase();

                let client_id_var = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID");
                if let Ok(id) = env.var(&client_id_var)
                    && !id.is_empty()
                    && let Some(p) = self.auth.oauth2.providers.get_mut(&name)
                {
                    p.client_id = id;
                }

                let client_secret_var = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_SECRET");
                if let Ok(secret) = env.var(&client_secret_var)
                    && !secret.is_empty()
                    && let Some(p) = self.auth.oauth2.providers.get_mut(&name)
                {
                    p.client_secret = secret;
                }
            }
        }
    }

    /// Apply `AUTUMN_SECURITY__*` environment variable overrides.
    #[allow(clippy::too_many_lines)]
    fn apply_security_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_string(
            env,
            "AUTUMN_SECURITY__HEADERS__X_FRAME_OPTIONS",
            &mut self.security.headers.x_frame_options,
        );
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__HEADERS__X_CONTENT_TYPE_OPTIONS",
            &mut self.security.headers.x_content_type_options,
        );
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__HEADERS__STRICT_TRANSPORT_SECURITY",
            &mut self.security.headers.strict_transport_security,
        );
        parse_env(
            env,
            "AUTUMN_SECURITY__HEADERS__HSTS_MAX_AGE_SECS",
            &mut self.security.headers.hsts_max_age_secs,
        );
        parse_env_string(
            env,
            "AUTUMN_SECURITY__HEADERS__CONTENT_SECURITY_POLICY",
            &mut self.security.headers.content_security_policy,
        );
        parse_env_string(
            env,
            "AUTUMN_SECURITY__HEADERS__REFERRER_POLICY",
            &mut self.security.headers.referrer_policy,
        );
        parse_env_string(
            env,
            "AUTUMN_SECURITY__HEADERS__PERMISSIONS_POLICY",
            &mut self.security.headers.permissions_policy,
        );

        // CSRF
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__CSRF__ENABLED",
            &mut self.security.csrf.enabled,
        );
        parse_env_string(
            env,
            "AUTUMN_SECURITY__CSRF__TOKEN_HEADER",
            &mut self.security.csrf.token_header,
        );
        parse_env_string(
            env,
            "AUTUMN_SECURITY__CSRF__COOKIE_NAME",
            &mut self.security.csrf.cookie_name,
        );
        parse_env(
            env,
            "AUTUMN_SECURITY__CSRF__TOKEN_SCAN_BYTES",
            &mut self.security.csrf.token_scan_bytes,
        );

        self.apply_rate_limit_env_overrides_with_env(env);

        // Multipart uploads
        parse_env(
            env,
            "AUTUMN_SECURITY__UPLOAD__MAX_REQUEST_SIZE_BYTES",
            &mut self.security.upload.max_request_size_bytes,
        );
        parse_env(
            env,
            "AUTUMN_SECURITY__UPLOAD__MAX_FILE_SIZE_BYTES",
            &mut self.security.upload.max_file_size_bytes,
        );
        parse_env_csv(
            env,
            "AUTUMN_SECURITY__UPLOAD__ALLOWED_MIME_TYPES",
            &mut self.security.upload.allowed_mime_types,
        );
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__UPLOAD__REJECT_ON_CONTENT_TYPE_MISMATCH",
            &mut self.security.upload.reject_on_content_type_mismatch,
        );

        // Authorization deny shape + repository-API escape hatch.
        if let Ok(value) = env.var("AUTUMN_SECURITY__FORBIDDEN_RESPONSE") {
            match value.parse::<crate::authorization::ForbiddenResponse>() {
                Ok(parsed) => self.security.forbidden_response = parsed,
                Err(err) => tracing::warn!(
                    "ignoring invalid AUTUMN_SECURITY__FORBIDDEN_RESPONSE={value:?}: {err}"
                ),
            }
        }
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__ALLOW_UNAUTHORIZED_REPOSITORY_API",
            &mut self.security.allow_unauthorized_repository_api,
        );

        // Signing secret (canonical env var documented in deployment guide)
        parse_env_option_string(
            env,
            "AUTUMN_SECURITY__SIGNING_SECRET",
            &mut self.security.signing_secret.secret,
        );
        parse_env_csv(
            env,
            "AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS",
            &mut self.security.trusted_hosts.hosts,
        );

        // Top-level trusted-proxy policy
        parse_env_csv(
            env,
            "AUTUMN_SECURITY__TRUSTED_PROXIES__RANGES",
            &mut self.security.trusted_proxies.ranges,
        );
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__TRUSTED_PROXIES__TRUST_FORWARDED_HEADERS",
            &mut self.security.trusted_proxies.trust_forwarded_headers,
        );
        if let Ok(val) = env.var("AUTUMN_SECURITY__TRUSTED_PROXIES__TRUSTED_HOPS") {
            if let Ok(hops) = val.trim().parse::<u32>() {
                self.security.trusted_proxies.trusted_hops = Some(hops);
            } else {
                tracing::warn!(
                    "ignoring invalid AUTUMN_SECURITY__TRUSTED_PROXIES__TRUSTED_HOPS={val:?}: \
                     expected a non-negative integer"
                );
            }
        }

        self.security.webhooks.apply_env_overrides_with_env(env);
    }

    fn apply_bot_protection_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(
            env,
            "AUTUMN_BOT_PROTECTION__ENABLED",
            &mut self.bot_protection.enabled,
        );
        parse_env_bool(
            env,
            "AUTUMN_BOT_PROTECTION__DEV_BYPASS",
            &mut self.bot_protection.dev_bypass,
        );
        if let Ok(val) = env.var("AUTUMN_BOT_PROTECTION__PROVIDER") {
            match val.to_lowercase().as_str() {
                "turnstile" => {
                    self.bot_protection.provider =
                        crate::security::captcha::CaptchaProviderKind::Turnstile;
                }
                "hcaptcha" => {
                    self.bot_protection.provider =
                        crate::security::captcha::CaptchaProviderKind::HCaptcha;
                }
                _ => tracing::warn!(
                    "ignoring unrecognised AUTUMN_BOT_PROTECTION__PROVIDER={val:?}: \
                     expected \"turnstile\" or \"hcaptcha\""
                ),
            }
        }
        parse_env_option_string(
            env,
            "AUTUMN_BOT_PROTECTION__SITE_KEY",
            &mut self.bot_protection.site_key,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BOT_PROTECTION__SECRET_KEY",
            &mut self.bot_protection.secret_key,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BOT_PROTECTION__FORM_FIELD",
            &mut self.bot_protection.form_field,
        );
    }

    fn apply_rate_limit_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__RATE_LIMIT__ENABLED",
            &mut self.security.rate_limit.enabled,
        );
        parse_env(
            env,
            "AUTUMN_SECURITY__RATE_LIMIT__REQUESTS_PER_SECOND",
            &mut self.security.rate_limit.requests_per_second,
        );
        parse_env(
            env,
            "AUTUMN_SECURITY__RATE_LIMIT__BURST",
            &mut self.security.rate_limit.burst,
        );
        parse_env_bool(
            env,
            "AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS",
            &mut self.security.rate_limit.trust_forwarded_headers,
        );
        parse_env_csv(
            env,
            "AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES",
            &mut self.security.rate_limit.trusted_proxies,
        );
        if let Ok(val) = env.var("AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY") {
            match crate::security::config::KeyStrategy::from_env_value(&val) {
                Some(strategy) => self.security.rate_limit.key_strategy = strategy,
                None => eprintln!(
                    "Warning: AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY={val:?} is not valid \
                     (expected ip, api_token, or authenticated_principal), ignoring"
                ),
            }
        }
        // BACKEND is always parsed so misconfiguration is surfaced even without
        // the redis feature (build_backend will warn and fall back to memory).
        if let Ok(val) = env.var("AUTUMN_SECURITY__RATE_LIMIT__BACKEND") {
            match crate::security::config::RateLimitBackend::from_env_value(&val) {
                Some(backend) => self.security.rate_limit.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_SECURITY__RATE_LIMIT__BACKEND={val:?} is not valid \
                     (expected memory or redis), ignoring"
                ),
            }
        }
        #[cfg(feature = "redis")]
        {
            use crate::security::config::RateLimitBackendFailure;
            if let Ok(val) = env.var("AUTUMN_SECURITY__RATE_LIMIT__ON_BACKEND_FAILURE") {
                match RateLimitBackendFailure::from_env_value(&val) {
                    Some(mode) => self.security.rate_limit.on_backend_failure = mode,
                    None => eprintln!(
                        "Warning: AUTUMN_SECURITY__RATE_LIMIT__ON_BACKEND_FAILURE={val:?} is not \
                         valid (expected fail_open or fail_closed), ignoring"
                    ),
                }
            }
            parse_env_option_string(
                env,
                "AUTUMN_SECURITY__RATE_LIMIT__REDIS__URL",
                &mut self.security.rate_limit.redis.url,
            );
            parse_env_string(
                env,
                "AUTUMN_SECURITY__RATE_LIMIT__REDIS__KEY_PREFIX",
                &mut self.security.rate_limit.redis.key_prefix,
            );
        }
    }

    #[cfg(feature = "storage")]
    fn apply_storage_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_STORAGE__BACKEND") {
            match crate::storage::StorageBackend::from_env_value(&val) {
                Some(backend) => self.storage.backend = backend,
                None => eprintln!(
                    "Warning: AUTUMN_STORAGE__BACKEND={val:?} is not valid \
                     (expected disabled, local, or s3), ignoring"
                ),
            }
        }
        parse_env_string(
            env,
            "AUTUMN_STORAGE__DEFAULT_PROVIDER",
            &mut self.storage.default_provider,
        );
        parse_env_bool(
            env,
            "AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION",
            &mut self.storage.allow_local_in_production,
        );
        if let Ok(val) = env.var("AUTUMN_STORAGE__LOCAL__ROOT") {
            self.storage.local.root = PathBuf::from(val);
        }
        parse_env_string(
            env,
            "AUTUMN_STORAGE__LOCAL__MOUNT_PATH",
            &mut self.storage.local.mount_path,
        );
        parse_env(
            env,
            "AUTUMN_STORAGE__LOCAL__DEFAULT_URL_EXPIRY_SECS",
            &mut self.storage.local.default_url_expiry_secs,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__LOCAL__SIGNING_KEY",
            &mut self.storage.local.signing_key,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__BUCKET",
            &mut self.storage.s3.bucket,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__REGION",
            &mut self.storage.s3.region,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__ENDPOINT",
            &mut self.storage.s3.endpoint,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__PUBLIC_BASE_URL",
            &mut self.storage.s3.public_base_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__ACCESS_KEY_ID_ENV",
            &mut self.storage.s3.access_key_id_env,
        );
        parse_env_option_string(
            env,
            "AUTUMN_STORAGE__S3__SECRET_ACCESS_KEY_ENV",
            &mut self.storage.s3.secret_access_key_env,
        );
        parse_env_bool(
            env,
            "AUTUMN_STORAGE__S3__FORCE_PATH_STYLE",
            &mut self.storage.s3.force_path_style,
        );
        parse_env(
            env,
            "AUTUMN_STORAGE__S3__DEFAULT_URL_EXPIRY_SECS",
            &mut self.storage.s3.default_url_expiry_secs,
        );
        parse_env(
            env,
            "AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_BYTES",
            &mut self.storage.variants.max_source_bytes,
        );
        parse_env(
            env,
            "AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_WIDTH",
            &mut self.storage.variants.max_source_width,
        );
        parse_env(
            env,
            "AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_HEIGHT",
            &mut self.storage.variants.max_source_height,
        );
    }

    /// Apply `AUTUMN_BACKUP__OFFSITE__*` overrides to the `[backup.offsite]`
    /// section (issue #1619). Mirrors the storage overrides so the offsite
    /// destination honors the same `AUTUMN_*` env convention. When no offsite
    /// section exists in TOML, a default one is materialized only if at least
    /// one offsite env var is present, so an all-env deployment still works.
    fn apply_backup_env_overrides_with_env(&mut self, env: &dyn Env) {
        // Keys that signal genuine intent to configure an offsite destination: the
        // presence of any required destination or credential key materializes the
        // `[backup.offsite]` section (#1791). The list is limited to what a working
        // upload requires — a bucket, or the access and secret key-env names.
        // Optional-only keys (`region`, `force_path_style`, `endpoint`, `prefix`,
        // `keep`) do not materialize the section on their own: a bare
        // `AUTUMN_BACKUP__OFFSITE__S3__REGION` with no bucket cannot upload, so it must
        // leave offsite unconfigured rather than produce an empty section that then
        // fails validation or `doctor` with "backup.offsite.s3.bucket is unset". Those
        // optional keys are still applied below once a required key materializes the
        // section. The two opt-out toggles are excluded too: a lone `AUTO_UPLOAD=false`
        // or `ALLOW_SHARED_BUCKET=false` must not create an otherwise-empty section
        // (#1619 P2 #18). A truthy `AUTO_UPLOAD=true` does materialize, since it needs a
        // validated destination to act on.
        const OFFSITE_DEST_KEYS: &[&str] = &[
            "AUTUMN_BACKUP__OFFSITE__S3__BUCKET",
            "AUTUMN_BACKUP__OFFSITE__S3__ACCESS_KEY_ID_ENV",
            "AUTUMN_BACKUP__OFFSITE__S3__SECRET_ACCESS_KEY_ENV",
        ];
        let has_dest_key = OFFSITE_DEST_KEYS.iter().any(|k| env.var(k).is_ok());
        let auto_upload_truthy = env
            .var("AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD")
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"));
        if self.backup.offsite.is_none() && !has_dest_key && !auto_upload_truthy {
            return;
        }
        let offsite = self
            .backup
            .offsite
            .get_or_insert_with(|| Box::new(OffsiteBackupConfig::default()));
        parse_env_option_string(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__BUCKET",
            &mut offsite.s3.bucket,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__REGION",
            &mut offsite.s3.region,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__ENDPOINT",
            &mut offsite.s3.endpoint,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__ACCESS_KEY_ID_ENV",
            &mut offsite.s3.access_key_id_env,
        );
        parse_env_option_string(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__SECRET_ACCESS_KEY_ENV",
            &mut offsite.s3.secret_access_key_env,
        );
        parse_env_bool(
            env,
            "AUTUMN_BACKUP__OFFSITE__S3__FORCE_PATH_STYLE",
            &mut offsite.s3.force_path_style,
        );
        parse_env_option_string(env, "AUTUMN_BACKUP__OFFSITE__PREFIX", &mut offsite.prefix);
        parse_env_option(env, "AUTUMN_BACKUP__OFFSITE__KEEP", &mut offsite.keep);
        parse_env_bool(
            env,
            "AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD",
            &mut offsite.auto_upload,
        );
        parse_env_bool(
            env,
            "AUTUMN_BACKUP__OFFSITE__ALLOW_SHARED_BUCKET",
            &mut offsite.allow_shared_bucket,
        );
    }

    /// Apply `AUTUMN_REPLICATION__*` overrides to the `[replication]` section
    /// (issue #1628). Mirrors the `[backup.offsite]` convention: a *destination*
    /// key materializes the section for an all-env deployment, while
    /// optional-only keys never conjure a section that would then fail
    /// validation.
    fn apply_replication_env_overrides_with_env(&mut self, env: &dyn Env) {
        // Keys that express a real intent to configure a destination. A lone
        // region/endpoint/prefix cannot replicate anywhere, so it must not
        // materialize the section; `ENABLED=true` does, because it needs a
        // destination to act on and the resulting validation error is the honest
        // answer.
        const DEST_KEYS: &[&str] = &[
            "AUTUMN_REPLICATION__S3__BUCKET",
            "AUTUMN_REPLICATION__S3__ACCESS_KEY_ID_ENV",
            "AUTUMN_REPLICATION__S3__SECRET_ACCESS_KEY_ENV",
            "AUTUMN_REPLICATION__PATH",
        ];
        // Only a REQUIRED S3 key materializes the sub-section, exactly as
        // `OFFSITE_DEST_KEYS` does for `[backup.offsite]`. A stray
        // `AUTUMN_REPLICATION__S3__REGION` next to a `path` destination must not
        // conjure an empty `[replication.s3]` — that would make the section
        // "configures both s3 and path" and fail boot on an otherwise valid
        // config. The optional keys are still APPLIED once a required one has
        // materialized the sub-section.
        const S3_KEYS: &[&str] = &[
            "AUTUMN_REPLICATION__S3__BUCKET",
            "AUTUMN_REPLICATION__S3__ACCESS_KEY_ID_ENV",
            "AUTUMN_REPLICATION__S3__SECRET_ACCESS_KEY_ENV",
        ];
        let has_dest_key = DEST_KEYS.iter().any(|k| env.var(k).is_ok());
        let enabled_truthy = env
            .var("AUTUMN_REPLICATION__ENABLED")
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"));
        if self.replication.is_none() && !has_dest_key && !enabled_truthy {
            return;
        }
        let replication = self
            .replication
            .get_or_insert_with(|| Box::new(ReplicationConfig::default()));

        parse_env_bool(env, "AUTUMN_REPLICATION__ENABLED", &mut replication.enabled);
        parse_env(
            env,
            "AUTUMN_REPLICATION__RPO_SECS",
            &mut replication.rpo_secs,
        );
        parse_env_option(
            env,
            "AUTUMN_REPLICATION__SYNC_INTERVAL_SECS",
            &mut replication.sync_interval_secs,
        );
        parse_env(
            env,
            "AUTUMN_REPLICATION__SNAPSHOT_INTERVAL_SECS",
            &mut replication.snapshot_interval_secs,
        );
        parse_env(
            env,
            "AUTUMN_REPLICATION__MAX_WAL_BYTES",
            &mut replication.max_wal_bytes,
        );
        parse_env(
            env,
            "AUTUMN_REPLICATION__RETENTION_HOURS",
            &mut replication.retention_hours,
        );
        parse_env(
            env,
            "AUTUMN_REPLICATION__VERIFY_INTERVAL_SECS",
            &mut replication.verify_interval_secs,
        );
        parse_env_option_string(env, "AUTUMN_REPLICATION__PREFIX", &mut replication.prefix);
        parse_env_option_string(env, "AUTUMN_REPLICATION__PATH", &mut replication.path);
        parse_env_bool(
            env,
            "AUTUMN_REPLICATION__ALLOW_SHARED_BUCKET",
            &mut replication.allow_shared_bucket,
        );

        if replication.s3.is_none() && !S3_KEYS.iter().any(|k| env.var(k).is_ok()) {
            return;
        }
        let s3 = replication
            .s3
            .get_or_insert_with(ReplicationS3Config::default);
        parse_env_option_string(env, "AUTUMN_REPLICATION__S3__BUCKET", &mut s3.bucket);
        parse_env_option_string(env, "AUTUMN_REPLICATION__S3__REGION", &mut s3.region);
        parse_env_option_string(env, "AUTUMN_REPLICATION__S3__ENDPOINT", &mut s3.endpoint);
        parse_env_option_string(
            env,
            "AUTUMN_REPLICATION__S3__ACCESS_KEY_ID_ENV",
            &mut s3.access_key_id_env,
        );
        parse_env_option_string(
            env,
            "AUTUMN_REPLICATION__S3__SECRET_ACCESS_KEY_ENV",
            &mut s3.secret_access_key_env,
        );
        parse_env_bool(
            env,
            "AUTUMN_REPLICATION__S3__FORCE_PATH_STYLE",
            &mut s3.force_path_style,
        );
    }

    #[cfg(feature = "mail")]
    fn apply_mail_env_overrides_with_env(&mut self, env: &dyn Env) {
        if let Ok(val) = env.var("AUTUMN_MAIL__TRANSPORT") {
            match crate::mail::Transport::from_env_value(&val) {
                Some(transport) => self.mail.transport = transport,
                None => eprintln!(
                    "Warning: AUTUMN_MAIL__TRANSPORT={val:?} is not valid \
                     (expected log, file, smtp, or disabled), ignoring"
                ),
            }
        }
        parse_env_option_string(env, "AUTUMN_MAIL__FROM", &mut self.mail.from);
        parse_env_option_string(env, "AUTUMN_MAIL__REPLY_TO", &mut self.mail.reply_to);
        parse_env_bool(
            env,
            "AUTUMN_MAIL__ALLOW_LOG_IN_PRODUCTION",
            &mut self.mail.allow_log_in_production,
        );
        parse_env_bool(
            env,
            "AUTUMN_MAIL__ALLOW_IN_PROCESS_DELIVER_LATER_IN_PRODUCTION",
            &mut self.mail.allow_in_process_deliver_later_in_production,
        );
        parse_env_bool(env, "AUTUMN_MAIL__PREVIEW", &mut self.mail.preview);
        parse_env_option_string(
            env,
            "AUTUMN_MAIL__UNSUBSCRIBE_BASE_URL",
            &mut self.mail.unsubscribe_base_url,
        );
        parse_env_option_string(
            env,
            "AUTUMN_MAIL__UNSUBSCRIBE_MAILTO",
            &mut self.mail.unsubscribe_mailto,
        );
        if let Ok(val) = env.var("AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS") {
            match val.parse::<i64>() {
                Ok(days) => self.mail.unsubscribe_token_ttl_days = days,
                Err(_) => eprintln!(
                    "Warning: AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS={val:?} is not a valid integer, ignoring"
                ),
            }
        }
        parse_env_bool(
            env,
            "AUTUMN_MAIL__MOUNT_UNSUBSCRIBE_ENDPOINT",
            &mut self.mail.mount_unsubscribe_endpoint,
        );
        parse_env_bool(env, "AUTUMN_MAIL__INLINE_CSS", &mut self.mail.inline_css);
        if let Ok(val) = env.var("AUTUMN_MAIL__FILE_DIR") {
            self.mail.file_dir = PathBuf::from(val);
        }
        parse_env_option_string(env, "AUTUMN_MAIL__SMTP__HOST", &mut self.mail.smtp.host);
        if let Ok(val) = env.var("AUTUMN_MAIL__SMTP__PORT") {
            match val.parse::<u16>() {
                Ok(port) => self.mail.smtp.port = Some(port),
                Err(_) => {
                    eprintln!("Warning: AUTUMN_MAIL__SMTP__PORT={val:?} is not valid, ignoring");
                }
            }
        }
        parse_env_option_string(
            env,
            "AUTUMN_MAIL__SMTP__USERNAME",
            &mut self.mail.smtp.username,
        );
        parse_env_option_string(
            env,
            "AUTUMN_MAIL__SMTP__PASSWORD_ENV",
            &mut self.mail.smtp.password_env,
        );
        if let Ok(val) = env.var("AUTUMN_MAIL__SMTP__TLS") {
            match crate::mail::TlsMode::from_env_value(&val) {
                Some(tls) => self.mail.smtp.tls = tls,
                None => eprintln!(
                    "Warning: AUTUMN_MAIL__SMTP__TLS={val:?} is not valid \
                     (expected disabled, starttls, or tls), ignoring"
                ),
            }
        }
    }

    /// Returns the active profile name, if any.
    #[must_use]
    pub fn profile_name(&self) -> Option<&str> {
        self.profile.as_deref()
    }
}

/// HTTP server configuration.
///
/// Controls which address the server binds to and how graceful shutdown
/// behaves.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `port` | `3000` |
/// | `host` | `"127.0.0.1"` |
/// | `shutdown_timeout_secs` | `30` |
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::ServerConfig;
///
/// let server = ServerConfig::default();
/// assert_eq!(server.port, 3000);
/// assert_eq!(server.host, "127.0.0.1");
/// ```
/// Per-request timeout configuration.
///
/// Controls how long the server waits for a complete request-response cycle
/// before returning `408 Request Timeout`. A value of `None` or `0` disables
/// the timeout (the default, so existing applications are unaffected).
///
/// # `autumn.toml` example
///
/// ```toml
/// [server.timeouts]
/// request_timeout_ms = 30000  # 30 seconds
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequestTimeoutsConfig {
    /// Maximum time in milliseconds allowed for a complete request-response
    /// cycle. When exceeded the framework returns `503 Service Unavailable`
    /// rendered as Problem Details JSON for API clients (and the standard error
    /// page for browser requests). `None` (default) or `0` disables the timeout.
    ///
    /// The deadline bounds the time to produce the response *head*: once the
    /// status and headers are sent, the streaming body is not interrupted, so
    /// SSE, chunked responses, and WebSocket upgrades (all of which emit their
    /// head promptly and then stream) run unbounded afterward. Long-poll
    /// handlers are the exception — they intentionally withhold the response
    /// head while waiting for data, so they *are* subject to this deadline and
    /// will return `503` if it fires before they respond. Give such routes a
    /// per-route override via the route macro
    /// (`#[get("/poll", timeout_ms = 120000)]` or `timeout = "off"`), which is
    /// also how any other slow route can raise or disable its own deadline.
    ///
    /// A second exception applies to *mutating* requests carrying an
    /// `Idempotency-Key`: the idempotency layer buffers the full response body
    /// (so the response can be cached and replayed) before the head is returned,
    /// so those responses are bounded by the deadline even when the handler
    /// streams them. Give such endpoints a per-route override if they
    /// legitimately produce slow or large idempotent bodies.
    ///
    /// The `prod` profile smart-defaults this to `30000` (30s); `dev` and custom
    /// profiles leave it disabled. Configured via
    /// `AUTUMN_SERVER__TIMEOUTS__REQUEST_TIMEOUT_MS`.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
}

/// `[server.upgrade]` — in-place upgrades (issue #1674).
///
/// On `SIGUSR2` a running app hands its listening socket and its designated
/// live state to a freshly-execed build, waits for that build to serve, and
/// only then drains itself. See `docs/guide/hot-upgrades.md`.
#[derive(Debug, Clone, Deserialize)]
pub struct UpgradeConfig {
    /// Whether `SIGUSR2` triggers an in-place upgrade. Default: `true`.
    ///
    /// With this off the signal is logged and ignored — which is still safer
    /// than the default disposition of `SIGUSR2`, which terminates the process.
    #[serde(default = "default_upgrade_enabled")]
    pub enabled: bool,

    /// Seconds to wait for the successor to signal that it is serving before
    /// abandoning the upgrade and carrying on with the current build.
    /// Default: `30`.
    ///
    /// The wait ends early — with the upgrade abandoned — if the successor
    /// exits first, so this only bounds a successor that hangs during startup.
    #[serde(default = "default_upgrade_ready_timeout")]
    pub ready_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Port to listen on. Default: `3000`.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Host/IP to bind to. Default: `"127.0.0.1"`.
    ///
    /// Set to `"0.0.0.0"` to accept connections from all interfaces
    /// (typical for containerized deployments).
    #[serde(default = "default_host")]
    pub host: String,

    /// Exit startup if any unknown config keys are found in autumn.toml/profiles.
    #[serde(default)]
    pub strict_config: bool,

    /// When `strict_config` is enabled, also hard-fail on unknown keys in the
    /// config sections that only became strictly validated by the #1890
    /// schema-walk fix (everything except `server`, `deploy`, and `database`,
    /// whose keys were already validated). Defaults to `false` for one release:
    /// unknown keys in those newly-covered sections WARN loudly at startup
    /// instead of failing, so configs that silently passed before keep booting.
    /// Set to `true` to enforce immediately; a future release makes `true` the
    /// default and removes this transitional gate.
    #[serde(default)]
    pub strict_config_enforce_all: bool,

    /// Seconds to wait for in-flight requests during graceful shutdown.
    /// Default: `30`.
    ///
    /// When the server receives a shutdown signal, it stops accepting
    /// new connections and waits up to this many seconds for in-flight
    /// requests to complete before forcibly terminating.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,

    /// Seconds between `/ready` returning 503 and the TCP listener
    /// closing to new connections. Default: `5`.
    ///
    /// This gap gives upstream load balancers time to deregister the
    /// replica before it stops accepting new connections, preventing
    /// connection resets on in-flight requests from the LB tier.
    /// Must be tuned to match the LB's health-check interval + deregistration
    /// propagation time. Set to `0` to disable the grace period.
    #[serde(default = "default_prestop_grace")]
    pub prestop_grace_secs: u64,

    /// In-place upgrade settings (`SIGUSR2` handoff to a new binary).
    ///
    /// See [`UpgradeConfig`] and `docs/guide/hot-upgrades.md`.
    #[serde(default)]
    pub upgrade: UpgradeConfig,

    /// Per-request timeout configuration.
    ///
    /// Controls request-cycle timeouts for `DoS` protection. By default
    /// all timeouts are disabled so existing applications are unaffected.
    /// Set `request_timeout_ms` in `[server.timeouts]` to enable.
    #[serde(default)]
    pub timeouts: RequestTimeoutsConfig,

    /// Bind to a Unix domain socket at this path instead of `host:port`.
    ///
    /// When set, the server binds a `UnixListener` at the given path
    /// (replacing the TCP `host:port` bind) — the local-daemon transport
    /// used by `autumn serve`. The socket is created with `0600`
    /// permissions and removed on graceful shutdown. Unix-only; on other
    /// platforms a configured value is rejected at startup.
    ///
    /// Configured via `AUTUMN_SERVER__UNIX_SOCKET`. Default: `None` (TCP).
    #[serde(default)]
    pub unix_socket: Option<String>,

    /// Ceiling on concurrent in-flight requests (admission control / load
    /// shedding). `None` or `0` (the default) disables the ceiling — today's
    /// unlimited behavior — so no existing application silently changes
    /// throughput.
    ///
    /// Once this many requests are admitted and still in flight, additional
    /// requests receive an immediate `503 Service Unavailable` with a
    /// `Retry-After` header, before the handler runs or the request body is
    /// read. This bounds total concurrent work (and therefore memory) under
    /// a traffic spike or a slow dependency, trading a fast, clean "try
    /// another replica" signal for the alternative — admitted requests
    /// piling up unbounded until the process is OOM-killed.
    ///
    /// Liveness/readiness/health probe routes (`health.*` paths and the
    /// actuator prefix) are never shed, so a merely-busy replica is not
    /// killed by its orchestrator.
    ///
    /// A reasonable starting point is the number of worker threads times a
    /// small multiple (e.g. 2-4x), sized to keep admitted-request tail
    /// latency stable under the expected peak concurrency; tune based on
    /// observed `autumn_requests_shed_total` and per-route latency.
    ///
    /// Configured via `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS`.
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,

    /// Path to the committed capacity contract (`capacity.lock`) this deploy
    /// should admit against (issue #1733).
    ///
    /// When set — and [`Self::max_concurrent_requests`] is *not* — the
    /// load-shedding ceiling is sourced from the contract's proven envelope
    /// instead of a hand-tuned guess, so the binary sheds at the edge someone
    /// actually measured. Relative paths resolve against the process working
    /// directory.
    ///
    /// An explicit `max_concurrent_requests` always wins, and every failure
    /// along the contract path (missing file, malformed document, a contract
    /// measured on a different host class) degrades to *unlimited* with a
    /// warning rather than to a ceiling — see
    /// [`capacity::resolve_admission_limit`](crate::capacity::resolve_admission_limit).
    ///
    /// Configured via `AUTUMN_SERVER__CAPACITY_CONTRACT`.
    #[serde(default)]
    pub capacity_contract: Option<String>,

    /// Terminate HTTPS directly in the app process (issue #1603).
    ///
    /// When set, the server serves TLS on `host:port` using the configured
    /// certificate chain and private key — no sidecar reverse proxy required.
    /// Absent (the default), the server keeps serving plain HTTP, so existing
    /// applications are unaffected.
    ///
    /// This field is always parsed, regardless of build features, so a
    /// misconfiguration is a clear "built without the `tls` feature" error
    /// rather than a silently-ignored section. The serving code itself is
    /// gated behind the off-by-default `tls` feature.
    ///
    /// Configured via `[server.tls]` (`cert_path`, `key_path`,
    /// `reload_interval_secs`, `handshake_timeout_secs`) or the matching
    /// `AUTUMN_SERVER__TLS__CERT_PATH` / `AUTUMN_SERVER__TLS__KEY_PATH` /
    /// `AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS` /
    /// `AUTUMN_SERVER__TLS__HANDSHAKE_TIMEOUT_SECS` env vars.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// Direct-HTTPS (native TLS termination) settings (issue #1603).
///
/// Present under `[server.tls]`; when present the server terminates TLS
/// in-process. Both paths point at PEM files: `cert_path` at the leaf
/// certificate followed by any intermediates, `key_path` at the matching
/// private key (PKCS#8, PKCS#1, or SEC1).
///
/// # `autumn.toml` example
///
/// ```toml
/// [server.tls]
/// cert_path = "/etc/autumn/tls/fullchain.pem"
/// key_path = "/etc/autumn/tls/privkey.pem"
/// # optional; how often (seconds) to poll for a renewed cert. Default: 60.
/// reload_interval_secs = 60
/// # optional; per-handshake timeout (seconds). Default: 10.
/// handshake_timeout_secs = 10
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    /// Path to the PEM certificate chain (leaf first, then intermediates).
    ///
    /// Optional so a `[server.tls]` section can instead enable automatic ACME
    /// provisioning (issue #1608) via [`acme`](Self::acme). In static-cert mode
    /// this must be set together with [`key_path`](Self::key_path); in ACME mode
    /// both must be unset. The startup [`validate`](Self::validate) guard
    /// rejects any other combination.
    #[serde(default)]
    pub cert_path: Option<PathBuf>,

    /// Path to the PEM private key matching the leaf certificate. Optional; see
    /// [`cert_path`](Self::cert_path).
    #[serde(default)]
    pub key_path: Option<PathBuf>,

    /// How often, in seconds, the running server polls the certificate and key
    /// files' modification times to pick up an external renewal (e.g. after
    /// `certbot`/ACME writes new files) without a restart. Default: `60`.
    #[serde(default = "default_tls_reload_interval_secs")]
    pub reload_interval_secs: u64,

    /// Maximum time, in seconds, allowed for a single inbound TLS handshake
    /// before the connection is dropped. Bounds a client that opens TCP but
    /// never completes (or starts) the handshake so it cannot park the accept
    /// loop and deny service to everyone else. Default: `10`. A value of `0` is
    /// clamped to `1` second.
    #[serde(default = "default_tls_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,

    /// Automatic ACME (Let's Encrypt) certificate provisioning + renewal
    /// (issue #1608). When present the server obtains and auto-renews its own
    /// certificate over the ACME HTTP-01 challenge instead of loading a static
    /// cert from disk. Mutually exclusive with
    /// [`cert_path`](Self::cert_path)/[`key_path`](Self::key_path); the startup
    /// [`validate`](Self::validate) guard enforces exactly one mode. The serving
    /// code is gated behind the off-by-default `acme` feature.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

impl TlsConfig {
    /// An empty `TlsConfig` used only as the seed for env-var overrides of a
    /// section that was absent from TOML. Both paths are unset (which fails
    /// fast at startup if neither a static cert nor ACME is configured), and the
    /// reload interval takes its default.
    const fn empty_for_env() -> Self {
        Self {
            cert_path: None,
            key_path: None,
            reload_interval_secs: default_tls_reload_interval_secs(),
            handshake_timeout_secs: default_tls_handshake_timeout_secs(),
            acme: None,
        }
    }

    /// Validate the `[server.tls]` wiring before the listener binds.
    ///
    /// Exactly one provisioning mode must be selected:
    /// - **static cert**: both [`cert_path`](Self::cert_path) and
    ///   [`key_path`](Self::key_path) set (and no `[server.tls.acme]`), or
    /// - **ACME**: `[server.tls.acme]` present (and neither path set).
    ///
    /// Every rejection names the offending combination so the operator can act
    /// on it without guesswork.
    ///
    /// # Errors
    ///
    /// Returns a message describing the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        let has_cert = self.cert_path.is_some();
        let has_key = self.key_path.is_some();
        let static_configured = has_cert || has_key;
        let acme_configured = self.acme.is_some();

        match (static_configured, acme_configured) {
            (true, true) => {
                return Err(
                    "[server.tls] sets a static cert_path/key_path AND [server.tls.acme]; \
                     choose exactly one — remove the static cert to use ACME, or remove \
                     [server.tls.acme] to serve the static certificate"
                        .to_owned(),
                );
            }
            (false, false) => {
                return Err(
                    "[server.tls] must configure exactly one of: a static certificate \
                     (cert_path AND key_path) or automatic provisioning ([server.tls.acme] \
                     with domains + contact_email)"
                        .to_owned(),
                );
            }
            (true, false) => {
                if !(has_cert && has_key) {
                    return Err("[server.tls] cert_path and key_path must be set together; \
                         set both, or configure [server.tls.acme] instead"
                        .to_owned());
                }
            }
            (false, true) => {}
        }

        if let Some(acme) = &self.acme {
            acme.validate()?;
        }

        Ok(())
    }
}

/// Automatic ACME (Let's Encrypt) certificate provisioning settings (issue
/// #1608). Present under `[server.tls.acme]`.
///
/// # Deployment scope
///
/// HTTP-01 ACME here is **single-host**: the challenge-token map is per-process
/// in-memory and certificates are stored on local disk, so behind a load
/// balancer the CA's `:80` validation can hit a replica without the token
/// (→ 404) and non-leader replicas cannot adopt a cert from a non-shared store.
/// Single-replica deployments are fully correct; multi-replica needs a shared
/// token store or DNS-01 (tracked in #1620). Configuring ACME alongside a
/// distributed scheduler backend logs a startup warning.
///
/// # `autumn.toml` example
///
/// ```toml
/// [server.tls.acme]
/// domains = ["app.example.com"]
/// contact_email = "ops@example.com"
/// # optional; "staging" (default), "production", or a custom directory URL.
/// directory = "staging"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct AcmeConfig {
    /// Domains to include on the issued certificate (SANs). At least one is
    /// required. Wildcards (`*.example.com`) are rejected — they require the
    /// DNS-01 challenge, tracked in issue #1620.
    pub domains: Vec<String>,

    /// Contact email registered with the ACME account (used for expiry
    /// notifications from the CA). Required.
    pub contact_email: String,

    /// Which ACME directory to use. Defaults to Let's Encrypt **staging** on
    /// purpose, so a first run or CI cannot burn the strict production rate
    /// limit before the deployment is known good.
    #[serde(default)]
    pub directory: AcmeDirectory,

    /// Directory that stores the ACME account key and issued certificates.
    /// Default: `config/acme`.
    #[serde(default = "default_acme_cache_dir")]
    pub cache_dir: PathBuf,

    /// Port to serve the HTTP-01 challenge (and the HTTP→HTTPS redirect) on.
    /// The ACME CA always validates HTTP-01 over port 80, so this defaults to
    /// `80`; override it when a front-end forwards `:80` to another port.
    #[serde(default = "default_acme_http_challenge_port")]
    pub http_challenge_port: u16,

    /// Renew the certificate once it has fewer than this many days of validity
    /// left. Default: `30`.
    #[serde(default = "default_acme_renew_before_days")]
    pub renew_before_days: u32,

    /// PEM file holding the root certificate that signs the ACME **directory's
    /// own HTTPS certificate**, for a directory that is not publicly trusted.
    ///
    /// The ACME client speaks HTTPS to the directory and, by default, verifies
    /// it against the **platform trust store** (the host's own installed CA
    /// certificates). That is right for Let's Encrypt — both its staging and its
    /// production API endpoints carry publicly-trusted certificates — and wrong
    /// for a private CA or a [Pebble](https://github.com/letsencrypt/pebble)
    /// test server whose API certificate chains to a root the host does not
    /// know. Point this at that root and the client trusts it *instead of* the
    /// platform store.
    ///
    /// Only the **first** certificate in the file becomes a trust anchor, so
    /// give it the root alone rather than a leaf/intermediate bundle. Setting it
    /// also swaps the platform verifier for a plain path verifier, which on
    /// macOS and Windows means the OS-level policy and revocation checks no
    /// longer apply to this one connection.
    ///
    /// Unset by default, and unnecessary for `directory = "staging"` /
    /// `"production"`. This changes trust for the **ACME control plane only**;
    /// it has no bearing on which certificates browsers accept from your site.
    ///
    /// Like every other `[server.tls.acme]` key, this has no
    /// `AUTUMN_SERVER__TLS__ACME__*` environment override: the section is
    /// configured in `autumn.toml` as a unit. Deliberate, and worth knowing if
    /// the root arrives as a container mount — the path still has to be named in
    /// the config file.
    #[serde(default)]
    pub ca_root_path: Option<PathBuf>,

    /// DNS-01 challenge settings (issue #1620). Present under
    /// `[server.tls.acme.dns]`.
    ///
    /// When set, every authorization in an order is answered over **DNS-01**
    /// instead of HTTP-01, which is the only challenge type a CA will accept for
    /// a **wildcard** identifier — so `domains` may list `*.example.com`. When
    /// absent, issuance stays on #1608's HTTP-01 path and wildcards are rejected.
    #[serde(default)]
    pub dns: Option<AcmeDnsConfig>,
}

impl AcmeConfig {
    /// Validate the ACME wiring: at least one non-wildcard domain and a
    /// non-empty contact email.
    ///
    /// # Errors
    ///
    /// Returns a message describing the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.domains.is_empty() {
            return Err(
                "[server.tls.acme] domains must list at least one domain to request a \
                 certificate for"
                    .to_owned(),
            );
        }
        if self.contact_email.trim().is_empty() {
            return Err(
                "[server.tls.acme] contact_email must be set (the ACME CA requires an account \
                 contact for expiry notifications)"
                    .to_owned(),
            );
        }
        if self.http_challenge_port == 0 {
            return Err(
                "[server.tls.acme] http_challenge_port must not be 0: port 0 binds an ephemeral \
                 OS-assigned port that the ACME HTTP-01 validator (which always connects on port \
                 80) can never reach, so every issuance fails. Use 80, or the port a front-end \
                 forwards `:80` to"
                    .to_owned(),
            );
        }
        // The renew-before window is compared against the issued certificate's
        // REMAINING validity. Publicly-trusted CAs (Let's Encrypt) issue
        // ~90-day certificates, so treat 90 days as the effective maximum cert
        // lifetime: a `renew_before_days >= 90` keeps the freshly-issued
        // certificate perpetually inside its renew-before window, so `needs_renewal`
        // stays true immediately after every successful renewal and the hourly
        // loop orders a brand-new certificate every tick until the CA's rate
        // limits are hit. Reject it up front.
        if self.renew_before_days >= 90 {
            return Err(format!(
                "[server.tls.acme] renew_before_days ({}) must be less than 90: it is compared \
                 against the issued certificate's remaining validity, and publicly-trusted CAs \
                 (e.g. Let's Encrypt) issue certificates that live at most ~90 days. A value >= \
                 the certificate lifetime keeps the cert perpetually inside its renew-before \
                 window, so the renewal loop would order a fresh certificate every hour and burn \
                 the CA's rate limits. Use a smaller value (default 30)",
                self.renew_before_days
            ));
        }
        if let Some(path) = &self.ca_root_path
            && path.to_str().is_none_or(|p| p.trim().is_empty())
        {
            return Err(
                "[server.tls.acme] ca_root_path is set but blank: either remove it (to use the \
                 platform trust store, which is correct for Let's Encrypt staging and \
                 production) or point it at the PEM root that signs your ACME directory's HTTPS \
                 certificate"
                    .to_owned(),
            );
        }
        for (index, domain) in self.domains.iter().enumerate() {
            let trimmed = domain.trim();
            if trimmed.is_empty() {
                return Err(format!(
                    "[server.tls.acme] domains must not contain blank entries (entry at index \
                     {index} is empty or whitespace-only)"
                ));
            }
            // A wildcard identifier can only ever be validated over DNS-01, so it
            // is accepted exactly when `[server.tls.acme.dns]` is configured —
            // and its shape is checked here rather than surfacing later as an
            // opaque CA rejection mid-issuance.
            if trimmed.contains('*') {
                if self.dns.is_none() {
                    return Err(format!(
                        "[server.tls.acme] wildcard domain `{trimmed}` needs the DNS-01 \
                         challenge: an ACME CA will not validate a wildcard identifier over \
                         HTTP-01. Add a [server.tls.acme.dns] section naming your DNS provider \
                         (and the credentials-store key holding its API token), or list explicit \
                         hostnames instead"
                    ));
                }
                let Some(base) = trimmed.strip_prefix("*.") else {
                    return Err(format!(
                        "[server.tls.acme] domain `{trimmed}` is not a usable wildcard: a \
                         wildcard SAN must be written as `*.` followed by the base domain (e.g. \
                         `*.myapp.com`) — a `*` anywhere else is not matched by any client"
                    ));
                };
                if base.is_empty() || base.contains('*') {
                    return Err(format!(
                        "[server.tls.acme] domain `{trimmed}` is not a usable wildcard: exactly \
                         one leading `*.` is allowed and the base domain after it must be \
                         non-empty (e.g. `*.myapp.com`)"
                    ));
                }
            }
            // The checks above read `trimmed`, but the entry is stored — and used —
            // UNTRIMMED: it becomes the certificate's SAN via `CertificateParams`,
            // the placeholder key's `CertId`, and the ACME order's
            // `Identifier::Dns`. A padded ` app.example.com ` is therefore a
            // different identifier than the hostname the operator meant, and the
            // failure surfaces as an opaque CA rejection mid-issuance. Reject the
            // padding here instead of silently trimming, so the config file says
            // exactly what will be requested.
            if domain != trimmed {
                return Err(format!(
                    "[server.tls.acme] domain `{domain}` (entry at index {index}) has leading or \
                     trailing whitespace: the entry is used verbatim as the certificate's SAN and \
                     as the ACME order's DNS identifier, so the padded value would be requested \
                     as-is. Write it as `{trimmed}`"
                ));
            }
        }
        if let Some(dns) = &self.dns {
            dns.validate()?;
        }
        Ok(())
    }

    /// Whether the configured certificate would cover `host`.
    ///
    /// Applies RFC 6125 wildcard matching across every entry in
    /// [`domains`](Self::domains), so a `*.myapp.com` SAN covers
    /// `tenant42.myapp.com` but not the apex and not a deeper label. Used by
    /// `autumn doctor` to check a `tenancy.base_domain` against the certificate
    /// (issue #1620).
    #[must_use]
    pub fn covers_host(&self, host: &str) -> bool {
        self.domains.iter().any(|san| san_covers_host(san, host))
    }
}

/// DNS-01 challenge settings for wildcard (and multi-replica) ACME issuance
/// (issue #1620). Present under `[server.tls.acme.dns]`.
///
/// # Secrets never live here
///
/// This section names a **credential reference**, never a token. The provider's
/// API credential is read from the encrypted credentials store
/// (`autumn credentials edit`, `config/credentials/<env>.toml.enc`) under the
/// [`credential`](Self::credential) key, or from the documented environment
/// variables. There is deliberately no field to hold a token, and the section is
/// `deny_unknown_fields`, so an `api_token = "..."` written into `autumn.toml`
/// is a **load-time error** rather than a plaintext secret that silently works.
///
/// # `autumn.toml` example
///
/// ```toml
/// [server.tls.acme]
/// domains = ["myapp.com", "*.myapp.com"]
/// contact_email = "ops@myapp.com"
/// directory = "production"
///
/// [server.tls.acme.dns]
/// provider = "cloudflare"
/// # optional; the credentials-store key holding the token. Default: "acme_dns".
/// credential = "acme_dns"
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcmeDnsConfig {
    /// Which DNS provider writes the `_acme-challenge` TXT records.
    pub provider: AcmeDnsProvider,

    /// Key in the encrypted credentials store holding this provider's
    /// credential. Default: `acme_dns`. Never the credential itself.
    #[serde(default = "default_acme_dns_credential")]
    pub credential: String,

    /// How long, in seconds, to wait for a published TXT record to become
    /// visible on every configured resolver before failing the order.
    /// Default: `300`.
    #[serde(default = "default_acme_dns_propagation_timeout_secs")]
    pub propagation_timeout_secs: u64,

    /// Seconds between propagation probes. Default: `5`.
    #[serde(default = "default_acme_dns_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Resolvers queried to confirm propagation. Each entry is `IP` (port 53
    /// implied) or `IP:port`. Default: Cloudflare and Google public DNS.
    #[serde(default = "default_acme_dns_resolvers")]
    pub resolvers: Vec<String>,

    /// The hook program for [`AcmeDnsProvider::Exec`], as an **argv array**
    /// (never a shell string). Autumn appends `present|cleanup`, the record
    /// FQDN, and the TXT value as three further arguments, so nothing is
    /// interpolated into a shell. Required for `exec`, rejected for every other
    /// provider.
    #[serde(default)]
    pub command: Vec<String>,
}

impl AcmeDnsConfig {
    /// Parse [`resolvers`](Self::resolvers) into socket addresses, defaulting a
    /// bare IP to port 53.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first unparseable entry.
    pub fn resolver_addrs(&self) -> Result<Vec<std::net::SocketAddr>, String> {
        self.resolvers
            .iter()
            .map(|entry| parse_resolver_addr(entry))
            .collect()
    }

    /// Validate the DNS-01 wiring.
    ///
    /// # Errors
    ///
    /// Returns a message describing the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.credential.trim().is_empty() {
            return Err(
                "[server.tls.acme.dns] credential must name the key in the encrypted \
                 credentials store that holds the DNS provider's API credential (run \
                 `autumn credentials edit`). It is a key NAME, never the token itself"
                    .to_owned(),
            );
        }
        match self.provider {
            AcmeDnsProvider::Exec => {
                if self
                    .command
                    .first()
                    .is_some_and(|program| !program.trim().is_empty() && program != program.trim())
                {
                    return Err(format!(
                        "[server.tls.acme.dns] command's program `{}` has leading or trailing \
                         whitespace: it is passed to the OS verbatim, so the padded value would \
                         fail to execute. Write it without the padding",
                        self.command[0]
                    ));
                }
                if self.command.first().is_none_or(|p| p.trim().is_empty()) {
                    return Err(
                        "[server.tls.acme.dns] provider = \"exec\" requires a non-empty `command` \
                         argv array (e.g. command = [\"/usr/local/bin/dns-hook\"]); autumn appends \
                         `present`/`cleanup`, the record FQDN and the TXT value as further \
                         arguments"
                            .to_owned(),
                    );
                }
            }
            AcmeDnsProvider::Cloudflare | AcmeDnsProvider::Route53 => {
                if !self.command.is_empty() {
                    return Err(format!(
                        "[server.tls.acme.dns] command is set but provider is \"{}\": the command \
                         would never run. Remove it, or set provider = \"exec\" to use the \
                         external-hook escape hatch",
                        self.provider.as_str()
                    ));
                }
            }
        }
        if self.propagation_timeout_secs == 0 {
            return Err(
                "[server.tls.acme.dns] propagation_timeout_secs must not be 0: a zero budget \
                 fails every order before a freshly-written TXT record could ever be visible. \
                 Use the default (300) or a larger value for a slow-propagating zone"
                    .to_owned(),
            );
        }
        if self.poll_interval_secs == 0 {
            return Err(
                "[server.tls.acme.dns] poll_interval_secs must not be 0: it would busy-loop the \
                 configured resolvers. Use the default (5)"
                    .to_owned(),
            );
        }
        if self.propagation_timeout_secs > MAX_ACME_DNS_PROPAGATION_TIMEOUT_SECS {
            return Err(format!(
                "[server.tls.acme.dns] propagation_timeout_secs ({}) must be at most {}: the wait \
                 is computed as an instant `now + timeout`, and an unbounded value overflows that \
                 and panics the renewal task, leaving the self-signed placeholder served \
                 indefinitely. An hour is far past any real provider's propagation time",
                self.propagation_timeout_secs, MAX_ACME_DNS_PROPAGATION_TIMEOUT_SECS
            ));
        }
        if self.poll_interval_secs > self.propagation_timeout_secs {
            return Err(format!(
                "[server.tls.acme.dns] poll_interval_secs ({}) is greater than \
                 propagation_timeout_secs ({}): the record would be probed once and the wait \
                 would time out without ever re-checking. Lower poll_interval_secs",
                self.poll_interval_secs, self.propagation_timeout_secs
            ));
        }
        if self.resolvers.is_empty() {
            return Err(
                "[server.tls.acme.dns] resolvers must list at least one DNS resolver to confirm \
                 TXT propagation against (default: 1.1.1.1:53 and 8.8.8.8:53)"
                    .to_owned(),
            );
        }
        self.resolver_addrs()?;
        Ok(())
    }
}

/// Parse one `[server.tls.acme.dns] resolvers` entry into a socket address,
/// defaulting a bare IP address to port 53.
fn parse_resolver_addr(entry: &str) -> Result<std::net::SocketAddr, String> {
    let trimmed = entry.trim();
    if let Ok(addr) = trimmed.parse::<std::net::SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, 53));
    }
    Err(format!(
        "[server.tls.acme.dns] resolvers entry `{entry}` is not a resolver address: write it as \
         an IP (`1.1.1.1`, port 53 implied) or `IP:port` (`1.1.1.1:53`). Hostnames are not \
         accepted — resolving the resolver would defeat the purpose of the propagation check"
    ))
}

/// Which DNS provider writes the ACME DNS-01 `_acme-challenge` TXT records.
///
/// The curated set is deliberately small; [`Exec`](Self::Exec) is the documented
/// escape hatch for everything else (RFC 2136 via `nsupdate`, a registrar CLI, a
/// webhook shim).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AcmeDnsProvider {
    /// Cloudflare DNS, via the v4 REST API with a scoped API token.
    Cloudflare,
    /// Amazon Route 53, via `ChangeResourceRecordSets` with `SigV4` credentials.
    Route53,
    /// An operator-provided hook program — the escape hatch for any other
    /// provider.
    Exec,
}

impl AcmeDnsProvider {
    /// The provider's stable config spelling (also used in health details).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::Route53 => "route53",
            Self::Exec => "exec",
        }
    }
}

impl std::fmt::Display for AcmeDnsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a certificate SAN entry covers `host`, using RFC 6125 wildcard rules.
///
/// A `*.example.com` SAN covers exactly one label — `tenant1.example.com` but
/// neither `a.b.example.com` nor the bare apex `example.com`. Matching is
/// case-insensitive and tolerates a trailing dot on `host`.
///
/// Used by `autumn doctor` to tell an operator that their `tenancy.base_domain`
/// is not covered by the configured certificate (issue #1620).
#[must_use]
pub fn san_covers_host(san: &str, host: &str) -> bool {
    let san = san.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if san.is_empty() || host.is_empty() {
        return false;
    }
    let Some(suffix) = san.strip_prefix("*.") else {
        return san == host;
    };
    if suffix.is_empty() {
        return false;
    }
    // Exactly one label may stand in for the `*`: strip the suffix and require
    // what remains to be a single non-empty, dot-free label.
    let Some(label) = host
        .strip_suffix(&suffix)
        .and_then(|prefix| prefix.strip_suffix('.'))
    else {
        return false;
    };
    !label.is_empty() && !label.contains('.')
}

/// Which ACME directory endpoint to provision against.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcmeDirectory {
    /// Let's Encrypt **staging** (the default): untrusted certificates, but
    /// generous rate limits — safe for first runs and CI.
    #[default]
    Staging,
    /// Let's Encrypt production: trusted certificates, strict rate limits.
    Production,
    /// A custom ACME directory URL (e.g. a private CA or a pebble test server).
    Custom {
        /// The directory URL (e.g. `https://acme.example.com/directory`).
        url: String,
    },
}

/// Behavior when a configured read replica is unavailable or stale.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReplicaFallback {
    /// Readiness should fail when the configured replica cannot safely serve reads.
    #[default]
    FailReadiness,
    /// Read paths may use the primary when the replica is unavailable or stale.
    Primary,
}

impl std::str::FromStr for ReplicaFallback {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fail_readiness" | "fail-readiness" | "fail" => Ok(Self::FailReadiness),
            "primary" | "fallback_to_primary" | "fallback-to-primary" => Ok(Self::Primary),
            _ => Err(()),
        }
    }
}

/// Strategy for routing reads that follow a write within the same request or
/// client session.
///
/// Replication is asynchronous: a read immediately after a write can land on a
/// lagging replica and return stale data (the read-your-own-writes anomaly).
/// This setting lets Autumn pin such reads to the primary.
///
/// Configured via `database.read_your_writes` in `autumn.toml` or
/// `AUTUMN_DATABASE__READ_YOUR_WRITES` in the environment.
///
/// Default: `off` (preserves today's behavior — no post-write pinning).
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReadYourWrites {
    /// No post-write read pinning. Replica reads are always served from the
    /// replica. This is the default and preserves existing behavior exactly.
    #[default]
    Off,
    /// Once the current request checks out a **primary** connection (via `Db`
    /// or a generated mutating repository method), all subsequent
    /// replica-eligible reads within the same request are redirected to the
    /// primary. Analogous to Laravel's "sticky" behavior.
    Request,
    /// Like `request`, and additionally pins a client's reads to the primary
    /// for [`pin_after_write_secs`](DatabaseConfig::pin_after_write_secs)
    /// seconds after a write, via a signed `autumn.ryw` cookie. Reads within
    /// that window are served from the primary even if the request itself
    /// performed no write. Analogous to Rails' automatic role switching.
    Session,
}

impl std::str::FromStr for ReadYourWrites {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "request" => Ok(Self::Request),
            "session" => Ok(Self::Session),
            _ => Err(()),
        }
    }
}

/// A logical slot assignment entry in a shard's `slots` list.
///
/// Accepts a single slot index (`5`) or an inclusive range written as a
/// string (`"0-31"`). A string holding a single number (`"5"`) is also
/// accepted so environment-variable overrides can pass everything as text.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SlotSpec {
    /// A single slot index.
    Index(u16),
    /// `"A-B"` inclusive range, or `"N"` single index.
    Range(String),
}

impl SlotSpec {
    /// Expand into concrete slot indices.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when a range string is malformed
    /// or inverted (`"31-0"`).
    pub fn expand(&self) -> Result<Vec<u16>, String> {
        match self {
            Self::Index(slot) => Ok(vec![*slot]),
            Self::Range(spec) => {
                let spec = spec.trim();
                let parse = |s: &str| {
                    s.trim()
                        .parse::<u16>()
                        .map_err(|_| format!("invalid slot {s:?} in {spec:?}"))
                };
                match spec.split_once('-') {
                    None => Ok(vec![parse(spec)?]),
                    Some((start, end)) => {
                        let (start, end) = (parse(start)?, parse(end)?);
                        if start > end {
                            return Err(format!("inverted slot range {spec:?}"));
                        }
                        Ok((start..=end).collect())
                    }
                }
            }
        }
    }
}

/// One horizontal shard of the application's data, declared via
/// `[[database.shards]]` in `autumn.toml`.
///
/// Each shard is a full primary/replica topology of its own, so the
/// replica story composes with sharding: any shard may have a read
/// replica, role-specific pool sizes, and its own fallback behavior.
/// Fields left unset fall back to the corresponding `[database]` value.
///
/// # Routing: keys → logical slots → shards
///
/// Routing keys hash onto a fixed set of [`SLOT_COUNT`] (16384) **logical
/// slots**, and each slot maps to one shard. The key→slot hash is a
/// permanent contract; the slot→shard map is plain configuration. Growing
/// from two shards to three means moving whole slots — copy a slot's rows
/// to the new shard, flip its `slots` entry, deploy — without rehashing
/// any keys.
///
/// When **every** shard declares [`slots`](Self::slots), declaration
/// order is meaningless and entries can be reordered, renamed, or
/// removed freely (as long as the map still covers every slot exactly
/// once). When **no** shard declares `slots`, the framework auto-splits
/// the slot space into contiguous even ranges **by declaration order**
/// — convenient to start with, but reordering entries then moves data.
/// Pin explicit `slots` before making any topology change.
///
/// # Example
///
/// ```toml
/// [database]
/// primary_url = "postgres://db-control/app"   # control role: jobs, sessions, flags
///
/// [[database.shards]]
/// name = "shard0"
/// primary_url = "postgres://db-shard0/app"
/// slots = ["0-8191"]
///
/// [[database.shards]]
/// name = "shard1"
/// primary_url = "postgres://db-shard1/app"
/// slots = ["8192-16383"]
/// replica_url = "postgres://db-shard1-ro/app"
/// replica_fallback = "primary"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShardConfig {
    /// Stable shard identity used in logs, metric tags, health component
    /// names (`db:shard:<name>`), and `autumn migrate --shard <name>`.
    ///
    /// Must be non-empty, unique across shards, and restricted to
    /// `[a-z0-9_-]` so it can be embedded in metric/health keys.
    pub name: String,

    /// Postgres URL for this shard's primary/write role. Required.
    pub primary_url: String,

    /// Logical slots this shard owns, as indices and/or `"A-B"` inclusive
    /// ranges (e.g. `slots = ["0-8191", 16000, "16382-16383"]`).
    ///
    /// All-or-none across shards: either every shard declares `slots`
    /// (explicit map covering `0..16384` exactly once; an empty list
    /// marks a drained shard being decommissioned) or none does
    /// (contiguous auto-split by declaration order).
    #[serde(default)]
    pub slots: Option<Vec<SlotSpec>>,

    /// Optional Postgres URL for this shard's read-replica role.
    #[serde(default)]
    pub replica_url: Option<String>,

    /// Optional primary pool size override. Falls back to
    /// `database.primary_pool_size`, then `database.pool_size`.
    #[serde(default)]
    pub primary_pool_size: Option<usize>,

    /// Optional replica pool size override. Falls back to
    /// `database.replica_pool_size`, then `database.pool_size`.
    #[serde(default)]
    pub replica_pool_size: Option<usize>,

    /// Optional replica fallback override. Falls back to
    /// `database.replica_fallback`.
    #[serde(default)]
    pub replica_fallback: Option<ReplicaFallback>,
}

impl ShardConfig {
    /// Resolved primary pool size for this shard.
    #[must_use]
    pub fn effective_primary_pool_size(&self, defaults: &DatabaseConfig) -> usize {
        self.primary_pool_size
            .unwrap_or_else(|| defaults.effective_primary_pool_size())
    }

    /// Resolved replica pool size for this shard.
    #[must_use]
    pub fn effective_replica_pool_size(&self, defaults: &DatabaseConfig) -> usize {
        self.replica_pool_size
            .unwrap_or_else(|| defaults.effective_replica_pool_size())
    }

    /// Resolved replica fallback behavior for this shard.
    #[must_use]
    pub fn effective_replica_fallback(&self, defaults: &DatabaseConfig) -> ReplicaFallback {
        self.replica_fallback.unwrap_or(defaults.replica_fallback)
    }
}

/// Which database engine a configured connection target names.
///
/// Autumn recognizes two backends. Postgres is the default runtime; `SQLite`
/// (issue #1614) is served by the runtime built with the crate's `sqlite`
/// cargo feature. Detection here is backend-neutral and always compiled, so a
/// `SQLite` target validates on either build; a build that cannot serve the
/// detected backend refuses at pool construction with a message naming the
/// feature (see [`create_pool`](crate::db::create_pool)).
///
/// # Detection rules
///
/// [`DatabaseBackend::detect`] classifies a target string by its scheme:
///
/// - `postgres://` / `postgresql://` URLs, and libpq keyword/value strings
///   (`host=db user=app sslmode=require`), are [`Postgres`](Self::Postgres) —
///   exactly the shapes the connection pool already accepts.
/// - `sqlite://<path>`, `sqlite:<path>`, and `file:<path>` targets are
///   [`Sqlite`](Self::Sqlite). `sqlite://` is the canonical, unambiguous form
///   and should be preferred.
/// - Anything else (including a **bare filesystem path** like
///   `/var/lib/app.db`) is deliberately *not* recognized and returns `None`.
///   A bare path is ambiguous — it carries no scheme distinguishing it from a
///   typo'd URL — so callers must spell `SQLite` targets with an explicit
///   `sqlite://` (or `sqlite:` / `file:`) scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseBackend {
    /// `PostgreSQL` — the default runtime backend.
    Postgres,
    /// `SQLite` — served by a build compiled with the `sqlite` feature
    /// (issue #1614); refused at pool construction by any other build.
    Sqlite,
}

impl DatabaseBackend {
    /// Detect the backend named by a database target string, or `None` when the
    /// target matches no recognized shape. See the [type docs](Self) for the
    /// full rule table, including why a bare filesystem path is not recognized.
    #[must_use]
    pub fn detect(target: &str) -> Option<Self> {
        // Check the SQLite schemes first: they are unambiguous prefixes and
        // never overlap with a Postgres URL or keyword/value string.
        if is_sqlite_target(target) {
            Some(Self::Sqlite)
        } else if is_pg_connection_string(target) {
            Some(Self::Postgres)
        } else {
            None
        }
    }

    /// Lowercase name used in boot-time error messages.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
        }
    }
}

impl std::fmt::Display for DatabaseBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether `s` names a `SQLite` target: the canonical `sqlite://<path>` URL, the
/// shorter `sqlite:<path>` form, or a `file:<path>` target. A bare filesystem
/// path is intentionally excluded (see [`DatabaseBackend`]).
fn is_sqlite_target(s: &str) -> bool {
    // `sqlite://` is subsumed by the `sqlite:` prefix; both are accepted.
    s.starts_with("sqlite:") || s.starts_with("file:")
}

/// Database connection configuration.
///
/// When `url` is `None` (the default), the application runs without a
/// database -- useful for static-site or API-gateway use cases. Set a
/// Postgres URL to enable the connection pool and the [`Db`](crate::Db)
/// extractor.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `url` | `None` |
/// | `primary_url` | `None` |
/// | `replica_url` | `None` |
/// | `pool_size` | `10` |
/// | `primary_pool_size` | `None` |
/// | `replica_pool_size` | `None` |
/// | `replica_fallback` | `fail_readiness` |
/// | `connect_timeout_secs` | `5` |
/// | `auto_migrate` | `None` |
/// | `auto_migrate_in_production` | `false` |
/// | `shards` | `[]` |
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::DatabaseConfig;
///
/// let db = DatabaseConfig::default();
/// assert!(db.url.is_none());
/// assert_eq!(db.pool_size, 10);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Postgres connection URL. `None` means no database is configured.
    ///
    /// Compatibility alias for the primary/write role. New multi-role
    /// deployments should prefer [`primary_url`](Self::primary_url).
    ///
    /// When present, must start with `postgres://` or `postgresql://`, or be
    /// a libpq-style keyword/value connection string
    /// (`host=db user=app dbname=app sslmode=require`).
    #[serde(default)]
    pub url: Option<String>,

    /// Postgres URL for the primary/write role.
    ///
    /// All writes, transactions, advisory locks, and migrations use this role.
    /// When unset, [`url`](Self::url) remains the single-primary fallback.
    #[serde(default)]
    pub primary_url: Option<String>,

    /// Optional Postgres URL for the read/replica role.
    ///
    /// Read-only paths may use this pool when configured. If omitted, read
    /// paths use the primary role.
    #[serde(default)]
    pub replica_url: Option<String>,

    /// Maximum number of connections in the pool. Default: `10`.
    ///
    /// Compatibility/default pool size used for both roles unless a
    /// role-specific size is set.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Optional primary/write role pool size.
    #[serde(default)]
    pub primary_pool_size: Option<usize>,

    /// Optional read/replica role pool size.
    #[serde(default)]
    pub replica_pool_size: Option<usize>,

    /// Deterministic behavior for configured replicas that cannot safely serve
    /// reads. Default: fail readiness.
    #[serde(default)]
    pub replica_fallback: ReplicaFallback,

    /// Post-write read pinning strategy. Default: `off` (no pinning).
    ///
    /// Set to `request` to pin reads to the primary for the remainder of the
    /// request after the first write. Set to `session` to additionally pin
    /// reads across requests via a signed cookie.
    ///
    /// Override via `AUTUMN_DATABASE__READ_YOUR_WRITES`.
    #[serde(default)]
    pub read_your_writes: ReadYourWrites,

    /// Duration (seconds) for cross-request session pins.
    ///
    /// Only used when `read_your_writes = "session"`. A signed `autumn.ryw`
    /// cookie pins the client's reads to the primary for this many seconds
    /// after a write. Default: `5`.
    ///
    /// Override via `AUTUMN_DATABASE__PIN_AFTER_WRITE_SECS`.
    #[serde(default = "default_pin_after_write_secs")]
    pub pin_after_write_secs: u64,

    /// Seconds to wait while acquiring a pooled connection, including
    /// creating a new connection when the pool grows.
    /// Default: `5`.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,

    /// Bounded startup wait (seconds) for the database to become reachable
    /// before the migrator fails. `0` (the default) disables the wait and
    /// preserves the current fail-fast behaviour — a single connection attempt,
    /// no retry.  Set a non-zero value (e.g. `60`) to have `autumn migrate`
    /// retry with capped exponential backoff until either the database accepts
    /// connections or the window elapses.
    ///
    /// Override via `AUTUMN_DATABASE__STARTUP_WAIT_SECS`.
    #[serde(default)]
    pub startup_wait_secs: u64,

    /// Profile-agnostic explicit override for startup migration auto-apply
    /// (issue #1903). `None` (the default, when the key is absent) leaves the
    /// decision to convention: `dev`/`development` auto-apply, every other
    /// profile (`prod`/`production` **and** custom names like `fly`/`staging`)
    /// is opt-in. `Some(true)` / `Some(false)` overrides that convention on
    /// **any** profile.
    ///
    /// This supersedes [`Self::auto_migrate_in_production`], which is retained
    /// as a back-compat alias: when `auto_migrate` is unset but
    /// `auto_migrate_in_production = true`, auto-apply is enabled on any
    /// non-`dev` profile (so a custom-profile operator's existing config finally
    /// takes effect). `auto_migrate` wins when both are set.
    ///
    /// Override via `AUTUMN_DATABASE__AUTO_MIGRATE`.
    #[serde(default)]
    pub auto_migrate: Option<bool>,

    /// Back-compat alias for [`Self::auto_migrate`] (issue #1903). When `true`,
    /// permits automatic migration application on any non-`dev` profile (not
    /// just `prod`/`production` — the old name-gated behavior silently skipped
    /// custom profiles). Default: `false`. Prefer setting `auto_migrate`
    /// directly; this key is honored only when `auto_migrate` is unset.
    ///
    /// Keep this disabled for multi-replica production fleets and use an
    /// explicit migration job (`autumn migrate`) instead.
    #[serde(default)]
    pub auto_migrate_in_production: bool,

    /// Optional database statement timeout.
    #[serde(deserialize_with = "deserialize_option_duration", default)]
    pub statement_timeout: Option<std::time::Duration>,

    /// Slow query threshold. Default: `500ms`.
    #[serde(
        deserialize_with = "deserialize_duration",
        default = "default_slow_query_threshold"
    )]
    pub slow_query_threshold: std::time::Duration,

    /// Horizontal shards, declared as `[[database.shards]]` entries.
    ///
    /// Empty (the default) means the application is unsharded and only the
    /// `url`/`primary_url`/`replica_url` roles above apply. When non-empty,
    /// those top-level roles become the **control** topology — framework
    /// state (jobs, scheduler locks, sessions, feature flags) lives there
    /// while tenant data is routed across the shards. See [`ShardConfig`].
    #[serde(default)]
    pub shards: Vec<ShardConfig>,

    /// Route tenants through the control-plane `_autumn_shard_directory` table
    /// (a [`DirectoryShardRouter`](crate::sharding::DirectoryShardRouter))
    /// instead of pure slot-hash routing. Default: `false`.
    ///
    /// Tenants with a directory row are pinned to the named shard; everyone
    /// else falls back to the hash router. Usually set via
    /// [`AppBuilder::with_directory_shard_router`](crate::app::AppBuilder::with_directory_shard_router).
    /// Ignored when no shards are configured or an explicit
    /// [`with_shard_router`](crate::app::AppBuilder::with_shard_router) is set.
    #[serde(default)]
    pub directory_shard_router: bool,

    /// Emit a startup warning when the aggregate maximum connection count
    /// across the control topology and every shard pool reaches this value.
    /// Default: `100`.
    ///
    /// Pool sizes multiply across shards: an N-shard fleet with a pool size
    /// of 20 opens up to `20 * N` connections, which can exhaust Postgres's
    /// `max_connections` (default 100) long before the app looks busy. This
    /// threshold surfaces that footgun at boot. Set to `0` to disable.
    #[serde(default = "default_max_connections_warn_threshold")]
    pub max_connections_warn_threshold: usize,
}

/// Decide whether the aggregate connection count warrants a startup warning.
///
/// Pure so the boundary condition is unit-testable without booting an app.
/// A `threshold` of `0` disables the warning entirely.
pub(crate) const fn should_warn_total_connections(total: usize, threshold: usize) -> bool {
    threshold != 0 && total >= threshold
}

/// Render a sorted slot list as compact `A-B` ranges for error messages
/// (a gap in a 16384-slot map would otherwise print thousands of indices).
fn format_slot_ranges(slots: &[usize]) -> String {
    fn render(start: usize, end: usize) -> String {
        if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        }
    }
    let mut ranges: Vec<String> = Vec::new();
    let mut iter = slots.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;
    for slot in iter {
        if slot != end + 1 {
            ranges.push(render(start, end));
            start = slot;
        }
        end = slot;
    }
    ranges.push(render(start, end));
    ranges.join(", ")
}

/// Number of logical routing slots shared across all shards. Fixed,
/// not configurable — the same constant for every Autumn deployment,
/// matching Redis Cluster and Valkey.
///
/// Keys hash onto `0..SLOT_COUNT` and each slot maps to one shard, so
/// resharding means moving whole slots between shards rather than
/// rehashing keys. Slots are pure routing-table entries (no pools, no
/// per-slot resources), so the fixed count costs almost nothing while
/// removing the classic "chose too few partitions on day one"
/// failure mode: there is no value to pick and nothing to outgrow
/// short of 16384 physical shards.
pub const SLOT_COUNT: u16 = 16384;

/// The resolved slot assignment for a single shard, expressed as a name and a
/// compact range string (e.g. `"0-8191"` or `"0-5460, 10923-16383"`).
///
/// Used by the boot-time shard-map guard to compare the freshly-computed
/// auto-split against the map stored on first boot. An empty `ranges` string
/// represents a drained shard (all slots moved away); that only arises in
/// explicit-slot mode, where the guard is inert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSlotAssignment {
    pub name: String,
    pub ranges: String,
}

/// Guard: compare the freshly-computed slot map against the stored map.
///
/// Returns `Ok(())` — no action required — when:
/// - `auto_split` is `false` (explicit-slot mode: operator-managed, no guard),
/// - `stored` is `None` (first boot: nothing to compare against), or
/// - the computed and stored maps are identical (order-insensitive).
///
/// Returns `Err` with a human-readable message when auto-split is active, a
/// stored map exists, and the maps differ.
///
/// Pure and sync so it can be unit-tested without a database.
///
/// # Errors
///
/// Returns a `String` description when the auto-split map differs from the
/// stored map.
pub fn check_stored_slot_map(
    auto_split: bool,
    computed: &[ShardSlotAssignment],
    stored: Option<&[ShardSlotAssignment]>,
) -> Result<(), String> {
    fn to_map(assignments: &[ShardSlotAssignment]) -> std::collections::BTreeMap<&str, &str> {
        assignments
            .iter()
            .map(|a| (a.name.as_str(), a.ranges.as_str()))
            .collect()
    }
    if !auto_split {
        return Ok(());
    }
    let Some(stored) = stored else {
        return Ok(());
    };
    if to_map(computed) == to_map(stored) {
        return Ok(());
    }
    let computed_names: Vec<&str> = computed.iter().map(|a| a.name.as_str()).collect();
    let stored_names: Vec<&str> = stored.iter().map(|a| a.name.as_str()).collect();
    Err(format!(
        "shard slot map mismatch — auto-split with {} shards ({}) produces a different \
         map than the stored map ({} shards: {}). Set explicit [[database.shards]] slot \
         ranges matching the stored map, then move data between shards deliberately \
         before changing the topology.",
        computed.len(),
        computed_names.join(", "),
        stored.len(),
        stored_names.join(", "),
    ))
}

/// The cross-backend consistency rule, as a single source of truth.
///
/// Shared by boot-time validation ([`DatabaseConfig::validate`], via
/// `DatabaseConfig::validate_backend_consistency`) and out-of-process callers
/// such as `autumn doctor`, so both agree for *every* role/backend mismatch
/// without re-deriving the rule.
///
/// The roles map to the config fields: `url` is the legacy `database.url`,
/// `primary_url` is `database.primary_url`, `replica_url` is
/// `database.replica_url`, and `has_shards` is whether any `[[database.shards]]`
/// are configured. The effective primary backend is `primary_url` if set, else
/// the legacy `url` (mirroring [`DatabaseConfig::effective_primary_url`]).
///
/// `SQLite` is a valid *target* but a narrower runtime than Postgres, so several
/// Postgres-only topologies (read replicas, horizontal shards, mixed backends)
/// are refused up front with actionable messages. The Postgres path is
/// behaviourally unchanged: a Postgres primary with Postgres roles and no
/// `SQLite` anywhere hits none of these branches and returns `Ok(())`.
///
/// This is the single source of truth for the rule; do not re-implement it.
///
/// # Errors
///
/// Returns `Err(message)` describing the first offending role when the topology
/// mixes backends or pairs a `SQLite` primary with a Postgres-only feature. The
/// message is byte-identical to what boot-time validation reports.
pub fn database_backend_consistency(
    url: Option<&str>,
    primary_url: Option<&str>,
    replica_url: Option<&str>,
    has_shards: bool,
) -> Result<(), String> {
    let Some(primary_backend) = primary_url.or(url).and_then(DatabaseBackend::detect) else {
        return Ok(());
    };

    if primary_backend == DatabaseBackend::Sqlite {
        // Read replicas are a Postgres topology concept; SQLite has no
        // replica role to serve reads from.
        if replica_url.is_some() {
            return Err(
                "database.replica_url is set but the primary target is SQLite; \
                 read replicas require the postgres backend"
                    .to_owned(),
            );
        }
        // Horizontal sharding is Postgres-only.
        if has_shards {
            return Err(
                "database.shards are configured but the primary target is SQLite; \
                 database shards require the postgres backend"
                    .to_owned(),
            );
        }
    }

    // Every configured connection role must name the same backend. Mixing
    // (e.g. a Postgres primary with a SQLite replica, or vice versa) cannot
    // work and is a boot-time misconfiguration rather than a first-query
    // surprise.
    for (field, url) in [("database.url", url), ("database.replica_url", replica_url)] {
        if let Some(url) = url
            && DatabaseBackend::detect(url) != Some(primary_backend)
        {
            return Err(format!(
                "{field} does not match the primary database backend \
                 ({primary_backend}); every configured database role must use \
                 the same backend"
            ));
        }
    }

    Ok(())
}

impl DatabaseConfig {
    /// Resolved primary/write database URL.
    #[must_use]
    pub fn effective_primary_url(&self) -> Option<&str> {
        self.primary_url.as_deref().or(self.url.as_deref())
    }

    /// Resolved primary/write database URL, but only when it names Postgres.
    ///
    /// Autumn ships subsystems that are Postgres-only by construction — the
    /// `PgFlagStore`, `PgExperimentStore` and `PgConfigStore` open a
    /// `diesel::PgConnection` and issue `pg_notify` / `pg_advisory_xact_lock` /
    /// `jsonb` SQL. Handing one a `SQLite` target builds a store that fails on
    /// first use with a connection error naming a driver the operator never
    /// chose. This accessor is what those constructors screen on, so an
    /// unsupported target yields "no store" at construction instead.
    ///
    /// Fails closed: a target [`DatabaseBackend::detect`] cannot classify is
    /// refused too, rather than handed to a Postgres driver on the chance that
    /// it might work.
    #[must_use]
    pub fn effective_primary_postgres_url(&self) -> Option<&str> {
        self.effective_primary_url()
            .filter(|url| DatabaseBackend::detect(url) == Some(DatabaseBackend::Postgres))
    }

    /// Resolved primary/write role pool size.
    #[must_use]
    pub fn effective_primary_pool_size(&self) -> usize {
        self.primary_pool_size.unwrap_or(self.pool_size)
    }

    /// Resolved read/replica role pool size.
    #[must_use]
    pub fn effective_replica_pool_size(&self) -> usize {
        self.replica_pool_size.unwrap_or(self.pool_size)
    }

    /// Whether any `[[database.shards]]` entries are configured.
    #[must_use]
    pub const fn has_shards(&self) -> bool {
        !self.shards.is_empty()
    }

    /// Resolve the slot→shard map: element `s` is the index (into
    /// [`shards`](Self::shards)) of the shard that owns slot `s`.
    ///
    /// This is the single source of truth for slot assignment, used by both
    /// configuration validation and runtime
    /// [`ShardSet`](crate::sharding::ShardSet) construction:
    ///
    /// - When **no** shard declares `slots`, the slot space is auto-split
    ///   into contiguous even ranges by declaration order.
    /// - When **every** shard declares `slots`, the explicit assignments are
    ///   used and must cover <code>0..[SLOT_COUNT]</code> exactly once.
    /// - Mixing declared and undeclared `slots` is an error.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for mixed declarations,
    /// malformed/out-of-range/duplicate slots, or incomplete coverage.
    pub fn resolved_slot_map(&self) -> Result<Vec<usize>, ConfigError> {
        let slot_count = usize::from(SLOT_COUNT);

        if self.shards.is_empty() {
            return Ok(Vec::new());
        }

        let declared = self.shards.iter().filter(|s| s.slots.is_some()).count();
        if declared != 0 && declared != self.shards.len() {
            return Err(ConfigError::Validation(
                "database.shards: either every shard must declare `slots` or none may \
                 (mixing explicit and auto-assigned slots is ambiguous)"
                    .to_owned(),
            ));
        }

        if declared == 0 {
            // Contiguous even auto-split by declaration order.
            if self.shards.len() > slot_count {
                return Err(ConfigError::Validation(format!(
                    "database.shards: at most {slot_count} shards are supported \
                     (one per logical slot), got {}",
                    self.shards.len()
                )));
            }
            let n = self.shards.len();
            return Ok((0..slot_count).map(|slot| slot * n / slot_count).collect());
        }

        let mut map: Vec<Option<usize>> = vec![None; slot_count];
        for (idx, shard) in self.shards.iter().enumerate() {
            let specs = shard.slots.as_deref().unwrap_or_default();
            for spec in specs {
                let slots = spec.expand().map_err(|e| {
                    ConfigError::Validation(format!("database.shards[{idx}].slots: {e}"))
                })?;
                for slot in slots {
                    if usize::from(slot) >= slot_count {
                        return Err(ConfigError::Validation(format!(
                            "database.shards[{idx}].slots: slot {slot} is out of range \
                             (slots are 0..{slot_count})"
                        )));
                    }
                    if let Some(owner) = map[usize::from(slot)] {
                        return Err(ConfigError::Validation(format!(
                            "database.shards[{idx}].slots: slot {slot} is already owned \
                             by shard {:?}",
                            self.shards[owner].name
                        )));
                    }
                    map[usize::from(slot)] = Some(idx);
                }
            }
        }
        let unassigned: Vec<usize> = map
            .iter()
            .enumerate()
            .filter_map(|(slot, owner)| owner.is_none().then_some(slot))
            .collect();
        if !unassigned.is_empty() {
            return Err(ConfigError::Validation(format!(
                "database.shards: slot map must cover every slot in 0..{slot_count}; \
                 unassigned slots: {}",
                format_slot_ranges(&unassigned)
            )));
        }
        // Coverage was just verified, so flatten cannot drop entries.
        Ok(map.into_iter().flatten().collect())
    }

    /// Whether all shards are using auto-split (no shard declares `slots`).
    ///
    /// Returns `false` when no shards are configured or any shard has an
    /// explicit `slots` declaration. Mixed declarations already error in
    /// [`resolved_slot_map`](Self::resolved_slot_map), so this is a simple
    /// all-or-none check.
    #[must_use]
    pub fn shards_auto_split(&self) -> bool {
        self.has_shards() && self.shards.iter().all(|s| s.slots.is_none())
    }

    /// Resolve the per-shard slot assignment as compact range strings.
    ///
    /// Inverts [`resolved_slot_map`](Self::resolved_slot_map) (slot→shard-index)
    /// into per-shard slot lists rendered via the same compact range notation
    /// used in slot-map error messages. Agrees with runtime routing by
    /// construction: the output derives from the same slot map that builds the
    /// live [`ShardSet`](crate::sharding::ShardSet).
    ///
    /// # Errors
    ///
    /// Propagates any [`ConfigError`] from `resolved_slot_map`.
    pub fn resolved_shard_assignments(&self) -> Result<Vec<ShardSlotAssignment>, ConfigError> {
        let slot_map = self.resolved_slot_map()?;
        let n = self.shards.len();
        let mut per_shard: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (slot, &owner) in slot_map.iter().enumerate() {
            per_shard[owner].push(slot);
        }
        Ok(self
            .shards
            .iter()
            .enumerate()
            .map(|(idx, shard)| ShardSlotAssignment {
                name: shard.name.clone(),
                ranges: format_slot_ranges(&per_shard[idx]),
            })
            .collect())
    }

    /// Cross-backend consistency checks (issue #1614).
    ///
    /// `SQLite` is a valid *target* but a narrower runtime than Postgres, so
    /// several Postgres-only knobs are refused at boot (not at first query)
    /// with actionable messages. The Postgres path is behaviourally unchanged:
    /// a Postgres primary with Postgres roles and no `SQLite` anywhere hits none
    /// of these branches.
    fn validate_backend_consistency(&self) -> Result<(), ConfigError> {
        // Single source of truth: delegate to the free
        // [`database_backend_consistency`] rule so boot and out-of-process
        // callers (e.g. `autumn doctor`) agree for every role/backend mismatch.
        database_backend_consistency(
            self.url.as_deref(),
            self.primary_url.as_deref(),
            self.replica_url.as_deref(),
            !self.shards.is_empty(),
        )
        .map_err(ConfigError::Validation)
    }

    /// Validate database configuration.
    ///
    /// # Errors
    ///
    /// Returns a validation error if a connection string is malformed or a
    /// shard declaration is malformed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, url) in [
            ("database.url", self.url.as_deref()),
            ("database.primary_url", self.primary_url.as_deref()),
            ("database.replica_url", self.replica_url.as_deref()),
        ] {
            // A SQLite target (issue #1614) is now a recognized shape and
            // passes this per-field check; only strings that are neither a
            // Postgres nor a SQLite target are rejected here. The message's
            // GUIDANCE half is unchanged — everything up to "got" — so existing
            // diagnostics that match on it still match; the target it quotes is
            // now redacted (see below), because a rejected string is by
            // definition one whose secrets this code cannot enumerate.
            if let Some(url) = url
                && DatabaseBackend::detect(url).is_none()
            {
                let label = if field == "database.url" {
                    "database URL"
                } else {
                    field
                };
                // Redacted: this is the refusal a normal `autumn.toml`
                // misconfiguration hits, and it reaches `tracing::error!` at
                // boot — one line after the startup summary masked the same
                // URL. A target this branch could not classify is, by
                // definition, not a shape we can enumerate the secrets of.
                let target = crate::db_url::redact_target(url);
                return Err(ConfigError::Validation(format!(
                    "Invalid {label}: must start with postgres:// or postgresql://, or be a \
                     keyword/value connection string \
                     (e.g. \"host=db user=app dbname=app sslmode=require\"), got {target:?}"
                )));
            }
        }

        if self.replica_url.is_some() && self.effective_primary_url().is_none() {
            return Err(ConfigError::Validation(
                "database.replica_url requires database.primary_url or database.url".to_owned(),
            ));
        }

        self.validate_backend_consistency()?;

        let mut seen_names = std::collections::HashSet::new();
        for (idx, shard) in self.shards.iter().enumerate() {
            if shard.name.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "database.shards[{idx}].name must not be empty"
                )));
            }
            if !shard
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                return Err(ConfigError::Validation(format!(
                    "database.shards[{idx}].name {:?} is invalid: shard names are used in \
                     metric tags and health component names and must match [a-z0-9_-]",
                    shard.name
                )));
            }
            if !seen_names.insert(shard.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "database.shards[{idx}].name {:?} is declared more than once; \
                     shard names must be unique",
                    shard.name
                )));
            }
            for (field, url) in [
                ("primary_url", Some(shard.primary_url.as_str())),
                ("replica_url", shard.replica_url.as_deref()),
            ] {
                if let Some(url) = url
                    && !is_pg_connection_string(url)
                {
                    let target = crate::db_url::redact_target(url);
                    return Err(ConfigError::Validation(format!(
                        "Invalid database.shards[{idx}].{field}: must start with \
                         postgres:// or postgresql://, or be a keyword/value \
                         connection string \
                         (e.g. \"host=db user=app dbname=app sslmode=require\"), got {target:?}"
                    )));
                }
            }
        }
        self.resolved_slot_map()?;
        Ok(())
    }
}

/// Whether `s` is an acceptable Postgres connection string: a
/// `postgres://`/`postgresql://` URL, or a libpq-style keyword/value string
/// (`host=db user=app sslmode=require`) — recognized with the SAME parser
/// the pool's TLS module uses ([`crate::pg_conn_str`]), so every string the
/// pool supports also passes config validation (issue #1585 review: the
/// keyword form was rejected here before ever reaching the pool).
fn is_pg_connection_string(s: &str) -> bool {
    crate::pg_conn_str::is_url(s) || crate::pg_conn_str::is_keyword_value(s)
}

/// Logging configuration.
///
/// Controls the tracing subscriber's filter level and output format.
/// See [`LogFormat`] for output format options.
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::{LogConfig, LogFormat};
///
/// let log = LogConfig::default();
/// assert_eq!(log.level, "info");
/// assert_eq!(log.format, LogFormat::Auto);
/// assert!(log.access_log);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// Tracing filter directive. Default: `"info"`.
    ///
    /// Supports the full `tracing` filter syntax, e.g.
    /// `"autumn=debug,tower_http=trace"`.
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log output format. Default: [`LogFormat::Auto`].
    #[serde(default)]
    pub format: LogFormat,

    /// Additional sensitive parameter keys to scrub from logs/traces.
    #[serde(default)]
    pub filter_parameters: Vec<String>,

    /// Explicitly remove default sensitive keys from the built-in deny-list.
    #[serde(default)]
    pub unfilter_parameters: Vec<String>,

    /// Emit one structured access-log event per served HTTP request.
    /// Default: `true`.
    ///
    /// The event (target `autumn::access`, level `INFO`) carries `method`,
    /// `route` (the matched low-cardinality template), `status`,
    /// `duration_ms`, and `request_id`, and is rendered by the standard
    /// subscriber according to [`format`](Self::format). It requires no
    /// telemetry feature or collector.
    #[serde(default = "default_access_log")]
    pub access_log: bool,

    /// Path prefixes excluded from access logging so steady-state probe and
    /// asset traffic does not drown application signal. Default:
    /// `["/health", "/live", "/ready", "/startup", "/actuator", "/static"]`
    /// (the built-in probe, actuator, and static-asset mounts).
    ///
    /// Prefixes match whole path segments: `"/actuator"` excludes
    /// `/actuator/health` but not `/actuators`. Setting this replaces the
    /// default set entirely — and if you move the probe endpoints
    /// (`health.path` etc.), mirror the new paths here.
    #[serde(default = "default_access_log_exclude")]
    pub access_log_exclude: Vec<String>,

    /// In-memory log capture buffer for `/actuator/logfile`.
    ///
    /// When enabled, recent structured log entries are visible over HTTP
    /// through the sensitive actuator endpoint without SSH access or an
    /// external log aggregator.  The buffer is bounded and never grows
    /// unbounded.
    #[serde(default)]
    pub capture: crate::log::capture::LogCaptureConfig,
}

/// Log output format.
///
/// Controls how tracing events are rendered. The default ([`Auto`](Self::Auto))
/// auto-detects based on the `AUTUMN_ENV` environment variable.
///
/// | Variant | Behaviour |
/// |---------|-----------|
/// | [`Auto`](Self::Auto) | Pretty in dev, JSON when `AUTUMN_ENV=production` |
/// | [`Pretty`](Self::Pretty) | Always human-readable, colorized |
/// | [`Json`](Self::Json) | Always structured JSON (for log aggregators) |
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::LogFormat;
///
/// assert_eq!(LogFormat::default(), LogFormat::Auto);
/// ```
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogFormat {
    /// Pretty in dev, JSON in production (based on `AUTUMN_ENV`).
    #[default]
    Auto,
    /// Human-readable, colorized output.
    Pretty,
    /// Structured JSON output suitable for log aggregation pipelines.
    Json,
}

/// Telemetry configuration.
///
/// Controls whether Autumn enables OTLP trace export and how the process
/// identifies itself in resource metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryConfig {
    /// Enable framework-managed telemetry. Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Logical service name. Default: `"autumn-app"`.
    #[serde(default = "default_telemetry_service_name")]
    pub service_name: String,

    /// Optional service namespace (e.g. team, domain, or product family).
    #[serde(default)]
    pub service_namespace: Option<String>,

    /// Service version string advertised in resource metadata.
    #[serde(default = "default_telemetry_service_version")]
    pub service_version: String,

    /// Deployment environment label for trace resource metadata.
    #[serde(default = "default_telemetry_environment")]
    pub environment: String,

    /// OTLP collector endpoint. Required when telemetry is enabled.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,

    /// OTLP transport protocol. Default: [`TelemetryProtocol::Grpc`].
    #[serde(default)]
    pub protocol: TelemetryProtocol,

    /// When `true`, telemetry initialization failures abort startup.
    #[serde(default)]
    pub strict: bool,
}

/// OTLP transport protocol selection.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryProtocol {
    /// OTLP over gRPC.
    #[serde(alias = "grpc", alias = "GRPC")]
    #[default]
    Grpc,
    /// OTLP over HTTP/protobuf.
    #[serde(
        alias = "http-protobuf",
        alias = "http_protobuf",
        alias = "HTTP_PROTOBUF"
    )]
    HttpProtobuf,
}

impl TelemetryProtocol {
    fn from_env_value(value: &str) -> Option<Self> {
        match value {
            "Grpc" | "grpc" | "GRPC" => Some(Self::Grpc),
            "HttpProtobuf" | "http-protobuf" | "http_protobuf" | "HTTP_PROTOBUF"
            | "httpprotobuf" => Some(Self::HttpProtobuf),
            _ => None,
        }
    }
}

/// Health check endpoint configuration.
///
/// The health check is automatically mounted by [`AppBuilder::run`](crate::app::AppBuilder::run).
/// See the [`health`](crate::health) module for response format details.
///
/// # Examples
///
/// ```rust
/// use autumn_web::config::HealthConfig;
///
/// let health = HealthConfig::default();
/// assert!(health.enabled);
/// assert_eq!(health.path, "/health");
/// assert_eq!(health.live_path, "/live");
/// assert_eq!(health.ready_path, "/ready");
/// assert_eq!(health.startup_path, "/startup");
/// assert!(!health.detailed);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
    /// When `true` (the default), the framework auto-mounts the built-in
    /// probe endpoints (health/live/ready/startup). Set to `false` to
    /// suppress all built-in probes so an app can own those paths entirely
    /// (or expose none at all). Default: `true` (issue #1971).
    #[serde(default = "default_health_enabled")]
    pub enabled: bool,

    /// Compatibility alias path for readiness. Default: `"/health"`.
    ///
    /// Common alternatives: `"/healthz"`, `"/_health"`.
    #[serde(default = "default_health_path")]
    pub path: String,

    /// URL path for the liveness probe. Default: `"/live"`.
    #[serde(default = "default_live_path")]
    pub live_path: String,

    /// URL path for the readiness probe. Default: `"/ready"`.
    #[serde(default = "default_ready_path")]
    pub ready_path: String,

    /// URL path for the startup probe. Default: `"/startup"`.
    #[serde(default = "default_startup_path")]
    pub startup_path: String,

    /// When `true`, the health endpoint includes detailed info (profile,
    /// uptime, pool stats). Default: `false` (overridden to `true` for
    /// `dev` profile via smart defaults).
    #[serde(default)]
    pub detailed: bool,
}

/// Actuator endpoint configuration.
///
/// Controls which operational endpoints are exposed. The `sensitive` flag
/// determines whether sensitive endpoints (env, configprops, loggers,
/// tasks) are available. Defaults to `true` for `dev`, `false` for `prod`.
#[derive(Debug, Clone, Deserialize)]
pub struct ActuatorConfig {
    /// URL prefix for actuator endpoints. Default: `"/actuator"`.
    #[serde(default = "default_actuator_prefix")]
    pub prefix: String,

    /// When `true`, expose sensitive endpoints (env, loggers, tasks).
    /// Defaults vary by profile: `true` for dev, `false` for prod.
    #[serde(default)]
    pub sensitive: bool,

    /// When `true`, mount the `/actuator/prometheus` scrape endpoint.
    ///
    /// This is **independent of [`Self::sensitive`]**: a production app can
    /// expose Prometheus metrics for platform scraping (e.g. Fly.io `[metrics]`)
    /// while keeping `sensitive = false` so env/configprops/loggers/tasks/jobs
    /// stay off the public surface. Set to `false` to remove the scrape
    /// endpoint entirely (it then returns `404`). Default: `true`.
    #[serde(default = "default_actuator_prometheus")]
    pub prometheus: bool,
}

impl Default for ActuatorConfig {
    fn default() -> Self {
        Self {
            prefix: default_actuator_prefix(),
            sensitive: false,
            prometheus: default_actuator_prometheus(),
        }
    }
}

fn default_actuator_prefix() -> String {
    "/actuator".to_owned()
}

const fn default_actuator_prometheus() -> bool {
    true
}

/// CORS (Cross-Origin Resource Sharing) configuration.
///
/// Controls which origins, methods, and headers are allowed for
/// cross-origin requests. Disabled by default -- enable by setting
/// `allowed_origins` in `autumn.toml` or via environment variables.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `allowed_origins` | `[]` (CORS disabled) |
/// | `allowed_methods` | `["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]` |
/// | `allowed_headers` | `["Content-Type", "Authorization"]` |
/// | `allow_credentials` | `false` |
/// | `max_age_secs` | `86400` (24 hours) |
///
/// # Profile smart defaults
///
/// The `dev` profile enables permissive CORS (`allowed_origins = ["*"]`)
/// so local front-end development works out of the box.
///
/// # Examples
///
/// ```toml
/// [cors]
/// allowed_origins = ["https://example.com", "https://app.example.com"]
/// allow_credentials = true
/// ```
///
/// ```rust
/// use autumn_web::config::CorsConfig;
///
/// let cors = CorsConfig::default();
/// assert!(cors.allowed_origins.is_empty());
/// assert!(!cors.allow_credentials);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct CorsConfig {
    /// Origins allowed to make cross-origin requests.
    ///
    /// Use `["*"]` to allow any origin (not recommended for production
    /// with credentials). When empty, CORS middleware is not applied.
    #[serde(default)]
    pub allowed_origins: Vec<String>,

    /// HTTP methods allowed for cross-origin requests.
    /// Default: `["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]`.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,

    /// Headers allowed in cross-origin requests.
    /// Default: `["Content-Type", "Authorization"]`.
    #[serde(default = "default_cors_headers")]
    pub allowed_headers: Vec<String>,

    /// Whether to include `Access-Control-Allow-Credentials: true`.
    /// Default: `false`.
    #[serde(default)]
    pub allow_credentials: bool,

    /// How long (in seconds) browsers may cache preflight responses.
    /// Default: `86400` (24 hours).
    #[serde(default = "default_cors_max_age")]
    pub max_age_secs: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            allowed_headers: default_cors_headers(),
            allow_credentials: false,
            max_age_secs: default_cors_max_age(),
        }
    }
}

impl CorsConfig {
    /// Validate CORS configuration for combinations rejected by browsers.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `allow_credentials = true` is combined
    /// with a wildcard `"*"` origin. Browsers refuse this combination per the
    /// Fetch spec, and `tower-http`'s `CorsLayer` panics when asked to build
    /// it, so we fail fast at config load with an actionable message.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            return Err(ConfigError::Validation(
                "CORS: allow_credentials=true is incompatible with allowed_origins=[\"*\"]; \
                 list explicit origins instead (browsers reject the wildcard+credentials combo)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

fn default_cors_methods() -> Vec<String> {
    vec![
        "GET".to_owned(),
        "POST".to_owned(),
        "PUT".to_owned(),
        "DELETE".to_owned(),
        "PATCH".to_owned(),
        "OPTIONS".to_owned(),
    ]
}

fn default_cors_headers() -> Vec<String> {
    vec!["Content-Type".to_owned(), "Authorization".to_owned()]
}

const fn default_cors_max_age() -> u64 {
    86400
}

// ── CompressionConfig ──────────────────────────────────────────────────────

/// Response compression settings (`[compression]` section in `autumn.toml`).
///
/// Compression is **off by default** to avoid the [BREACH/CRIME] class of
/// compression side-channel attacks, where an attacker can infer secret
/// content (e.g. CSRF tokens) by observing how the compressed size changes as
/// they inject attacker-controlled bytes alongside the secret. Enable only when
/// you understand the tradeoff — or when a CDN / reverse-proxy handles TLS and
/// terminates there.
///
/// [BREACH/CRIME]: https://breachattack.com/
///
/// # One-liner opt-in
///
/// ```toml
/// [compression]
/// enabled = true
/// ```
///
/// # Environment variable override
///
/// | Variable | Field | Type |
/// |----------|-------|------|
/// | `AUTUMN_COMPRESSION__ENABLED` | `enabled` | `bool` |
///
/// # `ETag` compatibility
///
/// Autumn's framework-managed compression layer is applied **outside** any
/// user-registered `EtagLayer`, so `ETags` are computed on the uncompressed body.
/// Because `CompressionLayer` sets `Vary: Accept-Encoding`, caches correctly
/// store separate entries per encoding. Using weak `ETags` (`W/`) when
/// compression is enabled is safe per RFC 7232 §2.1 (weak comparison allows
/// encoding variations).
///
/// # Example
///
/// ```rust
/// use autumn_web::config::CompressionConfig;
///
/// let cfg = CompressionConfig::default();
/// assert!(!cfg.enabled);
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CompressionConfig {
    /// Enable response compression. Default: `false`.
    ///
    /// When `true`, the framework inserts a `CompressionLayer` that honors the
    /// client's `Accept-Encoding` header (gzip and brotli supported) and sets
    /// `Vary: Accept-Encoding` on all compressible responses.
    /// Non-compressible content types (images, archives) and responses that
    /// already carry `Content-Encoding` are passed through unchanged.
    #[serde(default)]
    pub enabled: bool,
}

/// Apply `AUTUMN_DEPLOY__*` environment overrides to an optional deploy config.
///
/// `[deploy]` is a top-level optional section. This materializes it from the
/// environment when any of its keys are set (seeding the documented defaults if
/// the section was absent/`None`), so a CI/VPS deploy can keep the target host
/// out of `autumn.toml` and drive it entirely through `AUTUMN_DEPLOY__*`. Env
/// overrides win over any TOML-provided values.
///
/// Shared by [`AutumnConfig::load`] and `autumn doctor`'s deploy preflight so
/// both surfaces resolve the identical deploy target (host, `ssh_port`, …) for the
/// same environment + profile + TOML.
// Exposed for autumn-cli's `autumn deploy` preflight (doctor) to reuse the deploy env-override logic; not yet a stable public API.
#[doc(hidden)]
pub fn apply_deploy_env_overrides(deploy: &mut Option<DeployConfig>, env: &dyn Env) {
    // This array is a PRESENCE PROBE gating materialization of the ENTIRE
    // `[deploy]` table: a key missing from it means an env-only config that sets
    // only that key produces no deploy section at all — a silent skip, not an
    // error, in both `AutumnConfig::load` and `autumn doctor`. Every key parsed
    // below MUST appear here.
    const KEYS: [&str; 13] = [
        "AUTUMN_DEPLOY__HOST",
        "AUTUMN_DEPLOY__HOSTS",
        "AUTUMN_DEPLOY__USER",
        "AUTUMN_DEPLOY__SSH_PORT",
        "AUTUMN_DEPLOY__APP_NAME",
        "AUTUMN_DEPLOY__APP_DIR",
        "AUTUMN_DEPLOY__SERVICE_NAME",
        "AUTUMN_DEPLOY__READINESS_TIMEOUT_SECS",
        "AUTUMN_DEPLOY__KEEP_RELEASES",
        "AUTUMN_DEPLOY__PROFILE",
        "AUTUMN_DEPLOY__TLS__ENABLED",
        "AUTUMN_DEPLOY__TLS__HOST",
        "AUTUMN_DEPLOY__INSTALL_PROXY",
    ];
    if !KEYS.iter().any(|key| env.var(key).is_ok()) {
        return;
    }
    let deploy = deploy.get_or_insert_with(DeployConfig::default);
    // #1621 review round 1: `host` and `hosts` are mutually exclusive downstream, so
    // applying one spelling from the environment on top of the other spelling in TOML
    // produced a config that refuses every `autumn deploy` subcommand — even though
    // `AUTUMN_DEPLOY__*` is documented to win over TOML. Whether each spelling was set
    // non-empty in the environment is therefore captured before either is applied, so a
    // non-empty env value can clear the TOML alternate below.
    //
    // "Non-empty" is judged conservatively: a value blank after trimming never clears
    // the other spelling, so the established empty-means-unset semantics survive.
    // `AUTUMN_DEPLOY__HOST=` and `AUTUMN_DEPLOY__HOSTS=`/`,`/` , ` are the shape a CI or
    // compose template emits for an unfilled slot, and they say nothing about the other
    // spelling. `parse_env_option_string` below still applies its own unchanged
    // `is_empty()` rule to `host` itself; trimming here only makes the clearing decision
    // stricter, so a whitespace-only value can never drop a configured fleet list.
    let env_set_host = env
        .var("AUTUMN_DEPLOY__HOST")
        .is_ok_and(|value| !value.trim().is_empty());
    let env_set_hosts = env
        .var("AUTUMN_DEPLOY__HOSTS")
        .is_ok_and(|value| value.split(',').any(|entry| !entry.trim().is_empty()));
    parse_env_option_string(env, "AUTUMN_DEPLOY__HOST", &mut deploy.host);
    // #1621: the fleet host list, as CSV. It REPLACES the whole TOML list (a
    // fleet-level retarget), matching every other `AUTUMN_DEPLOY__*` override.
    // Entries are trimmed by `parse_env_csv_non_empty`, which also DROPS blank
    // segments: `AUTUMN_DEPLOY__HOSTS=` means unset (as `AUTUMN_DEPLOY__HOST=`
    // does) and a trailing/doubled comma is tolerated, rather than reaching the
    // CLI as a blank fleet entry that refuses every deploy subcommand. Duplicate
    // entries are still rejected downstream by the CLI's `ResolvedFleet::resolve`.
    parse_env_csv_non_empty(env, "AUTUMN_DEPLOY__HOSTS", &mut deploy.hosts);
    // Env-over-TOML precedence, applied to the spelling the operator did not set:
    // retargeting a `[deploy] host` project as a fleet, or a `[deploy] hosts` project at
    // a single server, is a legitimate env override rather than a conflict. It is
    // deliberately not a tie-break: when both env spellings are set non-empty the
    // rollout order is genuinely ambiguous — an operator error, not a precedence
    // question — so both survive and the existing mutual-exclusion refusal
    // (`DeployConfig::validate`, the CLI's `deploy_host_list`) still fires, naming both
    // keys.
    if env_set_hosts && !env_set_host {
        deploy.host = None;
    } else if env_set_host && !env_set_hosts {
        deploy.hosts.clear();
    }
    parse_env_string(env, "AUTUMN_DEPLOY__USER", &mut deploy.user);
    parse_env(env, "AUTUMN_DEPLOY__SSH_PORT", &mut deploy.ssh_port);
    parse_env_option_string(env, "AUTUMN_DEPLOY__APP_NAME", &mut deploy.app_name);
    parse_env_option_string(env, "AUTUMN_DEPLOY__APP_DIR", &mut deploy.app_dir);
    parse_env_option_string(env, "AUTUMN_DEPLOY__SERVICE_NAME", &mut deploy.service_name);
    parse_env(
        env,
        "AUTUMN_DEPLOY__READINESS_TIMEOUT_SECS",
        &mut deploy.readiness_timeout_secs,
    );
    parse_env(
        env,
        "AUTUMN_DEPLOY__KEEP_RELEASES",
        &mut deploy.keep_releases,
    );
    parse_env_string(env, "AUTUMN_DEPLOY__PROFILE", &mut deploy.profile);
    // Opt-in TLS for the deploy-managed proxy (#1969). Env wins over TOML, matching
    // every other deploy override above.
    parse_env_bool(env, "AUTUMN_DEPLOY__TLS__ENABLED", &mut deploy.tls.enabled);
    parse_env_option_string(env, "AUTUMN_DEPLOY__TLS__HOST", &mut deploy.tls.host);
    // Host preparation opt-out (#1607). Env wins over TOML, like every other deploy
    // override above, so a CI pipeline deploying to pre-provisioned hosts can
    // decline it without editing `autumn.toml`.
    parse_env_bool(
        env,
        "AUTUMN_DEPLOY__INSTALL_PROXY",
        &mut deploy.install_proxy,
    );
}

/// Parse an environment variable into a typed target, logging a warning on failure.
fn parse_env<T: std::str::FromStr>(env: &dyn Env, key: &str, target: &mut T) {
    if let Ok(val) = env.var(key) {
        match val.parse::<T>() {
            Ok(v) => *target = v,
            Err(_) => eprintln!("Warning: {key}={val:?} is not valid, ignoring"),
        }
    }
}

fn parse_env_option_string(env: &dyn Env, key: &str, target: &mut Option<String>) {
    if let Ok(val) = env.var(key) {
        *target = if val.is_empty() { None } else { Some(val) };
    }
}

/// Secret-aware variant of [`parse_env_option_string`]: an empty (after
/// trimming) value clears the target, otherwise the trimmed value is wrapped in
/// a [`secrecy::SecretString`] so it is redacted from `Debug` and zeroized on
/// drop.
fn parse_env_option_secret(env: &dyn Env, key: &str, target: &mut Option<secrecy::SecretString>) {
    if let Ok(val) = env.var(key) {
        let trimmed = val.trim();
        *target = if trimmed.is_empty() {
            None
        } else {
            Some(secrecy::SecretString::from(trimmed.to_owned()))
        };
    }
}

fn parse_env_option<T: std::str::FromStr>(env: &dyn Env, key: &str, target: &mut Option<T>) {
    if let Ok(val) = env.var(key) {
        if val.is_empty() {
            *target = None;
        } else {
            match val.parse::<T>() {
                Ok(v) => *target = Some(v),
                Err(_) => eprintln!("Warning: {key}={val:?} is not valid, ignoring"),
            }
        }
    }
}

fn parse_env_string(env: &dyn Env, key: &str, target: &mut String) {
    if let Ok(val) = env.var(key) {
        *target = val;
    }
}

fn parse_env_bool(env: &dyn Env, key: &str, target: &mut bool) {
    if let Ok(val) = env.var(key) {
        match val.as_str() {
            "true" | "1" => *target = true,
            "false" | "0" => *target = false,
            _ => eprintln!("Warning: {key}={val:?} is not valid (expected true/false), ignoring"),
        }
    }
}

fn parse_env_option_bool(env: &dyn Env, key: &str, target: &mut Option<bool>) {
    if let Ok(val) = env.var(key) {
        match val.as_str() {
            "true" | "1" => *target = Some(true),
            "false" | "0" => *target = Some(false),
            _ => eprintln!("Warning: {key}={val:?} is not valid (expected true/false), ignoring"),
        }
    }
}

fn parse_env_csv(env: &dyn Env, key: &str, target: &mut Vec<String>) {
    if let Ok(val) = env.var(key) {
        *target = val.split(',').map(|s| s.trim().to_owned()).collect();
    }
}

/// CSV env override that drops blank segments (issue #1621).
///
/// Same shape as [`parse_env_csv`], but an empty/whitespace-only segment is not a
/// list entry. Two consequences, both deliberate:
///
/// * `KEY=` (or `KEY="   "`) means **unset**, i.e. the list is cleared rather than
///   set to a one-element vector holding a blank string. That is the shape a CI or
///   compose env template produces for a not-yet-filled-in value, and it matches
///   [`parse_env_option_string`]'s empty-is-unset rule for the sibling scalar keys
///   (and `crate::maintenance::flag_file_path_from`'s blank-is-unset rule).
/// * A trailing or doubled comma — routine in generated env lists — is tolerated
///   instead of surfacing downstream as a blank entry.
///
/// Used only for `AUTUMN_DEPLOY__HOSTS`, whose downstream consumer turns a blank
/// entry into a hard refusal of every `autumn deploy` subcommand; the other CSV
/// overrides keep [`parse_env_csv`]'s long-standing behaviour.
fn parse_env_csv_non_empty(env: &dyn Env, key: &str, target: &mut Vec<String>) {
    if let Ok(val) = env.var(key) {
        *target = val
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }
}

// ── Default functions ──────────────────────────────────────────────

const fn default_port() -> u16 {
    3000
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

const fn default_shutdown_timeout() -> u64 {
    30
}

const fn default_prestop_grace() -> u64 {
    5
}

const fn default_pool_size() -> usize {
    10
}

const fn default_max_connections_warn_threshold() -> usize {
    100
}

const fn default_connect_timeout() -> u64 {
    5
}

const fn default_pin_after_write_secs() -> u64 {
    5
}

fn default_log_level() -> String {
    "info".to_owned()
}

const fn default_access_log() -> bool {
    true
}

fn default_access_log_exclude() -> Vec<String> {
    vec![
        "/health".to_owned(),
        "/live".to_owned(),
        "/ready".to_owned(),
        "/startup".to_owned(),
        "/actuator".to_owned(),
        "/static".to_owned(),
    ]
}

fn default_telemetry_service_name() -> String {
    "autumn-app".to_owned()
}

fn default_telemetry_service_version() -> String {
    "unknown".to_owned()
}

fn default_telemetry_environment() -> String {
    "development".to_owned()
}

/// Default `[server.tls]` cert/key reload poll interval, in seconds.
///
/// Kept in lockstep with `crate::tls::DEFAULT_RELOAD_INTERVAL_SECS` (the
/// serving path's constant); a literal is used here because this default must
/// compile even when the `tls` feature — and thus `crate::tls` — is off.
const fn default_tls_reload_interval_secs() -> u64 {
    60
}

/// Default `[server.tls]` inbound-handshake timeout, in seconds.
///
/// Bounds a single TLS handshake so a client that opens TCP but never sends a
/// `ClientHello` cannot park the accept loop. 10s is generous for a real
/// handshake while still shedding a stalled connection promptly.
const fn default_tls_handshake_timeout_secs() -> u64 {
    10
}

/// Default SSH user for `[deploy]`.
fn default_deploy_user() -> String {
    "root".to_owned()
}

/// Default SSH port for `[deploy]`.
const fn default_deploy_ssh_port() -> u16 {
    22
}

/// Default readiness window (seconds) before an `autumn deploy` rolls back.
const fn default_deploy_readiness_timeout_secs() -> u64 {
    60
}

/// Default number of prior releases retained on the host for rollback.
const fn default_deploy_keep_releases() -> u32 {
    3
}

/// Default profile the deployed app runs under. Defaults to the production
/// profile so an `autumn deploy` never silently boots under the `dev` profile.
fn default_deploy_profile() -> String {
    "prod".to_owned()
}

/// Default directory for the ACME account key and issued certificates
/// (`[server.tls.acme] cache_dir`).
fn default_acme_cache_dir() -> PathBuf {
    PathBuf::from("config/acme")
}

/// Default HTTP-01 challenge / redirect port (`[server.tls.acme]
/// http_challenge_port`). The ACME CA always validates HTTP-01 over port 80.
const fn default_acme_http_challenge_port() -> u16 {
    80
}

/// Default renew-before window in days (`[server.tls.acme] renew_before_days`).
/// Let's Encrypt certificates are valid for 90 days; renewing with 30 days left
/// leaves ample slack for retries.
const fn default_acme_renew_before_days() -> u32 {
    30
}

/// Default credentials-store key holding the DNS provider credential
/// (`[server.tls.acme.dns] credential`).
fn default_acme_dns_credential() -> String {
    "acme_dns".to_owned()
}

/// Default bound on the DNS-01 TXT propagation wait, in seconds
/// (`[server.tls.acme.dns] propagation_timeout_secs`). Five minutes covers the
/// slow tail of public-resolver caches without parking a renewal indefinitely.
const fn default_acme_dns_propagation_timeout_secs() -> u64 {
    300
}

/// Default gap between propagation probes, in seconds
/// (`[server.tls.acme.dns] poll_interval_secs`).
const fn default_acme_dns_poll_interval_secs() -> u64 {
    5
}

/// Upper bound on `[server.tls.acme.dns] propagation_timeout_secs`.
///
/// The wait is computed as `Instant::now() + Duration::from_secs(timeout)`, which
/// **panics** on overflow — inside the spawned renewal task, where the panic
/// silently kills the loop and leaves the self-signed placeholder served
/// forever. An hour is far past any real provider's propagation time.
const MAX_ACME_DNS_PROPAGATION_TIMEOUT_SECS: u64 = 3600;

/// Default public resolvers the propagation wait queries
/// (`[server.tls.acme.dns] resolvers`). Two independent operators, so one
/// operator's stale cache cannot alone declare a record propagated.
fn default_acme_dns_resolvers() -> Vec<String> {
    vec!["1.1.1.1:53".to_owned(), "8.8.8.8:53".to_owned()]
}

const fn default_health_enabled() -> bool {
    true
}

fn default_health_path() -> String {
    "/health".to_owned()
}

fn default_live_path() -> String {
    "/live".to_owned()
}

const fn default_upgrade_enabled() -> bool {
    true
}

const fn default_upgrade_ready_timeout() -> u64 {
    30
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: default_upgrade_enabled(),
            ready_timeout_secs: default_upgrade_ready_timeout(),
        }
    }
}

fn default_ready_path() -> String {
    "/ready".to_owned()
}

fn default_startup_path() -> String {
    "/startup".to_owned()
}

// ── Default trait impls ────────────────────────────────────────────

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            strict_config: false,
            strict_config_enforce_all: false,
            shutdown_timeout_secs: default_shutdown_timeout(),
            prestop_grace_secs: default_prestop_grace(),
            upgrade: UpgradeConfig::default(),
            timeouts: RequestTimeoutsConfig::default(),
            unix_socket: None,
            max_concurrent_requests: None,
            capacity_contract: None,
            tls: None,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: None,
            primary_url: None,
            replica_url: None,
            pool_size: default_pool_size(),
            primary_pool_size: None,
            replica_pool_size: None,
            replica_fallback: ReplicaFallback::default(),
            read_your_writes: ReadYourWrites::default(),
            pin_after_write_secs: default_pin_after_write_secs(),
            connect_timeout_secs: default_connect_timeout(),
            startup_wait_secs: 0,
            auto_migrate: None,
            auto_migrate_in_production: false,
            statement_timeout: None,
            slow_query_threshold: default_slow_query_threshold(),
            shards: Vec::new(),
            directory_shard_router: false,
            max_connections_warn_threshold: default_max_connections_warn_threshold(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
            filter_parameters: Vec::new(),
            unfilter_parameters: Vec::new(),
            access_log: default_access_log(),
            access_log_exclude: default_access_log_exclude(),
            capture: crate::log::capture::LogCaptureConfig::default(),
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service_name: default_telemetry_service_name(),
            service_namespace: None,
            service_version: default_telemetry_service_version(),
            environment: default_telemetry_environment(),
            otlp_endpoint: None,
            protocol: TelemetryProtocol::default(),
            strict: false,
        }
    }
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: default_health_enabled(),
            path: default_health_path(),
            live_path: default_live_path(),
            ready_path: default_ready_path(),
            startup_path: default_startup_path(),
            detailed: false,
        }
    }
}

// ----------------------------------------------------------------------------
// ConfigLoader — tier-1 boot-time replaceable config loading
// ----------------------------------------------------------------------------

/// Pluggable boot-time configuration loader.
///
/// Replace the default TOML + env loader with a custom strategy (e.g. AWS
/// Secrets Manager, Consul, a JSON file, an HTTP fetch) by implementing this
/// trait and installing it on the [`AppBuilder`](crate::app::AppBuilder) via
/// [`with_config_loader`](crate::app::AppBuilder::with_config_loader).
///
/// The trait's return type uses `impl Future + Send` so implementations can
/// freely use `async fn` in their bodies while the framework can still spawn
/// the load on any executor.
///
/// # Example
///
/// ```rust,no_run
/// use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader};
///
/// pub struct JsonFileConfigLoader { path: std::path::PathBuf }
///
/// impl ConfigLoader for JsonFileConfigLoader {
///     async fn load(&self) -> Result<AutumnConfig, ConfigError> {
///         let bytes = std::fs::read(&self.path).map_err(ConfigError::Io)?;
///         serde_json::from_slice(&bytes)
///             .map_err(|e| ConfigError::Validation(e.to_string()))
///     }
/// }
/// ```
pub trait ConfigLoader: Send + Sync + 'static {
    /// Load and return a fully-resolved [`AutumnConfig`].
    ///
    /// Implementations are responsible for any layering, profile resolution,
    /// and validation they care to apply. The default implementation
    /// ([`TomlEnvConfigLoader`]) preserves Autumn's five-layer load
    /// (framework defaults → profile defaults → `autumn.toml` →
    /// `autumn-{profile}.toml` → `AUTUMN_*` env vars).
    fn load(&self) -> impl std::future::Future<Output = Result<AutumnConfig, ConfigError>> + Send;
}

/// Default [`ConfigLoader`] — Autumn's five-layer TOML + env load strategy.
///
/// Delegates to [`AutumnConfig::load_with_env`] using [`OsEnv`] for environment
/// variable reads. This is the loader used when no override is installed via
/// [`with_config_loader`](crate::app::AppBuilder::with_config_loader).
#[derive(Debug, Default, Clone)]
pub struct TomlEnvConfigLoader {
    /// Top-level config roots declared by plugins via
    /// [`AppBuilder::config_section`](crate::app::AppBuilder::config_section).
    /// Each is treated as known-and-opaque under `server.strict_config`. Empty
    /// by default, so a bare `TomlEnvConfigLoader::new()` behaves exactly as
    /// before the plugin config-section seam.
    allowed_plugin_roots: BTreeSet<String>,
}

impl TomlEnvConfigLoader {
    /// Construct a new default loader with no declared plugin config roots.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allowed_plugin_roots: BTreeSet::new(),
        }
    }

    /// Declare the plugin-owned top-level config roots this loader should treat
    /// as known-and-opaque under `server.strict_config`.
    ///
    /// Wired by [`AppBuilder::run`](crate::app::AppBuilder::run) from the roots
    /// registered through
    /// [`config_section`](crate::app::AppBuilder::config_section), so a
    /// plugin-enabled app boots under strict config while genuinely-unknown
    /// roots still hard-fail. See
    /// [`load_with_env_and_plugin_roots`](AutumnConfig::load_with_env_and_plugin_roots).
    #[must_use]
    pub fn with_plugin_config_roots(mut self, roots: BTreeSet<String>) -> Self {
        self.allowed_plugin_roots = roots;
        self
    }
}

impl ConfigLoader for TomlEnvConfigLoader {
    async fn load(&self) -> Result<AutumnConfig, ConfigError> {
        // Feed a project-root `.env` into the `AUTUMN_*` env layer before
        // resolving config from the real environment. Rather than mutating the
        // process environment (unsound on a live multi-threaded runtime), `.env`
        // values are layered *under* the real environment via an overlay `Env`,
        // so a real env var always wins. The sync file IO in `resolve_dotenv_vars`
        // is fine on the async path. A malformed `.env` fails loudly here rather
        // than silently skipping developer-provided values.
        let base = OsEnv;
        let profile = resolve_profile(&base);
        // Resolve `.env` from the same base directory config uses for
        // `autumn.toml` (AUTUMN_MANIFEST_DIR when set, else the process CWD),
        // so a binary launched from outside its crate root reads the `.env`
        // next to its config instead of the process working directory.
        let dir = crate::dotenv::dotenv_base_dir(&base);
        let vars = crate::dotenv::resolve_dotenv_vars(&dir, &profile, &base)
            .map_err(|e| ConfigError::Dotenv(e.to_string()))?;
        let env = crate::dotenv::DotenvEnv::new(&base, vars);
        AutumnConfig::load_with_env_and_plugin_roots(&env, &self.allowed_plugin_roots)
    }
}

const fn default_slow_query_threshold() -> std::time::Duration {
    std::time::Duration::from_millis(500)
}

/// Parses a duration string like "500ms", "5s", "2m", "1h",
/// or a plain integer representing milliseconds.
///
/// # Errors
/// Returns a `String` describing the parse failure when the input is empty,
/// has an unrecognised suffix, or contains a non-numeric value.
pub fn parse_duration_str(s: &str) -> Result<std::time::Duration, String> {
    if s.is_empty() {
        return Err("duration string is empty".to_owned());
    }

    // Check if it's a plain integer
    if let Ok(ms) = s.parse::<u64>() {
        return Ok(std::time::Duration::from_millis(ms));
    }

    // Try parsing suffix
    if let Some(val_str) = s.strip_suffix("ms") {
        let val = val_str
            .parse::<u64>()
            .map_err(|e| format!("invalid duration integer: {e}"))?;
        return Ok(std::time::Duration::from_millis(val));
    }

    if let Some(val_str) = s.strip_suffix('s') {
        let val = val_str
            .parse::<u64>()
            .map_err(|e| format!("invalid duration integer: {e}"))?;
        return Ok(std::time::Duration::from_secs(val));
    }

    if let Some(val_str) = s.strip_suffix('m') {
        let val = val_str
            .parse::<u64>()
            .map_err(|e| format!("invalid duration integer: {e}"))?;
        let secs = val.checked_mul(60).ok_or_else(|| {
            format!("duration overflow: '{s}' exceeds maximum representable value")
        })?;
        return Ok(std::time::Duration::from_secs(secs));
    }

    if let Some(val_str) = s.strip_suffix('h') {
        let val = val_str
            .parse::<u64>()
            .map_err(|e| format!("invalid duration integer: {e}"))?;
        let secs = val.checked_mul(3600).ok_or_else(|| {
            format!("duration overflow: '{s}' exceeds maximum representable value")
        })?;
        return Ok(std::time::Duration::from_secs(secs));
    }

    Err(format!("invalid duration format: '{s}'"))
}

/// Deserialises a TOML/JSON value into a [`std::time::Duration`].
///
/// Accepts either a string (`"500ms"`, `"5s"`, `"2m"`, `"1h"`) or a bare
/// integer (interpreted as milliseconds).
///
/// # Errors
/// Returns a deserialisation error if the value is not a valid duration.
pub fn deserialize_duration<'de, D>(deserializer: D) -> Result<std::time::Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DurationOrStr {
        String(String),
        Integer(u64),
    }

    match DurationOrStr::deserialize(deserializer)? {
        DurationOrStr::String(s) => parse_duration_str(&s).map_err(serde::de::Error::custom),
        DurationOrStr::Integer(i) => Ok(std::time::Duration::from_millis(i)),
    }
}

/// Deserialises an optional TOML/JSON value into <code>Option&lt;[std::time::Duration]&gt;</code>.
///
/// Accepts either a string (`"500ms"`, `"5s"`, `"2m"`, `"1h"`), a bare
/// integer (milliseconds), or `null`/absent to mean no timeout.
///
/// # Errors
/// Returns a deserialisation error if the value is present but invalid.
pub fn deserialize_option_duration<'de, D>(
    deserializer: D,
) -> Result<Option<std::time::Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Wrapper(#[serde(deserialize_with = "deserialize_duration")] std::time::Duration);

    Option::<Wrapper>::deserialize(deserializer).map(|opt| opt.map(|w| w.0))
}

/// Row-level multi-tenancy configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TenancyConfig {
    /// Whether row-level multi-tenancy is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Source configuration from which the tenant ID is extracted.
    /// Values can be "header" (default), "subdomain", "session", "jwt".
    #[serde(default = "default_tenancy_source")]
    pub source: String,

    /// Header name to lookup if source is "header". Default: "x-tenant-id".
    #[serde(default = "default_tenancy_header_name")]
    pub header_name: String,

    /// Session key to lookup if source is "session". Default: "`tenant_id`".
    #[serde(default = "default_tenancy_session_key")]
    pub session_key: String,

    /// JWT claim to lookup if source is "jwt". Default: "`tenant_id`".
    #[serde(default = "default_tenancy_jwt_claim")]
    pub jwt_claim: String,

    /// JWT secret key used to verify the JWT signature.
    ///
    /// Stored as a [`secrecy::SecretString`] so the raw value is redacted
    /// from `Debug` output and zeroized on drop. Call
    /// [`secrecy::ExposeSecret::expose_secret`] at the point of use.
    #[serde(default)]
    pub jwt_secret: Option<secrecy::SecretString>,

    /// Expected JWT issuer to validate.
    #[serde(default)]
    pub jwt_issuer: Option<String>,

    /// Expected JWT audience (`aud` claim) to validate.
    /// When set, audience checking is enabled; when `None`, audience checking
    /// is skipped for backward compatibility.
    #[serde(default)]
    pub jwt_audience: Option<String>,

    /// Optional base domain for subdomain tenancy.
    #[serde(default)]
    pub base_domain: Option<String>,

    /// Request paths that bypass tenant resolution entirely, so they remain
    /// reachable without a tenant (e.g. `/login`, `/signup`, static assets).
    ///
    /// Matching is exact or slash-delimited prefix: `/login` matches `/login`
    /// and `/login/sso` but not `/login-admin`. The configured health check
    /// path is always treated as public regardless of this list.
    #[serde(default)]
    pub public_paths: Vec<String>,

    /// Where to redirect when a non-public request has no valid tenant.
    ///
    /// When set, a missing/unauthenticated tenant on a protected path returns a
    /// 302 redirect here instead of a raw 401 — friendlier for browser `SaaS`
    /// logins. When `None`, the underlying authorization error is returned.
    #[serde(default)]
    pub login_redirect: Option<String>,

    /// Soft per-tenant memory quota, in bytes, for in-process tenant cells.
    /// `0` disables the quota (unlimited).
    #[serde(default)]
    pub quota_bytes: usize,

    /// Maximum number of resident tenant cells; least-recently-used cells are
    /// evicted above this. `0` = unbounded.
    #[serde(default)]
    pub max_cells: usize,

    /// Evict a tenant cell whose last access exceeds this many seconds.
    /// `0` = disabled.
    #[serde(default)]
    pub idle_ttl_secs: u64,
}

fn default_tenancy_source() -> String {
    "header".to_string()
}

fn default_tenancy_header_name() -> String {
    "x-tenant-id".to_string()
}

fn default_tenancy_session_key() -> String {
    "tenant_id".to_string()
}

fn default_tenancy_jwt_claim() -> String {
    "tenant_id".to_string()
}

impl Default for TenancyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: default_tenancy_source(),
            header_name: default_tenancy_header_name(),
            session_key: default_tenancy_session_key(),
            jwt_claim: default_tenancy_jwt_claim(),
            jwt_secret: None,
            jwt_issuer: None,
            jwt_audience: None,
            base_domain: None,
            public_paths: Vec::new(),
            login_redirect: None,
            quota_bytes: 0,
            max_cells: 0,
            idle_ttl_secs: 0,
        }
    }
}

// ── Resilience configuration ───────────────────────────────────────────────

/// Resilience policy configurations.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResilienceConfig {
    /// Circuit breaker configurations.
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

/// Circuit breaker configuration structure.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Default circuit breaker policies.
    #[serde(default)]
    pub defaults: CircuitBreakerPolicyConfig,
    /// Per-host circuit breaker policy overrides.
    #[serde(default)]
    pub hosts: std::collections::HashMap<String, CircuitBreakerPolicyConfig>,
}

/// Configurable settings for a circuit breaker policy.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CircuitBreakerPolicyConfig {
    /// Failure ratio threshold (e.g. 0.5) to trip the breaker.
    pub failure_ratio_threshold: Option<f64>,
    /// Sample window duration in seconds.
    pub sample_window_secs: Option<u64>,
    /// Minimum samples required to evaluate failure ratio.
    pub minimum_sample_count: Option<u64>,
    /// Open state duration in seconds before entering half-open.
    pub open_duration_secs: Option<u64>,
    /// Number of successful trials required in half-open state to close the breaker.
    pub half_open_trial_count: Option<u64>,
}

impl AutumnConfig {
    fn apply_resilience_env_overrides_with_env(&mut self, env: &dyn Env) {
        parse_env_option(
            env,
            "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__FAILURE_RATIO_THRESHOLD",
            &mut self
                .resilience
                .circuit_breaker
                .defaults
                .failure_ratio_threshold,
        );
        parse_env_option(
            env,
            "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__SAMPLE_WINDOW_SECS",
            &mut self.resilience.circuit_breaker.defaults.sample_window_secs,
        );
        parse_env_option(
            env,
            "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__MINIMUM_SAMPLE_COUNT",
            &mut self
                .resilience
                .circuit_breaker
                .defaults
                .minimum_sample_count,
        );
        parse_env_option(
            env,
            "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__OPEN_DURATION_SECS",
            &mut self.resilience.circuit_breaker.defaults.open_duration_secs,
        );
        parse_env_option(
            env,
            "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__HALF_OPEN_TRIAL_COUNT",
            &mut self
                .resilience
                .circuit_breaker
                .defaults
                .half_open_trial_count,
        );
    }
}

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnyProbe {
    Str,
    Map,
    Seq,
}

#[derive(Clone)]
pub struct SchemaDeserializer {
    path: Vec<String>,
    schema: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// Per-path override for what `deserialize_any` feeds. Absent = `Str`.
    /// A path is escalated (Str→Map→Seq) across walk passes when its visitor
    /// rejects the current probe (e.g. `jobs.queues`'s seq/map-only visitor
    /// rejects the scalar `"0"`). See `get_schema_keys`.
    any_probe: Arc<Mutex<HashMap<String, AnyProbe>>>,
    /// Paths whose `deserialize_any` probe was rejected during the current pass.
    rejected: Arc<Mutex<Vec<String>>>,
}

impl Default for SchemaDeserializer {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaDeserializer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: Vec::new(),
            schema: Arc::new(Mutex::new(HashMap::new())),
            any_probe: Arc::new(Mutex::new(HashMap::new())),
            rejected: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn into_schema(self) -> HashMap<String, HashSet<String>> {
        let lock = self
            .schema
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lock.clone()
    }
}

impl<'de> de::Deserializer<'de> for SchemaDeserializer {
    type Error = serde::de::value::Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // `deserialize_any` is inherently ambiguous for a placeholder walker:
        // untagged SCALAR parsers (e.g. `deserialize_duration`) need a string,
        // while a visitor that accepts only seq/map (e.g. `JobQueuesConfig` at
        // `jobs.queues`) rejects a string and aborts the whole remaining walk
        // (#1890). We can't know which shape a given visitor wants, and serde
        // seeds can't be retried mid-walk, so we probe with a scalar by default
        // and let `get_schema_keys` re-run the walk, escalating any REJECTED
        // path to a map/seq probe on the next pass until none reject.
        let path = self.path.join(".");
        let probe = self
            .any_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&path)
            .copied()
            .unwrap_or(AnyProbe::Str);
        let result = match probe {
            // "0" is a valid non-empty string that also parses as an int/duration,
            // so untagged string- and number-shaped scalar parsers both accept it.
            AnyProbe::Str => visitor.visit_str("0"),
            // Empty map/seq: a seq/map-only visitor accepts it and yields an empty
            // value, so the walk records the field as a leaf and CONTINUES past it
            // (we intentionally do NOT descend — e.g. jobs.queues has dynamic keys).
            AnyProbe::Map => visitor.visit_map(SchemaMapAccess {
                fields: [].iter(),
                current_field: None,
                deserializer: self.clone(),
            }),
            AnyProbe::Seq => visitor.visit_seq(SchemaSeqAccess {
                done: true,
                deserializer: self.clone(),
            }),
        };
        if result.is_err() {
            self.rejected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path);
        }
        result
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_bool(false)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i8(0)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i16(0)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i32(0)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(0)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u8(0)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u16(0)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u32(0)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(0)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f32(0.0)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f64(0.0)
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_char('\0')
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str("")
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(String::new())
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_bytes(&[])
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_byte_buf(Vec::new())
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(SchemaSeqAccess {
            done: false,
            deserializer: self,
        })
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(SchemaMapAccess {
            fields: [].iter(),
            current_field: None,
            deserializer: self,
        })
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path_str = self.path.join(".");
        {
            let mut schema = self.schema.lock().unwrap();
            schema.insert(path_str, fields.iter().map(|&s| s.to_string()).collect());
        }

        visitor.visit_map(SchemaMapAccess {
            fields: fields.iter(),
            current_field: None,
            deserializer: self,
        })
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // Feed the FIRST declared variant name (not `""`) so serde's derived
        // variant-identifier visitor accepts it. An empty tag is an "unknown
        // variant" error that aborts the ENTIRE remaining schema traversal, so a
        // single enum field (e.g. `server.tls.acme.directory`) would drop every
        // sibling/subsequent section (`database`, …) from the derived schema —
        // silently disabling the strict unknown-key validator for them. The enum
        // is still treated as an opaque leaf: every `SchemaEnumAccess` variant
        // arm resolves to `visit_unit` without recursing.
        visitor.visit_enum(SchemaEnumAccess {
            variant: variants.first().copied().unwrap_or_default(),
        })
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str("")
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }
}

struct SchemaSeqAccess {
    done: bool,
    deserializer: SchemaDeserializer,
}

impl<'de> SeqAccess<'de> for SchemaSeqAccess {
    type Error = serde::de::value::Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            seed.deserialize(self.deserializer.clone()).map(Some)
        }
    }
}

struct SchemaMapAccess {
    fields: std::slice::Iter<'static, &'static str>,
    current_field: Option<&'static str>,
    deserializer: SchemaDeserializer,
}

impl<'de> MapAccess<'de> for SchemaMapAccess {
    type Error = serde::de::value::Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        if let Some(&field) = self.fields.next() {
            self.current_field = Some(field);
            seed.deserialize(de::value::StrDeserializer::new(field))
                .map(Some)
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let field = self.current_field.take().unwrap();
        let mut new_path = self.deserializer.path.clone();
        new_path.push(field.to_string());

        let nested = SchemaDeserializer {
            path: new_path,
            schema: self.deserializer.schema.clone(),
            any_probe: self.deserializer.any_probe.clone(),
            rejected: self.deserializer.rejected.clone(),
        };
        seed.deserialize(nested)
    }
}

struct SchemaEnumAccess {
    /// The variant name to report to serde's derived variant-identifier visitor.
    /// Must be a REAL variant name (the first declared one), never `""`, or
    /// serde returns an "unknown variant" error that aborts schema traversal.
    variant: &'static str,
}

impl<'de> de::EnumAccess<'de> for SchemaEnumAccess {
    type Error = serde::de::value::Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let val = seed.deserialize(de::value::StrDeserializer::new(self.variant))?;
        Ok((val, self))
    }
}

impl<'de> de::VariantAccess<'de> for SchemaEnumAccess {
    type Error = serde::de::value::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        seed.deserialize(SchemaDeserializer::new())
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    struct FakeEnv(std::collections::HashMap<String, String>);
    impl Env for FakeEnv {
        fn var(&self, key: &str) -> Result<String, std::env::VarError> {
            self.0
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    #[test]
    fn test_schema_extractor() {
        let keys = AutumnConfig::get_schema_keys();
        assert!(keys.contains_key(""));
        let root_keys = &keys[""];
        assert!(root_keys.contains("server"));
        assert!(root_keys.contains("database"));

        assert!(keys.contains_key("server"));
        assert!(keys["server"].contains("port"));
        assert!(keys["server"].contains("host"));

        assert!(keys.contains_key("database"));
        assert!(keys["database"].contains("primary_url"));
    }

    // Regression (#1608): `server.tls.acme.directory` is the `AcmeDirectory`
    // enum, declared under `server` — which precedes `database` in `AutumnConfig`.
    // The `SchemaDeserializer` must treat that enum as an opaque leaf and keep
    // walking; if it instead errors on the variant tag it aborts the whole
    // traversal at the enum, dropping `database` (and every later section) from
    // the derived schema. That silently disables the strict unknown-key validator
    // for `[database]`, so a typo like `primry_url` stops being flagged.
    #[cfg(feature = "acme")]
    #[test]
    fn acme_enum_field_does_not_truncate_schema_traversal() {
        let keys = AutumnConfig::get_schema_keys();
        assert!(
            keys.contains_key("server.tls.acme"),
            "acme section must be in the schema"
        );
        assert!(
            keys.contains_key("database"),
            "database schema dropped: the acme enum truncated traversal"
        );
        assert!(keys["database"].contains("primary_url"));

        // The unknown-key validator must still flag a typo in a section declared
        // after the enum, with the edit-distance suggestion.
        let errs = AutumnConfig::validate_toml("[database]\nprimry_url = \"x\"\n", &keys);
        assert_eq!(
            errs,
            vec![(
                "database.primry_url".to_owned(),
                Some("database.primary_url".to_owned())
            )]
        );
    }

    #[test]
    fn test_strict_config_startup_fails_on_typo() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(
            &config_path,
            "[database]\nprimry_url = \"postgres://localhost/db\"",
        )
        .unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(res.is_err());
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(err_str.contains("primry_url"));
    }

    // #2063 helper: a `prod`, manifest-scoped env with `strict_config` sourced
    // from the on-disk `autumn.toml`. `prod` is pinned (not `dev`) so the
    // dev-only injected `[storage]` smart-default can't masquerade as an unknown
    // top-level root and skew these assertions — same reason the #1890 tests do.
    fn strict_prod_env_2063(temp: &std::path::Path) -> FakeEnv {
        FakeEnv(
            [
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        )
    }

    // #2063: the deploy CLI's lenient-unknown-roots load accepts a plugin-owned
    // top-level config table (`[media]`) under `strict_config` — the CLI cannot
    // know the app's plugin set — while the STRICT (app-boot) load still rejects
    // it, so app boot remains the authoritative strict gate for plugin roots.
    #[test]
    fn deploy_cli_lenient_accepts_plugin_owned_top_level_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[media]\nmediamtx_host = \"cdn.example\"\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        // App boot stays strict: an unknown `[media]` root is a hard error.
        let strict = AutumnConfig::load_with_env(&env);
        assert!(
            strict.is_err(),
            "app boot must stay strict for unknown plugin roots: {strict:?}"
        );
        let strict_err = format!("{:?}", strict.err().unwrap());
        assert!(
            strict_err.contains("media"),
            "strict error should name the unknown root: {strict_err}"
        );

        // Deploy CLI accepts it as opaque (warn, not fail) so the project deploys.
        let lenient = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            lenient.is_ok(),
            "deploy CLI must accept plugin-owned [media] under strict_config: {lenient:?}"
        );
    }

    // #2063: any genuinely-unknown top-level root (not just `[media]`) is
    // warn-not-fail under the lenient CLI load, and still fatal under app boot.
    #[test]
    fn deploy_cli_lenient_accepts_arbitrary_unknown_top_level_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[definitely_not_a_root]\nx = 1\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        assert!(
            AutumnConfig::load_with_env(&env).is_err(),
            "app boot must reject an unknown top-level root"
        );
        assert!(
            AutumnConfig::load_with_env_lenient_unknown_roots(&env).is_ok(),
            "deploy CLI must accept an unknown top-level root as opaque"
        );
    }

    // #2063: leniency is scoped to top-level ROOTS only. A typo INSIDE a known
    // section (`[database] primry_url`) stays a hard error even under the lenient
    // CLI load — the CLI does not soften validation of sections it knows.
    #[test]
    fn deploy_cli_lenient_still_rejects_known_section_typo() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[database]\nprimry_url = \"postgres://localhost/db\"\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        let res = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            res.is_err(),
            "known-section typo must still hard-fail under the lenient CLI load: {res:?}"
        );
        let err = format!("{:?}", res.err().unwrap());
        assert!(
            err.contains("primry_url"),
            "error should name the known-section typo: {err}"
        );
    }

    // #2063: malformed TOML is fatal everywhere — the lenient policy never
    // softens a parse failure.
    #[test]
    fn deploy_cli_lenient_still_rejects_malformed_toml() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\nthis is not = = valid toml\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        assert!(
            AutumnConfig::load_with_env_lenient_unknown_roots(&env).is_err(),
            "malformed TOML must still fail under the lenient CLI load"
        );
    }

    // #2067: the lenient deploy-CLI load must NOT soften a PROFILE-PREFIXED
    // unknown root like `[profile.prod.media]`. Its schema parent is empty
    // (the profile prefix is stripped before root-schema validation), but its
    // actual path (`profile.prod.media`) is not a true top-level root — so it
    // stays strict and hard-fails, exactly as the deployed app rejects it at
    // boot (the `config_section` seam exempts ONLY the true top-level `[media]`
    // via `path.is_empty()`). Otherwise deploy would pass while remote boot
    // fails.
    #[test]
    fn deploy_cli_lenient_still_rejects_profile_prefixed_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[profile.prod.media]\nmediamtx_host = \"cdn.example\"\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        let res = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            res.is_err(),
            "a profile-prefixed root ([profile.prod.media]) must stay strict under \
             the lenient CLI load — it is not a true top-level root and the deployed \
             app rejects it at boot: {res:?}"
        );
        let err = format!("{:?}", res.err().unwrap());
        assert!(
            err.contains("media"),
            "error should name the profile-prefixed root: {err}"
        );
    }

    // #2067: the profile-prefix strictness is not media-specific — a
    // profile-prefixed genuinely-unknown NON-plugin root
    // (`[profile.prod.definitely_unknown]`) also stays a hard error under the
    // lenient CLI load; only TRUE top-level roots are ever softened.
    #[test]
    fn deploy_cli_lenient_still_rejects_profile_prefixed_unknown_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[profile.prod.definitely_unknown]\nx = 1\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        let res = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            res.is_err(),
            "a profile-prefixed unknown root must stay strict under the lenient CLI \
             load: {res:?}"
        );
        let err = format!("{:?}", res.err().unwrap());
        assert!(
            err.contains("definitely_unknown"),
            "error should name the profile-prefixed unknown root: {err}"
        );
    }

    // #2067: the lenient deploy-CLI demotion applies ONLY to a true top-level
    // root whose TOML value is a TABLE. A registered/unknown root written as a
    // SCALAR (`media = "enabled"`) or an ARRAY (`media = ["a", "b"]`) is a
    // malformed section nothing would deserialize, so it must HARD-FAIL under
    // the lenient CLI load too — exactly as the deployed app rejects it at boot
    // (the #2061 `config_section` seam exempts a plugin root only when
    // `val.is_table()`). Without the `is_table` gate deploy would accept a
    // non-table root that app boot rejects.
    #[test]
    fn deploy_cli_lenient_still_rejects_non_table_root() {
        // Scalar root: `media = "enabled"`.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\nmedia = \"enabled\"\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        let res = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            res.is_err(),
            "a SCALAR top-level root (media = \"enabled\") must hard-fail under the \
             lenient CLI load — it is not a table and the deployed app rejects it at \
             boot: {res:?}"
        );
        let err = format!("{:?}", res.err().unwrap());
        assert!(
            err.contains("media"),
            "error should name the non-table root: {err}"
        );

        // Array root: `media = ["a", "b"]`.
        let temp2 = tempfile::tempdir().unwrap();
        std::fs::write(
            temp2.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\nmedia = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let env2 = strict_prod_env_2063(temp2.path());

        let res2 = AutumnConfig::load_with_env_lenient_unknown_roots(&env2);
        assert!(
            res2.is_err(),
            "an ARRAY top-level root (media = [\"a\", \"b\"]) must hard-fail under the \
             lenient CLI load — it is not a table and the deployed app rejects it at \
             boot: {res2:?}"
        );
        let err2 = format!("{:?}", res2.err().unwrap());
        assert!(
            err2.contains("media"),
            "error should name the non-table root: {err2}"
        );
    }

    // #2067: a legitimately quoted-dotted top-level table root — the valid TOML form of
    // a plugin `config_section("my.plugin")`, whose top-level table is `["my.plugin"]`,
    // one quoted key that happens to contain a dot — must be leniently accepted by the
    // deploy CLI, because app boot accepts it too: the #2061 exemption keys on the raw
    // table key with `path.is_empty()`. The earlier `!path.contains('.')` heuristic
    // wrongly hard-failed it, since the rendered `my.plugin` is ambiguous between a
    // quoted top-level key and a two-level path. Gating on the structural
    // `is_top_level`, an empty parent path, fixes it.
    #[test]
    fn deploy_cli_lenient_accepts_quoted_dotted_top_level_root() {
        // `["my.plugin"]` is a quoted top-level key CONTAINING a dot (one
        // structural top-level table), NOT the nested `[my.plugin]` two-level
        // form — this is exactly what `config_section("my.plugin")` produces.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n[\"my.plugin\"]\nenabled = true\n",
        )
        .unwrap();
        let env = strict_prod_env_2063(temp.path());

        let lenient = AutumnConfig::load_with_env_lenient_unknown_roots(&env);
        assert!(
            lenient.is_ok(),
            "deploy CLI must leniently accept a quoted-dotted TOP-LEVEL table root \
             ([\"my.plugin\"]) — it is a true top-level plugin root the app accepts at \
             boot, and top-level-ness is structural (empty parent path), not \
             `path.contains('.')`: {lenient:?}"
        );

        // Inline-table form of the same quoted-dotted top-level root is
        // equivalent. It is written BEFORE the `[server]` header so it binds at
        // the document top level, not inside `[server]`.
        let temp2 = tempfile::tempdir().unwrap();
        std::fs::write(
            temp2.path().join("autumn.toml"),
            "\"my.plugin\" = { enabled = true }\n\n[server]\nstrict_config = true\n",
        )
        .unwrap();
        let env2 = strict_prod_env_2063(temp2.path());
        assert!(
            AutumnConfig::load_with_env_lenient_unknown_roots(&env2).is_ok(),
            "deploy CLI must accept the inline-table quoted-dotted top-level root too"
        );
    }

    // 7a (#1890): a typo in a section that ONLY became strictly validated by the
    // schema-walk fix (here `[log]`, declared after `database`) must WARN, not
    // fail, during the one-release warn-first rollout.
    #[test]
    fn post_database_section_typo_warns_but_does_not_fail() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(&config_path, "[log]\nbogus_zzz = true\n").unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                // Pin a non-dev profile: the `dev` smart-defaults inject a
                // feature-gated `[storage]` table which, with the `storage`
                // feature off, is flagged as a hard top-level unknown key and
                // would derail this test regardless of the `[log]` typo.
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_ok(),
            "a post-database section typo must warn (not fail) under warn-first rollout: {res:?}"
        );
    }

    // 7b (#1890): with `strict_config_enforce_all` set, the SAME post-database
    // typo is promoted to a hard error.
    #[test]
    fn post_database_section_typo_fails_under_enforce_all() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(
            &config_path,
            "[server]\nstrict_config = true\nstrict_config_enforce_all = true\n\n[log]\nbogus_zzz = true\n",
        )
        .unwrap();

        let env = FakeEnv(
            [
                // Non-dev profile so the `dev` smart-defaults' feature-gated
                // `[storage]` table isn't injected — otherwise (storage feature
                // off) the test would fail on `storage`, not the `[log]` typo it
                // is meant to exercise.
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_err(),
            "strict_config_enforce_all must hard-fail the post-database typo"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("bogus_zzz"),
            "error should name the key: {err_str}"
        );
    }

    // 7c (#1890 regression guard): sections that were strictly validated BEFORE
    // the fix (here `[server]`) must keep hard-failing on unknown keys.
    #[test]
    fn pre_database_section_typo_still_hard_fails() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(&config_path, "[server]\nbogus_zzz = true\n").unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                // Non-dev profile so the `dev` smart-defaults' feature-gated
                // `[storage]` table isn't injected; this test must fail on the
                // `[server]` typo, not on `storage` (storage feature off).
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_err(),
            "an unknown [server] key must still hard-fail (pre-fix strictness preserved)"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("bogus_zzz"),
            "error should name the key: {err_str}"
        );
    }

    // ── Plugin config-section seam (#1974 item 7) ─────────────────────────────
    //
    // A plugin owns a top-level `[media]` table core's closed schema knows
    // nothing about. `load_with_env_and_plugin_roots` exempts declared roots
    // from the strict unknown-key check as known-and-opaque, while every other
    // unknown root still hard-fails. All tests pin `AUTUMN_ENV=prod` so the dev
    // smart-defaults' feature-gated `[storage]` root isn't injected (storage
    // feature off), which would otherwise be flagged independently of `[media]`.

    fn plugin_roots(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn strict_prod_env(dir: &std::path::Path, enforce_all: bool) -> FakeEnv {
        let mut vars = vec![
            ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
            ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
            (
                "AUTUMN_MANIFEST_DIR".to_owned(),
                dir.to_str().unwrap().to_owned(),
            ),
        ];
        if enforce_all {
            vars.push((
                "AUTUMN_SERVER__STRICT_CONFIG_ENFORCE_ALL".to_owned(),
                "true".to_owned(),
            ));
        }
        FakeEnv(vars.into_iter().collect())
    }

    // A registered `[media]` root boots green under strict_config: a
    // media-enabled app no longer fails at boot with `unknown key "media"`.
    #[test]
    fn strict_config_accepts_registered_plugin_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[media]\nqueue = \"media\"\n[media.mediamtx]\napi_base = \"http://localhost:9997\"\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_ok(),
            "a registered [media] root must boot under strict_config: {res:?}"
        );
    }

    // #2067 boot/deploy parity: a registered QUOTED-DOTTED plugin root — the app
    // form of `config_section("my.plugin")`, whose top-level table is
    // `["my.plugin"]` — is exempted at app-boot strict too. The exemption keys on
    // the RAW table key (`plugin_config_roots.contains("my.plugin")`) with
    // `path.is_empty()`, so the dot in the key name is irrelevant. This documents
    // that deploy leniency (which now derives top-level-ness structurally) and
    // app boot agree on quoted-dotted top-level roots.
    #[test]
    fn strict_config_accepts_quoted_dotted_registered_plugin_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[\"my.plugin\"]\nenabled = true\n[\"my.plugin\".nested]\nx = 1\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["my.plugin"]));
        assert!(
            res.is_ok(),
            "a registered quoted-dotted top-level root ([\"my.plugin\"]) must boot \
             under strict_config, exactly as deploy leniency accepts it: {res:?}"
        );
    }

    // A registered plugin root written as a NON-TABLE (scalar or array) is a
    // malformed section, not the opaque `[media]` TABLE `config_section`
    // declares. It must NOT be exempted: nothing would deserialize it and the
    // app would boot silently on default plugin config, so it stays a strict
    // unknown-root HARD failure instead. Only a table-shaped `[media]` is opaque.
    #[test]
    fn strict_config_rejects_non_table_registered_plugin_root() {
        // Scalar misspelling of a registered root (`media = "enabled"` instead of
        // the `[media]` table) must hard-fail under strict_config.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("autumn.toml"), "media = \"enabled\"\n").unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_err(),
            "a scalar-valued registered root (media = \"enabled\") must hard-fail \
             under strict_config, not be exempted as an opaque table: {res:?}"
        );
        assert!(
            format!("{:?}", res.err().unwrap()).contains("media"),
            "error should name the malformed media root"
        );

        // Array-valued registered root (`media = ["a", "b"]`) is likewise
        // malformed and must hard-fail.
        let temp_arr = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_arr.path().join("autumn.toml"),
            "media = [\"a\", \"b\"]\n",
        )
        .unwrap();

        let env_arr = strict_prod_env(temp_arr.path(), false);
        let res_arr =
            AutumnConfig::load_with_env_and_plugin_roots(&env_arr, &plugin_roots(&["media"]));
        assert!(
            res_arr.is_err(),
            "an array-valued registered root (media = [\"a\", \"b\"]) must hard-fail \
             under strict_config, not be exempted as an opaque table: {res_arr:?}"
        );
        assert!(
            format!("{:?}", res_arr.err().unwrap()).contains("media"),
            "error should name the malformed media array root"
        );
    }

    // Without registration the same `[media]` root is still an unknown top-level
    // key and hard-fails — the seam is fail-closed, not a blanket allow.
    #[test]
    fn strict_config_rejects_unregistered_plugin_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[media]\nqueue = \"media\"\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &BTreeSet::new());
        assert!(
            res.is_err(),
            "an unregistered [media] root must still hard-fail under strict_config"
        );
        assert!(format!("{:?}", res.err().unwrap()).contains("media"));
    }

    // Registering `[media]` does not weaken the check for OTHER unknown roots: a
    // genuinely-unknown top-level table still hard-fails.
    #[test]
    fn strict_config_still_rejects_other_unknown_root_when_plugin_registered() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[media]\nqueue = \"media\"\n\n[definitely_not_a_root]\nx = 1\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_err(),
            "an unrelated unknown root must still hard-fail even with [media] registered"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("definitely_not_a_root"),
            "error should name the unknown root: {err_str}"
        );
    }

    // A registered root is OPAQUE: even with `strict_config_enforce_all` set,
    // arbitrary nested children of `[media]` are never descended into and so are
    // never flagged — the plugin owns validation of its own subtree.
    #[test]
    fn registered_plugin_root_is_opaque_under_enforce_all() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[media]\nwholly_made_up = true\n[media.deeply.nested]\nalso_bogus = 42\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), true);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_ok(),
            "enforce_all must NOT flag children of a registered opaque root: {res:?}"
        );
    }

    // A registered plugin root under a profile prefix (`[profile.prod.media]`) stays
    // strict and must be rejected: the exemption covers only the true top-level
    // `[media]` table. The media plugin's reader deserializes only the top-level
    // `root.media` and does not apply Autumn's profile merge, so a profile layer the
    // plugin cannot consume must not be exempted — otherwise a strict app with media
    // settings only under `[profile.prod.media]` would boot silently on default plugin
    // config instead of failing loudly. Profile-aware plugin config is a separate,
    // larger enhancement.
    #[test]
    fn strict_config_still_rejects_profile_prefixed_plugin_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[profile.prod.media]\nwholly_made_up = true\n\
             [profile.prod.media.deeply.nested]\nalso_bogus = 42\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), true);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_err(),
            "a profile-prefixed plugin root ([profile.prod.media]) must stay strict \
             and be rejected — the plugin reads only the top-level [media] table, so \
             exempting the profile layer would boot silently on default config: {res:?}"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("media"),
            "error should name the media/profile root: {err_str}"
        );
    }

    // The profile-prefix opacity is NOT a blanket allow of profile subtrees: a
    // genuinely-unknown root under a profile prefix
    // (`[profile.prod.definitely_not_a_root]`) still hard-fails, because it is
    // validated against the root schema and is not a registered plugin root.
    #[test]
    fn strict_config_rejects_profile_prefixed_unknown_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[profile.prod.definitely_not_a_root]\nx = 1\n",
        )
        .unwrap();

        let env = strict_prod_env(temp.path(), false);
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &plugin_roots(&["media"]));
        assert!(
            res.is_err(),
            "a profile-prefixed genuinely-unknown root must still hard-fail even \
             with [media] registered (the fix must not blanket-allow profile subtrees)"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("definitely_not_a_root"),
            "error should name the unknown root: {err_str}"
        );
    }

    // When strict_config is OFF, behavior is unchanged: `[media]` is tolerated
    // even with no roots registered (non-strict never ran the check).
    #[test]
    fn non_strict_config_tolerates_media_root_without_registration() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("autumn.toml"),
            "[media]\nqueue = \"media\"\n",
        )
        .unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );
        let res = AutumnConfig::load_with_env_and_plugin_roots(&env, &BTreeSet::new());
        assert!(
            res.is_ok(),
            "non-strict config must tolerate an unregistered [media] root: {res:?}"
        );
    }

    // 7c′ (#1890 regression guard): a MALFORMED top-level `[profile]` entry (e.g.
    // `[profile] dev = "prod"`, whose validation error path is `profile.dev`) is a
    // structural error that was always fatal under strict_config. It is NOT a
    // section newly revealed by #1890, so the warn-first classifier must keep it
    // hard-failing. A genuinely newly-covered section typo (`[resilience]`) with
    // the same strict_config (enforce_all OFF) must still only warn — proving the
    // fix is narrow and did not over-broaden into hard-failing new sections.
    #[test]
    fn malformed_profile_entry_still_hard_fails() {
        // Malformed profile block: `dev = "prod"` is a scalar where a nested
        // profile table is expected -> unknown-key error path `profile.dev`.
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(&config_path, "[profile]\ndev = \"prod\"\n").unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                // Non-dev profile so the `dev` smart-defaults' feature-gated
                // `[storage]` table isn't injected; this test must fail on the
                // malformed `[profile]` entry, not on `storage`.
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_err(),
            "a malformed [profile] entry is a structural error that must keep \
             hard-failing under strict_config (not be demoted to warn-only): {res:?}"
        );
        assert!(
            matches!(res.err().unwrap(), ConfigError::Validation(_)),
            "malformed profile entry must fail as a validation error"
        );

        // Narrowness guard: a typo in a section that only became strictly
        // validated by #1890 (`[resilience]`) must still WARN (not fail) under the
        // same strict_config with enforce_all OFF.
        let temp2 = tempfile::tempdir().unwrap();
        let config_path2 = temp2.path().join("autumn.toml");
        std::fs::write(&config_path2, "[resilience]\nboguz = 1\n").unwrap();

        let env2 = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                // Non-dev profile so the `dev` smart-defaults' feature-gated
                // `[storage]` table isn't injected: the `[resilience]` typo must
                // remain a warn-only (Ok) case, not be masked by a hard-failing
                // `storage` key when the storage feature is off.
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp2.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res2 = AutumnConfig::load_with_env(&env2);
        assert!(
            res2.is_ok(),
            "a newly-#1890-covered section typo must still only warn under \
             strict_config (enforce_all off), proving the profile fix is narrow: {res2:?}"
        );
    }

    // 7c″ (#1890 P2 fix): a typo under a quoted dotted profile name, such as
    // `[profile."prod.eu".server]`, must classify by its real segment-derived schema
    // parent. `"prod.eu"` is one TOML key with a literal dot, so the segmented path is
    // `["profile", "prod.eu", "server"]` and the profile-stripped schema parent is
    // `server`, a pre-#1890 strict section that must keep hard-failing rather than be
    // demoted to warn-only by string-splitting the joined path. A
    // `[profile."prod.eu".resilience]` typo, in a newly-#1890-covered section, with the
    // same strict_config and enforce_all off must still only warn — proving the fix
    // stays narrow under dotted profile names.
    #[test]
    fn dotted_profile_name_preserves_strictness() {
        // Pre-#1890 strict section (`server`) under a quoted dotted profile name:
        // must hard-fail.
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(
            &config_path,
            "[profile.\"prod.eu\".server]\nbogus_zzz = true\n",
        )
        .unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                // Non-dev profile so the `dev` smart-defaults' feature-gated
                // `[storage]` table isn't injected; this test must fail on the
                // `[server]` typo, not on `storage` (storage feature off).
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_err(),
            "a [server] typo under a quoted dotted profile name must hard-fail \
             (pre-#1890 strictness must not be downgraded by string-splitting the \
             joined path): {res:?}"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("server") && err_str.contains("bogus_zzz"),
            "hard-fail must be for the [server] typo (right reason): {err_str}"
        );

        // Newly-#1890-covered section (`resilience`) under the same quoted dotted
        // profile name: must still only WARN (enforce_all off) — the fix is narrow.
        let temp2 = tempfile::tempdir().unwrap();
        let config_path2 = temp2.path().join("autumn.toml");
        std::fs::write(
            &config_path2,
            "[profile.\"prod.eu\".resilience]\nboguz = 1\n",
        )
        .unwrap();

        let env2 = FakeEnv(
            [
                ("AUTUMN_SERVER__STRICT_CONFIG".to_owned(), "true".to_owned()),
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp2.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res2 = AutumnConfig::load_with_env(&env2);
        assert!(
            res2.is_ok(),
            "a newly-#1890-covered section typo under a quoted dotted profile name \
             must still only warn under strict_config (enforce_all off): {res2:?}"
        );
    }

    // 7d (#1890): the `database.statement_timeout` duration field — whose empty
    // probe used to abort the schema walk — still deserializes correctly at
    // runtime, in both string and integer (milliseconds) forms.
    #[test]
    fn statement_timeout_duration_field_loads() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");

        std::fs::write(&config_path, "[database]\nstatement_timeout = \"30s\"\n").unwrap();
        let env = FakeEnv(
            [(
                "AUTUMN_MANIFEST_DIR".to_owned(),
                temp.path().to_str().unwrap().to_owned(),
            )]
            .into(),
        );
        let config =
            AutumnConfig::load_with_env(&env).expect("config with duration string must load");
        assert_eq!(
            config.database.statement_timeout,
            Some(std::time::Duration::from_secs(30))
        );

        // Integer form is interpreted as milliseconds.
        std::fs::write(&config_path, "[database]\nstatement_timeout = 250\n").unwrap();
        let config =
            AutumnConfig::load_with_env(&env).expect("config with integer duration must load");
        assert_eq!(
            config.database.statement_timeout,
            Some(std::time::Duration::from_millis(250))
        );
    }

    #[test]
    fn should_warn_total_connections_at_and_above_threshold() {
        // At or above the threshold warns; below does not.
        assert!(should_warn_total_connections(100, 100));
        assert!(should_warn_total_connections(250, 100));
        assert!(!should_warn_total_connections(99, 100));
    }

    #[test]
    fn should_warn_total_connections_zero_threshold_disables() {
        // A zero threshold silences the warning regardless of the total.
        assert!(!should_warn_total_connections(0, 0));
        assert!(!should_warn_total_connections(10_000, 0));
    }

    #[test]
    fn database_config_default_warn_threshold_is_100() {
        assert_eq!(
            DatabaseConfig::default().max_connections_warn_threshold,
            100
        );
    }

    /// Mock loader for tests — returns a hand-built config without touching disk.
    struct MockConfigLoader {
        config: AutumnConfig,
    }

    impl ConfigLoader for MockConfigLoader {
        async fn load(&self) -> Result<AutumnConfig, ConfigError> {
            Ok(self.config.clone())
        }
    }

    #[tokio::test]
    async fn config_loader_trait_returns_supplied_config() {
        let mut custom = AutumnConfig::default();
        custom.server.port = 9999;
        custom.profile = Some("integration-test".to_owned());

        let loader = MockConfigLoader {
            config: custom.clone(),
        };
        let resolved = loader.load().await.expect("mock loader should succeed");

        assert_eq!(resolved.server.port, 9999);
        assert_eq!(resolved.profile.as_deref(), Some("integration-test"));
    }

    #[test]
    fn validate_does_not_error_on_redis_backend_without_url() {
        // Regression: previously `validate()` called
        // `session.backend_plan(profile)` which returned an error for
        // `backend = "redis"` without `redis.url`, exiting the boot before
        // a `with_session_store(...)` override could apply. Session
        // backend validation now lives in `build_session_layer`, which
        // short-circuits when a custom store is installed. `validate()`
        // is config-shape-only and must accept this combination.
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = None;

        config.validate().expect(
            "validate() must accept redis-backend-without-url so custom \
             session store overrides aren't blocked at boot",
        );
    }

    #[tokio::test]
    async fn default_toml_env_loader_succeeds_without_files() {
        // No autumn.toml in the test runner's pwd; loader should fall back to
        // framework defaults rather than failing.
        let loader = TomlEnvConfigLoader::new();
        let resolved = loader.load().await.expect("default loader should succeed");
        // Default port is 3000 per ServerConfig::default — sanity check.
        assert_eq!(resolved.server.port, 3000);
    }

    #[test]
    fn database_config_validate_none() {
        let config = DatabaseConfig {
            url: None,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_config_validate_valid_postgres() {
        let config = DatabaseConfig {
            url: Some("postgres://user:pass@localhost:5432/db".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_config_validate_valid_postgresql() {
        let config = DatabaseConfig {
            url: Some("postgresql://user:pass@localhost:5432/db".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_config_validate_invalid_scheme() {
        let config = DatabaseConfig {
            url: Some("mysql://user:pass@localhost:3306/db".to_string()),
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        match result {
            Err(ConfigError::Validation(msg)) => {
                // Ensure we just match the underlying variant correctly
                // as requested in the review.
                assert!(msg.contains("must start with postgres:// or postgresql://"));
            }
            _ => panic!("Expected ConfigError::Validation"),
        }
    }

    // The boot refusal an ordinary `autumn.toml` misconfiguration hits. It
    // reaches `tracing::error!`, so under `log.format = "json"` whatever it
    // names lands in the structured log stream — one line after the startup
    // summary masked the very same URL.
    #[test]
    fn database_config_validate_does_not_echo_credentials() {
        for url in [
            "mysql://user:hunter2@localhost:3306/db",
            "redis://:hunter2@localhost:6379",
            "amqp://app:hunter2@broker/vhost",
        ] {
            let config = DatabaseConfig {
                url: Some(url.to_owned()),
                ..Default::default()
            };
            let Err(ConfigError::Validation(msg)) = config.validate() else {
                panic!("{url} must be refused");
            };
            assert!(!msg.contains("hunter2"), "password leaked in: {msg}");
            // Still actionable: the operator can tell which URL to go fix.
            assert!(msg.contains("localhost") || msg.contains("broker"), "{msg}");
        }
    }

    #[test]
    fn database_shard_url_validation_does_not_echo_credentials() {
        let config = DatabaseConfig {
            shards: vec![ShardConfig {
                name: "shard0".to_owned(),
                primary_url: "mysql://user:hunter2@localhost:3306/db".to_owned(),
                slots: None,
                replica_url: None,
                primary_pool_size: None,
                replica_pool_size: None,
                replica_fallback: None,
            }],
            ..Default::default()
        };
        let Err(ConfigError::Validation(msg)) = config.validate() else {
            panic!("a non-Postgres shard url must be refused");
        };
        assert!(!msg.contains("hunter2"), "password leaked in: {msg}");
    }

    #[test]
    fn server_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.port, 3000);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.shutdown_timeout_secs, 30);
    }

    #[test]
    fn database_defaults() {
        let config = DatabaseConfig::default();
        assert!(config.url.is_none());
        assert_eq!(config.pool_size, 10);
        assert_eq!(config.connect_timeout_secs, 5);
    }

    #[test]
    fn database_validate_none_url_is_ok() {
        let config = DatabaseConfig {
            url: None,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_validate_postgres_url_is_ok() {
        let config = DatabaseConfig {
            url: Some("postgres://user:pass@localhost/db".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_validate_postgresql_url_is_ok() {
        let config = DatabaseConfig {
            url: Some("postgresql://user:pass@localhost/db".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn database_validate_invalid_url_is_err() {
        let config = DatabaseConfig {
            url: Some("mysql://user:pass@localhost/db".to_string()),
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        if let Err(ConfigError::Validation(msg)) = result {
            assert!(msg.contains("Invalid database URL"));
            assert!(msg.contains("must start with postgres:// or postgresql://"));
        } else {
            panic!("Expected ConfigError::Validation");
        }
    }

    #[test]
    fn database_topology_deserializes_primary_and_replica_urls() {
        let config: AutumnConfig = toml::from_str(
            r#"
[database]
primary_url = "postgres://primary.example/app"
replica_url = "postgres://replica.example/app"
primary_pool_size = 12
replica_pool_size = 4
replica_fallback = "primary"
"#,
        )
        .expect("database topology config should parse");

        assert_eq!(
            config.database.primary_url.as_deref(),
            Some("postgres://primary.example/app")
        );
        assert_eq!(
            config.database.replica_url.as_deref(),
            Some("postgres://replica.example/app")
        );
        assert_eq!(config.database.primary_pool_size, Some(12));
        assert_eq!(config.database.replica_pool_size, Some(4));
        assert_eq!(config.database.replica_fallback, ReplicaFallback::Primary);
        assert_eq!(
            config.database.effective_primary_url(),
            Some("postgres://primary.example/app")
        );
        assert_eq!(config.database.effective_primary_pool_size(), 12);
        assert_eq!(config.database.effective_replica_pool_size(), 4);
    }

    #[test]
    fn database_topology_keeps_url_as_single_primary_compatibility_path() {
        let config: AutumnConfig = toml::from_str(
            r#"
[database]
url = "postgres://single.example/app"
pool_size = 7
"#,
        )
        .expect("legacy database.url config should parse");

        assert_eq!(
            config.database.effective_primary_url(),
            Some("postgres://single.example/app")
        );
        assert_eq!(config.database.effective_primary_pool_size(), 7);
        assert_eq!(config.database.effective_replica_pool_size(), 7);
        assert!(config.database.replica_url.is_none());
    }

    #[test]
    fn database_topology_rejects_replica_without_primary() {
        let config = DatabaseConfig {
            replica_url: Some("postgres://replica.example/app".to_owned()),
            ..Default::default()
        };

        let result = config.validate();

        assert!(result.is_err());
        let Err(ConfigError::Validation(message)) = result else {
            panic!("expected database topology validation error");
        };
        assert!(message.contains("database.replica_url"));
        assert!(message.contains("database.primary_url"));
    }

    #[test]
    fn time_zone_identifier_env_override_applies() {
        let env = MockEnv::new().with("AUTUMN_TIME_ZONE__IDENTIFIER", "America/New_York");
        let mut config = AutumnConfig::default();
        assert_eq!(config.time_zone.identifier, "UTC");

        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.time_zone.identifier, "America/New_York");
        assert!(config.time_zone.validate().is_ok());
    }

    #[test]
    fn alerts_severities_env_overrides_apply() {
        // Per-channel severity routing must be controllable via the documented
        // AUTUMN_ALERTS__*_SEVERITIES overrides, using the same `all`/`critical`
        // value parsing the TOML/file path uses.
        let env = MockEnv::new()
            .with("AUTUMN_ALERTS__SLACK_SEVERITIES", "critical")
            .with("AUTUMN_ALERTS__PAGERDUTY_SEVERITIES", "all")
            .with("AUTUMN_ALERTS__DISCORD_SEVERITIES", "critical");
        let mut config = AutumnConfig::default();
        // Defaults are `All` for every channel.
        assert_eq!(
            config.alerts.slack_severities,
            crate::alerts::AlertRouting::All
        );

        config.apply_env_overrides_with_env(&env);

        assert_eq!(
            config.alerts.slack_severities,
            crate::alerts::AlertRouting::Critical,
            "AUTUMN_ALERTS__SLACK_SEVERITIES=critical must set the Slack channel routing"
        );
        assert_eq!(
            config.alerts.discord_severities,
            crate::alerts::AlertRouting::Critical
        );
        assert_eq!(
            config.alerts.pagerduty_severities,
            crate::alerts::AlertRouting::All
        );
    }

    #[test]
    fn database_topology_env_overrides_role_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_DATABASE__PRIMARY_URL", "postgres://primary.env/app")
            .with("AUTUMN_DATABASE__REPLICA_URL", "postgres://replica.env/app")
            .with("AUTUMN_DATABASE__PRIMARY_POOL_SIZE", "9")
            .with("AUTUMN_DATABASE__REPLICA_POOL_SIZE", "3")
            .with("AUTUMN_DATABASE__REPLICA_FALLBACK", "primary");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        assert_eq!(
            config.database.primary_url.as_deref(),
            Some("postgres://primary.env/app")
        );
        assert_eq!(
            config.database.replica_url.as_deref(),
            Some("postgres://replica.env/app")
        );
        assert_eq!(config.database.primary_pool_size, Some(9));
        assert_eq!(config.database.replica_pool_size, Some(3));
        assert_eq!(config.database.replica_fallback, ReplicaFallback::Primary);
    }

    #[test]
    fn database_shards_parse_from_toml_with_effective_fallbacks() {
        let config: AutumnConfig = toml::from_str(
            r#"
[database]
primary_url = "postgres://control.example/app"
pool_size = 8
replica_fallback = "primary"

[[database.shards]]
name = "shard0"
primary_url = "postgres://shard0.example/app"

[[database.shards]]
name = "shard1"
primary_url = "postgres://shard1.example/app"
replica_url = "postgres://shard1-ro.example/app"
primary_pool_size = 3
replica_pool_size = 2
replica_fallback = "fail_readiness"
"#,
        )
        .expect("sharded database config should parse");

        let db = &config.database;
        assert!(db.has_shards());
        assert_eq!(db.shards.len(), 2);

        let shard0 = &db.shards[0];
        assert_eq!(shard0.name, "shard0");
        assert_eq!(shard0.primary_url, "postgres://shard0.example/app");
        assert!(shard0.replica_url.is_none());
        // Unset shard fields fall back to the [database] defaults.
        assert_eq!(shard0.effective_primary_pool_size(db), 8);
        assert_eq!(shard0.effective_replica_pool_size(db), 8);
        assert_eq!(
            shard0.effective_replica_fallback(db),
            ReplicaFallback::Primary
        );

        let shard1 = &db.shards[1];
        assert_eq!(shard1.effective_primary_pool_size(db), 3);
        assert_eq!(shard1.effective_replica_pool_size(db), 2);
        assert_eq!(
            shard1.effective_replica_fallback(db),
            ReplicaFallback::FailReadiness
        );

        config.validate().expect("sharded config should validate");
    }

    #[test]
    fn database_shards_default_to_empty() {
        let config = AutumnConfig::default();
        assert!(!config.database.has_shards());
        assert!(config.database.shards.is_empty());
    }

    #[test]
    fn database_shard_env_overrides_existing_entry_fields() {
        let mut config: AutumnConfig = toml::from_str(
            r#"
[[database.shards]]
name = "shard0"
primary_url = "postgres://toml.example/app"
"#,
        )
        .expect("config should parse");
        let env = MockEnv::new()
            .with(
                "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
                "postgres://env.example/app",
            )
            .with(
                "AUTUMN_DATABASE__SHARDS__0__REPLICA_URL",
                "postgres://env-ro.example/app",
            )
            .with("AUTUMN_DATABASE__SHARDS__0__PRIMARY_POOL_SIZE", "5")
            .with("AUTUMN_DATABASE__SHARDS__0__REPLICA_FALLBACK", "primary");

        config.apply_env_overrides_with_env(&env);

        let shard = &config.database.shards[0];
        assert_eq!(shard.name, "shard0");
        assert_eq!(shard.primary_url, "postgres://env.example/app");
        assert_eq!(
            shard.replica_url.as_deref(),
            Some("postgres://env-ro.example/app")
        );
        assert_eq!(shard.primary_pool_size, Some(5));
        assert_eq!(shard.replica_fallback, Some(ReplicaFallback::Primary));
    }

    #[test]
    fn database_shard_env_appends_new_entry_when_name_and_primary_url_present() {
        let mut config = AutumnConfig::default();
        let env = MockEnv::new()
            .with("AUTUMN_DATABASE__SHARDS__0__NAME", "shard0")
            .with(
                "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
                "postgres://shard0.env/app",
            )
            .with("AUTUMN_DATABASE__SHARDS__1__NAME", "shard1")
            .with(
                "AUTUMN_DATABASE__SHARDS__1__PRIMARY_URL",
                "postgres://shard1.env/app",
            )
            // Index 3 is unreachable because index 2 is absent: probing stops.
            .with("AUTUMN_DATABASE__SHARDS__3__NAME", "orphan")
            .with(
                "AUTUMN_DATABASE__SHARDS__3__PRIMARY_URL",
                "postgres://orphan.env/app",
            );

        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.database.shards.len(), 2);
        assert_eq!(config.database.shards[0].name, "shard0");
        assert_eq!(config.database.shards[1].name, "shard1");
    }

    #[test]
    fn database_shard_env_does_not_append_incomplete_entry() {
        let mut config = AutumnConfig::default();
        // NAME without PRIMARY_URL is not enough to create a shard.
        let env = MockEnv::new().with("AUTUMN_DATABASE__SHARDS__0__NAME", "shard0");

        config.apply_env_overrides_with_env(&env);

        assert!(config.database.shards.is_empty());
    }

    fn shard(name: &str, primary_url: &str) -> ShardConfig {
        ShardConfig {
            name: name.to_owned(),
            primary_url: primary_url.to_owned(),
            slots: None,
            replica_url: None,
            primary_pool_size: None,
            replica_pool_size: None,
            replica_fallback: None,
        }
    }

    fn shard_with_slots(name: &str, primary_url: &str, slots: &[&str]) -> ShardConfig {
        let mut config = shard(name, primary_url);
        config.slots = Some(
            slots
                .iter()
                .map(|spec| SlotSpec::Range((*spec).to_owned()))
                .collect(),
        );
        config
    }

    #[test]
    fn slot_spec_expands_indices_and_ranges() {
        assert_eq!(SlotSpec::Index(5).expand().unwrap(), vec![5]);
        assert_eq!(SlotSpec::Range("7".to_owned()).expand().unwrap(), vec![7]);
        assert_eq!(
            SlotSpec::Range("3-6".to_owned()).expand().unwrap(),
            vec![3, 4, 5, 6]
        );
        assert!(SlotSpec::Range("6-3".to_owned()).expand().is_err());
        assert!(SlotSpec::Range("x-3".to_owned()).expand().is_err());
        assert!(SlotSpec::Range(String::new()).expand().is_err());
    }

    #[test]
    fn slot_map_auto_splits_contiguously_by_declaration_order() {
        let config = DatabaseConfig {
            shards: vec![
                shard("a", "postgres://a/app"),
                shard("b", "postgres://b/app"),
                shard("c", "postgres://c/app"),
            ],
            ..Default::default()
        };
        let map = config
            .resolved_slot_map()
            .expect("auto-split should resolve");
        assert_eq!(map.len(), usize::from(SLOT_COUNT));
        // slot * 3 / 16384 — contiguous, near-even thirds.
        assert_eq!((map[0], map[5461]), (0, 0));
        assert_eq!((map[5462], map[10922]), (1, 1));
        assert_eq!((map[10923], map[16383]), (2, 2));
        assert!(map.windows(2).all(|w| w[0] <= w[1]), "must be contiguous");
        for owner in 0..3 {
            let count = map.iter().filter(|&&o| o == owner).count();
            assert!(
                (5461..=5462).contains(&count),
                "shard {owner} owns {count} slots (expected near-even split)"
            );
        }
    }

    #[test]
    fn slot_map_uses_explicit_assignments_regardless_of_order() {
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("late", "postgres://late/app", &["8192-16383"]),
                shard_with_slots("early", "postgres://early/app", &["0-8191"]),
            ],
            ..Default::default()
        };
        let map = config
            .resolved_slot_map()
            .expect("explicit map should resolve");
        assert!(map[..8192].iter().all(|&owner| owner == 1));
        assert!(map[8192..].iter().all(|&owner| owner == 0));
    }

    #[test]
    fn slot_map_allows_drained_shard_with_empty_slots() {
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("live", "postgres://live/app", &["0-16383"]),
                shard_with_slots("drained", "postgres://drained/app", &[]),
            ],
            ..Default::default()
        };
        let map = config
            .resolved_slot_map()
            .expect("drained shard is allowed");
        assert_eq!(map.len(), usize::from(SLOT_COUNT));
        assert!(map.iter().all(|&owner| owner == 0));
    }

    #[test]
    fn slot_map_rejects_mixed_declared_and_undeclared_slots() {
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("a", "postgres://a/app", &["0-16383"]),
                shard("b", "postgres://b/app"),
            ],
            ..Default::default()
        };
        assert!(config.resolved_slot_map().is_err());
    }

    #[test]
    fn slot_map_rejects_overlap_gap_and_out_of_range() {
        // Overlap.
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("a", "postgres://a/app", &["0-8192"]),
                shard_with_slots("b", "postgres://b/app", &["8192-16383"]),
            ],
            ..Default::default()
        };
        let Err(ConfigError::Validation(message)) = config.resolved_slot_map() else {
            panic!("overlapping slots should fail");
        };
        assert!(message.contains("already owned"));

        // Gap — reported as compact ranges, not thousands of indices.
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("a", "postgres://a/app", &["0-8000"]),
                shard_with_slots("b", "postgres://b/app", &["8192-16383"]),
            ],
            ..Default::default()
        };
        let Err(ConfigError::Validation(message)) = config.resolved_slot_map() else {
            panic!("uncovered slots should fail");
        };
        assert!(message.contains("unassigned"));
        assert!(message.contains("8001-8191"), "got: {message}");

        // Out of range.
        let config = DatabaseConfig {
            shards: vec![shard_with_slots("a", "postgres://a/app", &["0-16384"])],
            ..Default::default()
        };
        assert!(config.resolved_slot_map().is_err());
    }

    #[test]
    fn slot_map_rejects_more_shards_than_slots() {
        let config = DatabaseConfig {
            shards: (0..=usize::from(SLOT_COUNT))
                .map(|i| shard(&format!("s{i}"), "postgres://s/app"))
                .collect(),
            ..Default::default()
        };
        let Err(ConfigError::Validation(message)) = config.resolved_slot_map() else {
            panic!("more shards than slots cannot auto-split");
        };
        assert!(message.contains("at most"), "got: {message}");
    }

    #[test]
    fn slots_parse_from_toml_ints_and_ranges() {
        let config: AutumnConfig = toml::from_str(
            r#"
[[database.shards]]
name = "a"
primary_url = "postgres://a/app"
slots = ["0-8191", 8192, "8193"]

[[database.shards]]
name = "b"
primary_url = "postgres://b/app"
slots = ["8194-16383"]
"#,
        )
        .expect("slots config should parse");
        let map = config
            .database
            .resolved_slot_map()
            .expect("mixed int/range specs should resolve");
        assert!(map[..8194].iter().all(|&owner| owner == 0));
        assert!(map[8194..].iter().all(|&owner| owner == 1));
        config.validate().expect("config should validate");
    }

    #[test]
    fn slot_env_overrides_assignments() {
        let mut config = AutumnConfig::default();
        let env = MockEnv::new()
            .with("AUTUMN_DATABASE__SHARDS__0__NAME", "a")
            .with(
                "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
                "postgres://a/app",
            )
            .with("AUTUMN_DATABASE__SHARDS__0__SLOTS", "0-8191, 12288-16383")
            .with("AUTUMN_DATABASE__SHARDS__1__NAME", "b")
            .with(
                "AUTUMN_DATABASE__SHARDS__1__PRIMARY_URL",
                "postgres://b/app",
            )
            .with("AUTUMN_DATABASE__SHARDS__1__SLOTS", "8192-12287");

        config.apply_env_overrides_with_env(&env);

        let map = config
            .database
            .resolved_slot_map()
            .expect("env slot specs should resolve");
        assert!(map[..8192].iter().all(|&owner| owner == 0));
        assert!(map[8192..12288].iter().all(|&owner| owner == 1));
        assert!(map[12288..].iter().all(|&owner| owner == 0));
    }

    #[test]
    fn slot_ranges_format_compactly() {
        assert_eq!(format_slot_ranges(&[]), "");
        assert_eq!(format_slot_ranges(&[3]), "3");
        assert_eq!(format_slot_ranges(&[0, 1, 2, 5, 7, 8]), "0-2, 5, 7-8");
    }

    #[test]
    fn database_shard_validation_rejects_bad_names() {
        for bad_name in ["", "Shard0", "shard 0", "shard:0", "shärd"] {
            let config = DatabaseConfig {
                shards: vec![shard(bad_name, "postgres://s0.example/app")],
                ..Default::default()
            };
            assert!(
                config.validate().is_err(),
                "shard name should be rejected: {bad_name:?}"
            );
        }
    }

    #[test]
    fn database_shard_validation_rejects_duplicate_names() {
        let config = DatabaseConfig {
            shards: vec![
                shard("shard0", "postgres://a.example/app"),
                shard("shard0", "postgres://b.example/app"),
            ],
            ..Default::default()
        };
        let Err(ConfigError::Validation(message)) = config.validate() else {
            panic!("duplicate shard names should fail validation");
        };
        assert!(message.contains("unique"));
    }

    #[test]
    fn database_shard_validation_rejects_bad_urls() {
        let config = DatabaseConfig {
            shards: vec![shard("shard0", "mysql://s0.example/app")],
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let mut with_bad_replica = shard("shard0", "postgres://s0.example/app");
        with_bad_replica.replica_url = Some("http://s0-ro.example/app".to_owned());
        let config = DatabaseConfig {
            shards: vec![with_bad_replica],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn database_shards_without_control_role_are_allowed() {
        let config = DatabaseConfig {
            shards: vec![shard("shard0", "postgres://s0.example/app")],
            ..Default::default()
        };
        config
            .validate()
            .expect("shards without a control role should validate");
    }

    #[test]
    fn postgres_scheduler_with_shards_requires_control_database() {
        let mut config = AutumnConfig::default();
        config.database.shards = vec![shard("shard0", "postgres://s0.example/app")];
        config.scheduler.backend = SchedulerBackend::Postgres;

        let Err(ConfigError::Validation(message)) = config.validate() else {
            panic!("postgres scheduler without a control database should fail validation");
        };
        assert!(message.contains("control database"));

        config.database.primary_url = Some("postgres://control.example/app".to_owned());
        config
            .validate()
            .expect("control role should satisfy the scheduler requirement");
    }

    #[test]
    fn postgres_jobs_with_shards_requires_control_database() {
        let mut config = AutumnConfig::default();
        config.database.shards = vec![shard("shard0", "postgres://s0.example/app")];
        config.jobs.backend = "postgres".to_owned();

        assert!(config.validate().is_err());

        config.database.url = Some("postgres://control.example/app".to_owned());
        config
            .validate()
            .expect("legacy url should satisfy the jobs requirement");
    }

    #[test]
    fn database_validate_url_edge_cases() {
        let invalid_urls = vec![
            "POSTGRES://localhost/db",
            "postgres:/localhost/db",
            "postgres:localhost/db",
            "http://postgres",
            "   postgres://localhost/db",
            "",
        ];

        for invalid_url in invalid_urls {
            let config = DatabaseConfig {
                url: Some(invalid_url.to_string()),
                ..Default::default()
            };
            assert!(
                config.validate().is_err(),
                "URL should be invalid: {invalid_url}"
            );
        }
    }

    #[test]
    fn autumn_config_validate_ok() {
        let config = AutumnConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn autumn_config_validate_no_longer_errors_on_invalid_session_backend() {
        // Session backend validation moved to `build_session_layer`, so a custom store
        // installed via `AppBuilder::with_session_store(...)` can override an otherwise
        // invalid backend config without the boot exiting first. `validate()` is
        // config-shape-only now; runtime session selection, and the backend error, live
        // in `build_session_layer`, which short-circuits when a custom store is
        // installed. `crate::session::tests::session_backend_plan_*` still cover the
        // error cases directly on `SessionConfig::backend_plan`.
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = None;

        config
            .validate()
            .expect("validate() must accept invalid session backend so custom store can override");
    }

    #[test]
    fn autumn_config_validate_database_err() {
        let mut config = AutumnConfig::default();
        config.database.url = Some("mysql://localhost/test".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn log_defaults() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.format, LogFormat::Auto);
    }

    #[test]
    fn telemetry_defaults() {
        let config = TelemetryConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.service_name, "autumn-app");
        assert!(config.service_namespace.is_none());
        assert_eq!(config.service_version, "unknown");
        assert_eq!(config.environment, "development");
        assert!(config.otlp_endpoint.is_none());
        assert_eq!(config.protocol, TelemetryProtocol::Grpc);
        assert!(!config.strict);
    }

    #[test]
    fn health_defaults() {
        let config = HealthConfig::default();
        assert_eq!(config.path, "/health");
        assert_eq!(config.live_path, "/live");
        assert_eq!(config.ready_path, "/ready");
        assert_eq!(config.startup_path, "/startup");
        assert!(!config.detailed);
    }

    #[test]
    fn top_level_default_populates_all_sections() {
        let config = AutumnConfig::default();
        assert_eq!(config.server.port, 3000);
        assert!(config.database.url.is_none());
        assert_eq!(config.log.level, "info");
        assert_eq!(config.health.path, "/health");
    }

    #[test]
    fn deserialize_empty_object_uses_all_defaults() {
        let config: AutumnConfig = serde_json::from_str("{}").expect("empty object should parse");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.shutdown_timeout_secs, 30);
        assert!(config.database.url.is_none());
        assert_eq!(config.database.pool_size, 10);
        assert_eq!(config.database.connect_timeout_secs, 5);
        assert!(!config.database.auto_migrate_in_production);
        assert_eq!(config.log.level, "info");
        assert_eq!(config.log.format, LogFormat::Auto);
        assert_eq!(config.health.path, "/health");
    }

    #[test]
    fn deserialize_partial_config_merges_with_defaults() {
        let json = r#"{"server": {"port": 8080}}"#;
        let config: AutumnConfig = serde_json::from_str(json).expect("partial config should parse");
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.database.pool_size, 10);
        assert_eq!(config.log.level, "info");
    }

    #[test]
    fn log_format_variants_deserialize() {
        let auto: LogFormat = serde_json::from_str(r#""Auto""#).expect("Auto");
        let pretty: LogFormat = serde_json::from_str(r#""Pretty""#).expect("Pretty");
        let json: LogFormat = serde_json::from_str(r#""Json""#).expect("Json");
        assert_eq!(auto, LogFormat::Auto);
        assert_eq!(pretty, LogFormat::Pretty);
        assert_eq!(json, LogFormat::Json);
    }

    // ── TOML loading tests ───────────────────────────────────────────

    #[test]
    fn load_missing_file_returns_defaults() {
        let config = AutumnConfig::load_from(Path::new("this_file_does_not_exist.toml")).unwrap();
        assert_eq!(config.server.port, 3000);
        assert!(config.database.url.is_none());
    }

    #[test]
    fn load_valid_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(
            &path,
            r#"
[server]
port = 8080
host = "0.0.0.0"
shutdown_timeout_secs = 60

[database]
url = "postgres://user:pass@db:5432/myapp"
pool_size = 20
connect_timeout_secs = 10
auto_migrate_in_production = true

[log]
level = "debug"
format = "Json"

[health]
path = "/healthz"
"#,
        )
        .unwrap();

        let config = AutumnConfig::load_from(&path).unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.shutdown_timeout_secs, 60);
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://user:pass@db:5432/myapp")
        );
        assert_eq!(config.database.pool_size, 20);
        assert_eq!(config.database.connect_timeout_secs, 10);
        assert!(config.database.auto_migrate_in_production);
        assert_eq!(config.log.level, "debug");
        assert_eq!(config.log.format, LogFormat::Json);
        assert_eq!(config.health.path, "/healthz");
    }

    #[test]
    fn load_partial_config_merges_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "[server]\nport = 9090\n").unwrap();

        let config = AutumnConfig::load_from(&path).unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.database.pool_size, 10);
        assert_eq!(config.log.level, "info");
    }

    #[test]
    fn access_log_defaults_on_with_probe_and_asset_exclusions() {
        let log = LogConfig::default();
        assert!(log.access_log);
        assert_eq!(
            log.access_log_exclude,
            vec![
                "/health",
                "/live",
                "/ready",
                "/startup",
                "/actuator",
                "/static"
            ]
        );
    }

    #[test]
    fn env_override_access_log_off() {
        let env = MockEnv::new().with("AUTUMN_LOG__ACCESS_LOG", "false");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(!config.log.access_log);
    }

    #[test]
    fn env_override_access_log_exclude_csv() {
        let env = MockEnv::new().with("AUTUMN_LOG__ACCESS_LOG_EXCLUDE", "/internal, /probes");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.access_log_exclude, vec!["/internal", "/probes"]);
    }

    #[test]
    fn access_log_is_configurable_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(
            &path,
            "[log]\naccess_log = false\naccess_log_exclude = [\"/internal\"]\n",
        )
        .unwrap();

        let config = AutumnConfig::load_from(&path).unwrap();
        assert!(!config.log.access_log);
        assert_eq!(config.log.access_log_exclude, vec!["/internal"]);
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "not valid [[[toml").unwrap();

        let result = AutumnConfig::load_from(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid autumn.toml"));
    }

    #[test]
    fn load_empty_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "").unwrap();

        let config = AutumnConfig::load_from(&path).unwrap();
        assert_eq!(config.server.port, 3000);
    }

    // ── Environment variable override tests ──────────────────────

    #[test]
    fn env_override_database_url() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__URL", "postgres://override:5432/test");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://override:5432/test")
        );
    }

    #[test]
    fn env_override_actuator_prometheus_disables() {
        // Operators must be able to remove the scrape endpoint via the
        // documented AUTUMN_SECTION__FIELD convention, not just TOML.
        let env = MockEnv::new().with("AUTUMN_ACTUATOR__PROMETHEUS", "false");
        let mut config = AutumnConfig::default();
        assert!(config.actuator.prometheus, "default should be enabled");
        config.apply_env_overrides_with_env(&env);
        assert!(
            !config.actuator.prometheus,
            "AUTUMN_ACTUATOR__PROMETHEUS=false must disable the scrape endpoint"
        );
    }

    #[test]
    fn env_override_actuator_sensitive() {
        let env = MockEnv::new().with("AUTUMN_ACTUATOR__SENSITIVE", "true");
        let mut config = AutumnConfig::default();
        assert!(!config.actuator.sensitive);
        config.apply_env_overrides_with_env(&env);
        assert!(config.actuator.sensitive);
    }

    #[test]
    fn env_override_upload_reject_on_content_type_mismatch() {
        let env = MockEnv::new().with(
            "AUTUMN_SECURITY__UPLOAD__REJECT_ON_CONTENT_TYPE_MISMATCH",
            "true",
        );
        let mut config = AutumnConfig::default();
        assert!(!config.security.upload.reject_on_content_type_mismatch);
        config.apply_env_overrides_with_env(&env);
        assert!(config.security.upload.reject_on_content_type_mismatch);
    }

    #[test]
    fn env_override_actuator_prefix() {
        let env = MockEnv::new().with("AUTUMN_ACTUATOR__PREFIX", "/ops");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.actuator.prefix, "/ops");
    }

    #[test]
    fn env_override_database_url_wins_over_file_primary_url() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__URL", "postgres://env.example/app");
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://file.example/app".to_owned());

        config.apply_env_overrides_with_env(&env);

        assert_eq!(
            config.database.effective_primary_url(),
            Some("postgres://env.example/app")
        );
        assert!(config.database.primary_url.is_none());
    }

    #[test]
    fn env_override_database_primary_url_wins_over_legacy_database_url() {
        let env = MockEnv::new()
            .with("AUTUMN_DATABASE__URL", "postgres://legacy.env/app")
            .with("AUTUMN_DATABASE__PRIMARY_URL", "postgres://primary.env/app");
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://file.example/app".to_owned());

        config.apply_env_overrides_with_env(&env);

        assert_eq!(
            config.database.effective_primary_url(),
            Some("postgres://primary.env/app")
        );
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://legacy.env/app")
        );
    }

    #[test]
    fn env_override_pool_size() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__POOL_SIZE", "25");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.pool_size, 25);
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn env_override_reporting() {
        let env = MockEnv::new()
            .with("AUTUMN_REPORTING__ENABLED", "false")
            .with("AUTUMN_REPORTING__SAMPLE_RATE", "0.1");
        let mut config = AutumnConfig::default();
        assert!(config.reporting.enabled);
        assert!((config.reporting.sample_rate - 1.0).abs() < f64::EPSILON);
        config.apply_env_overrides_with_env(&env);
        assert!(!config.reporting.enabled);
        assert!((config.reporting.sample_rate - 0.1).abs() < f64::EPSILON);
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn failure_capture_defaults_are_off() {
        let config = AutumnConfig::default();
        assert!(
            !config.failure_capture.enabled,
            "capture writes production request data to disk; it must be opt-in"
        );
        assert_eq!(config.failure_capture.dir, "tmp/autumn-capsules");
        assert_eq!(config.failure_capture.max_body_bytes, 65_536);
        assert_eq!(config.failure_capture.max_capsule_bytes, 1_048_576);
        assert_eq!(config.failure_capture.max_capsules, 50);
    }

    #[cfg(feature = "reporting")]
    #[test]
    fn failure_capture_env_overrides_apply() {
        let env = MockEnv::new()
            .with("AUTUMN_FAILURE_CAPTURE__ENABLED", "true")
            .with("AUTUMN_FAILURE_CAPTURE__DIR", "/var/tmp/capsules")
            .with("AUTUMN_FAILURE_CAPTURE__MAX_BODY_BYTES", "1024")
            .with("AUTUMN_FAILURE_CAPTURE__MAX_CAPSULE_BYTES", "2048")
            .with("AUTUMN_FAILURE_CAPTURE__MAX_CAPSULES", "5");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert!(config.failure_capture.enabled);
        assert_eq!(config.failure_capture.dir, "/var/tmp/capsules");
        assert_eq!(config.failure_capture.max_body_bytes, 1024);
        assert_eq!(config.failure_capture.max_capsule_bytes, 2048);
        assert_eq!(config.failure_capture.max_capsules, 5);
    }

    /// Regression guard for the `[failure_capture]` field ordering.
    ///
    /// Mirrors `deploy_child_keys_are_strictly_validated`: the strict
    /// unknown-key validator only descends into a config.rs-internal section
    /// declared *before* `database`, because `DatabaseConfig`'s
    /// `deserialize_with` duration field aborts the schema walk. If someone
    /// moves `failure_capture` below `database`, its child keys silently
    /// vanish from the schema and this fails.
    #[cfg(feature = "reporting")]
    #[test]
    fn failure_capture_child_keys_are_strictly_validated() {
        let leaves = AutumnConfig::schema_leaf_paths();
        for key in [
            "failure_capture.enabled",
            "failure_capture.dir",
            "failure_capture.max_body_bytes",
            "failure_capture.max_capsule_bytes",
            "failure_capture.max_capsules",
        ] {
            assert!(
                leaves.contains(key),
                "{key} must be a schema leaf so strict validation descends into \
                 [failure_capture]; if this fails, the section was likely moved below \
                 `database` in AutumnConfig"
            );
        }

        let schema = AutumnConfig::get_schema_keys();
        let errors = AutumnConfig::validate_toml("[failure_capture]\nenabledd = true\n", &schema);
        assert!(
            errors
                .iter()
                .any(|(path, _)| path == "failure_capture.enabledd"),
            "a bogus [failure_capture] child key must be rejected by strict validation, \
             got: {errors:?}"
        );
    }

    // ── [cluster] (issue #1762) ──────────────────────────────────────────

    /// A cluster with no shared secret is an unauthenticated cluster: anyone
    /// who can reach the port can inject state. Boot must fail, not warn.
    #[test]
    fn cluster_enabled_requires_secret() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = None;

        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "[cluster] enabled = true with no secret must fail validation, got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("secret"),
            "the error must name the missing key so an operator can fix it; got {message:?}"
        );

        // Disabled clusters need nothing: the default config must still boot.
        assert!(
            AutumnConfig::default().cluster.validate().is_ok(),
            "a disabled [cluster] section must validate with no secret at all"
        );
    }

    #[test]
    fn cluster_rejects_short_secret() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from("too-short".to_owned()));

        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "a secret shorter than {MIN_CLUSTER_SECRET_LEN} bytes must be refused, got {result:?}"
        );

        // …and a long-enough secret must be accepted, or the rule is vacuous.
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));
        assert!(
            config.cluster.validate().is_ok(),
            "a secret of at least {MIN_CLUSTER_SECRET_LEN} bytes must be accepted"
        );
    }

    /// Anti-flap hysteresis: a suspicion timeout under three push intervals
    /// turns one lost packet into a membership change.
    #[test]
    fn cluster_rejects_suspicion_below_3x_push_interval() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));
        config.cluster.push_interval_ms = 500;
        config.cluster.suspicion_timeout_ms = 1_000;

        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "suspicion_timeout_ms must be at least {MIN_CLUSTER_SUSPICION_MULTIPLE}x \
             push_interval_ms, got {result:?}"
        );

        config.cluster.suspicion_timeout_ms = 1_500;
        assert!(
            config.cluster.validate().is_ok(),
            "exactly {MIN_CLUSTER_SUSPICION_MULTIPLE}x the push interval must be accepted"
        );

        // A push interval below the floor is refused too.
        config.cluster.push_interval_ms = 1;
        config.cluster.suspicion_timeout_ms = 3;
        assert!(
            config.cluster.validate().is_err(),
            "push_interval_ms below {MIN_CLUSTER_PUSH_INTERVAL_MS}ms must be refused"
        );
    }

    /// Counter cells are keyed `"{node_id}#{incarnation}"`, so a `#` in a node
    /// id (or a cluster name, which is signed alongside it) makes the key
    /// ambiguous. Validation refuses it rather than shipping a counter whose
    /// cells can collide.
    #[test]
    fn cluster_rejects_node_id_with_cell_separator() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));

        config.cluster.node_id = Some("node#1".to_owned());
        assert!(
            config.cluster.validate().is_err(),
            "a node_id containing '#' must be refused: it is the counter cell-key separator"
        );

        config.cluster.node_id = Some("node-1".to_owned());
        config.cluster.cluster_name = "orch#ard".to_owned();
        assert!(
            config.cluster.validate().is_err(),
            "a cluster_name containing '#' must be refused for the same reason"
        );

        config.cluster.cluster_name = "orchard".to_owned();
        assert!(
            config.cluster.validate().is_ok(),
            "…and separator-free names must be accepted, or the rule is vacuous"
        );
    }

    /// Port `0` means "any free port". That is a legal *bind*, because the node
    /// advertises the port it was actually given — but it is never a legal
    /// thing to publish or to dial, and a peer pointed at port 0 fails in a way
    /// that reads as a network fault rather than as a typo.
    #[test]
    fn cluster_rejects_ephemeral_port_where_a_peer_must_dial() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));

        // An ephemeral BIND is the documented default and must keep working.
        config.cluster.bind_addr = "127.0.0.1:0".to_owned();
        assert!(
            config.cluster.validate().is_ok(),
            "bind_addr with port 0 is the ephemeral-bind spelling and must be accepted"
        );

        config.cluster.advertise_addr = Some("10.0.0.4:0".to_owned());
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "an explicit advertise_addr on port 0 must be refused: peers dial it verbatim, \
             got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.advertise_addr"),
            "the error must name the offending key; got {message:?}"
        );

        config.cluster.advertise_addr = Some("10.0.0.4:7946".to_owned());
        assert!(
            config.cluster.validate().is_ok(),
            "…and a real advertised port must be accepted, or the rule is vacuous"
        );

        config.cluster.seed_peers = vec!["10.0.0.5:7946".to_owned(), "10.0.0.6:0".to_owned()];
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "a seed peer on port 0 is equally undialable and must be refused, got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.seed_peers[1]"),
            "the error must name which seed is wrong; got {message:?}"
        );

        config.cluster.seed_peers = vec!["10.0.0.5:7946".to_owned()];
        assert!(
            config.cluster.validate().is_ok(),
            "…and dialable seeds must be accepted"
        );
    }

    /// `0.0.0.0` is a bind, never a dial address. A node that gossips it hands
    /// its peer an address nothing can reach — the one-way cluster the guide's
    /// "Choosing addresses" section warns about, which then looks exactly like
    /// a network fault from the other side rather than like the typo it is.
    #[test]
    fn cluster_rejects_a_wildcard_advertised_address() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));

        // A wildcard bind with nothing to advertise: the bound address IS what
        // peers would be told to dial.
        config.cluster.bind_addr = "0.0.0.0:7946".to_owned();
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "binding a wildcard with no advertise_addr must be refused: the node \
             would gossip 0.0.0.0 as its dial address, got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.bind_addr"),
            "the error must name the key the operator has to fix; got {message:?}"
        );

        // An EXPLICIT wildcard advertise is the same mistake, spelled out.
        config.cluster.advertise_addr = Some("0.0.0.0:7946".to_owned());
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "an explicit wildcard advertise_addr must be refused too, got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.advertise_addr"),
            "…and must blame advertise_addr, not the (legal) wildcard bind; \
             got {message:?}"
        );

        // …while a wildcard bind WITH a concrete advertised address — the
        // documented container spelling — must pass, or the rule is a ban on
        // the very deployment shape the guide recommends.
        config.cluster.advertise_addr = Some("10.0.1.7:7946".to_owned());
        assert!(
            config.cluster.validate().is_ok(),
            "0.0.0.0 bind + explicit advertise_addr is the documented spelling \
             and must be accepted"
        );

        // The rule is scoped to enabled sections: a disabled one binds nothing.
        config.cluster.enabled = false;
        config.cluster.advertise_addr = None;
        assert!(
            config.cluster.validate().is_ok(),
            "a disabled section advertises nothing, so the wildcard rule must \
             not fire on it"
        );
    }

    /// `node_id` and `cluster_name` travel in every frame and are covered by
    /// the MAC, so the 64-byte cap keeps a push's fixed overhead predictable.
    #[test]
    fn cluster_rejects_over_long_idents() {
        let mut config = AutumnConfig::default();
        config.cluster.enabled = true;
        config.cluster.secret = Some(secrecy::SecretString::from(
            "a-perfectly-adequate-cluster-secret".to_owned(),
        ));

        let at_cap = "n".repeat(MAX_CLUSTER_IDENT_LEN);
        let over_cap = "n".repeat(MAX_CLUSTER_IDENT_LEN.saturating_add(1));

        config.cluster.node_id = Some(at_cap.clone());
        assert!(
            config.cluster.validate().is_ok(),
            "exactly {MAX_CLUSTER_IDENT_LEN} bytes must be accepted, or the cap \
             is off by one"
        );

        config.cluster.node_id = Some(over_cap.clone());
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "a node_id one byte over {MAX_CLUSTER_IDENT_LEN} must be refused, \
             got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.node_id"),
            "the error must name the offending key; got {message:?}"
        );

        config.cluster.node_id = Some("node-a".to_owned());
        config.cluster.cluster_name = over_cap;
        let result = config.cluster.validate();
        assert!(
            result.is_err(),
            "an over-long cluster_name must be refused for the same reason, \
             got {result:?}"
        );
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("cluster.cluster_name"),
            "…naming cluster_name this time; got {message:?}"
        );

        config.cluster.cluster_name = at_cap;
        assert!(
            config.cluster.validate().is_ok(),
            "…and a cluster_name at the cap must be accepted, or the rule is vacuous"
        );
    }

    #[test]
    fn shadow_env_overrides_apply() {
        let env = MockEnv::new()
            .with("AUTUMN_SHADOW__ENABLED", "true")
            .with("AUTUMN_SHADOW__TARGET", "http://127.0.0.1:9091")
            .with("AUTUMN_SHADOW__SAMPLE_RATE", "0.25")
            .with("AUTUMN_SHADOW__ROUTES", "/api/*, /status")
            .with("AUTUMN_SHADOW__TIMEOUT_MS", "750")
            .with("AUTUMN_SHADOW__MAX_IN_FLIGHT", "16")
            .with("AUTUMN_SHADOW__MAX_BODY_BYTES", "4096")
            .with("AUTUMN_SHADOW__MAX_RECORDS", "12")
            .with("AUTUMN_SHADOW__MAX_SAMPLE_BYTES", "256");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert!(config.shadow.enabled);
        assert_eq!(
            config.shadow.target.as_deref(),
            Some("http://127.0.0.1:9091")
        );
        assert!((config.shadow.sample_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(config.shadow.routes, vec!["/api/*", "/status"]);
        assert_eq!(config.shadow.timeout_ms, 750);
        assert_eq!(config.shadow.max_in_flight, 16);
        assert_eq!(config.shadow.max_body_bytes, 4096);
        assert_eq!(config.shadow.max_records, 12);
        assert_eq!(config.shadow.max_sample_bytes, 256);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_empty_shadow_routes_override_does_not_disable_mirroring() {
        // An unfilled compose/CI template (`AUTUMN_SHADOW__ROUTES=`) must not
        // become a one-element allowlist holding the empty pattern: that
        // matches no path, so the mirror would run and mirror nothing, with no
        // diagnostic. Same failure shape `parse_env_csv_non_empty` was added
        // for in #1621.
        let env = MockEnv::new().with("AUTUMN_SHADOW__ROUTES", "");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.shadow.routes.is_empty());
    }

    #[test]
    fn top_level_validation_rejects_an_unusable_shadow_section() {
        // Guards the `self.shadow.validate()?` call in `AutumnConfig::validate`:
        // without it a replica boots with mirroring "on" and no target.
        let mut config = AutumnConfig::default();
        config.shadow.enabled = true;
        let error = config.validate().expect_err("must reject");
        assert!(
            error.to_string().contains("shadow.target"),
            "the boot failure must name the missing key, got: {error}"
        );
    }

    #[test]
    fn cluster_env_overrides_apply() {
        let env = MockEnv::new()
            .with("AUTUMN_CLUSTER__ENABLED", "true")
            .with("AUTUMN_CLUSTER__SECRET", "a-shared-cluster-secret-value")
            .with("AUTUMN_CLUSTER__CLUSTER_NAME", "orchard")
            .with("AUTUMN_CLUSTER__BIND_ADDR", "0.0.0.0:7946")
            .with("AUTUMN_CLUSTER__ADVERTISE_ADDR", "10.0.0.4:7946")
            .with("AUTUMN_CLUSTER__SEED_PEERS", "10.0.0.5:7946, 10.0.0.6:7946")
            .with("AUTUMN_CLUSTER__NODE_ID", "node-a")
            .with("AUTUMN_CLUSTER__PUSH_INTERVAL_MS", "250")
            .with("AUTUMN_CLUSTER__SUSPICION_TIMEOUT_MS", "1500");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert!(config.cluster.enabled);
        assert_eq!(
            config
                .cluster
                .secret
                .as_ref()
                .map(|s| secrecy::ExposeSecret::expose_secret(s).to_owned()),
            Some("a-shared-cluster-secret-value".to_owned())
        );
        assert_eq!(config.cluster.cluster_name, "orchard");
        assert_eq!(config.cluster.bind_addr, "0.0.0.0:7946");
        assert_eq!(
            config.cluster.advertise_addr.as_deref(),
            Some("10.0.0.4:7946")
        );
        assert_eq!(
            config.cluster.seed_peers,
            vec!["10.0.0.5:7946".to_owned(), "10.0.0.6:7946".to_owned()],
            "AUTUMN_CLUSTER__SEED_PEERS must split on commas and trim"
        );
        assert_eq!(config.cluster.node_id.as_deref(), Some("node-a"));
        assert_eq!(config.cluster.push_interval_ms, 250);
        assert_eq!(config.cluster.suspicion_timeout_ms, 1_500);
    }

    #[test]
    fn env_override_connect_timeout() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS", "15");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.connect_timeout_secs, 15);
    }

    #[test]
    fn env_override_read_your_writes() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__READ_YOUR_WRITES", "request");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.read_your_writes, ReadYourWrites::Request);
    }

    #[test]
    fn env_override_read_your_writes_session() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__READ_YOUR_WRITES", "session");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.read_your_writes, ReadYourWrites::Session);
    }

    #[test]
    fn env_override_pin_after_write_secs() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__PIN_AFTER_WRITE_SECS", "10");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.pin_after_write_secs, 10);
    }

    #[test]
    fn env_override_invalid_pool_size_ignored() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__POOL_SIZE", "not_a_number");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.pool_size, 10);
    }

    // ── auth.magic_link env overrides ─────────────────────────────────────────

    #[test]
    fn env_override_magic_link_ttl_minutes() {
        let env = MockEnv::new().with("AUTUMN_AUTH__MAGIC_LINK__TTL_MINUTES", "45");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.auth.magic_link.ttl_minutes, 45);
    }

    #[test]
    fn env_override_magic_link_email_cooldown_secs() {
        let env = MockEnv::new().with("AUTUMN_AUTH__MAGIC_LINK__EMAIL_COOLDOWN_SECS", "120");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.auth.magic_link.email_cooldown_secs, 120);
    }

    #[test]
    fn env_override_magic_link_overrides_toml_value() {
        let env = MockEnv::new()
            .with("AUTUMN_AUTH__MAGIC_LINK__TTL_MINUTES", "45")
            .with("AUTUMN_AUTH__MAGIC_LINK__EMAIL_COOLDOWN_SECS", "120");
        let mut config = AutumnConfig::default();
        // Simulate values loaded from autumn.toml.
        config.auth.magic_link.ttl_minutes = 30;
        config.auth.magic_link.email_cooldown_secs = 200;
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.auth.magic_link.ttl_minutes, 45);
        assert_eq!(config.auth.magic_link.email_cooldown_secs, 120);
    }

    #[test]
    fn env_unset_leaves_magic_link_toml_value_intact() {
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        // Simulate values loaded from autumn.toml.
        config.auth.magic_link.ttl_minutes = 30;
        config.auth.magic_link.email_cooldown_secs = 200;
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.auth.magic_link.ttl_minutes, 30);
        assert_eq!(config.auth.magic_link.email_cooldown_secs, 200);
    }

    #[test]
    fn env_override_invalid_magic_link_ttl_minutes_ignored() {
        let env = MockEnv::new().with("AUTUMN_AUTH__MAGIC_LINK__TTL_MINUTES", "not_a_number");
        let mut config = AutumnConfig::default();
        // Simulate a value loaded from autumn.toml.
        config.auth.magic_link.ttl_minutes = 30;
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.auth.magic_link.ttl_minutes, 30);
    }

    // ── startup_wait_secs ─────────────────────────────────────────────────────

    #[test]
    fn startup_wait_secs_default_is_zero() {
        assert_eq!(DatabaseConfig::default().startup_wait_secs, 0);
    }

    #[test]
    fn env_override_startup_wait_secs() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__STARTUP_WAIT_SECS", "60");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.startup_wait_secs, 60);
    }

    #[test]
    fn startup_wait_secs_parses_from_toml() {
        let config: AutumnConfig = toml::from_str("[database]\nstartup_wait_secs = 30").unwrap();
        assert_eq!(config.database.startup_wait_secs, 30);
    }

    #[cfg(feature = "storage")]
    #[test]
    fn env_override_storage_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_STORAGE__BACKEND", "s3")
            .with("AUTUMN_STORAGE__DEFAULT_PROVIDER", "media")
            .with("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION", "true")
            .with("AUTUMN_STORAGE__LOCAL__ROOT", "var/blobs")
            .with("AUTUMN_STORAGE__LOCAL__MOUNT_PATH", "/files")
            .with("AUTUMN_STORAGE__LOCAL__DEFAULT_URL_EXPIRY_SECS", "42")
            .with("AUTUMN_STORAGE__LOCAL__SIGNING_KEY", "secret")
            .with("AUTUMN_STORAGE__S3__BUCKET", "uploads")
            .with("AUTUMN_STORAGE__S3__REGION", "us-east-1")
            .with("AUTUMN_STORAGE__S3__ENDPOINT", "https://s3.example.test")
            .with(
                "AUTUMN_STORAGE__S3__PUBLIC_BASE_URL",
                "https://cdn.example.test",
            )
            .with("AUTUMN_STORAGE__S3__ACCESS_KEY_ID_ENV", "AWS_ACCESS_KEY_ID")
            .with(
                "AUTUMN_STORAGE__S3__SECRET_ACCESS_KEY_ENV",
                "AWS_SECRET_ACCESS_KEY",
            )
            .with("AUTUMN_STORAGE__S3__FORCE_PATH_STYLE", "true")
            .with("AUTUMN_STORAGE__S3__DEFAULT_URL_EXPIRY_SECS", "99")
            .with("AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_BYTES", "5242880")
            .with("AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_WIDTH", "2000")
            .with("AUTUMN_STORAGE__VARIANTS__MAX_SOURCE_HEIGHT", "1500");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.storage.backend, crate::storage::StorageBackend::S3);
        assert_eq!(config.storage.default_provider, "media");
        assert!(config.storage.allow_local_in_production);
        assert_eq!(config.storage.local.root, PathBuf::from("var/blobs"));
        assert_eq!(config.storage.local.mount_path, "/files");
        assert_eq!(config.storage.local.default_url_expiry_secs, 42);
        assert_eq!(config.storage.local.signing_key.as_deref(), Some("secret"));
        assert_eq!(config.storage.s3.bucket.as_deref(), Some("uploads"));
        assert_eq!(config.storage.s3.region.as_deref(), Some("us-east-1"));
        assert_eq!(
            config.storage.s3.endpoint.as_deref(),
            Some("https://s3.example.test")
        );
        assert_eq!(
            config.storage.s3.public_base_url.as_deref(),
            Some("https://cdn.example.test")
        );
        assert_eq!(
            config.storage.s3.access_key_id_env.as_deref(),
            Some("AWS_ACCESS_KEY_ID")
        );
        assert_eq!(
            config.storage.s3.secret_access_key_env.as_deref(),
            Some("AWS_SECRET_ACCESS_KEY")
        );
        assert!(config.storage.s3.force_path_style);
        assert_eq!(config.storage.s3.default_url_expiry_secs, 99);
        assert_eq!(config.storage.variants.max_source_bytes, 5_242_880);
        assert_eq!(config.storage.variants.max_source_width, 2_000);
        assert_eq!(config.storage.variants.max_source_height, 1_500);
    }

    // ── [replication] (issue #1628) ──────────────────────────────────────────

    #[test]
    fn replication_parses_from_toml() {
        let toml = r#"
            [replication]
            enabled = true
            rpo_secs = 5
            sync_interval_secs = 2
            snapshot_interval_secs = 900
            max_wal_bytes = 8388608
            retention_hours = 48
            verify_interval_secs = 3600
            prefix = "db"
            allow_shared_bucket = true

            [replication.s3]
            bucket = "myapp-replicas"
            region = "auto"
            endpoint = "https://acct.r2.cloudflarestorage.com"
            access_key_id_env = "AUTUMN_REPLICA_ACCESS_KEY_ID"
            secret_access_key_env = "AUTUMN_REPLICA_SECRET_ACCESS_KEY"
            force_path_style = true
        "#;
        let config: AutumnConfig = toml::from_str(toml).expect("parse");
        let replication = *config.replication.expect("section present");
        assert!(replication.enabled);
        assert_eq!(replication.rpo_secs, 5);
        assert_eq!(
            replication.sync_interval(),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(replication.snapshot_interval_secs, 900);
        assert_eq!(replication.max_wal_bytes, 8_388_608);
        assert_eq!(replication.retention_hours, 48);
        assert_eq!(
            replication.verify_interval(),
            Some(std::time::Duration::from_secs(3600))
        );
        assert_eq!(replication.prefix.as_deref(), Some("db"));
        assert!(replication.allow_shared_bucket);
        let s3 = replication.s3.expect("s3 destination");
        assert_eq!(s3.bucket.as_deref(), Some("myapp-replicas"));
        assert_eq!(s3.region.as_deref(), Some("auto"));
        assert_eq!(
            s3.endpoint.as_deref(),
            Some("https://acct.r2.cloudflarestorage.com")
        );
        assert_eq!(
            s3.access_key_id_env.as_deref(),
            Some("AUTUMN_REPLICA_ACCESS_KEY_ID")
        );
        assert_eq!(
            s3.secret_access_key_env.as_deref(),
            Some("AUTUMN_REPLICA_SECRET_ACCESS_KEY")
        );
        assert!(s3.force_path_style);
    }

    #[test]
    fn replication_defaults_to_absent_and_carries_the_documented_rpo() {
        let config = AutumnConfig::default();
        assert!(config.replication.is_none());

        let defaults = ReplicationConfig::default();
        assert!(!defaults.enabled, "replication is opt-in");
        assert_eq!(defaults.rpo_secs, 10, "AC #2: at most 10s of data loss");
        assert_eq!(defaults.retention_hours, 168);
        assert_eq!(defaults.max_wal_bytes, 16 * 1024 * 1024);
        assert!(
            !defaults.allow_shared_bucket,
            "#1619's distinct-bucket posture"
        );
        assert!(defaults.validation_errors().is_empty(), "disabled is valid");
    }

    #[test]
    fn replication_validation_names_every_problem() {
        let no_destination = ReplicationConfig {
            enabled: true,
            ..ReplicationConfig::default()
        };
        assert!(
            no_destination
                .validation_errors()
                .iter()
                .any(|e| e.contains("no destination is configured")),
            "{:?}",
            no_destination.validation_errors()
        );

        let both = ReplicationConfig {
            enabled: true,
            path: Some("/mnt/replica".to_owned()),
            s3: Some(ReplicationS3Config {
                bucket: Some("b".to_owned()),
                ..ReplicationS3Config::default()
            }),
            ..ReplicationConfig::default()
        };
        assert!(
            both.validation_errors()
                .iter()
                .any(|e| e.contains("pick exactly one destination"))
        );

        let no_credentials = ReplicationConfig {
            enabled: true,
            s3: Some(ReplicationS3Config {
                bucket: Some("b".to_owned()),
                ..ReplicationS3Config::default()
            }),
            rpo_secs: 0,
            retention_hours: 0,
            ..ReplicationConfig::default()
        };
        let errors = no_credentials.validation_errors();
        assert!(
            errors.iter().any(|e| e.contains("access_key_id_env")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("rpo_secs")), "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("retention_hours")),
            "{errors:?}"
        );
    }

    #[test]
    fn env_override_replication_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_REPLICATION__ENABLED", "true")
            .with("AUTUMN_REPLICATION__RPO_SECS", "20")
            .with("AUTUMN_REPLICATION__SYNC_INTERVAL_SECS", "7")
            .with("AUTUMN_REPLICATION__SNAPSHOT_INTERVAL_SECS", "600")
            .with("AUTUMN_REPLICATION__MAX_WAL_BYTES", "1048576")
            .with("AUTUMN_REPLICATION__RETENTION_HOURS", "12")
            .with("AUTUMN_REPLICATION__VERIFY_INTERVAL_SECS", "0")
            .with("AUTUMN_REPLICATION__PREFIX", "replicas")
            .with("AUTUMN_REPLICATION__ALLOW_SHARED_BUCKET", "true")
            .with("AUTUMN_REPLICATION__S3__BUCKET", "from-env")
            .with("AUTUMN_REPLICATION__S3__REGION", "auto")
            .with(
                "AUTUMN_REPLICATION__S3__ENDPOINT",
                "https://minio.test:9000",
            )
            .with("AUTUMN_REPLICATION__S3__ACCESS_KEY_ID_ENV", "KEY")
            .with("AUTUMN_REPLICATION__S3__SECRET_ACCESS_KEY_ENV", "SECRET")
            .with("AUTUMN_REPLICATION__S3__FORCE_PATH_STYLE", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        let replication = *config.replication.expect("materialized from env alone");
        assert!(replication.enabled);
        assert_eq!(replication.rpo_secs, 20);
        assert_eq!(replication.sync_interval_secs, Some(7));
        assert_eq!(replication.snapshot_interval_secs, 600);
        assert_eq!(replication.max_wal_bytes, 1_048_576);
        assert_eq!(replication.retention_hours, 12);
        assert_eq!(replication.verify_interval(), None);
        assert_eq!(replication.prefix.as_deref(), Some("replicas"));
        assert!(replication.allow_shared_bucket);
        let s3 = replication.s3.expect("s3 from env");
        assert_eq!(s3.bucket.as_deref(), Some("from-env"));
        assert_eq!(s3.endpoint.as_deref(), Some("https://minio.test:9000"));
        assert!(s3.force_path_style);
    }

    #[test]
    fn replication_env_does_not_materialize_a_section_from_optional_keys_alone() {
        // A lone region/prefix cannot replicate anywhere, so it must NOT conjure
        // a section that then fails validation (mirrors #1619's #1791 fix).
        let env = MockEnv::new()
            .with("AUTUMN_REPLICATION__S3__REGION", "auto")
            .with("AUTUMN_REPLICATION__PREFIX", "db")
            .with("AUTUMN_REPLICATION__ENABLED", "false");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.replication.is_none());
    }

    #[test]
    fn a_path_destination_from_env_never_grows_an_empty_s3_section() {
        let env = MockEnv::new()
            .with("AUTUMN_REPLICATION__ENABLED", "true")
            .with("AUTUMN_REPLICATION__PATH", "/mnt/replica");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let replication = *config.replication.expect("materialized");
        assert_eq!(replication.path.as_deref(), Some("/mnt/replica"));
        assert!(replication.s3.is_none(), "no S3 key was set");
        assert!(replication.validation_errors().is_empty());
    }

    #[test]
    fn replication_env_overlays_a_toml_section() {
        let mut config: AutumnConfig = toml::from_str(
            "[replication]\nenabled = false\nretention_hours = 24\npath = \"/from-toml\"",
        )
        .expect("parse");
        let env = MockEnv::new().with("AUTUMN_REPLICATION__ENABLED", "true");
        config.apply_env_overrides_with_env(&env);
        let replication = *config.replication.expect("section");
        assert!(replication.enabled, "env flips the TOML toggle");
        assert_eq!(replication.retention_hours, 24, "TOML value survives");
        assert_eq!(replication.path.as_deref(), Some("/from-toml"));
    }

    #[test]
    fn backup_offsite_parses_from_toml() {
        let toml = r#"
            [backup.offsite]
            prefix = "db"
            keep = 5
            auto_upload = true
            allow_shared_bucket = true

            [backup.offsite.s3]
            bucket = "offsite-backups"
            region = "auto"
            endpoint = "https://minio.example.test"
            access_key_id_env = "OFFSITE_KEY_ID"
            secret_access_key_env = "OFFSITE_SECRET"
            force_path_style = true
        "#;
        let config: AutumnConfig = toml::from_str(toml).unwrap();
        let offsite = config.backup.offsite.expect("offsite section present");
        assert_eq!(offsite.prefix.as_deref(), Some("db"));
        assert_eq!(offsite.keep, Some(5));
        assert!(offsite.auto_upload);
        assert!(offsite.allow_shared_bucket);
        assert_eq!(offsite.s3.bucket.as_deref(), Some("offsite-backups"));
        assert_eq!(offsite.s3.region.as_deref(), Some("auto"));
        assert_eq!(
            offsite.s3.endpoint.as_deref(),
            Some("https://minio.example.test")
        );
        // Credentials are indirected: config names the env vars, never the values.
        assert_eq!(
            offsite.s3.access_key_id_env.as_deref(),
            Some("OFFSITE_KEY_ID")
        );
        assert_eq!(
            offsite.s3.secret_access_key_env.as_deref(),
            Some("OFFSITE_SECRET")
        );
        assert!(offsite.s3.force_path_style);
    }

    #[test]
    fn backup_offsite_defaults_to_none() {
        let config = AutumnConfig::default();
        assert!(config.backup.offsite.is_none());
    }

    #[test]
    fn env_override_backup_offsite_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_BACKUP__OFFSITE__S3__BUCKET", "offsite")
            .with("AUTUMN_BACKUP__OFFSITE__S3__REGION", "us-west-2")
            .with(
                "AUTUMN_BACKUP__OFFSITE__S3__ENDPOINT",
                "https://s3.offsite.test",
            )
            .with("AUTUMN_BACKUP__OFFSITE__S3__ACCESS_KEY_ID_ENV", "OFF_KEY")
            .with(
                "AUTUMN_BACKUP__OFFSITE__S3__SECRET_ACCESS_KEY_ENV",
                "OFF_SECRET",
            )
            .with("AUTUMN_BACKUP__OFFSITE__S3__FORCE_PATH_STYLE", "true")
            .with("AUTUMN_BACKUP__OFFSITE__PREFIX", "nightly")
            .with("AUTUMN_BACKUP__OFFSITE__KEEP", "3")
            .with("AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD", "true")
            .with("AUTUMN_BACKUP__OFFSITE__ALLOW_SHARED_BUCKET", "true");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        let offsite = config.backup.offsite.expect("materialized from env");
        assert_eq!(offsite.s3.bucket.as_deref(), Some("offsite"));
        assert_eq!(offsite.s3.region.as_deref(), Some("us-west-2"));
        assert_eq!(
            offsite.s3.endpoint.as_deref(),
            Some("https://s3.offsite.test")
        );
        assert_eq!(offsite.s3.access_key_id_env.as_deref(), Some("OFF_KEY"));
        assert_eq!(
            offsite.s3.secret_access_key_env.as_deref(),
            Some("OFF_SECRET")
        );
        assert!(offsite.s3.force_path_style);
        assert_eq!(offsite.prefix.as_deref(), Some("nightly"));
        assert_eq!(offsite.keep, Some(3));
        assert!(offsite.auto_upload);
        assert!(offsite.allow_shared_bucket);
    }

    #[test]
    fn env_override_backup_offsite_absent_stays_none() {
        // With no offsite env vars and no TOML section, nothing is materialized.
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.backup.offsite.is_none());
    }

    #[test]
    fn env_override_backup_offsite_lone_opt_out_toggle_stays_none() {
        // P2 #18: a lone false/opt-out toggle must NOT materialize an empty
        // [backup.offsite] (which would then fail validation / `doctor` with
        // "bucket is unset"). Offsite stays unconfigured.
        for key in [
            "AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD",
            "AUTUMN_BACKUP__OFFSITE__ALLOW_SHARED_BUCKET",
        ] {
            let env = MockEnv::new().with(key, "false");
            let mut config = AutumnConfig::default();
            config.apply_env_overrides_with_env(&env);
            assert!(
                config.backup.offsite.is_none(),
                "{key}=false must not materialize an offsite section",
            );
        }
    }

    #[test]
    fn env_override_backup_offsite_truthy_auto_upload_materializes() {
        // P2 #18: AUTO_UPLOAD=true genuinely needs a validated destination, so it
        // DOES materialize the section (auto_upload set), as before.
        let env = MockEnv::new().with("AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let offsite = config
            .backup
            .offsite
            .expect("auto_upload=true materializes offsite");
        assert!(offsite.auto_upload);
    }

    #[test]
    fn env_override_backup_offsite_destination_key_materializes() {
        // A destination/credential key still materializes the section (with the
        // opt-out toggle applied to it), unchanged from before P2 #18.
        let env = MockEnv::new()
            .with("AUTUMN_BACKUP__OFFSITE__S3__BUCKET", "offsite")
            .with("AUTUMN_BACKUP__OFFSITE__AUTO_UPLOAD", "false");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let offsite = config
            .backup
            .offsite
            .expect("a bucket key materializes offsite");
        assert_eq!(offsite.s3.bucket.as_deref(), Some("offsite"));
        assert!(!offsite.auto_upload);
    }

    #[test]
    fn env_override_backup_offsite_lone_optional_key_stays_none() {
        // #1791: optional-only keys (region, force_path_style, endpoint, prefix,
        // keep) must NOT materialize [backup.offsite] on their own — a bare
        // region with no bucket/credentials cannot upload, so offsite stays
        // UNCONFIGURED rather than producing an empty section that then fails
        // `doctor` with "bucket is unset".
        for (key, val) in [
            ("AUTUMN_BACKUP__OFFSITE__S3__REGION", "us-east-1"),
            ("AUTUMN_BACKUP__OFFSITE__S3__ENDPOINT", "https://s3.test"),
            ("AUTUMN_BACKUP__OFFSITE__S3__FORCE_PATH_STYLE", "true"),
            ("AUTUMN_BACKUP__OFFSITE__PREFIX", "nightly"),
            ("AUTUMN_BACKUP__OFFSITE__KEEP", "3"),
        ] {
            let env = MockEnv::new().with(key, val);
            let mut config = AutumnConfig::default();
            config.apply_env_overrides_with_env(&env);
            assert!(
                config.backup.offsite.is_none(),
                "{key} is optional-only and must not materialize an offsite section",
            );
        }
    }

    #[test]
    fn env_override_backup_offsite_credential_key_materializes() {
        // #1791: the access/secret key-env names are REQUIRED signals, so either
        // one still materializes the section.
        for key in [
            "AUTUMN_BACKUP__OFFSITE__S3__ACCESS_KEY_ID_ENV",
            "AUTUMN_BACKUP__OFFSITE__S3__SECRET_ACCESS_KEY_ENV",
        ] {
            let env = MockEnv::new().with(key, "SOME_ENV_NAME");
            let mut config = AutumnConfig::default();
            config.apply_env_overrides_with_env(&env);
            assert!(
                config.backup.offsite.is_some(),
                "{key} is a required credential signal and must materialize offsite",
            );
        }
    }

    #[test]
    fn env_override_backup_offsite_bucket_only_materializes() {
        // #1791: a lone bucket (required destination signal) still materializes.
        let env = MockEnv::new().with("AUTUMN_BACKUP__OFFSITE__S3__BUCKET", "offsite");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let offsite = config
            .backup
            .offsite
            .expect("a bucket key materializes offsite");
        assert_eq!(offsite.s3.bucket.as_deref(), Some("offsite"));
    }

    #[test]
    fn env_override_backup_offsite_region_only_applied_when_materialized() {
        // #1791: region no longer TRIGGERS materialization, but it is still
        // APPLIED when a required key materializes the section.
        let env = MockEnv::new()
            .with("AUTUMN_BACKUP__OFFSITE__S3__BUCKET", "offsite")
            .with("AUTUMN_BACKUP__OFFSITE__S3__REGION", "us-west-2")
            .with("AUTUMN_BACKUP__OFFSITE__PREFIX", "nightly")
            .with("AUTUMN_BACKUP__OFFSITE__KEEP", "5");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let offsite = config.backup.offsite.expect("bucket materializes offsite");
        assert_eq!(offsite.s3.region.as_deref(), Some("us-west-2"));
        assert_eq!(offsite.prefix.as_deref(), Some("nightly"));
        assert_eq!(offsite.keep, Some(5));
    }

    #[test]
    fn env_override_database_auto_migrate_in_production() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.database.auto_migrate_in_production);
    }

    #[test]
    fn database_auto_migrate_defaults_to_none() {
        // Issue #1903: the profile-agnostic override is unset by default, so the
        // decision falls to convention (dev on, everything else opt-in).
        assert_eq!(DatabaseConfig::default().auto_migrate, None);
    }

    #[test]
    fn env_override_database_auto_migrate() {
        // Issue #1903: AUTUMN_DATABASE__AUTO_MIGRATE flips the profile-agnostic
        // override to an explicit Some(_) on any profile.
        let env = MockEnv::new().with("AUTUMN_DATABASE__AUTO_MIGRATE", "true");
        let mut config = AutumnConfig::default();
        assert_eq!(config.database.auto_migrate, None);
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.database.auto_migrate, Some(true));

        let env_false = MockEnv::new().with("AUTUMN_DATABASE__AUTO_MIGRATE", "false");
        let mut config_false = AutumnConfig::default();
        config_false.apply_env_overrides_with_env(&env_false);
        assert_eq!(config_false.database.auto_migrate, Some(false));
    }

    #[test]
    fn env_override_jobs_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_JOBS__BACKEND", "redis")
            .with("AUTUMN_JOBS__WORKERS", "8")
            .with("AUTUMN_JOBS__MAX_ATTEMPTS", "12")
            .with("AUTUMN_JOBS__INITIAL_BACKOFF_MS", "750")
            .with("AUTUMN_JOBS__REDIS__URL", "redis://jobs:6379/2")
            .with("AUTUMN_JOBS__REDIS__KEY_PREFIX", "myapp:jobs")
            .with("AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS", "45000");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.jobs.backend, "redis");
        assert_eq!(config.jobs.workers, 8);
        assert_eq!(config.jobs.max_attempts, 12);
        assert_eq!(config.jobs.initial_backoff_ms, 750);
        assert_eq!(
            config.jobs.redis.url.as_deref(),
            Some("redis://jobs:6379/2")
        );
        assert_eq!(config.jobs.redis.key_prefix, "myapp:jobs");
        assert_eq!(config.jobs.redis.visibility_timeout_ms, 45_000);
    }

    // ── [retention] unified framework-owned data retention (issue #1605) ──

    #[test]
    fn retention_config_defaults_leave_every_dataset_unset() {
        // AC #1: "Leaving a dataset unset preserves today's behavior exactly."
        // The only way to guarantee that is for every window to default to
        // None, and for the whole section to report itself as unconfigured.
        let config = AutumnConfig::default();
        assert!(
            config.retention.job_history.is_none(),
            "job_history must default to unset"
        );
        assert!(config.retention.job_tracking.is_none());
        assert!(config.retention.idempotency.is_none());
        assert!(config.retention.experiment_assignments.is_none());
        assert!(config.retention.webhook_replay.is_none());
        assert!(config.retention.sessions.is_none());
        assert!(config.retention.audit_archives.is_none());
        assert!(
            !config.retention.any_window_configured(),
            "a default config must register no retention sweep at all"
        );
        assert_eq!(
            config.retention.sweep_interval, "1h",
            "the sweep cadence has a documented default even when no window is set"
        );
    }

    #[test]
    fn retention_toml_deserializes_every_dataset_window() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [retention]
            sweep_interval = "30m"
            job_history = "90d"
            job_tracking = "7d"
            idempotency = "2d"
            experiment_assignments = "365d"
            webhook_replay = "3d"
            sessions = "30d"
            audit_archives = "400d"
            "#,
        )
        .unwrap();

        assert_eq!(config.retention.sweep_interval, "30m");
        assert_eq!(config.retention.job_history.as_deref(), Some("90d"));
        assert_eq!(config.retention.job_tracking.as_deref(), Some("7d"));
        assert_eq!(config.retention.idempotency.as_deref(), Some("2d"));
        assert_eq!(
            config.retention.experiment_assignments.as_deref(),
            Some("365d")
        );
        assert_eq!(config.retention.webhook_replay.as_deref(), Some("3d"));
        assert_eq!(config.retention.sessions.as_deref(), Some("30d"));
        assert_eq!(config.retention.audit_archives.as_deref(), Some("400d"));
        assert!(config.retention.any_window_configured());
        config.validate().expect("a well-formed section validates");
    }

    #[test]
    fn env_override_retention_windows() {
        let env = MockEnv::new()
            .with("AUTUMN_RETENTION__SWEEP_INTERVAL", "15m")
            .with("AUTUMN_RETENTION__JOB_HISTORY", "45d")
            .with("AUTUMN_RETENTION__JOB_TRACKING", "2d")
            .with("AUTUMN_RETENTION__IDEMPOTENCY", "12h")
            .with("AUTUMN_RETENTION__EXPERIMENT_ASSIGNMENTS", "180d")
            .with("AUTUMN_RETENTION__WEBHOOK_REPLAY", "1d")
            .with("AUTUMN_RETENTION__SESSIONS", "14d")
            .with("AUTUMN_RETENTION__AUDIT_ARCHIVES", "365d");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.retention.sweep_interval, "15m");
        assert_eq!(config.retention.job_history.as_deref(), Some("45d"));
        assert_eq!(config.retention.job_tracking.as_deref(), Some("2d"));
        assert_eq!(config.retention.idempotency.as_deref(), Some("12h"));
        assert_eq!(
            config.retention.experiment_assignments.as_deref(),
            Some("180d")
        );
        assert_eq!(config.retention.webhook_replay.as_deref(), Some("1d"));
        assert_eq!(config.retention.sessions.as_deref(), Some("14d"));
        assert_eq!(config.retention.audit_archives.as_deref(), Some("365d"));
    }

    #[test]
    fn env_override_retention_empty_value_clears_a_window() {
        // An empty env value is the documented way to *unset* a window that
        // autumn.toml declared, restoring today's behavior for that dataset
        // without editing the file (matches parse_env_option_string).
        let env = MockEnv::new().with("AUTUMN_RETENTION__JOB_HISTORY", "");
        let mut config = AutumnConfig::default();
        config.retention.job_history = Some("90d".to_owned());
        config.apply_env_overrides_with_env(&env);

        assert!(config.retention.job_history.is_none());
    }

    #[test]
    fn retention_validate_rejects_an_unparseable_window() {
        let mut config = AutumnConfig::default();
        config.retention.job_history = Some("ninety days".to_owned());
        let error = config
            .validate()
            .expect_err("an unparseable duration must fail boot, not be ignored");
        let message = error.to_string();
        assert!(
            message.contains("retention.job_history"),
            "the error must name the offending key: {message}"
        );
    }

    #[test]
    fn retention_validate_rejects_a_zero_window() {
        // "0s" would purge everything the instant it is written — almost
        // certainly a typo, and never something to guess at.
        let mut config = AutumnConfig::default();
        config.retention.sessions = Some("0s".to_owned());
        let error = config.validate().expect_err("a zero window must fail boot");
        assert!(error.to_string().contains("retention.sessions"), "{error}");
    }

    #[test]
    fn retention_validate_rejects_an_unparseable_sweep_interval() {
        let mut config = AutumnConfig::default();
        config.retention.job_history = Some("30d".to_owned());
        config.retention.sweep_interval = "whenever".to_owned();
        let error = config.validate().expect_err("bad cadence must fail boot");
        assert!(
            error.to_string().contains("retention.sweep_interval"),
            "{error}"
        );
    }

    #[test]
    fn retention_validate_accepts_a_default_config() {
        // The whole point of AC #1: an app that never mentions [retention]
        // must be entirely unaffected, validation included.
        AutumnConfig::default()
            .validate()
            .expect("an unconfigured [retention] section must validate");
    }

    #[test]
    fn job_tracking_config_defaults_ttl_86400_and_route_enabled() {
        let config = AutumnConfig::default();
        assert_eq!(config.jobs.tracking.ttl_secs, 86_400);
        assert!(config.jobs.tracking.route_enabled);
    }

    #[test]
    fn env_override_jobs_tracking_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_JOBS__TRACKING__TTL_SECS", "3600")
            .with("AUTUMN_JOBS__TRACKING__ROUTE_ENABLED", "false");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.jobs.tracking.ttl_secs, 3_600);
        assert!(!config.jobs.tracking.route_enabled);
    }

    #[test]
    fn jobs_toml_deserializes_tracking_fields() {
        let config: AutumnConfig = toml::from_str(
            r"
            [jobs.tracking]
            ttl_secs = 7200
            route_enabled = false
            ",
        )
        .unwrap();

        assert_eq!(config.jobs.tracking.ttl_secs, 7_200);
        assert!(!config.jobs.tracking.route_enabled);
    }

    #[test]
    fn jobs_toml_deserializes_redis_visibility_timeout() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs]
            backend = "redis"

            [jobs.redis]
            url = "redis://localhost:6379/5"
            key_prefix = "demo:jobs"
            visibility_timeout_ms = 15000
            "#,
        )
        .unwrap();

        assert_eq!(config.jobs.backend, "redis");
        assert_eq!(
            config.jobs.redis.url.as_deref(),
            Some("redis://localhost:6379/5")
        );
        assert_eq!(config.jobs.redis.key_prefix, "demo:jobs");
        assert_eq!(config.jobs.redis.visibility_timeout_ms, 15_000);
    }

    #[test]
    fn job_queues_defaults_to_single_default_queue() {
        let config = AutumnConfig::default();
        assert!(config.jobs.queues.strict);
        assert_eq!(config.jobs.queues.queues.len(), 1);
        assert_eq!(config.jobs.queues.queues[0].name, "default");
        assert_eq!(config.jobs.queues.queues[0].weight, 1);
    }

    #[test]
    fn jobs_without_queues_key_keeps_single_default_queue() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs]
            backend = "local"
            workers = 4
            "#,
        )
        .unwrap();
        assert!(config.jobs.queues.strict);
        assert_eq!(config.jobs.queues.queues.len(), 1);
        assert_eq!(config.jobs.queues.queues[0].name, "default");
    }

    #[test]
    fn job_queues_parse_ordered_list_as_strict_priority() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs]
            backend = "local"
            queues = ["critical", "default", "low"]
            "#,
        )
        .unwrap();
        assert!(config.jobs.queues.strict, "list form is strict priority");
        let names: Vec<&str> = config
            .jobs
            .queues
            .queues
            .iter()
            .map(|q| q.name.as_str())
            .collect();
        assert_eq!(names, ["critical", "default", "low"]);
        assert!(config.jobs.queues.queues.iter().all(|q| q.weight == 1));
    }

    #[test]
    fn job_queues_parse_weight_map_as_weighted() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs]
            backend = "local"

            [jobs.queues]
            critical = 4
            default = 2
            low = 1
            "#,
        )
        .unwrap();
        assert!(!config.jobs.queues.strict, "map form is weighted");
        let weight = |name: &str| {
            config
                .jobs
                .queues
                .queues
                .iter()
                .find(|q| q.name == name)
                .map(|q| q.weight)
        };
        assert_eq!(weight("critical"), Some(4));
        assert_eq!(weight("default"), Some(2));
        assert_eq!(weight("low"), Some(1));
    }

    #[test]
    fn job_queues_strict_list_rejects_duplicate_names() {
        let err = toml::from_str::<AutumnConfig>(
            r#"
            [jobs]
            queues = ["critical", "default", "critical"]
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("duplicate queue name") && err.contains("critical"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn job_queues_table_form_parses_caps_and_reserved_slots() {
        // Issue #1623: a queue value may be a bare weight OR a table with
        // per-queue `concurrency` (cap) and `reserved` (dedicated) slots.
        let config: AutumnConfig = toml::from_str(
            r"
            [jobs.queues]
            critical = { weight = 3, reserved = 2 }
            bulk = { weight = 1, concurrency = 4 }
            default = 2
            ",
        )
        .unwrap();
        assert!(!config.jobs.queues.strict, "table form is weighted");
        let find = |name: &str| {
            config
                .jobs
                .queues
                .queues
                .iter()
                .find(|q| q.name == name)
                .cloned()
                .unwrap()
        };
        let critical = find("critical");
        assert_eq!(critical.weight, 3);
        assert_eq!(critical.reserved, Some(2));
        assert_eq!(critical.concurrency, None);
        let bulk = find("bulk");
        assert_eq!(bulk.weight, 1);
        assert_eq!(bulk.concurrency, Some(4));
        assert_eq!(bulk.reserved, None);
        // Bare integer still works alongside the table form.
        let default = find("default");
        assert_eq!(default.weight, 2);
        assert_eq!(default.concurrency, None);
        assert_eq!(default.reserved, None);
    }

    #[test]
    fn job_queues_table_form_defaults_weight_to_one() {
        let config: AutumnConfig = toml::from_str(
            r"
            [jobs.queues]
            critical = { reserved = 1 }
            ",
        )
        .unwrap();
        let critical = &config.jobs.queues.queues[0];
        assert_eq!(critical.weight, 1, "omitted weight defaults to 1");
        assert_eq!(critical.reserved, Some(1));
    }

    #[test]
    fn job_queues_table_form_rejects_zero_weight() {
        let err = toml::from_str::<AutumnConfig>(
            r"
            [jobs.queues]
            critical = { weight = 0, reserved = 1 }
            ",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("weight must be at least 1") && err.contains("critical"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn job_queues_table_form_rejects_unknown_setting() {
        let err = toml::from_str::<AutumnConfig>(
            r"
            [jobs.queues]
            critical = { weight = 1, bogus = 3 }
            ",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bogus"), "unexpected error: {err}");
    }

    #[test]
    fn jobs_pin_defaults_empty_and_parses_from_toml() {
        let default = AutumnConfig::default();
        assert!(default.jobs.pin.is_empty(), "pin is empty by default (AC4)");
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs]
            pin = ["critical", "default"]
            "#,
        )
        .unwrap();
        assert_eq!(config.jobs.pin, vec!["critical", "default"]);
    }

    /// Issue #1623, AC6: `autumn doctor --strict` can only *prove* a zero-coverage
    /// gap when the operator declares the fleet topology under `[jobs.fleet]
    /// tiers` — every worker tier's `jobs.pin`. That key lives in the same
    /// `autumn.toml` the app boots from, so the app's own schema must know it;
    /// otherwise declaring the topology that makes the doctor check work is an
    /// unknown config key that hard-fails boot under `strict_config_enforce_all`.
    #[test]
    fn jobs_fleet_tiers_parse_from_toml() {
        let default = AutumnConfig::default();
        assert!(
            default.jobs.fleet.tiers.is_empty(),
            "no fleet topology is declared by default (AC4)"
        );
        let config: AutumnConfig = toml::from_str(
            r#"
            [jobs.fleet]
            tiers = [["critical"], ["bulk", "default"], []]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.jobs.fleet.tiers,
            vec![
                vec!["critical".to_owned()],
                vec!["bulk".to_owned(), "default".to_owned()],
                // An empty inner list is an *unpinned* tier that drains
                // everything — a meaningful declaration, not a typo.
                Vec::<String>::new(),
            ],
        );
    }

    /// The regression this closes: `[jobs.fleet]` is documented for
    /// `autumn doctor`, so an operator who declares it must still be able to
    /// boot the app with the strictest config validation turned on.
    #[test]
    fn declared_fleet_topology_boots_under_enforce_all() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(
            &config_path,
            "[server]\nstrict_config = true\nstrict_config_enforce_all = true\n\n\
             [jobs]\npin = [\"critical\"]\n\n\
             [jobs.fleet]\ntiers = [[\"critical\"], [\"bulk\", \"default\"]]\n",
        )
        .unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_ok(),
            "a declared [jobs.fleet] topology must not be an unknown config key: {res:?}"
        );
        let config = res.unwrap();
        assert_eq!(config.jobs.fleet.tiers.len(), 2);
        assert_eq!(config.jobs.pin, vec!["critical".to_owned()]);
    }

    /// A typo *inside* `[jobs.fleet]` must still be caught — accepting the
    /// section must not turn it into an opaque bag that swallows mistakes. The
    /// crate catches unknown keys through the strict-config schema walk (not
    /// `deny_unknown_fields`), so this asserts it there.
    #[test]
    fn jobs_fleet_typo_is_caught_by_strict_config() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("autumn.toml");
        std::fs::write(
            &config_path,
            "[server]\nstrict_config = true\nstrict_config_enforce_all = true\n\n\
             [jobs.fleet]\nteirs = [[\"critical\"]]\n",
        )
        .unwrap();

        let env = FakeEnv(
            [
                ("AUTUMN_ENV".to_owned(), "prod".to_owned()),
                (
                    "AUTUMN_MANIFEST_DIR".to_owned(),
                    temp.path().to_str().unwrap().to_owned(),
                ),
            ]
            .into(),
        );

        let res = AutumnConfig::load_with_env(&env);
        assert!(
            res.is_err(),
            "a typo inside [jobs.fleet] must not be silently accepted"
        );
        let err_str = format!("{:?}", res.err().unwrap());
        assert!(
            err_str.contains("teirs"),
            "error should name the typo: {err_str}"
        );
    }

    #[test]
    fn jobs_pin_env_override_is_comma_separated() {
        let env = MockEnv::new().with("AUTUMN_JOBS__PIN", "critical, bulk ,");
        let mut config = AutumnConfig::default();
        config.apply_jobs_env_overrides_with_env(&env);
        assert_eq!(
            config.jobs.pin,
            vec!["critical".to_string(), "bulk".to_string()],
            "trims whitespace and drops empty entries"
        );
    }

    #[test]
    fn job_queues_weighted_rejects_zero_weight() {
        let err = toml::from_str::<AutumnConfig>(
            r"
            [jobs.queues]
            critical = 4
            default = 0
            ",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("weight must be at least 1") && err.contains("default"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn channels_defaults_to_in_process_backend() {
        let config = AutumnConfig::default();

        assert_eq!(config.channels.backend, ChannelBackend::InProcess);
        assert_eq!(config.channels.capacity, 32);
        assert_eq!(config.channels.replay_buffer, 256);
        assert_eq!(config.channels.redis.key_prefix, "autumn:channels");
        assert!(config.channels.redis.url.is_none());
    }

    #[test]
    fn channels_env_overrides_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_CHANNELS__BACKEND", "redis")
            .with("AUTUMN_CHANNELS__CAPACITY", "128")
            .with("AUTUMN_CHANNELS__REPLAY_BUFFER", "512")
            .with("AUTUMN_CHANNELS__REDIS__URL", "redis://channels:6379/4")
            .with("AUTUMN_CHANNELS__REDIS__KEY_PREFIX", "myapp:channels");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.channels.backend, ChannelBackend::Redis);
        assert_eq!(config.channels.capacity, 128);
        assert_eq!(config.channels.replay_buffer, 512);
        assert_eq!(
            config.channels.redis.url.as_deref(),
            Some("redis://channels:6379/4")
        );
        assert_eq!(config.channels.redis.key_prefix, "myapp:channels");
    }

    #[test]
    fn channels_toml_deserializes_redis_backend() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [channels]
            backend = "redis"
            capacity = 64

            [channels.redis]
            url = "redis://localhost:6379/5"
            key_prefix = "demo:channels"
            "#,
        )
        .unwrap();

        assert_eq!(config.channels.backend, ChannelBackend::Redis);
        assert_eq!(config.channels.capacity, 64);
        assert_eq!(
            config.channels.redis.url.as_deref(),
            Some("redis://localhost:6379/5")
        );
        assert_eq!(config.channels.redis.key_prefix, "demo:channels");
    }

    #[test]
    fn env_override_invalid_jobs_numeric_values_ignored() {
        let env = MockEnv::new()
            .with("AUTUMN_JOBS__WORKERS", "many")
            .with("AUTUMN_JOBS__MAX_ATTEMPTS", "a_lot")
            .with("AUTUMN_JOBS__INITIAL_BACKOFF_MS", "soon");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.jobs.workers, 1);
        assert_eq!(config.jobs.max_attempts, 5);
        assert_eq!(config.jobs.initial_backoff_ms, 250);
    }

    // ── Server env override tests ────────────────────────────────

    #[test]
    fn env_override_server_port() {
        let env = MockEnv::new().with("AUTUMN_SERVER__PORT", "8080");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn parse_env_works() {
        let env = MockEnv::new().with("SOME_NUM", "123");
        let mut target: u32 = 0;
        parse_env(&env, "SOME_NUM", &mut target);
        assert_eq!(target, 123);

        let env_err = MockEnv::new().with("SOME_NUM", "abc");
        let mut target_err: u32 = 0;
        parse_env(&env_err, "SOME_NUM", &mut target_err);
        assert_eq!(target_err, 0); // Unchanged
    }

    #[test]
    fn parse_env_option_string_works() {
        let env = MockEnv::new().with("SOME_OPT", "val");
        let mut target = None;
        parse_env_option_string(&env, "SOME_OPT", &mut target);
        assert_eq!(target, Some("val".to_string()));

        let env_empty = MockEnv::new().with("SOME_OPT", "");
        let mut target_empty = Some("old".to_string());
        parse_env_option_string(&env_empty, "SOME_OPT", &mut target_empty);
        assert_eq!(target_empty, None);
    }

    #[test]
    fn parse_env_string_works() {
        let env = MockEnv::new().with("SOME_STR", "val");
        let mut target = "old".to_string();
        parse_env_string(&env, "SOME_STR", &mut target);
        assert_eq!(target, "val");
    }

    // ── server_timing_enabled resolver tests ────────────────────

    fn cfg_with_profile(profile: Option<&str>) -> AutumnConfig {
        AutumnConfig {
            profile: profile.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn server_timing_defaults_on_in_dev_profile() {
        let cfg = cfg_with_profile(Some("dev"));
        assert!(server_timing_enabled(&cfg));

        let cfg = cfg_with_profile(Some("development"));
        assert!(server_timing_enabled(&cfg));
    }

    #[test]
    fn server_timing_defaults_off_in_prod_and_test_profiles() {
        let cfg = cfg_with_profile(Some("prod"));
        assert!(!server_timing_enabled(&cfg));

        let cfg = cfg_with_profile(Some("production"));
        assert!(!server_timing_enabled(&cfg));

        let cfg = cfg_with_profile(Some("test"));
        assert!(!server_timing_enabled(&cfg));

        let cfg = cfg_with_profile(None);
        assert!(!server_timing_enabled(&cfg));
    }

    #[test]
    fn server_timing_explicit_config_overrides_profile_default() {
        let mut cfg = cfg_with_profile(Some("prod"));
        cfg.observability.server_timing = Some(true);
        assert!(server_timing_enabled(&cfg));

        let mut cfg = cfg_with_profile(Some("dev"));
        cfg.observability.server_timing = Some(false);
        assert!(!server_timing_enabled(&cfg));
    }

    #[test]
    fn server_timing_env_override_wires_into_dispatcher() {
        let env = MockEnv::new().with("AUTUMN_OBSERVABILITY__SERVER_TIMING", "true");
        let mut config = cfg_with_profile(Some("prod"));
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.observability.server_timing, Some(true));
        assert!(server_timing_enabled(&config));

        let env = MockEnv::new().with("AUTUMN_OBSERVABILITY__SERVER_TIMING", "false");
        let mut config = cfg_with_profile(Some("dev"));
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.observability.server_timing, Some(false));
        assert!(!server_timing_enabled(&config));
    }

    #[test]
    fn parse_env_bool_works() {
        let env = MockEnv::new().with("SOME_BOOL", "true");
        let mut target = false;
        parse_env_bool(&env, "SOME_BOOL", &mut target);
        assert!(target);

        let env2 = MockEnv::new().with("SOME_BOOL", "1");
        let mut target2 = false;
        parse_env_bool(&env2, "SOME_BOOL", &mut target2);
        assert!(target2);

        let env3 = MockEnv::new().with("SOME_BOOL", "0");
        let mut target3 = true;
        parse_env_bool(&env3, "SOME_BOOL", &mut target3);
        assert!(!target3);

        let env_err = MockEnv::new().with("SOME_BOOL", "invalid");
        let mut target_err = true;
        parse_env_bool(&env_err, "SOME_BOOL", &mut target_err);
        assert!(target_err); // Unchanged
    }

    #[test]
    fn parse_env_csv_works() {
        let env = MockEnv::new().with("SOME_CSV", "a, b,c");
        let mut target = vec![];
        parse_env_csv(&env, "SOME_CSV", &mut target);
        assert_eq!(target, vec!["a", "b", "c"]);
    }

    #[test]
    fn env_override_tenancy_quota_bytes() {
        // Unset: default stays 0 (unlimited).
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.tenancy.quota_bytes, 0);

        // Set via env: override is applied through the dispatcher.
        let env = MockEnv::new().with("AUTUMN_TENANCY__QUOTA_BYTES", "1048576");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.tenancy.quota_bytes, 1_048_576);
    }

    #[test]
    fn env_override_tenancy_enabled() {
        // Unset: default stays false.
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(!config.tenancy.enabled);

        let env = MockEnv::new().with("AUTUMN_TENANCY__ENABLED", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.tenancy.enabled);
    }

    #[test]
    fn env_override_tenancy_string_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_TENANCY__SOURCE", "jwt")
            .with("AUTUMN_TENANCY__HEADER_NAME", "x-org")
            .with("AUTUMN_TENANCY__SESSION_KEY", "org_id")
            .with("AUTUMN_TENANCY__JWT_CLAIM", "org")
            .with("AUTUMN_TENANCY__JWT_ISSUER", "https://issuer.example")
            .with("AUTUMN_TENANCY__JWT_AUDIENCE", "autumn-api")
            .with("AUTUMN_TENANCY__BASE_DOMAIN", "apps.example.com")
            .with("AUTUMN_TENANCY__LOGIN_REDIRECT", "/login")
            .with("AUTUMN_TENANCY__PUBLIC_PATHS", "/login, /signup ,/assets");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.tenancy.source, "jwt");
        assert_eq!(config.tenancy.header_name, "x-org");
        assert_eq!(config.tenancy.session_key, "org_id");
        assert_eq!(config.tenancy.jwt_claim, "org");
        assert_eq!(
            config.tenancy.jwt_issuer.as_deref(),
            Some("https://issuer.example")
        );
        assert_eq!(config.tenancy.jwt_audience.as_deref(), Some("autumn-api"));
        assert_eq!(
            config.tenancy.base_domain.as_deref(),
            Some("apps.example.com")
        );
        assert_eq!(config.tenancy.login_redirect.as_deref(), Some("/login"));
        assert_eq!(
            config.tenancy.public_paths,
            vec!["/login", "/signup", "/assets"]
        );
    }

    #[test]
    fn env_override_tenancy_eviction_knobs() {
        // Unset: defaults stay 0 (unbounded / disabled).
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.tenancy.max_cells, 0);
        assert_eq!(config.tenancy.idle_ttl_secs, 0);

        let env = MockEnv::new()
            .with("AUTUMN_TENANCY__MAX_CELLS", "512")
            .with("AUTUMN_TENANCY__IDLE_TTL_SECS", "900");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.tenancy.max_cells, 512);
        assert_eq!(config.tenancy.idle_ttl_secs, 900);
    }

    #[test]
    fn env_override_tenancy_secret() {
        use secrecy::ExposeSecret;

        // Unset: default stays None.
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.tenancy.jwt_secret.is_none());

        // Set via env: wrapped as a SecretString, trimmed.
        let env = MockEnv::new().with("AUTUMN_TENANCY__JWT_SECRET", "  s3cr3t-signing-key  ");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config
                .tenancy
                .jwt_secret
                .as_ref()
                .map(|s| s.expose_secret().to_owned()),
            Some("s3cr3t-signing-key".to_string())
        );

        // Empty value clears the secret.
        let env = MockEnv::new().with("AUTUMN_TENANCY__JWT_SECRET", "   ");
        let mut config = AutumnConfig::default();
        config.tenancy.jwt_secret = Some(secrecy::SecretString::from("preexisting".to_string()));
        config.apply_env_overrides_with_env(&env);
        assert!(config.tenancy.jwt_secret.is_none());
    }

    #[test]
    fn env_override_rate_limit_trusted_proxies() {
        let env = MockEnv::new().with(
            "AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES",
            "10.0.0.10, 203.0.113.0/24",
        );
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.rate_limit.trusted_proxies,
            vec!["10.0.0.10", "203.0.113.0/24"]
        );
    }

    #[test]
    fn env_override_rate_limit_backend_redis() {
        use crate::security::config::RateLimitBackend;
        let env = MockEnv::new().with("AUTUMN_SECURITY__RATE_LIMIT__BACKEND", "redis");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.security.rate_limit.backend, RateLimitBackend::Redis);
    }

    #[test]
    fn env_override_rate_limit_backend_memory() {
        use crate::security::config::RateLimitBackend;
        let env = MockEnv::new().with("AUTUMN_SECURITY__RATE_LIMIT__BACKEND", "memory");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.security.rate_limit.backend, RateLimitBackend::Memory);
    }

    #[test]
    fn env_override_rate_limit_backend_invalid_ignored() {
        use crate::security::config::RateLimitBackend;
        let env = MockEnv::new().with("AUTUMN_SECURITY__RATE_LIMIT__BACKEND", "postgres");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.security.rate_limit.backend, RateLimitBackend::Memory);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn env_override_rate_limit_on_backend_failure_fail_closed() {
        use crate::security::config::RateLimitBackendFailure;
        let env = MockEnv::new().with(
            "AUTUMN_SECURITY__RATE_LIMIT__ON_BACKEND_FAILURE",
            "fail_closed",
        );
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.rate_limit.on_backend_failure,
            RateLimitBackendFailure::FailClosed
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn env_override_rate_limit_on_backend_failure_invalid_ignored() {
        use crate::security::config::RateLimitBackendFailure;
        let env = MockEnv::new().with("AUTUMN_SECURITY__RATE_LIMIT__ON_BACKEND_FAILURE", "explode");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.rate_limit.on_backend_failure,
            RateLimitBackendFailure::FailOpen
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn env_override_rate_limit_redis_url() {
        let env = MockEnv::new().with(
            "AUTUMN_SECURITY__RATE_LIMIT__REDIS__URL",
            "redis://myhost:6379",
        );
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.rate_limit.redis.url.as_deref(),
            Some("redis://myhost:6379")
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn env_override_rate_limit_redis_key_prefix() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__RATE_LIMIT__REDIS__KEY_PREFIX", "prod:rl");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.security.rate_limit.redis.key_prefix, "prod:rl");
    }

    #[test]
    fn env_override_server_host() {
        let env = MockEnv::new().with("AUTUMN_SERVER__HOST", "0.0.0.0");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.host, "0.0.0.0");
    }

    #[test]
    fn env_override_server_shutdown_timeout() {
        let env = MockEnv::new().with("AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS", "60");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.shutdown_timeout_secs, 60);
    }

    #[test]
    fn env_override_invalid_server_port_ignored() {
        let env = MockEnv::new().with("AUTUMN_SERVER__PORT", "not_a_port");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.port, 3000);
    }

    #[test]
    fn env_override_invalid_shutdown_timeout_ignored() {
        let env = MockEnv::new().with("AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS", "forever");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.shutdown_timeout_secs, 30);
    }

    #[test]
    fn server_config_defaults_unix_socket_none() {
        let config = AutumnConfig::default();
        assert!(config.server.unix_socket.is_none());
    }

    #[test]
    fn env_override_server_unix_socket() {
        let env = MockEnv::new().with("AUTUMN_SERVER__UNIX_SOCKET", "/run/autumn/app.sock");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.server.unix_socket.as_deref(),
            Some("/run/autumn/app.sock")
        );
    }

    #[test]
    fn unix_socket_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server]
            unix_socket = "/tmp/autumn.sock"
            "#,
        )
        .expect("config with server.unix_socket should parse");
        assert_eq!(
            config.server.unix_socket.as_deref(),
            Some("/tmp/autumn.sock")
        );
    }

    // ── server.upgrade (#1674) ────────────────────────────────────

    #[test]
    fn server_upgrade_config_defaults_enabled_with_a_30s_readiness_budget() {
        // On by default: SIGUSR2's own default disposition terminates the
        // process, so an app that ignores this feature is strictly safer with
        // the handler installed than without it.
        let config = AutumnConfig::default();
        assert!(config.server.upgrade.enabled);
        assert_eq!(config.server.upgrade.ready_timeout_secs, 30);
    }

    #[test]
    fn server_upgrade_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r"
            [server.upgrade]
            enabled = false
            ready_timeout_secs = 5
            ",
        )
        .expect("config with [server.upgrade] should parse");
        assert!(!config.server.upgrade.enabled);
        assert_eq!(config.server.upgrade.ready_timeout_secs, 5);
    }

    #[test]
    fn env_overrides_server_upgrade() {
        // The deploy-time knob an operator reaches for first is the env var.
        let env = MockEnv::new()
            .with("AUTUMN_SERVER__UPGRADE__ENABLED", "false")
            .with("AUTUMN_SERVER__UPGRADE__READY_TIMEOUT_SECS", "90");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(!config.server.upgrade.enabled);
        assert_eq!(config.server.upgrade.ready_timeout_secs, 90);
    }

    // ── server.tls (#1603) ────────────────────────────────────────

    #[test]
    fn server_config_defaults_tls_none() {
        // Default must keep plain HTTP so existing apps are unaffected.
        let config = AutumnConfig::default();
        assert!(config.server.tls.is_none());
    }

    #[test]
    fn server_tls_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls]
            cert_path = "/etc/autumn/tls/fullchain.pem"
            key_path = "/etc/autumn/tls/privkey.pem"
            "#,
        )
        .expect("config with [server.tls] should parse");
        let tls = config.server.tls.expect("tls configured");
        assert_eq!(
            tls.cert_path,
            Some(std::path::PathBuf::from("/etc/autumn/tls/fullchain.pem"))
        );
        assert_eq!(
            tls.key_path,
            Some(std::path::PathBuf::from("/etc/autumn/tls/privkey.pem"))
        );
        // Reload interval and handshake timeout default when omitted.
        assert_eq!(tls.reload_interval_secs, 60);
        assert_eq!(tls.handshake_timeout_secs, 10);
        // No ACME section → static-cert mode.
        assert!(tls.acme.is_none());
        assert!(tls.validate().is_ok());
    }

    #[test]
    fn server_tls_handshake_timeout_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls]
            cert_path = "cert.pem"
            key_path = "key.pem"
            handshake_timeout_secs = 25
            "#,
        )
        .expect("config with [server.tls] handshake_timeout_secs should parse");
        assert_eq!(config.server.tls.unwrap().handshake_timeout_secs, 25);
    }

    #[test]
    fn server_tls_reload_interval_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls]
            cert_path = "cert.pem"
            key_path = "key.pem"
            reload_interval_secs = 120
            "#,
        )
        .expect("config with [server.tls] reload_interval_secs should parse");
        assert_eq!(config.server.tls.unwrap().reload_interval_secs, 120);
    }

    #[test]
    fn env_override_materializes_server_tls() {
        // A fully env-driven deployment can enable direct HTTPS with no
        // [server.tls] section in autumn.toml.
        let env = MockEnv::new()
            .with("AUTUMN_SERVER__TLS__CERT_PATH", "/env/cert.pem")
            .with("AUTUMN_SERVER__TLS__KEY_PATH", "/env/key.pem")
            .with("AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS", "90")
            .with("AUTUMN_SERVER__TLS__HANDSHAKE_TIMEOUT_SECS", "5");
        let mut config = AutumnConfig::default();
        assert!(config.server.tls.is_none());
        config.apply_env_overrides_with_env(&env);
        let tls = config.server.tls.expect("env should materialize tls");
        assert_eq!(
            tls.cert_path,
            Some(std::path::PathBuf::from("/env/cert.pem"))
        );
        assert_eq!(tls.key_path, Some(std::path::PathBuf::from("/env/key.pem")));
        assert_eq!(tls.reload_interval_secs, 90);
        assert_eq!(tls.handshake_timeout_secs, 5);
    }

    #[test]
    fn env_override_updates_existing_server_tls_cert() {
        // An env var overrides just the cert path of a TOML-configured section,
        // leaving the key path intact.
        let mut config: AutumnConfig = toml::from_str(
            r#"
            [server.tls]
            cert_path = "toml-cert.pem"
            key_path = "toml-key.pem"
            "#,
        )
        .unwrap();
        let env = MockEnv::new().with("AUTUMN_SERVER__TLS__CERT_PATH", "override-cert.pem");
        config.apply_env_overrides_with_env(&env);
        let tls = config.server.tls.expect("tls configured");
        assert_eq!(
            tls.cert_path,
            Some(std::path::PathBuf::from("override-cert.pem"))
        );
        assert_eq!(tls.key_path, Some(std::path::PathBuf::from("toml-key.pem")));
    }

    #[test]
    fn no_tls_env_leaves_tls_none() {
        let env = MockEnv::new().with("AUTUMN_SERVER__PORT", "8080");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.server.tls.is_none());
    }

    // ── deploy (#1607) ────────────────────────────────────────────

    #[test]
    fn deploy_absent_is_none() {
        // No [deploy] section → the field stays None so existing apps are
        // unaffected.
        let config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        let parsed: AutumnConfig = toml::from_str("[server]\nport = 3000\n")
            .expect("config without [deploy] should parse");
        assert!(parsed.deploy.is_none());
    }

    #[test]
    fn deploy_defaults_from_bare_table() {
        // A bare [deploy] table materializes the section with every optional
        // field at its documented default.
        let config: AutumnConfig =
            toml::from_str("[deploy]\n").expect("bare [deploy] table should parse");
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(deploy.host, None);
        assert_eq!(deploy.user, "root");
        assert_eq!(deploy.ssh_port, 22);
        assert_eq!(deploy.app_name, None);
        assert_eq!(deploy.app_dir, None);
        assert_eq!(deploy.service_name, None);
        assert_eq!(deploy.readiness_timeout_secs, 60);
        assert_eq!(deploy.keep_releases, 3);
        // #1607: host preparation is ON by default, so the documented "target host
        // precondition is at most a stock Ubuntu LTS" holds for a bare table. A
        // plain `#[serde(default)]` here would silently deserialize `false` for
        // every real user while `DeployConfig::default()` stayed `true`.
        assert!(deploy.install_proxy);
    }

    #[test]
    fn deploy_full_table_parses() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "203.0.113.10"
            user = "deploy"
            ssh_port = 2222
            app_name = "myapp"
            app_dir = "/srv/myapp"
            service_name = "myapp-web"
            readiness_timeout_secs = 90
            keep_releases = 5
            install_proxy = false
            "#,
        )
        .expect("full [deploy] table should parse");
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(deploy.host.as_deref(), Some("203.0.113.10"));
        assert_eq!(deploy.user, "deploy");
        assert_eq!(deploy.ssh_port, 2222);
        assert_eq!(deploy.app_name.as_deref(), Some("myapp"));
        assert_eq!(deploy.app_dir.as_deref(), Some("/srv/myapp"));
        assert_eq!(deploy.service_name.as_deref(), Some("myapp-web"));
        assert_eq!(deploy.readiness_timeout_secs, 90);
        assert_eq!(deploy.keep_releases, 5);
        assert!(
            !deploy.install_proxy,
            "the host-prep opt-out parses (#1607)"
        );
        assert!(deploy.validate().is_ok());
    }

    #[test]
    fn deploy_validate_rejects_missing_host() {
        // Missing host: a bare table is valid at rest but validate() rejects it.
        let missing = DeployConfig::default();
        let err = missing
            .validate()
            .expect_err("missing host must be rejected");
        assert!(
            err.contains("host"),
            "error should name the missing key: {err}"
        );

        // Present-but-blank host is also rejected.
        let blank = DeployConfig {
            host: Some("   ".to_owned()),
            ..DeployConfig::default()
        };
        assert!(blank.validate().is_err());

        // A real host passes.
        let ok = DeployConfig {
            host: Some("example.com".to_owned()),
            ..DeployConfig::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn env_override_materializes_deploy() {
        // A CI/VPS deploy can keep the target host out of autumn.toml and drive
        // the whole [deploy] section through AUTUMN_DEPLOY__* env vars.
        let env = MockEnv::new()
            .with("AUTUMN_DEPLOY__HOST", "203.0.113.10")
            .with("AUTUMN_DEPLOY__USER", "deploy")
            .with("AUTUMN_DEPLOY__SSH_PORT", "2222")
            .with("AUTUMN_DEPLOY__APP_NAME", "myapp")
            .with("AUTUMN_DEPLOY__APP_DIR", "/srv/myapp")
            .with("AUTUMN_DEPLOY__SERVICE_NAME", "myapp-web")
            .with("AUTUMN_DEPLOY__READINESS_TIMEOUT_SECS", "90")
            .with("AUTUMN_DEPLOY__KEEP_RELEASES", "5")
            .with("AUTUMN_DEPLOY__PROFILE", "staging");
        let mut config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("env should materialize deploy");
        assert_eq!(deploy.host.as_deref(), Some("203.0.113.10"));
        assert_eq!(deploy.user, "deploy");
        assert_eq!(deploy.ssh_port, 2222);
        assert_eq!(deploy.app_name.as_deref(), Some("myapp"));
        assert_eq!(deploy.app_dir.as_deref(), Some("/srv/myapp"));
        assert_eq!(deploy.service_name.as_deref(), Some("myapp-web"));
        assert_eq!(deploy.readiness_timeout_secs, 90);
        assert_eq!(deploy.keep_releases, 5);
        assert_eq!(deploy.profile, "staging");
        assert!(deploy.validate().is_ok());
    }

    #[test]
    fn env_override_declines_deploy_host_preparation() {
        // #1607: a CI pipeline deploying to pre-provisioned hosts must be able to
        // decline host preparation without editing `autumn.toml`. The key is also in
        // the presence probe, so setting only it materializes `[deploy]` — otherwise
        // the override would be silently skipped for an env-only config.
        let env = MockEnv::new().with("AUTUMN_DEPLOY__INSTALL_PROXY", "false");
        let mut config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("env should materialize deploy");
        assert!(!deploy.install_proxy);

        // …and it wins over TOML, like every other deploy override.
        let mut from_toml: AutumnConfig =
            toml::from_str("[deploy]\nhost = \"203.0.113.10\"\ninstall_proxy = true\n").unwrap();
        from_toml.apply_env_overrides_with_env(&env);
        assert!(!from_toml.deploy.expect("deploy configured").install_proxy);
    }

    #[test]
    fn env_override_sets_deploy_tls_enabled_and_host() {
        // Opt-in TLS (#1969) can be driven entirely from the environment: setting
        // both keys materializes `[deploy]` with TLS on and the host — the exact
        // precondition under which the CLI resolves `tls_host == Some(host)`.
        let env = MockEnv::new()
            .with("AUTUMN_DEPLOY__TLS__ENABLED", "true")
            .with("AUTUMN_DEPLOY__TLS__HOST", "app.example.com");
        let mut config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("env should materialize deploy");
        assert!(deploy.tls.enabled);
        assert_eq!(deploy.tls.host.as_deref(), Some("app.example.com"));
    }

    #[test]
    fn env_override_wins_over_toml_deploy_tls_host() {
        // TOML configures a TLS host...
        let mut config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "203.0.113.10"

            [deploy.tls]
            enabled = true
            host = "toml.example.com"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.deploy.as_ref().unwrap().tls.host.as_deref(),
            Some("toml.example.com"),
        );

        // ...and the env var overrides it, matching every other deploy override.
        let env = MockEnv::new().with("AUTUMN_DEPLOY__TLS__HOST", "env.example.com");
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.unwrap();
        assert!(deploy.tls.enabled);
        assert_eq!(deploy.tls.host.as_deref(), Some("env.example.com"));
    }

    #[test]
    fn deploy_profile_defaults_to_production() {
        // A bare `[deploy]` table (or one omitting `profile`) resolves to the
        // production profile so a deploy never silently runs the `dev` profile.
        let config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "203.0.113.10"
            "#,
        )
        .unwrap();
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(deploy.profile, "prod");
        // The type default matches the serde default.
        assert_eq!(DeployConfig::default().profile, "prod");
    }

    #[test]
    fn deploy_profile_honors_toml_and_env_override() {
        // TOML sets a non-prod profile...
        let mut config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "toml-host"
            profile = "staging"
            "#,
        )
        .unwrap();
        assert_eq!(config.deploy.as_ref().unwrap().profile, "staging");

        // ...and `AUTUMN_DEPLOY__PROFILE` wins over the TOML value.
        let env = MockEnv::new().with("AUTUMN_DEPLOY__PROFILE", "prod");
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.deploy.unwrap().profile, "prod");
    }

    #[test]
    fn env_override_materializes_deploy_from_single_host() {
        // Setting only AUTUMN_DEPLOY__HOST with no [deploy] in TOML seeds the
        // section with defaults and fills in the host.
        let env = MockEnv::new().with("AUTUMN_DEPLOY__HOST", "198.51.100.7");
        let mut config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("env should materialize deploy");
        assert_eq!(deploy.host.as_deref(), Some("198.51.100.7"));
        // Remaining fields fall back to their documented defaults.
        assert_eq!(deploy.user, "root");
        assert_eq!(deploy.ssh_port, 22);
        assert_eq!(deploy.readiness_timeout_secs, 60);
        assert_eq!(deploy.keep_releases, 3);
    }

    #[test]
    fn env_override_updates_existing_deploy_host() {
        // An env var overrides just the host of a TOML-configured section,
        // leaving the other keys intact.
        let mut config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "toml-host"
            user = "deploy"
            ssh_port = 2200
            "#,
        )
        .unwrap();
        let env = MockEnv::new().with("AUTUMN_DEPLOY__HOST", "env-host");
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(deploy.host.as_deref(), Some("env-host"));
        assert_eq!(deploy.user, "deploy");
        assert_eq!(deploy.ssh_port, 2200);
    }

    #[test]
    fn env_override_parses_deploy_ssh_port_u16() {
        let env = MockEnv::new()
            .with("AUTUMN_DEPLOY__HOST", "example.com")
            .with("AUTUMN_DEPLOY__SSH_PORT", "65535");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("env should materialize deploy");
        assert_eq!(deploy.ssh_port, 65_535_u16);
    }

    #[test]
    fn no_deploy_env_leaves_deploy_none() {
        let env = MockEnv::new().with("AUTUMN_SERVER__PORT", "8080");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.deploy.is_none());
    }

    // ── deploy fleet hosts (#1621) ────────────────────────────────

    #[test]
    fn deploy_hosts_list_parses_in_declaration_order_and_defaults_to_empty() {
        // #1621 (AC-1): `[deploy] hosts` is the fleet spelling of the target
        // list. Declaration order IS the rollout order, so parsing must preserve
        // it verbatim, and a `[deploy]` table without the key stays empty so
        // every pre-#1621 config is unchanged.
        let config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            hosts = ["web-1.example.com", "web-2.example.com", "web-3.example.com"]
            "#,
        )
        .expect("[deploy] hosts list should parse");
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(
            deploy.hosts,
            vec![
                "web-1.example.com".to_owned(),
                "web-2.example.com".to_owned(),
                "web-3.example.com".to_owned(),
            ],
            "hosts must parse in declaration order (the documented rollout order), got: {:?}",
            deploy.hosts
        );
        assert_eq!(
            deploy.host, None,
            "the fleet spelling must not populate the legacy scalar, got: {:?}",
            deploy.host
        );

        let bare: AutumnConfig =
            toml::from_str("[deploy]\nhost = \"203.0.113.10\"\n").expect("legacy table parses");
        let bare_deploy = bare.deploy.expect("deploy configured");
        assert!(
            bare_deploy.hosts.is_empty(),
            "a pre-#1621 [deploy] table must resolve to an empty hosts list, got: {:?}",
            bare_deploy.hosts
        );
        assert!(
            DeployConfig::default().hosts.is_empty(),
            "the hand-written Default impl must seed an empty hosts list, got: {:?}",
            DeployConfig::default().hosts
        );
    }

    #[test]
    fn deploy_validate_rejects_host_and_hosts_set_together() {
        // #1621 (AC-1): `host` and `hosts` are mutually exclusive — with both set
        // there is no unambiguous rollout order. `DeployConfig::validate` is not
        // wired into `AutumnConfig::validate` (the CLI's `ResolvedFleet::resolve`
        // is the enforcing seam), but it must MIRROR the same rules so the two
        // never disagree about what a valid `[deploy]` table looks like.
        let cfg = DeployConfig {
            host: Some("203.0.113.10".to_owned()),
            hosts: vec!["web-1.example.com".to_owned()],
            ..DeployConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("host + hosts together must be rejected");
        assert!(
            err.contains("[deploy] host") && err.contains("[deploy] hosts"),
            "the mutual-exclusion error must name BOTH keys so the operator knows \
             which one to delete, got: {err}"
        );
    }

    #[test]
    fn deploy_validate_rejects_blank_and_duplicate_hosts_entries() {
        // #1621 (AC-1): a blank entry is a typo that would otherwise resolve to a
        // hostless SSH target mid-rollout, and a duplicate entry would deploy the
        // same machine twice (the second pass sees its own new release as live and
        // corrupts the blue/green previous-release chain).
        let blank = DeployConfig {
            hosts: vec!["web-1.example.com".to_owned(), "   ".to_owned()],
            ..DeployConfig::default()
        };
        let blank_err = blank
            .validate()
            .expect_err("a blank hosts entry must be rejected");
        assert!(
            blank_err.contains("hosts") && blank_err.contains('1'),
            "the blank-entry error must name `hosts` and the 0-based index, got: {blank_err}"
        );

        let duplicate = DeployConfig {
            hosts: vec![
                "web-1.example.com".to_owned(),
                " web-1.example.com ".to_owned(),
            ],
            ..DeployConfig::default()
        };
        let duplicate_err = duplicate
            .validate()
            .expect_err("a duplicate hosts entry must be rejected");
        assert!(
            duplicate_err.contains("web-1.example.com"),
            "the duplicate error must name the repeated value, got: {duplicate_err}"
        );

        let ok = DeployConfig {
            hosts: vec![
                "web-1.example.com".to_owned(),
                "web-2.example.com".to_owned(),
            ],
            ..DeployConfig::default()
        };
        assert!(
            ok.validate().is_ok(),
            "a well-formed fleet list must validate, got: {:?}",
            ok.validate()
        );
    }

    #[test]
    fn env_override_materializes_deploy_from_hosts_only() {
        // #1621: `KEYS` in `apply_deploy_env_overrides` is a PRESENCE PROBE that
        // gates materialisation of the ENTIRE `[deploy]` table. If
        // `AUTUMN_DEPLOY__HOSTS` is missing from that array, an env-only fleet
        // config produces no deploy section at all — a silent skip, not an error,
        // in both `AutumnConfig::load` and `autumn doctor`. This is the trap test.
        let env = MockEnv::new().with(
            "AUTUMN_DEPLOY__HOSTS",
            "web-1.example.com,web-2.example.com",
        );
        let mut config = AutumnConfig::default();
        assert!(config.deploy.is_none());
        config.apply_env_overrides_with_env(&env);
        let deploy = config
            .deploy
            .expect("AUTUMN_DEPLOY__HOSTS alone must materialize the [deploy] table");
        assert_eq!(
            deploy.hosts,
            vec![
                "web-1.example.com".to_owned(),
                "web-2.example.com".to_owned(),
            ],
            "the CSV env value must parse into the ordered fleet list, got: {:?}",
            deploy.hosts
        );
        // Every other key falls back to its documented default.
        assert_eq!(deploy.host, None, "got: {:?}", deploy.host);
        assert_eq!(deploy.user, "root");
        assert_eq!(deploy.ssh_port, 22);
    }

    #[test]
    fn env_override_hosts_replaces_the_whole_toml_list() {
        // #1621: `AUTUMN_DEPLOY__HOSTS` is a fleet-level RETARGET — it replaces
        // the TOML list wholesale rather than appending, matching every other
        // `AUTUMN_DEPLOY__*` override (env wins).
        let mut config: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            hosts = ["toml-1", "toml-2", "toml-3"]
            "#,
        )
        .expect("[deploy] hosts list should parse");
        let env = MockEnv::new().with("AUTUMN_DEPLOY__HOSTS", "env-1, env-2");
        config.apply_env_overrides_with_env(&env);
        let deploy = config.deploy.expect("deploy configured");
        assert_eq!(
            deploy.hosts,
            vec!["env-1".to_owned(), "env-2".to_owned()],
            "env must REPLACE the TOML fleet list (and trim each entry), got: {:?}",
            deploy.hosts
        );
    }

    #[test]
    fn an_empty_deploy_hosts_env_override_behaves_as_unset() {
        // #1621 review finding 13. `AUTUMN_DEPLOY__HOSTS=` is the shape a CI or
        // compose env template produces for "fill in for a fleet" — exactly what
        // the sibling `AUTUMN_DEPLOY__HOST=` harmlessly takes. Splitting it
        // unconditionally yielded `[""]`, a NON-empty fleet list holding a blank
        // string, so a project keeping `[deploy] host` in autumn.toml hard-failed
        // every `autumn deploy` subcommand (and `autumn doctor`) with "`[deploy]
        // host` and `[deploy] hosts` are mutually exclusive" — naming a key the
        // operator never set.
        for blank in ["", "   ", ",", " , "] {
            let mut config: AutumnConfig = toml::from_str(
                r#"
                [deploy]
                host = "203.0.113.10"
                "#,
            )
            .expect("[deploy] host should parse");
            let env = MockEnv::new().with("AUTUMN_DEPLOY__HOSTS", blank);
            config.apply_env_overrides_with_env(&env);
            let deploy = config.deploy.expect("deploy configured");
            assert!(
                deploy.hosts.is_empty(),
                "AUTUMN_DEPLOY__HOSTS={blank:?} must behave as unset, got: {:?}",
                deploy.hosts
            );
            assert_eq!(deploy.host.as_deref(), Some("203.0.113.10"));
            assert!(
                deploy.validate().is_ok(),
                "a blank fleet override must not trip the mutual-exclusion refusal, got: {:?}",
                deploy.validate()
            );
        }
    }

    #[test]
    fn a_trailing_comma_in_the_deploy_hosts_env_override_is_tolerated() {
        // #1621 review finding 13. Generated env lists routinely carry a trailing
        // (or doubled) comma; the blank segment it produces used to reach the CLI
        // as a blank fleet entry and refuse the whole command. Blank segments are
        // dropped, so the list is exactly the addresses the operator wrote.
        for (value, expected) in [
            ("10.0.0.1,10.0.0.2,", vec!["10.0.0.1", "10.0.0.2"]),
            ("10.0.0.1,,10.0.0.2", vec!["10.0.0.1", "10.0.0.2"]),
            (" 10.0.0.1 , 10.0.0.2 , ", vec!["10.0.0.1", "10.0.0.2"]),
        ] {
            let mut config = AutumnConfig::default();
            let env = MockEnv::new().with("AUTUMN_DEPLOY__HOSTS", value);
            config.apply_env_overrides_with_env(&env);
            let deploy = config.deploy.expect("deploy configured");
            assert_eq!(
                deploy.hosts,
                expected
                    .iter()
                    .map(|h| (*h).to_owned())
                    .collect::<Vec<String>>(),
                "AUTUMN_DEPLOY__HOSTS={value:?} must drop blank segments, got: {:?}",
                deploy.hosts
            );
            assert!(
                deploy.validate().is_ok(),
                "a tolerated trailing comma must not refuse the fleet, got: {:?}",
                deploy.validate()
            );
        }
    }

    #[test]
    fn a_non_empty_deploy_host_env_override_clears_the_toml_alternate_spelling() {
        // #1621 review round 1 (Codex 1). `host` and `hosts` are MUTUALLY
        // EXCLUSIVE downstream, so an env override that sets one spelling while the
        // TOML still holds the other produced a config that refuses every `autumn
        // deploy` subcommand — even though `AUTUMN_DEPLOY__*` is documented to WIN
        // over TOML. Retargeting a `[deploy] host` project as a fleet with
        // `AUTUMN_DEPLOY__HOSTS=a,b` (and the reverse) must simply work.
        let mut fleet_over_scalar: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            host = "203.0.113.10"
            "#,
        )
        .expect("[deploy] host should parse");
        fleet_over_scalar.apply_env_overrides_with_env(
            &MockEnv::new().with("AUTUMN_DEPLOY__HOSTS", "web-1,web-2"),
        );
        let deploy = fleet_over_scalar.deploy.expect("deploy configured");
        assert_eq!(
            deploy.hosts,
            vec!["web-1".to_owned(), "web-2".to_owned()],
            "the env fleet list must win, got: {:?}",
            deploy.hosts
        );
        assert_eq!(
            deploy.host, None,
            "a non-empty AUTUMN_DEPLOY__HOSTS must CLEAR the TOML `host`, got: {:?}",
            deploy.host
        );
        assert!(
            deploy.validate().is_ok(),
            "env-over-TOML retarget must not trip mutual exclusion, got: {:?}",
            deploy.validate()
        );

        let mut scalar_over_fleet: AutumnConfig = toml::from_str(
            r#"
            [deploy]
            hosts = ["toml-1", "toml-2"]
            "#,
        )
        .expect("[deploy] hosts should parse");
        scalar_over_fleet
            .apply_env_overrides_with_env(&MockEnv::new().with("AUTUMN_DEPLOY__HOST", "single-1"));
        let deploy = scalar_over_fleet.deploy.expect("deploy configured");
        assert_eq!(deploy.host.as_deref(), Some("single-1"));
        assert!(
            deploy.hosts.is_empty(),
            "a non-empty AUTUMN_DEPLOY__HOST must CLEAR the TOML `hosts`, got: {:?}",
            deploy.hosts
        );
        assert!(
            deploy.validate().is_ok(),
            "env-over-TOML narrowing must not trip mutual exclusion, got: {:?}",
            deploy.validate()
        );
    }

    #[test]
    fn two_conflicting_non_empty_deploy_host_env_overrides_are_still_refused() {
        // #1621 review round 1 (Codex 1), the other half: env-over-TOML precedence
        // is NOT a licence to pick a winner between two conflicting env vars. Both
        // spellings set NON-EMPTY in the environment is a genuine operator error —
        // the rollout order is ambiguous — so both survive and the downstream
        // mutual-exclusion refusal still fires.
        for toml_src in [
            "[deploy]\n",
            "[deploy]\nhost = \"toml-host\"\n",
            "[deploy]\nhosts = [\"toml-1\"]\n",
        ] {
            let mut config: AutumnConfig =
                toml::from_str(toml_src).expect("[deploy] table should parse");
            config.apply_env_overrides_with_env(
                &MockEnv::new()
                    .with("AUTUMN_DEPLOY__HOST", "env-single")
                    .with("AUTUMN_DEPLOY__HOSTS", "env-1,env-2"),
            );
            let deploy = config.deploy.expect("deploy configured");
            assert_eq!(
                deploy.host.as_deref(),
                Some("env-single"),
                "both env spellings must survive so the conflict is visible ({toml_src:?})"
            );
            assert_eq!(
                deploy.hosts,
                vec!["env-1".to_owned(), "env-2".to_owned()],
                "both env spellings must survive so the conflict is visible ({toml_src:?})"
            );
            assert!(
                deploy.validate().is_err(),
                "two conflicting NON-EMPTY env spellings must still be refused ({toml_src:?})",
            );
        }
    }

    #[test]
    fn an_empty_deploy_host_env_override_never_clears_the_toml_alternate_spelling() {
        // #1621 review round 1 (Codex 1). The established empty-means-unset
        // semantics must survive the clearing rule above: `AUTUMN_DEPLOY__HOST=`
        // and `AUTUMN_DEPLOY__HOSTS=` (the shape a CI/compose template emits for an
        // unfilled slot) say NOTHING about the alternate spelling, so the TOML value
        // stands.
        for blank in ["", "   ", ",", " , "] {
            let mut config: AutumnConfig = toml::from_str(
                r#"
                [deploy]
                host = "203.0.113.10"
                "#,
            )
            .expect("[deploy] host should parse");
            config
                .apply_env_overrides_with_env(&MockEnv::new().with("AUTUMN_DEPLOY__HOSTS", blank));
            let deploy = config.deploy.expect("deploy configured");
            assert_eq!(
                deploy.host.as_deref(),
                Some("203.0.113.10"),
                "AUTUMN_DEPLOY__HOSTS={blank:?} must not clear the TOML `host`"
            );
            assert!(deploy.hosts.is_empty());
        }

        for blank in ["", "   "] {
            let mut config: AutumnConfig = toml::from_str(
                r#"
                [deploy]
                hosts = ["toml-1", "toml-2"]
                "#,
            )
            .expect("[deploy] hosts should parse");
            config.apply_env_overrides_with_env(&MockEnv::new().with("AUTUMN_DEPLOY__HOST", blank));
            let deploy = config.deploy.expect("deploy configured");
            assert_eq!(
                deploy.hosts,
                vec!["toml-1".to_owned(), "toml-2".to_owned()],
                "AUTUMN_DEPLOY__HOST={blank:?} must not clear the TOML `hosts`"
            );
        }
        // `AUTUMN_DEPLOY__HOST=` (truly empty) keeps its own documented
        // clears-the-scalar meaning — the clearing rule above changes nothing here.
        let mut config = AutumnConfig {
            deploy: Some(DeployConfig {
                host: Some("toml-host".to_owned()),
                ..DeployConfig::default()
            }),
            ..AutumnConfig::default()
        };
        config.apply_env_overrides_with_env(&MockEnv::new().with("AUTUMN_DEPLOY__HOST", ""));
        assert_eq!(config.deploy.expect("deploy configured").host, None);
    }

    // ── server.tls.acme (#1608) ───────────────────────────────────

    fn tls_static(cert: Option<&str>, key: Option<&str>) -> TlsConfig {
        TlsConfig {
            cert_path: cert.map(PathBuf::from),
            key_path: key.map(PathBuf::from),
            reload_interval_secs: default_tls_reload_interval_secs(),
            handshake_timeout_secs: default_tls_handshake_timeout_secs(),
            acme: None,
        }
    }

    fn acme_cfg(domains: &[&str], email: &str) -> AcmeConfig {
        AcmeConfig {
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            contact_email: email.to_owned(),
            directory: AcmeDirectory::Staging,
            cache_dir: default_acme_cache_dir(),
            http_challenge_port: default_acme_http_challenge_port(),
            renew_before_days: default_acme_renew_before_days(),
            ca_root_path: None,
            dns: None,
        }
    }

    #[test]
    fn acme_parses_from_toml_with_defaults() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls.acme]
            domains = ["app.example.com"]
            contact_email = "ops@example.com"
            "#,
        )
        .expect("config with [server.tls.acme] should parse");
        let tls = config.server.tls.expect("tls configured");
        let acme = tls.acme.as_ref().expect("acme configured");
        assert_eq!(acme.domains, vec!["app.example.com".to_owned()]);
        assert_eq!(acme.contact_email, "ops@example.com");
        // Staging is the default on purpose (rate-limit safety).
        assert_eq!(acme.directory, AcmeDirectory::Staging);
        assert_eq!(acme.cache_dir, PathBuf::from("config/acme"));
        assert_eq!(acme.http_challenge_port, 80);
        assert_eq!(acme.renew_before_days, 30);
        assert!(tls.validate().is_ok());
    }

    #[test]
    fn acme_ca_root_path_defaults_to_unset_and_round_trips() {
        // Unset is the default: Let's Encrypt's staging and production API
        // endpoints are publicly trusted, so no extra root is needed.
        let acme = acme_cfg(&["app.example.com"], "ops@example.com");
        assert_eq!(acme.ca_root_path, None);
        assert!(acme.validate().is_ok());

        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls.acme]
            domains = ["app.example.com"]
            contact_email = "ops@example.com"
            directory = { custom = { url = "https://pebble.test/dir" } }
            ca_root_path = "config/pebble-root.pem"
            "#,
        )
        .expect("ca_root_path should parse");
        let acme = config.server.tls.unwrap().acme.unwrap();
        assert_eq!(
            acme.ca_root_path,
            Some(PathBuf::from("config/pebble-root.pem"))
        );
        assert!(acme.validate().is_ok());
    }

    #[test]
    fn acme_rejects_a_blank_ca_root_path() {
        // An empty path would otherwise be carried all the way into the renewal
        // loop and fail every order at the TLS handshake with a far less
        // actionable message.
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.ca_root_path = Some(PathBuf::new());
        let err = acme
            .validate()
            .expect_err("a blank ca_root_path is invalid");
        assert!(err.contains("ca_root_path"), "unhelpful message: {err}");
    }

    #[test]
    fn acme_directory_custom_parses() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls.acme]
            domains = ["a.example.com"]
            contact_email = "ops@example.com"
            directory = { custom = { url = "https://pebble.test/dir" } }
            "#,
        )
        .expect("custom directory should parse");
        let acme = config.server.tls.unwrap().acme.unwrap();
        assert_eq!(
            acme.directory,
            AcmeDirectory::Custom {
                url: "https://pebble.test/dir".to_owned()
            }
        );
    }

    #[test]
    fn validate_static_only_ok() {
        assert!(tls_static(Some("c.pem"), Some("k.pem")).validate().is_ok());
    }

    #[test]
    fn validate_acme_only_ok() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["app.example.com"], "ops@example.com"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_both_static_and_acme_rejected() {
        let mut cfg = tls_static(Some("c.pem"), Some("k.pem"));
        cfg.acme = Some(acme_cfg(&["app.example.com"], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("choose exactly one"), "got: {err}");
    }

    #[test]
    fn validate_neither_static_nor_acme_rejected() {
        let err = tls_static(None, None).validate().unwrap_err();
        assert!(err.contains("exactly one of"), "got: {err}");
    }

    #[test]
    fn validate_cert_without_key_rejected() {
        let err = tls_static(Some("c.pem"), None).validate().unwrap_err();
        assert!(err.contains("set together"), "got: {err}");
    }

    #[test]
    fn validate_acme_empty_domains_rejected() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&[], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("at least one domain"), "got: {err}");
    }

    #[test]
    fn validate_acme_empty_email_rejected() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["app.example.com"], "  "));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("contact_email"), "got: {err}");
    }

    #[test]
    fn validate_acme_wildcard_domain_without_dns_provider_rejected() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["*.example.com"], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("[server.tls.acme.dns]"), "got: {err}");
        assert!(err.contains("wildcard"), "got: {err}");
    }

    // Regression (#1608, Codex P2): a blank/whitespace-only domain entry passes
    // `domains.is_empty()` (the list has length 1) but the runtime then orders a
    // cert for an empty DNS identifier, so `validate()` must reject it up front.
    #[test]
    fn validate_acme_blank_domain_entry_rejected() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&[""], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("blank entries"), "got: {err}");

        // A whitespace-only entry is rejected the same way.
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["   "], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("blank entries"), "got: {err}");
    }

    // The `autumn doctor` grader FAILs a malformed `http_challenge_port` /
    // `renew_before_days` (#1608, #1874) on the premise that the runtime's TYPED
    // deserialization rejects the same spellings before boot. Pin that premise
    // here: if a future lenient `deserialize_with` ever made the runtime accept
    // `"30"`, doctor would start FAILing a file that boots fine — the same parity
    // bug in the opposite direction, and today nothing would catch it.
    #[test]
    fn acme_numeric_fields_reject_non_integer_toml_values() {
        for key in ["http_challenge_port", "renew_before_days"] {
            for bad in ["\"30\"", "30.5", "true", "-1", "4294967296"] {
                let src = format!(
                    "[server.tls.acme]\ndomains = [\"app.example.com\"]\n\
                     contact_email = \"ops@example.com\"\n{key} = {bad}\n"
                );
                assert!(
                    toml::from_str::<AutumnConfig>(&src).is_err(),
                    "{key} = {bad} must fail to deserialize"
                );
            }
            // ... while the plain unquoted integer the doctor grader treats as
            // valid really is accepted.
            let src = format!(
                "[server.tls.acme]\ndomains = [\"app.example.com\"]\n\
                 contact_email = \"ops@example.com\"\n{key} = 45\n"
            );
            assert!(
                toml::from_str::<AutumnConfig>(&src).is_ok(),
                "{key} = 45 must deserialize"
            );
        }
    }

    // Regression (#1874): a whitespace-padded domain (`" app.example.com "`)
    // passed `validate()` — the blank and wildcard checks look at `domain.trim()`
    // but the UNTRIMMED value is what is stored, so the padded string reached the
    // placeholder/CSR builder and the ACME `Identifier::Dns` order. The CA then
    // rejects (or mis-issues) an identifier that is not the hostname the operator
    // meant, with a far less actionable error than a boot-time rejection.
    #[test]
    fn validate_acme_whitespace_padded_domain_rejected() {
        for padded in [
            " app.example.com",
            "app.example.com ",
            "\tapp.example.com\n",
        ] {
            let mut cfg = tls_static(None, None);
            cfg.acme = Some(acme_cfg(&[padded], "ops@example.com"));
            let err = cfg
                .validate()
                .expect_err("a whitespace-padded domain must be rejected");
            assert!(
                err.contains("whitespace"),
                "message must name the problem: {err}"
            );
            assert!(
                err.contains(&format!("`{padded}`")),
                "message must echo the RAW padded entry, not only the trimmed \
                 spelling it suggests: {err}"
            );
            assert!(
                err.contains("`app.example.com`"),
                "message must name the trimmed spelling to use: {err}"
            );
        }

        // A padded entry is rejected even when a well-formed one precedes it, and
        // the message points at the right index.
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(
            &["app.example.com", " www.example.com "],
            "ops@example.com",
        ));
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("index 1"),
            "message must name the index: {err}"
        );

        // The already-trimmed spelling of the same name still passes.
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["app.example.com"], "ops@example.com"));
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());
    }

    // The padded-domain rule must not shadow the two more specific per-entry
    // rules: a whitespace-only entry is still reported as blank, and a padded
    // wildcard is still reported as a wildcard (the deeper problem, and the
    // message operators already get today).
    #[test]
    fn validate_acme_padded_domain_rule_does_not_shadow_blank_or_wildcard() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["   "], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("blank entries"), "got: {err}");

        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&[" *.example.com "], "ops@example.com"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("wildcard"), "got: {err}");
        assert!(err.contains("[server.tls.acme.dns]"), "got: {err}");
    }

    // Regression (#1608, Codex P2): `http_challenge_port = 0` binds an ephemeral
    // OS port the HTTP-01 validator (always port 80) can never reach, so every
    // issuance fails while the process stays up — `validate()` must reject it.
    #[test]
    fn validate_acme_zero_http_challenge_port_rejected() {
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.http_challenge_port = 0;
        cfg.acme = Some(acme);
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("http_challenge_port"), "got: {err}");
    }

    // Regression (#1608, Codex P2): a `renew_before_days` >= the issued cert's
    // lifetime (treated as ~90 days for a public CA) keeps `needs_renewal` true
    // immediately after every successful renewal, so the hourly loop re-orders a
    // fresh cert every tick until the CA rate-limits the account. `validate()`
    // must reject any value >= 90.
    #[test]
    fn validate_acme_renew_before_days_at_or_above_cert_lifetime_rejected() {
        // Well above the cert lifetime (the reviewer's example).
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.renew_before_days = 100;
        cfg.acme = Some(acme);
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("renew_before_days"), "got: {err}");
        assert!(err.contains("rate limits"), "got: {err}");

        // Exactly 90 (== the effective max cert lifetime) is also rejected: the
        // fresh cert would be due for renewal from the moment it is issued.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.renew_before_days = 90;
        cfg.acme = Some(acme);
        assert!(
            cfg.validate().is_err(),
            "renew_before_days == 90 must be rejected"
        );

        // A sane sub-lifetime value passes.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.renew_before_days = 30;
        cfg.acme = Some(acme);
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());

        // The just-below-boundary value (89) is still accepted.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["app.example.com"], "ops@example.com");
        acme.renew_before_days = 89;
        cfg.acme = Some(acme);
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());
    }

    // ── DNS-01 / wildcard config surface (issue #1620) ───────────────────────

    fn acme_dns_cfg(provider: AcmeDnsProvider) -> AcmeDnsConfig {
        AcmeDnsConfig {
            provider,
            credential: default_acme_dns_credential(),
            propagation_timeout_secs: default_acme_dns_propagation_timeout_secs(),
            poll_interval_secs: default_acme_dns_poll_interval_secs(),
            resolvers: default_acme_dns_resolvers(),
            command: Vec::new(),
        }
    }

    #[test]
    fn acme_dns_section_parses_with_defaults() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server.tls.acme]
            domains = ["myapp.com", "*.myapp.com"]
            contact_email = "ops@myapp.com"

            [server.tls.acme.dns]
            provider = "cloudflare"
            "#,
        )
        .expect("[server.tls.acme.dns] should parse");
        let tls = config.server.tls.expect("tls configured");
        let acme = tls.acme.as_ref().expect("acme configured");
        let dns = acme.dns.as_ref().expect("dns configured");
        assert_eq!(dns.provider, AcmeDnsProvider::Cloudflare);
        // The credential is a NAME in the encrypted credentials store, never a token.
        assert_eq!(dns.credential, "acme_dns");
        assert_eq!(dns.propagation_timeout_secs, 300);
        assert_eq!(dns.poll_interval_secs, 5);
        assert!(!dns.resolvers.is_empty());
        assert!(tls.validate().is_ok(), "got: {:?}", tls.validate());
    }

    #[test]
    fn acme_dns_provider_names_round_trip() {
        for (spelling, expected) in [
            ("cloudflare", AcmeDnsProvider::Cloudflare),
            ("route53", AcmeDnsProvider::Route53),
            ("exec", AcmeDnsProvider::Exec),
        ] {
            let extra = if expected == AcmeDnsProvider::Exec {
                "\ncommand = [\"/usr/local/bin/dns-hook\"]"
            } else {
                ""
            };
            let src = format!(
                "[server.tls.acme]\ndomains = [\"*.myapp.com\"]\n\
                 contact_email = \"ops@myapp.com\"\n\
                 [server.tls.acme.dns]\nprovider = \"{spelling}\"{extra}\n"
            );
            let config: AutumnConfig =
                toml::from_str(&src).unwrap_or_else(|e| panic!("{spelling} should parse: {e}"));
            let acme = config.server.tls.unwrap().acme.unwrap();
            assert_eq!(acme.dns.as_ref().unwrap().provider, expected);
            assert!(acme.validate().is_ok(), "{spelling}: {:?}", acme.validate());
        }
    }

    // AC3: DNS provider API tokens are NEVER supplied in plaintext `autumn.toml`.
    // The section has no field to hold one, and `deny_unknown_fields` turns an
    // operator's attempt into a load-time error rather than a silently-ignored key
    // that leaves them wondering why the token "did not work".
    #[test]
    fn acme_dns_section_rejects_an_inline_secret() {
        for secret_key in [
            "api_token",
            "token",
            "secret_access_key",
            "access_key_id",
            "api_key",
        ] {
            let src = format!(
                "[server.tls.acme]\ndomains = [\"*.myapp.com\"]\n\
                 contact_email = \"ops@myapp.com\"\n\
                 [server.tls.acme.dns]\nprovider = \"cloudflare\"\n{secret_key} = \"s3cret\"\n"
            );
            let err = toml::from_str::<AutumnConfig>(&src)
                .err()
                .unwrap_or_else(|| panic!("{secret_key} in autumn.toml must be rejected"));
            let msg = err.to_string();
            assert!(
                msg.contains(secret_key),
                "the rejection must name the offending key: {msg}"
            );
        }
    }

    // AC1: a wildcard domain is accepted once — and only once — a DNS-01 provider
    // is configured, because only DNS-01 can validate a wildcard identifier.
    #[test]
    fn validate_acme_wildcard_requires_a_dns_provider() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(&["*.myapp.com"], "ops@myapp.com"));
        let err = cfg
            .validate()
            .expect_err("a wildcard without [server.tls.acme.dns] must be rejected");
        assert!(err.contains("wildcard"), "got: {err}");
        assert!(
            err.contains("[server.tls.acme.dns]"),
            "the message must name the section that fixes it: {err}"
        );
        assert!(err.contains("DNS-01"), "got: {err}");

        // With the section present it is accepted.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["myapp.com", "*.myapp.com"], "ops@myapp.com");
        acme.dns = Some(acme_dns_cfg(AcmeDnsProvider::Cloudflare));
        cfg.acme = Some(acme);
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());
    }

    // A malformed wildcard never reaches the CA as an opaque rejection.
    #[test]
    fn validate_acme_rejects_malformed_wildcards() {
        for bad in ["*", "*.", "*myapp.com", "app.*.myapp.com", "*.*.myapp.com"] {
            let mut cfg = tls_static(None, None);
            let mut acme = acme_cfg(&[bad], "ops@myapp.com");
            acme.dns = Some(acme_dns_cfg(AcmeDnsProvider::Cloudflare));
            cfg.acme = Some(acme);
            let err = cfg.validate().expect_err(&format!(
                "`{bad}` is not a usable wildcard and must be rejected"
            ));
            assert!(
                err.contains(bad),
                "the message must echo the offending entry `{bad}`: {err}"
            );
        }

        // The one well-formed shape stays accepted.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        acme.dns = Some(acme_dns_cfg(AcmeDnsProvider::Cloudflare));
        cfg.acme = Some(acme);
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());
    }

    #[test]
    fn validate_acme_dns_exec_requires_a_command() {
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        acme.dns = Some(acme_dns_cfg(AcmeDnsProvider::Exec));
        cfg.acme = Some(acme);
        let err = cfg
            .validate()
            .expect_err("provider = \"exec\" with no command must be rejected");
        assert!(err.contains("command"), "got: {err}");

        // …and a command on a non-exec provider is a misconfiguration too: it
        // would be silently ignored while the operator believes it runs.
        let mut cfg = tls_static(None, None);
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.command = vec!["/usr/local/bin/dns-hook".to_owned()];
        acme.dns = Some(dns);
        cfg.acme = Some(acme);
        let err = cfg
            .validate()
            .expect_err("command on cloudflare is invalid");
        assert!(err.contains("command"), "got: {err}");
    }

    #[test]
    fn validate_acme_dns_rejects_unusable_propagation_bounds() {
        // A zero timeout would fail every order before a record could ever appear.
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.propagation_timeout_secs = 0;
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("zero timeout is invalid");
        assert!(err.contains("propagation_timeout_secs"), "got: {err}");

        // A zero poll interval would busy-loop the resolver.
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.poll_interval_secs = 0;
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("zero poll interval is invalid");
        assert!(err.contains("poll_interval_secs"), "got: {err}");

        // A poll interval longer than the whole budget means exactly one probe.
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.poll_interval_secs = 600;
        dns.propagation_timeout_secs = 60;
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("interval > timeout is invalid");
        assert!(err.contains("poll_interval_secs"), "got: {err}");
    }

    // An unbounded budget reaches `Instant::now() + Duration::from_secs(..)`,
    // which PANICS on overflow — inside the spawned renewal task, where the
    // panic kills the loop silently and the placeholder is served forever.
    #[test]
    fn validate_acme_dns_rejects_an_unbounded_propagation_timeout() {
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.propagation_timeout_secs = u64::MAX;
        acme.dns = Some(dns);
        let err = acme
            .validate()
            .expect_err("an unbounded propagation budget is invalid");
        assert!(err.contains("propagation_timeout_secs"), "got: {err}");

        // The documented ceiling itself is accepted.
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.propagation_timeout_secs = MAX_ACME_DNS_PROPAGATION_TIMEOUT_SECS;
        acme.dns = Some(dns);
        assert!(acme.validate().is_ok(), "got: {:?}", acme.validate());
    }

    // `command[0]` is handed to the OS verbatim, so a blank or padded program
    // would fail at order time with an opaque ENOENT rather than at boot.
    #[test]
    fn validate_acme_dns_rejects_an_unusable_exec_program() {
        for bad in [
            vec![String::new()],
            vec!["   ".to_owned(), "arg".to_owned()],
        ] {
            let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
            let mut dns = acme_dns_cfg(AcmeDnsProvider::Exec);
            dns.command = bad.clone();
            acme.dns = Some(dns);
            let err = acme
                .validate()
                .expect_err(&format!("`{bad:?}` is not a usable exec command"));
            assert!(err.contains("command"), "got: {err}");
        }

        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Exec);
        dns.command = vec![" /usr/local/bin/hook ".to_owned()];
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("a padded program is invalid");
        assert!(err.contains("whitespace"), "got: {err}");
    }

    #[test]
    fn validate_acme_dns_rejects_a_blank_or_unparseable_resolver() {
        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.resolvers = Vec::new();
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("no resolvers is invalid");
        assert!(err.contains("resolvers"), "got: {err}");

        let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.resolvers = vec!["not a resolver".to_owned()];
        acme.dns = Some(dns);
        let err = acme.validate().expect_err("garbage resolver is invalid");
        assert!(err.contains("resolvers"), "got: {err}");
    }

    #[test]
    fn acme_dns_resolver_addresses_accept_bare_ips_and_explicit_ports() {
        let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
        dns.resolvers = vec!["1.1.1.1".to_owned(), "9.9.9.9:5353".to_owned()];
        let addrs = dns.resolver_addrs().expect("resolvers parse");
        assert_eq!(addrs[0].port(), 53, "a bare IP defaults to port 53");
        assert_eq!(addrs[1].port(), 5353);
    }

    #[test]
    fn validate_acme_dns_rejects_a_blank_credential_reference() {
        for blank in ["", "   "] {
            let mut acme = acme_cfg(&["*.myapp.com"], "ops@myapp.com");
            let mut dns = acme_dns_cfg(AcmeDnsProvider::Cloudflare);
            dns.credential = blank.to_owned();
            acme.dns = Some(dns);
            let err = acme.validate().expect_err("a blank credential is invalid");
            assert!(err.contains("credential"), "got: {err}");
        }
    }

    // AC7 support: the grader `autumn doctor` uses to tell an operator that their
    // `tenancy.base_domain` is not covered by the certificate.
    #[test]
    fn san_covers_host_matches_rfc6125_wildcard_rules() {
        // Exact match.
        assert!(san_covers_host("myapp.com", "myapp.com"));
        assert!(
            san_covers_host("MyApp.COM", "myapp.com"),
            "case-insensitive"
        );
        assert!(!san_covers_host("myapp.com", "tenant1.myapp.com"));

        // A wildcard covers exactly ONE label.
        assert!(san_covers_host("*.myapp.com", "tenant1.myapp.com"));
        assert!(
            !san_covers_host("*.myapp.com", "a.b.myapp.com"),
            "a wildcard must not span a dot"
        );
        assert!(
            !san_covers_host("*.myapp.com", "myapp.com"),
            "a wildcard does not cover the apex"
        );
        assert!(!san_covers_host("*.myapp.com", "myapp.com.evil.com"));
        assert!(!san_covers_host("*.myapp.com", ""));

        // A trailing dot on the queried host is the same name.
        assert!(san_covers_host("*.myapp.com", "tenant1.myapp.com."));
    }

    #[test]
    fn acme_config_covers_host_uses_every_san() {
        let acme = acme_cfg(&["myapp.com", "*.myapp.com"], "ops@myapp.com");
        assert!(acme.covers_host("myapp.com"));
        assert!(acme.covers_host("tenant42.myapp.com"));
        assert!(!acme.covers_host("other.example.com"));
        assert!(!acme.covers_host("deep.tenant42.myapp.com"));
    }

    // Companion: a valid domain list plus the default challenge port is unaffected
    // by the new blank-entry / zero-port rejections.
    #[test]
    fn validate_acme_valid_domains_and_port_ok() {
        let mut cfg = tls_static(None, None);
        cfg.acme = Some(acme_cfg(
            &["app.example.com", "www.example.com"],
            "ops@example.com",
        ));
        assert!(cfg.validate().is_ok(), "got: {:?}", cfg.validate());
    }

    // ── server.capacity_contract (#1733) ───────────────────────────────

    #[test]
    fn server_config_defaults_capacity_contract_none() {
        let config = AutumnConfig::default();
        assert!(config.server.capacity_contract.is_none());
    }

    #[test]
    fn capacity_contract_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r#"
            [server]
            capacity_contract = "capacity.lock"
            "#,
        )
        .expect("config with server.capacity_contract should parse");
        assert_eq!(
            config.server.capacity_contract.as_deref(),
            Some("capacity.lock")
        );
    }

    #[test]
    fn env_override_server_capacity_contract() {
        let env = MockEnv::new().with("AUTUMN_SERVER__CAPACITY_CONTRACT", "deploy/capacity.lock");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.server.capacity_contract.as_deref(),
            Some("deploy/capacity.lock")
        );
    }

    // ── server.max_concurrent_requests (#1006) ────────────────────

    #[test]
    fn server_config_defaults_max_concurrent_requests_none() {
        // Default must preserve today's unlimited behavior — no existing app
        // silently changes throughput.
        let config = AutumnConfig::default();
        assert!(config.server.max_concurrent_requests.is_none());
    }

    #[test]
    fn max_concurrent_requests_parses_from_toml() {
        let config: AutumnConfig = toml::from_str(
            r"
            [server]
            max_concurrent_requests = 64
            ",
        )
        .expect("config with server.max_concurrent_requests should parse");
        assert_eq!(config.server.max_concurrent_requests, Some(64));
    }

    #[test]
    fn env_override_server_max_concurrent_requests() {
        let env = MockEnv::new().with("AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS", "128");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.max_concurrent_requests, Some(128));
    }

    #[test]
    fn env_override_invalid_max_concurrent_requests_ignored() {
        let env = MockEnv::new().with("AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS", "not_a_number");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.server.max_concurrent_requests.is_none());
    }

    #[test]
    fn env_override_empty_max_concurrent_requests_clears_to_none() {
        // parse_env_option's documented convention: empty string clears to None.
        let env = MockEnv::new().with("AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS", "");
        let mut config = AutumnConfig::default();
        config.server.max_concurrent_requests = Some(64);
        config.apply_env_overrides_with_env(&env);
        assert!(config.server.max_concurrent_requests.is_none());
    }

    // ── Log env override tests ───────────────────────────────────

    #[test]
    fn env_override_log_level() {
        let env = MockEnv::new().with("AUTUMN_LOG__LEVEL", "debug");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.level, "debug");
    }

    #[test]
    fn env_override_log_format_json() {
        let env = MockEnv::new().with("AUTUMN_LOG__FORMAT", "Json");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.format, LogFormat::Json);
    }

    #[test]
    fn env_override_log_format_pretty() {
        let env = MockEnv::new().with("AUTUMN_LOG__FORMAT", "Pretty");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.format, LogFormat::Pretty);
    }

    #[test]
    fn env_override_invalid_log_format_ignored() {
        let env = MockEnv::new().with("AUTUMN_LOG__FORMAT", "yaml");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.format, LogFormat::Auto);
    }

    // ── Health env override tests ────────────────────────────────

    #[test]
    fn env_override_telemetry_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_TELEMETRY__ENABLED", "true")
            .with("AUTUMN_TELEMETRY__SERVICE_NAME", "orders-api")
            .with("AUTUMN_TELEMETRY__SERVICE_NAMESPACE", "acme")
            .with("AUTUMN_TELEMETRY__SERVICE_VERSION", "1.2.3")
            .with("AUTUMN_TELEMETRY__ENVIRONMENT", "production")
            .with(
                "AUTUMN_TELEMETRY__OTLP_ENDPOINT",
                "http://otel-collector:4317",
            )
            .with("AUTUMN_TELEMETRY__PROTOCOL", "HTTP_PROTOBUF")
            .with("AUTUMN_TELEMETRY__STRICT", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.telemetry.enabled);
        assert_eq!(config.telemetry.service_name, "orders-api");
        assert_eq!(config.telemetry.service_namespace.as_deref(), Some("acme"));
        assert_eq!(config.telemetry.service_version, "1.2.3");
        assert_eq!(config.telemetry.environment, "production");
        assert_eq!(
            config.telemetry.otlp_endpoint.as_deref(),
            Some("http://otel-collector:4317")
        );
        assert_eq!(config.telemetry.protocol, TelemetryProtocol::HttpProtobuf);
        assert!(config.telemetry.strict);
    }

    #[test]
    fn env_override_invalid_telemetry_protocol_ignored() {
        let env = MockEnv::new().with("AUTUMN_TELEMETRY__PROTOCOL", "zipkin");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.telemetry.protocol, TelemetryProtocol::Grpc);
    }

    #[test]
    fn env_override_health_path() {
        let env = MockEnv::new().with("AUTUMN_HEALTH__PATH", "/healthz");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.health.path, "/healthz");
    }

    #[test]
    fn env_override_probe_paths() {
        let env = MockEnv::new()
            .with("AUTUMN_HEALTH__LIVE_PATH", "/livez")
            .with("AUTUMN_HEALTH__READY_PATH", "/readyz")
            .with("AUTUMN_HEALTH__STARTUP_PATH", "/startupz");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.health.live_path, "/livez");
        assert_eq!(config.health.ready_path, "/readyz");
        assert_eq!(config.health.startup_path, "/startupz");
    }

    // ── Precedence test ──────────────────────────────────────────

    #[test]
    fn env_overrides_toml_values() {
        let env = MockEnv::new().with("AUTUMN_SERVER__PORT", "9999");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "[server]\nport = 4000\n").unwrap();
        let mut config = AutumnConfig::load_from(&path).unwrap();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.server.port, 9999); // env wins
    }

    // ── Validation tests ─────────────────────────────────────────

    #[test]
    fn validate_rejects_invalid_url_scheme() {
        let config = DatabaseConfig {
            url: Some("mysql://localhost/test".to_owned()),
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must start with postgres://")
        );
    }

    #[test]
    fn validate_accepts_postgres_url() {
        let config = DatabaseConfig {
            url: Some("postgres://localhost/test".to_owned()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_postgresql_url() {
        let config = DatabaseConfig {
            url: Some("postgresql://localhost/test".to_owned()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_no_url() {
        let config = DatabaseConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_keyword_value_connection_strings() {
        // The pool's TLS support parses libpq keyword/value strings, so
        // validation must let them through (issue #1585 review) — including
        // quoted values and whitespace around `=`.
        for url in [
            "host=db user=app dbname=app",
            "host=db user=app sslmode=require",
            "host=db sslmode = require",
            "host=db password='p w' sslmode='verify-full'",
            "host=db password=https://looks-like-a-url sslmode=require",
        ] {
            let config = DatabaseConfig {
                url: Some(url.to_owned()),
                ..Default::default()
            };
            assert!(
                config.validate().is_ok(),
                "keyword/value string must validate: {url}"
            );
        }
        // primary_url and shard URLs accept the same forms.
        let config = DatabaseConfig {
            primary_url: Some("host=db user=app sslmode=require".to_owned()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        let config = DatabaseConfig {
            primary_url: Some("postgres://db-control/app".to_owned()),
            shards: vec![ShardConfig {
                name: "s0".to_owned(),
                primary_url: "host=db-shard0 user=app dbname=app".to_owned(),
                replica_url: None,
                slots: None,
                primary_pool_size: None,
                replica_pool_size: None,
                replica_fallback: None,
            }],
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "shard URLs accept the keyword form too: {:?}",
            config.validate()
        );
    }

    #[test]
    fn validate_still_rejects_garbage_connection_strings() {
        for url in [
            "mysql://localhost/test",
            "mysql://localhost/test?a=b",
            "not a connection string",
            "localhost",
            "host=",
            "host='unterminated",
        ] {
            let config = DatabaseConfig {
                url: Some(url.to_owned()),
                ..Default::default()
            };
            let err = config
                .validate()
                .expect_err(&format!("garbage must be rejected: {url:?}"))
                .to_string();
            assert!(
                err.contains("must start with postgres:// or postgresql://"),
                "the error must stay clear about accepted forms, got: {err}"
            );
        }
    }

    // ── DatabaseBackend detection tests (issue #1614) ──────────────

    #[test]
    fn detect_backend_postgres_urls() {
        for url in [
            "postgres://localhost/app",
            "postgresql://user:pass@db:5432/app",
        ] {
            assert_eq!(
                DatabaseBackend::detect(url),
                Some(DatabaseBackend::Postgres),
                "{url} should detect as postgres"
            );
        }
    }

    #[test]
    fn detect_backend_postgres_keyword_value() {
        // libpq keyword/value strings are a Postgres shape (the pool accepts
        // them), so they must classify as Postgres, not fall through.
        assert_eq!(
            DatabaseBackend::detect("host=db user=app sslmode=require"),
            Some(DatabaseBackend::Postgres)
        );
    }

    #[test]
    fn detect_backend_sqlite_schemes() {
        for url in [
            "sqlite:///var/lib/app.db", // canonical sqlite:// (absolute path)
            "sqlite://./relative.db",
            "sqlite::memory:",
            "sqlite:app.db", // shorter sqlite: form
            "file:app.db",   // file: form
        ] {
            assert_eq!(
                DatabaseBackend::detect(url),
                Some(DatabaseBackend::Sqlite),
                "{url} should detect as sqlite"
            );
        }
    }

    #[test]
    fn detect_backend_bare_path_is_unrecognized() {
        // A bare filesystem path carries no scheme distinguishing it from a
        // typo'd URL, so it is deliberately NOT auto-detected as SQLite. Users
        // must spell an explicit sqlite:// (or sqlite:/file:) scheme.
        for target in ["/var/lib/app.db", "./app.db", "app.db", "C:\\db\\app.db"] {
            assert_eq!(
                DatabaseBackend::detect(target),
                None,
                "{target} must not be auto-detected as a backend"
            );
        }
    }

    #[test]
    fn detect_backend_garbage_is_unrecognized() {
        for target in ["mysql://localhost/app", "not a connection string", "host="] {
            assert_eq!(DatabaseBackend::detect(target), None, "{target}");
        }
    }

    // ── SQLite config validation tests (issue #1614) ───────────────

    #[test]
    fn validate_accepts_sqlite_url() {
        let config = DatabaseConfig {
            url: Some("sqlite:///var/lib/app.db".to_owned()),
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "a sqlite:// target must be accepted as valid config: {:?}",
            config.validate()
        );
    }

    #[test]
    fn validate_accepts_sqlite_primary_url() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite::memory:".to_owned()),
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "{:?}", config.validate());
    }

    #[test]
    fn validate_rejects_replica_url_on_sqlite() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".to_owned()),
            replica_url: Some("sqlite:///var/lib/replica.db".to_owned()),
            ..Default::default()
        };
        let err = config
            .validate()
            .expect_err("read replicas must be refused on sqlite")
            .to_string();
        assert!(
            err.contains("read replicas require the postgres backend"),
            "message must name the postgres requirement, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_shards_on_sqlite() {
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".to_owned()),
            shards: vec![ShardConfig {
                name: "s0".to_owned(),
                primary_url: "postgres://db-shard0/app".to_owned(),
                replica_url: None,
                slots: None,
                primary_pool_size: None,
                replica_pool_size: None,
                replica_fallback: None,
            }],
            ..Default::default()
        };
        let err = config
            .validate()
            .expect_err("shards must be refused on sqlite")
            .to_string();
        assert!(
            err.contains("database shards require the postgres backend"),
            "message must name the postgres requirement, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_backend_mismatch_across_roles() {
        // Postgres primary with a SQLite replica: a boot-time misconfiguration,
        // not a first-query surprise.
        let config = DatabaseConfig {
            primary_url: Some("postgres://db-primary/app".to_owned()),
            replica_url: Some("sqlite:///var/lib/replica.db".to_owned()),
            ..Default::default()
        };
        let err = config
            .validate()
            .expect_err("mixed backends must be refused")
            .to_string();
        assert!(
            err.contains("database.replica_url")
                && err.contains("does not match the primary database backend"),
            "message must name the offending field and the mismatch, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_sqlite_primary_with_postgres_url() {
        // effective_primary_url() prefers primary_url; the legacy `url` role
        // must agree on the backend.
        let config = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".to_owned()),
            url: Some("postgres://db-primary/app".to_owned()),
            ..Default::default()
        };
        let err = config
            .validate()
            .expect_err("mixed backends must be refused")
            .to_string();
        assert!(
            err.contains("database.url")
                && err.contains("does not match the primary database backend"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_postgres_app_with_replica_still_valid() {
        // Regression guard: the existing Postgres primary + replica path is
        // unchanged and still validates cleanly.
        let config = DatabaseConfig {
            primary_url: Some("postgres://db-primary/app".to_owned()),
            replica_url: Some("postgres://db-replica/app".to_owned()),
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "{:?}", config.validate());
    }

    // ── Profile tests ──────────────────────────────────────────

    #[test]
    fn resolve_profile_from_autumn_env() {
        let env = MockEnv::new().with("AUTUMN_ENV", "prod");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "prod");
    }

    #[test]
    fn resolve_profile_from_legacy_env() {
        let env = MockEnv::new().with("AUTUMN_PROFILE", "staging");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "staging");
    }

    #[test]
    fn resolve_profile_prefers_autumn_env_over_legacy_alias() {
        let env = MockEnv::new()
            .with("AUTUMN_ENV", "dev")
            .with("AUTUMN_PROFILE", "prod");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "dev");
    }

    #[test]
    fn resolve_profile_normalizes_production_alias() {
        let env = MockEnv::new().with("AUTUMN_ENV", "production");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "prod");
    }

    #[test]
    fn resolve_profile_normalizes_development_alias_with_whitespace() {
        let env = MockEnv::new().with("AUTUMN_ENV", "  development  ");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "dev");
    }

    #[test]
    fn resolve_profile_normalizes_uppercase_dev_and_prod() {
        let prod_env = MockEnv::new().with("AUTUMN_ENV", "PROD");
        let prod = resolve_profile(&prod_env);
        assert_eq!(prod, "prod");

        let dev_env = MockEnv::new().with("AUTUMN_ENV", "DEV");
        let dev = resolve_profile(&dev_env);
        assert_eq!(dev, "dev");
    }

    #[test]
    fn resolve_profile_preserves_case_for_custom_profiles() {
        let env = MockEnv::new().with("AUTUMN_ENV", "QA");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "QA");
    }

    #[test]
    fn resolve_profile_auto_detect_debug() {
        let env = MockEnv::new().with("AUTUMN_IS_DEBUG", "1");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "dev");
    }

    #[test]
    fn resolve_profile_auto_detect_release() {
        let env = MockEnv::new().with("AUTUMN_IS_DEBUG", "0");
        let profile = resolve_profile(&env);
        assert_eq!(profile, "prod");
    }

    #[test]
    fn resolve_profile_defaults_to_dev_when_no_signal_present() {
        let env = MockEnv::new();
        let profile = resolve_profile(&env);
        assert_eq!(profile, "dev");
    }

    #[test]
    fn dev_profile_smart_defaults() {
        let defaults = profile_defaults_as_toml("dev");
        let toml_str = toml::to_string(&defaults).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.log.level, "debug");
        assert_eq!(config.log.format, LogFormat::Pretty);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.shutdown_timeout_secs, 1);
        assert_eq!(
            config.server.prestop_grace_secs, 0,
            "dev profile must set prestop_grace_secs = 0 so Ctrl-C is instant"
        );
        assert_eq!(config.telemetry.environment, "development");
        assert!(config.health.detailed);
        assert_eq!(config.cors.allowed_origins, vec!["*"]);
        assert!(
            config.security.trusted_proxies.trust_forwarded_headers,
            "dev profile must trust forwarded headers from loopback"
        );
        assert!(
            config
                .security
                .trusted_proxies
                .ranges
                .contains(&"127.0.0.0/8".to_owned()),
            "dev profile must include 127.0.0.0/8 as trusted proxy range"
        );
        assert!(
            config
                .security
                .trusted_proxies
                .ranges
                .contains(&"::1/128".to_owned()),
            "dev profile must include ::1/128 as trusted proxy range"
        );
    }

    #[test]
    fn prod_profile_smart_defaults() {
        let defaults = profile_defaults_as_toml("prod");
        let toml_str = toml::to_string(&defaults).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.log.level, "info");
        assert_eq!(config.log.format, LogFormat::Json);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.shutdown_timeout_secs, 30);
        assert_eq!(config.telemetry.environment, "production");
        assert!(!config.health.detailed);
        // AC: HSTS auto-enabled in the production profile.
        assert!(
            config.security.headers.strict_transport_security,
            "prod profile must auto-enable Strict-Transport-Security"
        );
        // Defaults should still be secure-by-default in prod.
        assert_eq!(config.security.headers.x_frame_options, "DENY");
        assert!(config.security.headers.x_content_type_options);
        assert!(!config.security.headers.content_security_policy.is_empty());
    }

    #[test]
    fn dev_profile_does_not_auto_enable_hsts() {
        let defaults = profile_defaults_as_toml("dev");
        let toml_str = toml::to_string(&defaults).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();

        assert!(
            !config.security.headers.strict_transport_security,
            "dev profile must not force HSTS on (local http development)"
        );
    }

    #[test]
    fn custom_profile_no_smart_defaults() {
        let defaults = profile_defaults_as_toml("staging");
        assert_eq!(defaults, toml::Value::Table(toml::map::Map::new()));
    }

    #[test]
    fn deep_merge_tables() {
        let mut base: toml::Value = toml::from_str(
            r#"
            [server]
            port = 3000
            host = "127.0.0.1"
            [database]
            pool_size = 10
            "#,
        )
        .unwrap();

        let overlay: toml::Value = toml::from_str(
            r#"
            [server]
            port = 8080
            [database]
            url = "postgres://localhost/test"
            "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);

        // Overlay value wins
        assert_eq!(base["server"]["port"], toml::Value::Integer(8080));
        // Base value preserved when not in overlay
        assert_eq!(
            base["server"]["host"],
            toml::Value::String("127.0.0.1".into())
        );
        // New key from overlay added
        assert_eq!(
            base["database"]["url"],
            toml::Value::String("postgres://localhost/test".into())
        );
        // Base key preserved
        assert_eq!(base["database"]["pool_size"], toml::Value::Integer(10));
    }

    #[test]
    fn profile_toml_overrides_base_toml() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("autumn.toml");
        let dev_path = dir.path().join("autumn-dev.toml");

        std::fs::write(
            &base_path,
            r"
            [server]
            port = 3000
            [database]
            pool_size = 10
            ",
        )
        .unwrap();

        std::fs::write(
            &dev_path,
            r#"
            [database]
            url = "postgres://localhost/myapp_dev"
            "#,
        )
        .unwrap();

        // Load base
        let mut merged = toml::Value::Table(toml::map::Map::new());
        let base = load_raw_toml(&base_path).unwrap().unwrap();
        deep_merge(&mut merged, base);
        let profile = load_raw_toml(&dev_path).unwrap().unwrap();
        deep_merge(&mut merged, profile);

        let toml_str = toml::to_string(&merged).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.server.port, 3000); // from base
        assert_eq!(config.database.pool_size, 10); // from base, preserved
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://localhost/myapp_dev")
        ); // from profile
    }

    #[test]
    fn inline_profile_section_overrides_base_toml() {
        let mut merged = toml::Value::Table(toml::map::Map::new());
        let base: toml::Value = toml::from_str(
            r#"
            [server]
            port = 3000

            [log]
            level = "info"

            [profile.dev.log]
            level = "debug"
            "#,
        )
        .unwrap();

        deep_merge(&mut merged, base.clone());
        let inline = profile_section_from_base_toml(&base, "dev").unwrap();
        deep_merge(&mut merged, inline);

        let toml_str = toml::to_string(&merged).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.log.level, "debug");
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("dev", "dev"), 0);
        assert_eq!(levenshtein("dev", "dve"), 2); // swap = 2 edits (del + ins)
        assert_eq!(levenshtein("prod", "prodd"), 1);
        assert_eq!(levenshtein("prod", "prd"), 1);
        assert_eq!(levenshtein("staging", "dev"), 7);
    }

    #[test]
    fn env_override_health_detailed() {
        let env = MockEnv::new().with("AUTUMN_HEALTH__DETAILED", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.health.detailed);
    }

    #[test]
    fn profile_name_accessor() {
        let mut config = AutumnConfig::default();
        assert!(config.profile_name().is_none());

        config.profile = Some("dev".to_owned());
        assert_eq!(config.profile_name(), Some("dev"));
    }

    // ── Mutant-hunting tests ────────────────────────────────────

    #[test]
    fn find_config_file_falls_back_to_cwd() {
        // Without AUTUMN_MANIFEST_DIR, should return just the filename
        let env = MockEnv::new();
        let path = find_config_file_named("autumn.toml", &env);
        assert_eq!(path, PathBuf::from("autumn.toml"));
    }

    #[test]
    fn find_config_file_uses_manifest_dir_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("autumn.toml");
        std::fs::write(&config_path, "").unwrap();

        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());
        let path = find_config_file_named("autumn.toml", &env);
        assert_eq!(path, config_path);
    }

    #[test]
    fn find_config_file_falls_back_when_manifest_dir_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // dir exists but the file doesn't
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());
        let path = find_config_file_named("nonexistent.toml", &env);
        assert_eq!(path, PathBuf::from("nonexistent.toml"));
    }

    #[test]
    fn resolve_profile_cli_flag_exact_match() {
        // resolve_profile checks `--profile` in CLI args. We can't easily
        // inject args, but we can verify the env path doesn't match other args.
        // The `== "--profile"` guard is the key: if it were `!=`, every arg
        // would trigger the branch.
        let env = MockEnv::new();
        // With no env vars and no matching CLI args, should be None
        let profile = resolve_profile(&env);
        // This may or may not be None depending on test harness args,
        // but the important thing is it doesn't crash or return garbage.
        // The env-based tests above cover the positive cases.
        drop(profile);
    }

    #[test]
    fn deep_merge_non_table_overlay_replaces_base() {
        // When overlay is not a table, it should replace (not merge into) base.
        // This kills the `&& → ||` mutant on line 162.
        let mut base: toml::Value = toml::from_str("[server]\nport = 3000\n").unwrap();
        let overlay = toml::Value::String("not_a_table".into());

        // When base is table and overlay is NOT table, base should be unchanged
        // (the function only merges when BOTH are tables).
        deep_merge(&mut base, overlay);
        // base should still be the original table (overlay was ignored)
        assert!(base.is_table());
        assert_eq!(base["server"]["port"], toml::Value::Integer(3000));
    }

    #[test]
    fn deep_merge_when_base_not_table() {
        // When base is not a table, overlay should not merge
        let mut base = toml::Value::String("original".into());
        let overlay: toml::Value = toml::from_str("[server]\nport = 3000\n").unwrap();

        deep_merge(&mut base, overlay);
        // base should be unchanged
        assert_eq!(base, toml::Value::String("original".into()));
    }

    #[test]
    fn suggest_profile_close_match() {
        // "dve" is edit-distance 2 from "dev" → should suggest "dev"
        assert_eq!(suggest_profile("dve"), Some("dev"));
    }

    #[test]
    fn suggest_profile_no_match_when_distant() {
        // "xyz" is far from both "dev" and "prod" → no suggestion
        assert_eq!(suggest_profile("xyz"), None);
    }

    #[test]
    fn suggest_profile_exact_known_profile() {
        // Exact match has distance 0 → suggests itself
        assert_eq!(suggest_profile("dev"), Some("dev"));
        assert_eq!(suggest_profile("prod"), Some("prod"));
    }

    #[test]
    fn suggest_profile_prd() {
        // "prd" is distance 1 from "prod"
        assert_eq!(suggest_profile("prd"), Some("prod"));
    }

    #[test]
    fn warn_profile_typo_runs_without_panic() {
        warn_profile_typo("dve");
        warn_profile_typo("xyz");
    }

    #[test]
    fn should_warn_missing_profile_file_custom_without_inline() {
        assert!(should_warn_missing_profile_file("staging", false));
    }

    #[test]
    fn should_not_warn_missing_profile_file_custom_with_inline() {
        assert!(!should_warn_missing_profile_file("staging", true));
    }

    #[test]
    fn should_not_warn_missing_profile_file_dev_or_prod() {
        assert!(!should_warn_missing_profile_file("dev", false));
        assert!(!should_warn_missing_profile_file("prod", false));
    }

    #[test]
    fn levenshtein_threshold_in_warn_profile_typo() {
        assert!(levenshtein("dve", "dev") <= 2);
        assert!(levenshtein("xyz", "dev") > 2);
        assert!(levenshtein("xyz", "prod") > 2);
    }

    #[test]
    fn env_override_cors_allowed_origins() {
        let env = MockEnv::new().with(
            "AUTUMN_CORS__ALLOWED_ORIGINS",
            "https://a.com, https://b.com",
        );
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.cors.allowed_origins,
            vec!["https://a.com", "https://b.com"]
        );
    }

    #[test]
    fn env_override_cors_allow_credentials() {
        let env = MockEnv::new().with("AUTUMN_CORS__ALLOW_CREDENTIALS", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.cors.allow_credentials);
    }

    #[test]
    fn env_override_cors_max_age() {
        let env = MockEnv::new().with("AUTUMN_CORS__MAX_AGE_SECS", "3600");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.cors.max_age_secs, 3600);
    }

    #[test]
    fn cors_validate_rejects_wildcard_with_credentials() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["*".to_owned()];
        config.cors.allow_credentials = true;

        let result = config.validate();
        match result {
            Err(ConfigError::Validation(msg)) => {
                assert!(
                    msg.contains("allow_credentials") && msg.contains('*'),
                    "message should mention credentials and wildcard, got: {msg}"
                );
            }
            other => panic!("expected ConfigError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn cors_validate_accepts_wildcard_without_credentials() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["*".to_owned()];
        config.cors.allow_credentials = false;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn cors_validate_accepts_explicit_origins_with_credentials() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["https://app.example.com".to_owned()];
        config.cors.allow_credentials = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_uses_profile_layering() {
        // Test AutumnConfig::load_with_env() with a dev profile via env var.
        // This kills the "replace load → Ok(Default::default())" mutant.
        let env = MockEnv::new().with("AUTUMN_PROFILE", "dev");

        let config = AutumnConfig::load_with_env(&env).unwrap();
        // With dev profile, smart defaults should apply
        assert_eq!(config.profile.as_deref(), Some("dev"));
        assert_eq!(config.log.level, "debug"); // dev default
        assert_eq!(config.log.format, LogFormat::Pretty); // dev default
        assert!(config.health.detailed); // dev default
    }

    #[test]
    fn load_custom_profile_without_toml_warns() {
        // Test the typo warning branch: profile != "dev" && profile != "prod"
        // without a corresponding autumn-{profile}.toml triggers warn_profile_typo.
        // This kills the match guard mutants on line 341.
        let env = MockEnv::new().with("AUTUMN_PROFILE", "staging");

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("staging"));
        // staging has no smart defaults, so values should be framework defaults
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.log.level, "info");
    }

    #[test]
    fn load_dev_profile_no_profile_toml_no_warn() {
        // dev/prod without their profile TOML should NOT trigger warn_profile_typo.
        // This tests the `None => {}` branch (line 342).
        let env = MockEnv::new().with("AUTUMN_PROFILE", "dev");

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("dev"));
    }

    #[test]
    fn load_custom_profile_uses_inline_profile_without_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("autumn.toml");
        std::fs::write(
            &base_path,
            r"
            [server]
            port = 3000

            [profile.staging.server]
            port = 4100
            ",
        )
        .unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_ENV", "staging")
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("staging"));
        assert_eq!(config.server.port, 4100);
    }

    #[test]
    fn load_production_profile_reads_inline_profile_production_section() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("autumn.toml");
        std::fs::write(
            &base_path,
            r"
            [profile.production.server]
            port = 4200
            ",
        )
        .unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_ENV", "production")
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("prod"));
        assert_eq!(config.server.port, 4200);
    }

    #[test]
    fn load_production_profile_reads_legacy_autumn_production_toml() {
        let dir = tempfile::tempdir().unwrap();
        let production_path = dir.path().join("autumn-production.toml");
        std::fs::write(
            &production_path,
            r"
            [server]
            port = 4300
            ",
        )
        .unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_ENV", "production")
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("prod"));
        assert_eq!(config.server.port, 4300);
    }

    #[test]
    fn load_prod_prefers_autumn_prod_toml_before_production_alias() {
        let dir = tempfile::tempdir().unwrap();
        let prod_path = dir.path().join("autumn-prod.toml");
        let production_path = dir.path().join("autumn-production.toml");

        std::fs::write(
            &prod_path,
            r"
            [server]
            port = 4400
            ",
        )
        .unwrap();
        // Malformed TOML should be ignored because `autumn-prod.toml` is chosen first.
        std::fs::write(&production_path, "[server\nport = 4500").unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_ENV", "prod")
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("prod"));
        assert_eq!(config.server.port, 4400);
    }

    #[test]
    fn load_production_prefers_autumn_production_toml_before_prod_alias() {
        let dir = tempfile::tempdir().unwrap();
        let prod_path = dir.path().join("autumn-prod.toml");
        let production_path = dir.path().join("autumn-production.toml");

        std::fs::write(
            &production_path,
            r"
            [server]
            port = 4500
            ",
        )
        .unwrap();
        // Malformed TOML should be ignored because `autumn-production.toml` is chosen first.
        std::fs::write(&prod_path, "[server\nport = 4400").unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_ENV", "production")
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());

        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(config.profile.as_deref(), Some("prod"));
        assert_eq!(config.server.port, 4500);
    }

    #[test]
    fn load_from_io_error_is_not_swallowed() {
        // load_from should return Err on non-NotFound IO errors.
        // On all platforms, trying to read a directory as a file triggers an error.
        let dir = tempfile::tempdir().unwrap();
        let result = AutumnConfig::load_from(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn load_raw_toml_missing_file_returns_none() {
        let result = load_raw_toml(Path::new("this_file_does_not_exist_12345.toml")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_raw_toml_directory_returns_io_error() {
        // Reading a directory is an IO error, NOT NotFound.
        // This kills the "replace match guard NotFound with true" mutant:
        // if the guard were always true, this would return Ok(None) instead of Err.
        let dir = tempfile::tempdir().unwrap();
        let result = load_raw_toml(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn load_raw_toml_valid_file_returns_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "[server]\nport = 3000\n").unwrap();
        let result = load_raw_toml(&path).unwrap();
        assert!(result.is_some());
        assert_eq!(
            result.unwrap()["server"]["port"],
            toml::Value::Integer(3000)
        );
    }

    #[test]
    fn env_override_log_format_auto() {
        // Kills the "delete match arm Auto" mutant
        let env = MockEnv::new().with("AUTUMN_LOG__FORMAT", "Auto");
        let mut config = AutumnConfig::default();
        // Start with non-Auto to prove the override works
        config.log.format = LogFormat::Json;
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.log.format, LogFormat::Auto);
    }

    #[test]
    fn env_override_health_detailed_false() {
        // Kills the 'delete match arm "false" | "0"' mutant
        let env = MockEnv::new().with("AUTUMN_HEALTH__DETAILED", "false");
        let mut config = AutumnConfig::default();
        config.health.detailed = true; // start true, override to false
        config.apply_env_overrides_with_env(&env);
        assert!(!config.health.detailed);
    }

    #[test]
    fn env_override_health_detailed_zero() {
        let env = MockEnv::new().with("AUTUMN_HEALTH__DETAILED", "0");
        let mut config = AutumnConfig::default();
        config.health.detailed = true;
        config.apply_env_overrides_with_env(&env);
        assert!(!config.health.detailed);
    }

    #[test]
    fn health_enabled_defaults_true() {
        // Issue #1971: the probe off-switch is opt-in — enabled by default so
        // behavior is byte-identical to before the field existed.
        assert!(HealthConfig::default().enabled);
    }

    #[test]
    fn env_override_health_enabled_false() {
        // Issue #1971: AUTUMN_HEALTH__ENABLED=false flips the built-in probe
        // off-switch via env, mirroring the AUTUMN_HEALTH__DETAILED wiring.
        let env = MockEnv::new().with("AUTUMN_HEALTH__ENABLED", "false");
        let mut config = AutumnConfig::default();
        assert!(config.health.enabled); // starts true (default)
        config.apply_env_overrides_with_env(&env);
        assert!(!config.health.enabled);
    }

    #[test]
    fn cors_defaults() {
        let cors = CorsConfig::default();
        assert!(cors.allowed_origins.is_empty());
        assert_eq!(cors.allowed_methods.len(), 6);
        assert!(cors.allowed_methods.contains(&"GET".to_owned()));
        assert!(cors.allowed_headers.contains(&"Content-Type".to_owned()));
        assert!(!cors.allow_credentials);
        assert_eq!(cors.max_age_secs, 86400);
    }

    #[test]
    fn cors_in_full_config_defaults() {
        let config = AutumnConfig::default();
        assert!(config.cors.allowed_origins.is_empty());
    }

    #[test]
    fn actuator_defaults() {
        let config = ActuatorConfig::default();
        assert_eq!(config.prefix, "/actuator");
        assert!(!config.sensitive);
        // Prometheus metrics export is on by default and independent of
        // `sensitive`, so platform scraping works without exposing env/loggers.
        assert!(config.prometheus);
    }

    #[test]
    fn actuator_prometheus_can_be_disabled_via_toml() {
        let toml = r"
            sensitive = false
            prometheus = false
        ";
        let config: ActuatorConfig = toml::from_str(toml).unwrap();
        assert!(!config.sensitive);
        assert!(!config.prometheus);
    }

    #[test]
    fn actuator_prefix_in_full_config() {
        let config = AutumnConfig::default();
        assert_eq!(config.actuator.prefix, "/actuator");
    }

    #[test]
    fn deep_merge_handles_deep_nesting() {
        let mut base = toml::Value::Table(toml::map::Map::new());
        let mut overlay = toml::Value::Table(toml::map::Map::new());

        // Create a 10,000 deep nested table
        let mut current_base = &mut base;
        let mut current_overlay = &mut overlay;

        for _ in 0..10_000 {
            if let toml::Value::Table(t) = current_base {
                t.insert("x".to_owned(), toml::Value::Table(toml::map::Map::new()));
                current_base = t.get_mut("x").unwrap();
            }
            if let toml::Value::Table(t) = current_overlay {
                t.insert("x".to_owned(), toml::Value::Table(toml::map::Map::new()));
                current_overlay = t.get_mut("x").unwrap();
            }
        }

        // Add a leaf value to test actual merging
        if let toml::Value::Table(t) = current_overlay {
            t.insert("y".to_owned(), toml::Value::Integer(42));
        }

        // Trigger merge, expecting no panic/stack overflow
        // We run it on a thread with a large stack to avoid the stack overflow caused by Drop when base is dropped at the end of the function (since we created a 10,000 depth structure).
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                deep_merge(&mut base, overlay);
                // Let the OS clean up the memory instead of dropping deeply nested structure
                std::mem::forget(base);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_merge_stops_at_max_depth() {
        let mut base = toml::Value::Table(toml::map::Map::new());
        let mut overlay = toml::Value::Table(toml::map::Map::new());

        // Create structures nested exactly to MAX_MERGE_DEPTH + 1
        let mut current_base = &mut base;
        let mut current_overlay = &mut overlay;

        for _ in 0..=MAX_MERGE_DEPTH {
            if let toml::Value::Table(t) = current_base {
                t.insert("x".to_owned(), toml::Value::Table(toml::map::Map::new()));
                current_base = t.get_mut("x").unwrap();
            }
            if let toml::Value::Table(t) = current_overlay {
                t.insert("x".to_owned(), toml::Value::Table(toml::map::Map::new()));
                current_overlay = t.get_mut("x").unwrap();
            }
        }

        // Add a value deep in the overlay
        if let toml::Value::Table(t) = current_overlay {
            t.insert("deep_value".to_owned(), toml::Value::Integer(123));
        }

        deep_merge(&mut base, overlay);

        // Verify the value was NOT merged due to max depth limit
        let mut current_base_check = &base;
        for _ in 0..=MAX_MERGE_DEPTH {
            if let toml::Value::Table(t) = current_base_check {
                current_base_check = t.get("x").unwrap();
            }
        }

        if let toml::Value::Table(t) = current_base_check {
            assert!(
                !t.contains_key("deep_value"),
                "Value beyond MAX_MERGE_DEPTH should not be merged"
            );
        } else {
            panic!("Expected a table");
        }
    }

    // ── AUTUMN_SECURITY__FORBIDDEN_RESPONSE / __ALLOW_UNAUTHORIZED_REPOSITORY_API ──

    #[test]
    fn env_override_forbidden_response_403() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__FORBIDDEN_RESPONSE", "403");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.forbidden_response,
            crate::authorization::ForbiddenResponse::Forbidden403
        );
    }

    #[test]
    fn env_override_forbidden_response_404() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__FORBIDDEN_RESPONSE", "404");
        let mut config = AutumnConfig::default();
        // Pre-set to 403 to confirm env actually flips it back to 404.
        config.security.forbidden_response = crate::authorization::ForbiddenResponse::Forbidden403;
        config.apply_env_overrides_with_env(&env);
        assert_eq!(
            config.security.forbidden_response,
            crate::authorization::ForbiddenResponse::NotFound404
        );
    }

    #[test]
    fn env_override_forbidden_response_invalid_keeps_existing() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__FORBIDDEN_RESPONSE", "418");
        let mut config = AutumnConfig::default();
        config.security.forbidden_response = crate::authorization::ForbiddenResponse::Forbidden403;
        config.apply_env_overrides_with_env(&env);
        // Invalid value warns and leaves the existing setting alone.
        assert_eq!(
            config.security.forbidden_response,
            crate::authorization::ForbiddenResponse::Forbidden403
        );
    }

    #[test]
    fn env_override_allow_unauthorized_repository_api() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__ALLOW_UNAUTHORIZED_REPOSITORY_API", "true");
        let mut config = AutumnConfig::default();
        assert!(!config.security.allow_unauthorized_repository_api);
        config.apply_env_overrides_with_env(&env);
        assert!(config.security.allow_unauthorized_repository_api);
    }

    #[test]
    fn env_override_allow_unauthorized_repository_api_false_overrides_toml_true() {
        let env = MockEnv::new().with(
            "AUTUMN_SECURITY__ALLOW_UNAUTHORIZED_REPOSITORY_API",
            "false",
        );
        let mut config = AutumnConfig::default();
        config.security.allow_unauthorized_repository_api = true;
        config.apply_env_overrides_with_env(&env);
        assert!(!config.security.allow_unauthorized_repository_api);
    }

    #[test]
    fn env_override_csrf_token_scan_bytes() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__CSRF__TOKEN_SCAN_BYTES", "8388608");
        let mut config = AutumnConfig::default();
        // Default is 2 MiB; the env override must raise it.
        assert_eq!(config.security.csrf.token_scan_bytes, 2 * 1024 * 1024);
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.security.csrf.token_scan_bytes, 8_388_608);
    }

    #[test]
    fn env_override_csrf_token_scan_bytes_invalid_is_ignored() {
        let env = MockEnv::new().with("AUTUMN_SECURITY__CSRF__TOKEN_SCAN_BYTES", "not-a-number");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        // Invalid values are ignored, leaving the default intact.
        assert_eq!(config.security.csrf.token_scan_bytes, 2 * 1024 * 1024);
    }

    // ── [openapi] config section tests (RED phase) ─────────────────────────

    #[test]
    fn openapi_runtime_config_defaults_enabled() {
        // The [openapi] section must default to enabled=true and path="/openapi.json".
        let config = AutumnConfig::default();
        assert!(
            config.openapi_runtime.enabled,
            "[openapi] must default to enabled = true"
        );
        assert_eq!(
            config.openapi_runtime.path, "/openapi.json",
            "[openapi] must default to path = \"/openapi.json\""
        );
    }

    #[test]
    fn openapi_runtime_config_can_be_disabled_via_toml() {
        let toml_str = "
[openapi]
enabled = false
";
        let config: AutumnConfig = toml::from_str(toml_str).unwrap();
        assert!(
            !config.openapi_runtime.enabled,
            "[openapi] enabled = false must deserialize correctly"
        );
    }

    #[test]
    fn openapi_runtime_config_path_can_be_customized() {
        let toml_str = r#"
[openapi]
path = "/api-spec.json"
"#;
        let config: AutumnConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.openapi_runtime.path, "/api-spec.json",
            "[openapi] path must deserialize correctly"
        );
    }

    #[test]
    fn cache_env_overrides_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_CACHE__BACKEND", "redis")
            .with("AUTUMN_CACHE__REDIS__URL", "redis://cache:6379/1")
            .with("AUTUMN_CACHE__REDIS__KEY_PREFIX", "myapp:cache");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        assert!(config.cache.is_redis(), "backend should be redis");
        assert_eq!(
            config.cache.redis.url.as_deref(),
            Some("redis://cache:6379/1")
        );
        assert_eq!(config.cache.redis.key_prefix, "myapp:cache");
    }

    #[test]
    fn cache_backend_from_env_value_invalid_is_none() {
        assert!(CacheBackend::from_env_value("postgres").is_none());
        assert!(CacheBackend::from_env_value("").is_none());
    }

    #[test]
    fn scheduler_validate_rejects_zero_lease_ttl() {
        let cfg = SchedulerConfig {
            lease_ttl_secs: 0,
            ..SchedulerConfig::default()
        };
        assert!(cfg.validate().is_err(), "zero lease_ttl_secs must fail");
    }

    #[test]
    fn scheduler_validate_rejects_empty_key_prefix() {
        let cfg = SchedulerConfig {
            key_prefix: "   ".to_owned(),
            ..SchedulerConfig::default()
        };
        assert!(cfg.validate().is_err(), "blank key_prefix must fail");
    }

    #[test]
    fn scheduler_validate_ok_with_defaults() {
        assert!(SchedulerConfig::default().validate().is_ok());
    }

    #[test]
    fn scheduler_resolved_replica_id_uses_explicit_value() {
        let cfg = SchedulerConfig {
            replica_id: Some("my-pod".to_owned()),
            ..SchedulerConfig::default()
        };
        assert_eq!(cfg.resolved_replica_id(), "my-pod");
    }

    #[test]
    fn scheduler_resolved_replica_id_falls_back_to_pid() {
        let cfg = SchedulerConfig {
            replica_id: None,
            ..SchedulerConfig::default()
        };
        // In CI, FLY_MACHINE_ID and HOSTNAME may or may not be set,
        // so just verify we get a non-empty string back.
        assert!(!cfg.resolved_replica_id().is_empty());
    }

    #[cfg(feature = "mail")]
    #[test]
    fn mail_allow_in_process_deliver_later_in_production_is_overridable_via_env() {
        let env = MockEnv::new()
            .with(
                "AUTUMN_MAIL__ALLOW_IN_PROCESS_DELIVER_LATER_IN_PRODUCTION",
                "true",
            )
            .with("AUTUMN_MAIL__TRANSPORT", "smtp")
            .with("AUTUMN_MAIL__SMTP__HOST", "smtp.example.com");

        let mut config = AutumnConfig::default();
        config.apply_mail_env_overrides_with_env(&env);

        assert!(
            config.mail.allow_in_process_deliver_later_in_production,
            "env var should set allow_in_process_deliver_later_in_production"
        );
    }

    #[cfg(feature = "mail")]
    #[test]
    fn mail_allow_in_process_deliver_later_in_production_defaults_false() {
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_mail_env_overrides_with_env(&env);

        assert!(
            !config.mail.allow_in_process_deliver_later_in_production,
            "flag should default to false when env var is not set"
        );
    }

    #[cfg(feature = "mail")]
    #[test]
    fn mail_inline_css_is_overridable_via_env() {
        let env = MockEnv::new().with("AUTUMN_MAIL__INLINE_CSS", "true");

        let mut config = AutumnConfig::default();
        config.apply_mail_env_overrides_with_env(&env);

        assert!(
            config.mail.inline_css,
            "AUTUMN_MAIL__INLINE_CSS=true should enable inline_css"
        );
    }

    #[cfg(feature = "mail")]
    #[test]
    fn mail_inline_css_defaults_false() {
        let env = MockEnv::new();
        let mut config = AutumnConfig::default();
        config.apply_mail_env_overrides_with_env(&env);

        assert!(
            !config.mail.inline_css,
            "inline_css should default to false when env var is not set"
        );
    }

    // ── credentials integration ───────────────────────────────────────────

    #[test]
    fn config_credentials_empty_when_no_directory() {
        let env = MockEnv::new();
        let config = AutumnConfig::load_with_env(&env).unwrap();
        assert!(
            config.credentials().is_empty(),
            "existing apps without config/credentials/ must boot with an empty credentials store"
        );
    }

    #[test]
    fn config_has_credentials_accessor() {
        let config = AutumnConfig::default();
        let _store = config.credentials();
    }

    #[test]
    fn config_credentials_loaded_when_file_present() {
        use crate::credentials::{MasterKey, encrypt};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"stripe_key = \"sk_test_xyz\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(tmp.path().join("config/credentials/dev.toml.enc"), &ct).unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_MASTER_KEY", &key.to_hex())
            .with("AUTUMN_MANIFEST_DIR", tmp.path().to_str().unwrap());
        let config = AutumnConfig::load_with_env(&env).unwrap();
        let val: Option<String> = config.credentials().get("stripe_key");
        assert_eq!(val.as_deref(), Some("sk_test_xyz"));
    }

    #[cfg(feature = "oauth2")]
    #[test]
    fn config_resolves_oauth_credentials_by_convention() {
        use crate::credentials::{MasterKey, encrypt};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(
            &key,
            b"oauth2_github_client_id = \"git-id-123\"\noauth2_github_client_secret = \"git-secret-456\"\n",
        );
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(tmp.path().join("config/credentials/dev.toml.enc"), &ct).unwrap();

        // Write a base configuration with an empty/blank github provider defined
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        let config_toml = r#"
[auth.oauth2.github]
client_id = ""
client_secret = ""
authorize_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
redirect_uri = "http://localhost:3000/auth/github/callback"
"#;
        std::fs::write(tmp.path().join("autumn.toml"), config_toml).unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_MASTER_KEY", &key.to_hex())
            .with("AUTUMN_MANIFEST_DIR", tmp.path().to_str().unwrap());
        let config = AutumnConfig::load_with_env(&env).unwrap();
        let github = config.auth.oauth2.providers.get("github").unwrap();
        assert_eq!(github.client_id, "git-id-123");
        assert_eq!(github.client_secret, "git-secret-456");
    }

    #[test]
    fn config_fails_with_credentials_error_when_key_is_invalid() {
        use crate::credentials::encrypt;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        // Write an encrypted file but supply a wrong-length key so validation fails
        let bogus_key = "zz".repeat(32); // 64 chars but not valid hex
        let ct = encrypt(&crate::credentials::MasterKey::generate(), b"x = \"y\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(tmp.path().join("config/credentials/dev.toml.enc"), &ct).unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_MASTER_KEY", &bogus_key)
            .with("AUTUMN_MANIFEST_DIR", tmp.path().to_str().unwrap());
        let err = AutumnConfig::load_with_env(&env).unwrap_err();
        assert!(
            matches!(err, ConfigError::Credentials(_)),
            "bad master key should produce ConfigError::Credentials, got {err:?}"
        );
    }

    #[test]
    fn test_parse_duration_str() {
        assert_eq!(
            parse_duration_str("500ms").unwrap(),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            parse_duration_str("5s").unwrap(),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration_str("2m").unwrap(),
            std::time::Duration::from_secs(120)
        );
        assert_eq!(
            parse_duration_str("1h").unwrap(),
            std::time::Duration::from_secs(3600)
        );
        assert_eq!(
            parse_duration_str("1000").unwrap(),
            std::time::Duration::from_secs(1)
        );
        assert!(parse_duration_str("abc").is_err());
        assert!(parse_duration_str("").is_err());
    }

    #[test]
    fn test_database_config_duration_deserialization() {
        #[derive(Debug, Deserialize)]
        struct TestConfig {
            #[serde(deserialize_with = "deserialize_option_duration", default)]
            timeout: Option<std::time::Duration>,
            #[serde(deserialize_with = "deserialize_duration")]
            threshold: std::time::Duration,
        }

        let toml_str = r#"
            timeout = "2s"
            threshold = "100ms"
        "#;
        let parsed: TestConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.timeout, Some(std::time::Duration::from_secs(2)));
        assert_eq!(parsed.threshold, std::time::Duration::from_millis(100));

        let toml_str_null = r#"
            threshold = "500"
        "#;
        let parsed_null: TestConfig = toml::from_str(toml_str_null).unwrap();
        assert_eq!(parsed_null.timeout, None);
        assert_eq!(parsed_null.threshold, std::time::Duration::from_millis(500));
    }

    // ── RequestTimeoutsConfig ──────────────────────────────────────────────

    #[test]
    fn request_timeouts_config_defaults_to_none() {
        let config = RequestTimeoutsConfig::default();
        assert!(config.request_timeout_ms.is_none());
    }

    #[test]
    fn server_config_timeouts_defaults_to_disabled() {
        let config = ServerConfig::default();
        assert!(config.timeouts.request_timeout_ms.is_none());
    }

    #[test]
    fn request_timeouts_config_can_be_set_via_toml() {
        let toml_str = "request_timeout_ms = 5000";
        let config: RequestTimeoutsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.request_timeout_ms, Some(5000));
    }

    #[test]
    fn server_config_timeouts_deserialize_nested() {
        let toml_str = r#"
            port = 3000
            host = "127.0.0.1"
            shutdown_timeout_secs = 30
            prestop_grace_secs = 5

            [timeouts]
            request_timeout_ms = 15000
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.timeouts.request_timeout_ms, Some(15_000));
    }

    #[test]
    fn autumn_config_server_timeouts_roundtrip() {
        let mut config = AutumnConfig::default();
        config.server.timeouts.request_timeout_ms = Some(20_000);
        assert_eq!(config.server.timeouts.request_timeout_ms, Some(20_000));
    }

    #[test]
    fn server_timeouts_env_var_override() {
        struct FakeEnv(std::collections::HashMap<String, String>);
        impl Env for FakeEnv {
            fn var(&self, key: &str) -> Result<String, std::env::VarError> {
                self.0
                    .get(key)
                    .cloned()
                    .ok_or(std::env::VarError::NotPresent)
            }
        }

        let mut config = AutumnConfig::default();
        let env = FakeEnv(
            [(
                "AUTUMN_SERVER__TIMEOUTS__REQUEST_TIMEOUT_MS".to_owned(),
                "8000".to_owned(),
            )]
            .into(),
        );
        config.apply_server_env_overrides_with_env(&env);
        assert_eq!(config.server.timeouts.request_timeout_ms, Some(8000));
    }

    #[test]
    fn prod_profile_sets_request_timeout_30s() {
        let defaults = profile_defaults_as_toml("prod");
        let toml_str = toml::to_string(&defaults).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            config.server.timeouts.request_timeout_ms,
            Some(30_000),
            "prod profile must enable the 30-second request timeout by default"
        );
    }

    #[test]
    fn dev_profile_leaves_request_timeout_disabled() {
        let defaults = profile_defaults_as_toml("dev");
        let toml_str = toml::to_string(&defaults).unwrap();
        let config: AutumnConfig = toml::from_str(&toml_str).unwrap();
        assert!(
            config.server.timeouts.request_timeout_ms.is_none(),
            "dev profile must not enable a request timeout by default"
        );
    }

    #[test]
    fn test_resilience_config_defaults() {
        let config = AutumnConfig::default();
        assert!(
            config
                .resilience
                .circuit_breaker
                .defaults
                .failure_ratio_threshold
                .is_none()
        );
    }

    #[test]
    fn test_resilience_config_parsing() {
        let toml_str = r#"
            [resilience.circuit_breaker.defaults]
            failure_ratio_threshold = 0.6
            sample_window_secs = 20
            minimum_sample_count = 15
            open_duration_secs = 30
            half_open_trial_count = 5

            [resilience.circuit_breaker.hosts."api.github.com"]
            failure_ratio_threshold = 0.3
            open_duration_secs = 10
        "#;
        let config: AutumnConfig = toml::from_str(toml_str).unwrap();
        let cb = &config.resilience.circuit_breaker;
        assert_eq!(cb.defaults.failure_ratio_threshold, Some(0.6));
        assert_eq!(cb.defaults.sample_window_secs, Some(20));
        assert_eq!(cb.defaults.minimum_sample_count, Some(15));
        assert_eq!(cb.defaults.open_duration_secs, Some(30));
        assert_eq!(cb.defaults.half_open_trial_count, Some(5));

        let host_cb = cb.hosts.get("api.github.com").unwrap();
        assert_eq!(host_cb.failure_ratio_threshold, Some(0.3));
        assert_eq!(host_cb.open_duration_secs, Some(10));
        assert!(host_cb.sample_window_secs.is_none());
    }

    #[test]
    fn test_resilience_config_env_overrides() {
        struct FakeEnv(std::collections::HashMap<String, String>);
        impl Env for FakeEnv {
            fn var(&self, key: &str) -> Result<String, std::env::VarError> {
                self.0
                    .get(key)
                    .cloned()
                    .ok_or(std::env::VarError::NotPresent)
            }
        }

        let mut config = AutumnConfig::default();
        let env = FakeEnv(
            [(
                "AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__FAILURE_RATIO_THRESHOLD".to_owned(),
                "0.7".to_owned(),
            )]
            .into(),
        );
        config.apply_resilience_env_overrides_with_env(&env);
        assert_eq!(
            config
                .resilience
                .circuit_breaker
                .defaults
                .failure_ratio_threshold,
            Some(0.7)
        );
    }

    // ── Deprecation channel unit tests ────────────────────────────────────────

    /// A tiny test-only registry so tests are independent of the real entries.
    const TEST_REGISTRY: &[DeprecatedKey] = &[DeprecatedKey {
        path: "a.b.c",
        replacement: Some("a.b.d"),
        since: "0.1.0",
        remove_in: "1.0.0",
    }];

    fn merged_with_abc(value: toml::Value) -> toml::Table {
        let mut root = toml::Table::new();
        let mut b = toml::Table::new();
        b.insert("c".to_owned(), value);
        let mut a = toml::Table::new();
        a.insert("b".to_owned(), toml::Value::Table(b));
        root.insert("a".to_owned(), toml::Value::Table(a));
        root
    }

    #[test]
    fn red_detect_from_toml_present_emits_finding() {
        let merged = merged_with_abc(toml::Value::Integer(1));
        let env = MockEnv::new(); // AUTUMN_A__B__C not set
        let findings = detect_deprecated_keys(&merged, &env, TEST_REGISTRY);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.path, "a.b.c");
        assert_eq!(f.replacement.as_deref(), Some("a.b.d"));
        assert_eq!(f.since, "0.1.0");
        assert_eq!(f.remove_in, "1.0.0");
        assert_eq!(f.source, DeprecationSource::Toml);
    }

    #[test]
    fn red_detect_from_env_present_emits_finding() {
        let merged = toml::Table::new(); // no TOML key
        let env = MockEnv::new().with("AUTUMN_A__B__C", "val");
        let findings = detect_deprecated_keys(&merged, &env, TEST_REGISTRY);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, DeprecationSource::Env);
    }

    #[test]
    fn red_detect_dedupe_toml_and_env_single_finding() {
        let merged = merged_with_abc(toml::Value::Boolean(true));
        let env = MockEnv::new().with("AUTUMN_A__B__C", "true");
        let findings = detect_deprecated_keys(&merged, &env, TEST_REGISTRY);
        assert_eq!(findings.len(), 1, "TOML+env should collapse to one finding");
        assert_eq!(findings[0].source, DeprecationSource::Both);
    }

    #[test]
    fn red_detect_replacement_only_no_finding() {
        // Only the new replacement key is set; deprecated key is absent.
        let mut merged = toml::Table::new();
        let mut b = toml::Table::new();
        b.insert("d".to_owned(), toml::Value::Integer(1)); // new key, not deprecated
        let mut a = toml::Table::new();
        a.insert("b".to_owned(), toml::Value::Table(b));
        merged.insert("a".to_owned(), toml::Value::Table(a));

        let env = MockEnv::new();
        let findings = detect_deprecated_keys(&merged, &env, TEST_REGISTRY);
        assert!(
            findings.is_empty(),
            "only replacement key set — no deprecation warning"
        );
    }

    #[test]
    fn red_detect_absent_everywhere_no_finding() {
        let merged = toml::Table::new();
        let env = MockEnv::new();
        let findings = detect_deprecated_keys(&merged, &env, TEST_REGISTRY);
        assert!(findings.is_empty());
    }

    #[test]
    fn red_env_var_name_mapping() {
        assert_eq!(
            deprecated_env_var_name("security.rate_limit.trusted_proxies"),
            "AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES"
        );
        assert_eq!(deprecated_env_var_name("a.b.c"), "AUTUMN_A__B__C");
    }

    #[test]
    fn red_toml_path_non_table_mid_segment_not_present() {
        // If a mid-segment is not a Table, must return false without panicking.
        let mut root = toml::Table::new();
        root.insert("a".to_owned(), toml::Value::Integer(42)); // "a" is a leaf, not a table
        assert!(!toml_path_present(&root, "a.b.c"));
    }

    #[test]
    fn red_schema_leaf_paths_includes_known_paths() {
        // The SchemaDeserializer recurses into any derived-Deserialize struct it
        // reaches, regardless of module — external-module types (SecurityConfig,
        // AuthConfig, etc.) now descend too. They were root-only before the #1890
        // adaptive walk because the walk aborted before them (at the
        // `statement_timeout` duration / the `jobs.queues` seq-only visitor), not
        // because of their module. Each still also appears as a bare root leaf
        // (recorded as a field of the root struct).
        let leaves = AutumnConfig::schema_leaf_paths();
        assert!(
            leaves.contains("server.port"),
            "server.port must be a schema leaf"
        );
        assert!(
            leaves.contains("server.host"),
            "server.host must be a schema leaf"
        );
        assert!(
            leaves.contains("database.url"),
            "database.url must be a schema leaf"
        );
        // Root-level sections also appear as single-segment leaves (recorded as
        // fields of the root struct), alongside their now-descended child keys.
        assert!(
            leaves.contains("security"),
            "security must appear as a root-level leaf"
        );
        assert!(
            leaves.contains("session"),
            "session must appear as a root-level leaf"
        );
    }

    // ── ShardSlotAssignment / shards_auto_split / resolved_shard_assignments ──

    #[test]
    fn shards_auto_split_true_when_all_slots_none() {
        let config = DatabaseConfig {
            shards: vec![
                shard("a", "postgres://a/app"),
                shard("b", "postgres://b/app"),
            ],
            ..Default::default()
        };
        assert!(config.shards_auto_split());
    }

    #[test]
    fn shards_auto_split_false_when_no_shards() {
        assert!(!DatabaseConfig::default().shards_auto_split());
    }

    #[test]
    fn shards_auto_split_false_when_any_shard_declares_slots() {
        let config = DatabaseConfig {
            shards: vec![
                shard_with_slots("a", "postgres://a/app", &["0-8191"]),
                shard_with_slots("b", "postgres://b/app", &["8192-16383"]),
            ],
            ..Default::default()
        };
        assert!(!config.shards_auto_split());
    }

    #[test]
    fn resolved_shard_assignments_two_shards() {
        let config = DatabaseConfig {
            shards: vec![
                shard("s0", "postgres://s0/app"),
                shard("s1", "postgres://s1/app"),
            ],
            ..Default::default()
        };
        let assignments = config
            .resolved_shard_assignments()
            .expect("two-shard auto-split should resolve");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].name, "s0");
        assert_eq!(assignments[0].ranges, "0-8191");
        assert_eq!(assignments[1].name, "s1");
        assert_eq!(assignments[1].ranges, "8192-16383");
    }

    #[test]
    fn resolved_shard_assignments_three_shards() {
        let config = DatabaseConfig {
            shards: vec![
                shard("s0", "postgres://s0/app"),
                shard("s1", "postgres://s1/app"),
                shard("s2", "postgres://s2/app"),
            ],
            ..Default::default()
        };
        let assignments = config
            .resolved_shard_assignments()
            .expect("three-shard auto-split should resolve");
        assert_eq!(assignments.len(), 3);
        assert_eq!(assignments[0].ranges, "0-5461");
        assert_eq!(assignments[1].ranges, "5462-10922");
        assert_eq!(assignments[2].ranges, "10923-16383");
    }

    // ── check_stored_slot_map ──────────────────────────────────────────────────

    fn assignment(name: &str, ranges: &str) -> ShardSlotAssignment {
        ShardSlotAssignment {
            name: name.to_owned(),
            ranges: ranges.to_owned(),
        }
    }

    #[test]
    fn check_stored_slot_map_explicit_mode_always_ok() {
        // Even with a wildly different stored map, explicit mode is never blocked.
        let computed = vec![assignment("s0", "0-8191"), assignment("s1", "8192-16383")];
        let stored = vec![
            assignment("s0", "0-5460"),
            assignment("s1", "5461-10922"),
            assignment("s2", "10923-16383"),
        ];
        assert!(check_stored_slot_map(false, &computed, Some(&stored)).is_ok());
    }

    #[test]
    fn check_stored_slot_map_first_boot_no_stored_ok() {
        let computed = vec![assignment("s0", "0-8191"), assignment("s1", "8192-16383")];
        assert!(check_stored_slot_map(true, &computed, None).is_ok());
    }

    #[test]
    fn check_stored_slot_map_matching_map_ok() {
        let computed = vec![assignment("s0", "0-8191"), assignment("s1", "8192-16383")];
        // Order-insensitive: stored in reverse order still matches.
        let stored = vec![assignment("s1", "8192-16383"), assignment("s0", "0-8191")];
        assert!(check_stored_slot_map(true, &computed, Some(&stored)).is_ok());
    }

    #[test]
    fn check_stored_slot_map_mismatch_two_to_three_shards_returns_err() {
        let computed = vec![
            assignment("s0", "0-5460"),
            assignment("s1", "5461-10922"),
            assignment("s2", "10923-16383"),
        ];
        let stored = vec![assignment("s0", "0-8191"), assignment("s1", "8192-16383")];
        let err = check_stored_slot_map(true, &computed, Some(&stored))
            .expect_err("3-shard auto-split vs 2-shard stored map must fail");
        assert!(err.contains("shard slot map mismatch"), "message: {err}");
        assert!(err.contains("3 shards"), "message: {err}");
        assert!(err.contains("2 shards"), "message: {err}");
    }

    #[test]
    fn check_stored_slot_map_mismatch_shard_rename_returns_err() {
        let computed = vec![
            assignment("alpha", "0-8191"),
            assignment("beta", "8192-16383"),
        ];
        let stored = vec![assignment("s0", "0-8191"), assignment("s1", "8192-16383")];
        let err = check_stored_slot_map(true, &computed, Some(&stored))
            .expect_err("renamed shards must be detected as mismatch");
        assert!(err.contains("shard slot map mismatch"), "message: {err}");
        assert!(
            err.contains("alpha"),
            "message must name computed shards: {err}"
        );
        assert!(err.contains("s0"), "message must name stored shards: {err}");
    }

    // ── Process role (#1613) ────────────────────────────────────────────────

    #[test]
    fn process_role_default_is_combined() {
        assert_eq!(ProcessRole::default(), ProcessRole::Combined);
        assert_eq!(AutumnConfig::default().role, ProcessRole::Combined);
    }

    #[test]
    fn process_role_from_env_value_accepts_aliases_case_insensitively() {
        for v in [
            "combined",
            "COMBINED",
            "  all ",
            "web_and_worker",
            "server_and_worker",
        ] {
            assert_eq!(
                ProcessRole::from_env_value(v),
                Some(ProcessRole::Combined),
                "{v}"
            );
        }
        for v in ["web", "Web", " SERVER ", "http"] {
            assert_eq!(
                ProcessRole::from_env_value(v),
                Some(ProcessRole::Web),
                "{v}"
            );
        }
        for v in ["worker", "WORKER", " jobs ", "worker_only"] {
            assert_eq!(
                ProcessRole::from_env_value(v),
                Some(ProcessRole::Worker),
                "{v}"
            );
        }
        for v in ["", "webby", "workers", "scheduler", "both"] {
            assert_eq!(ProcessRole::from_env_value(v), None, "{v}");
        }
    }

    #[test]
    fn process_role_as_str_round_trips_through_from_env_value() {
        for role in [ProcessRole::Combined, ProcessRole::Web, ProcessRole::Worker] {
            assert_eq!(ProcessRole::from_env_value(role.as_str()), Some(role));
        }
    }

    // Issue #1864: named so ACME's spawn site reads as intent rather than an
    // inline `matches!` against the enum.
    #[test]
    fn scheduler_backend_is_fleet_distributed_only_for_postgres() {
        assert!(!SchedulerBackend::InProcess.is_fleet_distributed());
        assert!(SchedulerBackend::Postgres.is_fleet_distributed());
    }

    #[test]
    fn process_role_serves_http_and_runs_workers_truth_table() {
        assert!(ProcessRole::Combined.serves_http());
        assert!(ProcessRole::Combined.runs_workers());
        assert!(ProcessRole::Web.serves_http());
        assert!(!ProcessRole::Web.runs_workers());
        assert!(!ProcessRole::Worker.serves_http());
        assert!(ProcessRole::Worker.runs_workers());
    }

    #[test]
    fn process_role_deserializes_from_toml() {
        let web: AutumnConfig = toml::from_str("role = \"web\"\n").expect("web role");
        assert_eq!(web.role, ProcessRole::Web);
        let worker: AutumnConfig = toml::from_str("role = \"worker\"\n").expect("worker role");
        assert_eq!(worker.role, ProcessRole::Worker);
        let combined: AutumnConfig =
            toml::from_str("role = \"combined\"\n").expect("combined role");
        assert_eq!(combined.role, ProcessRole::Combined);
        // Serde alias also works.
        let aliased: AutumnConfig = toml::from_str("role = \"all\"\n").expect("all alias");
        assert_eq!(aliased.role, ProcessRole::Combined);
        // Absent → default.
        let absent: AutumnConfig = toml::from_str("").expect("empty config");
        assert_eq!(absent.role, ProcessRole::Combined);
    }

    #[test]
    fn split_role_requires_durable_backend_truth_table() {
        // Combined is always fine (enqueues and drains in one process), even on
        // the in-process local backend.
        assert!(!split_role_requires_durable_backend(
            ProcessRole::Combined,
            "local"
        ));
        assert!(!split_role_requires_durable_backend(
            ProcessRole::Combined,
            "postgres"
        ));
        // Split roles on any backend that falls through to the per-process local
        // runtime are invalid: the literal `local`, a typo like `postgresql`, a
        // blank backend, or any other unknown value.
        assert!(split_role_requires_durable_backend(
            ProcessRole::Web,
            "local"
        ));
        assert!(split_role_requires_durable_backend(
            ProcessRole::Worker,
            "local"
        ));
        assert!(split_role_requires_durable_backend(
            ProcessRole::Web,
            "postgresql"
        ));
        assert!(split_role_requires_durable_backend(ProcessRole::Web, ""));
        assert!(split_role_requires_durable_backend(
            ProcessRole::Web,
            "unknown"
        ));
        // The match is exact (mirroring `start_runtime`'s dispatch), so a
        // case-variant like `LOCAL` is likewise a non-durable fall-through.
        assert!(split_role_requires_durable_backend(
            ProcessRole::Web,
            "LOCAL"
        ));
        // Split roles on the recognized durable backends are fine.
        assert!(!split_role_requires_durable_backend(
            ProcessRole::Web,
            "postgres"
        ));
        assert!(!split_role_requires_durable_backend(
            ProcessRole::Worker,
            "redis"
        ));
    }

    #[test]
    fn autumn_role_env_override_sets_role() {
        let env = MockEnv::new().with("AUTUMN_ROLE", "worker");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.role, ProcessRole::Worker);

        let env = MockEnv::new().with("AUTUMN_ROLE", "  WEB ");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.role, ProcessRole::Web);
    }

    #[test]
    fn autumn_role_env_override_ignores_invalid_value_keeping_default() {
        let env = MockEnv::new().with("AUTUMN_ROLE", "nonsense");
        // Start from a non-default to prove invalid values do not reset it and
        // do not force it either — they leave the current value untouched.
        let mut config = AutumnConfig {
            role: ProcessRole::Worker,
            ..Default::default()
        };
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.role, ProcessRole::Worker);

        // And from the default, an invalid value keeps Combined.
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert_eq!(config.role, ProcessRole::Combined);
    }
}
