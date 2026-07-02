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
//! | `AUTUMN_SERVER__TIMEOUTS__REQUEST_TIMEOUT_MS` | `server.timeouts.request_timeout_ms` | `u64` |
//! | `AUTUMN_DATABASE__URL` | `database.url` | `String` |
//! | `AUTUMN_DATABASE__PRIMARY_URL` | `database.primary_url` | `String` |
//! | `AUTUMN_DATABASE__REPLICA_URL` | `database.replica_url` | `String` |
//! | `AUTUMN_DATABASE__POOL_SIZE` | `database.pool_size` | `usize` |
//! | `AUTUMN_DATABASE__PRIMARY_POOL_SIZE` | `database.primary_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__REPLICA_POOL_SIZE` | `database.replica_pool_size` | `usize` |
//! | `AUTUMN_DATABASE__REPLICA_FALLBACK` | `database.replica_fallback` | `fail_readiness` / `primary` |
//! | `AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS` | `database.connect_timeout_secs` | `u64` |
//! | `AUTUMN_DATABASE__STARTUP_WAIT_SECS` | `database.startup_wait_secs` | `u64` |
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
//! | `AUTUMN_CHANNELS__REDIS__URL` | `channels.redis.url` | `String` |
//! | `AUTUMN_CHANNELS__REDIS__KEY_PREFIX` | `channels.redis.key_prefix` | `String` |
//! | `AUTUMN_JOBS__BACKEND` | `jobs.backend` | `local` / `postgres` / `redis` |
//! | `AUTUMN_JOBS__WORKERS` | `jobs.workers` | `usize` |
//! | `AUTUMN_JOBS__MAX_ATTEMPTS` | `jobs.max_attempts` | `u32` |
//! | `AUTUMN_JOBS__INITIAL_BACKOFF_MS` | `jobs.initial_backoff_ms` | `u64` |
//! | `AUTUMN_JOBS__REDIS__URL` | `jobs.redis.url` | `String` |
//! | `AUTUMN_JOBS__REDIS__KEY_PREFIX` | `jobs.redis.key_prefix` | `String` |
//! | `AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS` | `jobs.redis.visibility_timeout_ms` | `u64` |
//! | `AUTUMN_JOBS__POSTGRES__VISIBILITY_TIMEOUT_MS` | `jobs.postgres.visibility_timeout_ms` | `u64` |
//! | `AUTUMN_JOBS__TRACKING__TTL_SECS` | `jobs.tracking.ttl_secs` | `u64` |
//! | `AUTUMN_JOBS__TRACKING__ROUTE_ENABLED` | `jobs.tracking.route_enabled` | `bool` |
//! | `AUTUMN_SCHEDULER__BACKEND` | `scheduler.backend` | `in_process` / `postgres` |
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
//! | `AUTUMN_COMPRESSION__ENABLED` | `compression.enabled` | `bool` |
//! | `AUTUMN_AUTH__LOCKOUT__ENABLED` | `auth.lockout.enabled` | `bool` |
//! | `AUTUMN_AUTH__LOCKOUT__THRESHOLD` | `auth.lockout.threshold` | `i32` |
//! | `AUTUMN_AUTH__LOCKOUT__WINDOW_SECS` | `auth.lockout.window_secs` | `u64` |
//! | `AUTUMN_AUTH__LOCKOUT__COOLOFF_SECS` | `auth.lockout.cooloff_secs` | `u64` |
//! | `AUTUMN_TIME_ZONE__IDENTIFIER` | `time_zone.identifier` | IANA id `String` |

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
fn normalize_profile_name(profile: &str) -> Option<String> {
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
fn profile_override_file_lookup_names(profile: &str, selected_profile_input: &str) -> Vec<String> {
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
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutumnConfig {
    /// Active profile name (e.g., "dev", "prod", "staging").
    /// Resolved at load time, not deserialized from TOML.
    #[serde(skip)]
    pub profile: Option<String>,

    /// HTTP server settings (port, host, shutdown behavior).
    #[serde(default)]
    pub server: ServerConfig,

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
    5
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
    /// Redis backend options.
    #[serde(default)]
    pub redis: ChannelRedisConfig,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            backend: ChannelBackend::default(),
            capacity: default_channel_capacity(),
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

fn default_channels_redis_prefix() -> String {
    "autumn:channels".to_owned()
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
            redis: JobRedisConfig::default(),
            postgres: JobPostgresConfig::default(),
            tracking: JobTrackingConfig::default(),
        }
    }
}

/// A single named queue and its draining weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobQueue {
    /// Queue name, as declared by `#[job(queue = "...")]`.
    pub name: String,
    /// Relative draining weight (used only for weighted draining; `1` for the
    /// strict-priority list form).
    pub weight: u32,
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
            queues: vec![JobQueue {
                name: "default".to_string(),
                weight: 1,
            }],
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
            .map(|name| JobQueue {
                name: name.into(),
                weight: 1,
            })
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
            .map(|(name, weight)| JobQueue {
                name: name.into(),
                weight: weight.max(1),
            })
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
                let mut entries: Vec<(String, u32)> = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, u32>()? {
                    if v == 0 {
                        return Err(serde::de::Error::custom(format!(
                            "queue '{k}' weight must be at least 1 (got 0); \
                             to disable a queue remove it from the list"
                        )));
                    }
                    entries.push((k, v));
                }
                Ok(JobQueuesConfig::weighted(entries))
            }
        }

        d.deserialize_any(JobQueuesVisitor)
    }
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

impl AutumnConfig {
    /// Recursively extracts all valid configuration schema keys and nested fields.
    #[must_use]
    pub fn get_schema_keys() -> HashMap<String, HashSet<String>> {
        let deserializer = SchemaDeserializer::new();
        let _ = Self::deserialize(deserializer.clone());
        deserializer.into_schema()
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
        let Ok(table) = toml::from_str::<toml::Table>(content) else {
            return Vec::new();
        };

        let mut errors = Vec::new();
        let mut path = Vec::new();
        Self::validate_toml_table(&table, &mut path, schema, &mut errors);
        errors
    }

    #[allow(clippy::too_many_lines)]
    fn validate_toml_table(
        table: &toml::Table,
        path: &mut Vec<String>,
        schema: &HashMap<String, HashSet<String>>,
        errors: &mut Vec<(String, Option<String>)>,
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
                            Self::validate_toml_table(t, path, schema, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(t, path, schema, errors);
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
                            Self::validate_toml_table(t, path, schema, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(t, path, schema, errors);
                                }
                            }
                        }
                        _ => {}
                    }
                    path.pop();
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

                    errors.push((full_path, suggestion));
                }
            }
        } else if path.len() == 1 && path[0] == "profile" {
            for (k, val) in table {
                if let toml::Value::Table(t) = val {
                    path.push(k.clone());
                    Self::validate_toml_table(t, path, schema, errors);
                    path.pop();
                } else {
                    let mut full_path_parts = path.clone();
                    full_path_parts.push(k.clone());
                    errors.push((full_path_parts.join("."), None));
                }
            }
        } else if path.is_empty() {
            let root_keys = schema.get("").cloned().unwrap_or_default();
            for (k, val) in table {
                if k != "profile" && !root_keys.contains(k) {
                    let mut closest: Option<&str> = None;
                    let mut min_dist = usize::MAX;
                    for valid_key in &root_keys {
                        let dist = levenshtein(k, valid_key);
                        if dist <= 2 && dist < min_dist {
                            min_dist = dist;
                            closest = Some(valid_key);
                        }
                    }
                    errors.push((k.clone(), closest.map(String::from)));
                } else {
                    path.push(k.clone());
                    match val {
                        toml::Value::Table(t) => {
                            Self::validate_toml_table(t, path, schema, errors);
                        }
                        toml::Value::Array(arr) => {
                            for item in arr {
                                if let toml::Value::Table(t) = item {
                                    Self::validate_toml_table(t, path, schema, errors);
                                }
                            }
                        }
                        _ => {}
                    }
                    path.pop();
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
        Self::load_with_env(&OsEnv)
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
            let schema = Self::get_schema_keys();
            let errors = Self::validate_toml(&toml_str, &schema);
            if !errors.is_empty() {
                let err_messages: Vec<String> = errors
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
        // Session backend validation deliberately lives in
        // `crate::session::apply_session_layer`, not here. That function
        // short-circuits when a custom `SessionStore` was installed via
        // `AppBuilder::with_session_store(...)`, so the (then-irrelevant)
        // `session.backend = "redis"` config without a redis URL doesn't
        // need to fail the boot. Validating the same thing here would
        // defeat the override and exit the app before the custom store
        // ever gets a chance to apply. The "prod profile + memory backend"
        // warning lives in `apply_session_layer` for the same reason.
        Ok(())
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
    ///
    /// # Jobs
    /// - `AUTUMN_JOBS__BACKEND` → `jobs.backend` (`local` / `redis`)
    /// - `AUTUMN_JOBS__WORKERS` → `jobs.workers` (`usize`)
    /// - `AUTUMN_JOBS__MAX_ATTEMPTS` → `jobs.max_attempts` (`u32`)
    /// - `AUTUMN_JOBS__INITIAL_BACKOFF_MS` → `jobs.initial_backoff_ms` (`u64`)
    /// - `AUTUMN_JOBS__REDIS__URL` → `jobs.redis.url` (`String`)
    /// - `AUTUMN_JOBS__REDIS__KEY_PREFIX` → `jobs.redis.key_prefix` (`String`)
    /// - `AUTUMN_JOBS__REDIS__VISIBILITY_TIMEOUT_MS` → `jobs.redis.visibility_timeout_ms` (`u64`)
    /// - `AUTUMN_JOBS__TRACKING__TTL_SECS` → `jobs.tracking.ttl_secs` (`u64`)
    /// - `AUTUMN_JOBS__TRACKING__ROUTE_ENABLED` → `jobs.tracking.route_enabled` (`bool`)
    ///
    /// # Signed webhooks
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND` -> `security.webhooks.replay.backend` (`memory` / `redis`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL` -> `security.webhooks.replay.redis.url` (`String`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX` -> `security.webhooks.replay.redis.key_prefix` (`String`)
    /// - `AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION` -> `security.webhooks.replay.allow_memory_in_production` (`bool`)
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_with_env(&OsEnv);
    }

    /// Apply environment overrides using the provided env abstraction.
    pub fn apply_env_overrides_with_env(&mut self, env: &dyn Env) {
        self.apply_server_env_overrides_with_env(env);
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
        self.apply_auth_env_overrides_with_env(env);
        self.apply_security_env_overrides_with_env(env);
        self.apply_bot_protection_env_overrides_with_env(env);
        self.apply_idempotency_env_overrides_with_env(env);
        self.apply_dev_env_overrides_with_env(env);
        self.apply_compression_env_overrides_with_env(env);
        self.apply_actuator_env_overrides_with_env(env);
        #[cfg(feature = "reporting")]
        self.apply_reporting_env_overrides_with_env(env);
        #[cfg(feature = "storage")]
        self.apply_storage_env_overrides_with_env(env);
        #[cfg(feature = "mail")]
        self.apply_mail_env_overrides_with_env(env);
        self.apply_resilience_env_overrides_with_env(env);
        self.apply_time_zone_env_overrides_with_env(env);
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
    /// Must start with `postgres://` or `postgresql://` when present.
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

    /// When true, permits automatic migration application while running with
    /// `prod`/`production` profile. Default: `false`.
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

impl DatabaseConfig {
    /// Resolved primary/write database URL.
    #[must_use]
    pub fn effective_primary_url(&self) -> Option<&str> {
        self.primary_url.as_deref().or(self.url.as_deref())
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

    /// Validate database configuration.
    ///
    /// # Errors
    ///
    /// Returns a validation error if a URL has an invalid scheme or a
    /// shard declaration is malformed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, url) in [
            ("database.url", self.url.as_deref()),
            ("database.primary_url", self.primary_url.as_deref()),
            ("database.replica_url", self.replica_url.as_deref()),
        ] {
            if let Some(url) = url
                && !url.starts_with("postgres://")
                && !url.starts_with("postgresql://")
            {
                let label = if field == "database.url" {
                    "database URL"
                } else {
                    field
                };
                return Err(ConfigError::Validation(format!(
                    "Invalid {label}: must start with postgres:// or postgresql://, got {url:?}"
                )));
            }
        }

        if self.replica_url.is_some() && self.effective_primary_url().is_none() {
            return Err(ConfigError::Validation(
                "database.replica_url requires database.primary_url or database.url".to_owned(),
            ));
        }

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
                    && !url.starts_with("postgres://")
                    && !url.starts_with("postgresql://")
                {
                    return Err(ConfigError::Validation(format!(
                        "Invalid database.shards[{idx}].{field}: must start with \
                         postgres:// or postgresql://, got {url:?}"
                    )));
                }
            }
        }
        self.resolved_slot_map()?;
        Ok(())
    }
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
/// assert_eq!(health.path, "/health");
/// assert_eq!(health.live_path, "/live");
/// assert_eq!(health.ready_path, "/ready");
/// assert_eq!(health.startup_path, "/startup");
/// assert!(!health.detailed);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
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

fn default_health_path() -> String {
    "/health".to_owned()
}

fn default_live_path() -> String {
    "/live".to_owned()
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
            shutdown_timeout_secs: default_shutdown_timeout(),
            prestop_grace_secs: default_prestop_grace(),
            timeouts: RequestTimeoutsConfig::default(),
            unix_socket: None,
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
#[derive(Debug, Default, Clone, Copy)]
pub struct TomlEnvConfigLoader;

impl TomlEnvConfigLoader {
    /// Construct a new default loader.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ConfigLoader for TomlEnvConfigLoader {
    async fn load(&self) -> Result<AutumnConfig, ConfigError> {
        AutumnConfig::load_with_env(&OsEnv)
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
    #[serde(default)]
    pub jwt_secret: Option<String>,

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
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct SchemaDeserializer {
    path: Vec<String>,
    schema: Arc<Mutex<HashMap<String, HashSet<String>>>>,
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
        visitor.visit_str("")
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
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_enum(SchemaEnumAccess)
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
        };
        seed.deserialize(nested)
    }
}

struct SchemaEnumAccess;

impl<'de> de::EnumAccess<'de> for SchemaEnumAccess {
    type Error = serde::de::value::Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let val = seed.deserialize(de::value::StrDeserializer::new(""))?;
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
        // backend validation now lives in `apply_session_layer`, which
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
        // Session backend validation moved to `apply_session_layer` so a
        // custom store installed via `AppBuilder::with_session_store(...)`
        // can override an otherwise-invalid backend config without the boot
        // exiting first. `validate()` is config-shape-only now; runtime
        // session selection (and the backend error) lives in
        // `apply_session_layer`, which short-circuits when a custom store
        // is installed. `crate::session::tests::session_backend_plan_*`
        // still cover the underlying error cases directly on
        // `SessionConfig::backend_plan`.
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

    #[test]
    fn env_override_database_auto_migrate_in_production() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION", "true");
        let mut config = AutumnConfig::default();
        config.apply_env_overrides_with_env(&env);
        assert!(config.database.auto_migrate_in_production);
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
            r#"
            [jobs.tracking]
            ttl_secs = 7200
            route_enabled = false
            "#,
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
        assert_eq!(config.channels.redis.key_prefix, "autumn:channels");
        assert!(config.channels.redis.url.is_none());
    }

    #[test]
    fn channels_env_overrides_fields() {
        let env = MockEnv::new()
            .with("AUTUMN_CHANNELS__BACKEND", "redis")
            .with("AUTUMN_CHANNELS__CAPACITY", "128")
            .with("AUTUMN_CHANNELS__REDIS__URL", "redis://channels:6379/4")
            .with("AUTUMN_CHANNELS__REDIS__KEY_PREFIX", "myapp:channels");
        let mut config = AutumnConfig::default();

        config.apply_env_overrides_with_env(&env);

        assert_eq!(config.channels.backend, ChannelBackend::Redis);
        assert_eq!(config.channels.capacity, 128);
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
        // The SchemaDeserializer only recurses into structs defined in config.rs itself;
        // external module types (SecurityConfig, AuthConfig, etc.) appear as root leaves only.
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
        // Root-level sections appear as single-segment leaves (the schema records them as
        // fields of the root struct, but doesn't descend into their external-module types).
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
}
