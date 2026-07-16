//! Configuration for [`MediaPlugin`](crate::MediaPlugin).
//!
//! [`MediaConfig`] models the `[media]` section of an Autumn `autumn.toml`
//! profile. Because Autumn resolves config *after* [`Plugin::build`] runs, the
//! plugin cannot read `[media]` from inside `build`; instead the application
//! loads a [`MediaConfig`] up front (via [`MediaConfig::from_autumn_toml`] or
//! the [`MediaConfig::from_arroyo_env`] compatibility shim) and hands it to the
//! plugin builder — mirroring the `autumn-storage-s3`
//! `from_config(&cfg) -> …with_blob_store(store)` pattern.
//!
//! [`Plugin::build`]: autumn_web::plugin::Plugin::build
//!
//! ```toml
//! [media]
//! room_max_participants = 6
//!
//! [media.mediamtx]
//! api_base = "http://127.0.0.1:9997"
//!
//! [media.storage]
//! backend = "s3"
//! bucket = "${MEDIA_BUCKET}"
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

// ── Defaults ────────────────────────────────────────────────────────────────
//
// The localhost dev values shown in the ratified `[media]` spec. Kept as free
// functions so `serde(default = "…")` and the `Default` impls share one source
// of truth and can never drift.

fn default_mediamtx_api_base() -> String {
    "http://127.0.0.1:9997".to_owned()
}

fn default_mediamtx_rtmp_base() -> String {
    "rtmp://127.0.0.1:1935/live".to_owned()
}

fn default_mediamtx_hls_base() -> String {
    "http://127.0.0.1:8888".to_owned()
}

fn default_mediamtx_webrtc_base() -> String {
    "http://127.0.0.1:8889".to_owned()
}

fn default_mediamtx_playback_base() -> String {
    "http://127.0.0.1:9996".to_owned()
}

fn default_ffmpeg_bin() -> String {
    "/usr/bin/ffmpeg".to_owned()
}

/// Neutral local media root used when the storage backend is `local` and no
/// filesystem root is configured.
fn default_local_root() -> String {
    "media".to_owned()
}

fn default_s3_key_prefix() -> String {
    "media".to_owned()
}

const fn default_recording_retention_days() -> u32 {
    14
}

/// Hard cap on participants in a mesh room (no SFU).
pub const DEFAULT_ROOM_MAX_PARTICIPANTS: usize = 6;

const fn default_room_max_participants() -> usize {
    DEFAULT_ROOM_MAX_PARTICIPANTS
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors returned while loading or validating a [`MediaConfig`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MediaConfigError {
    /// `[media.storage].backend = "s3"` but no bucket was configured.
    #[error("media.storage.bucket is required when backend = \"s3\"")]
    MissingBucket,
    /// Exactly one of `access_key_id` / `secret_access_key` was set. Both must
    /// be provided together, or neither (to use the ambient credential chain).
    #[error(
        "partial S3 credentials: media.storage.access_key_id and \
         media.storage.secret_access_key must both be set, or neither"
    )]
    MissingS3Credential,
    /// The TOML file could not be parsed.
    #[error("failed to parse media config TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// The TOML file could not be read from disk.
    #[error("failed to read media config file: {0}")]
    Io(#[from] std::io::Error),
}

// ── Config structs ──────────────────────────────────────────────────────────

/// `MediaMTX` origin URLs (browser-facing plus optional server-side probe
/// overrides).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MediaMtxConfig {
    /// `MediaMTX` control API base (e.g. `/v3/paths/list`).
    pub api_base: String,
    /// RTMP ingest base URL.
    pub rtmp_base: String,
    /// Browser-facing HLS playback base URL.
    pub hls_base: String,
    /// Optional server-side HLS probe base; falls back to `hls_base` at access
    /// time via [`MediaMtxConfig::hls_probe_base`].
    pub hls_probe_base: Option<String>,
    /// Browser-facing WebRTC/WHEP base URL.
    pub webrtc_base: String,
    /// Browser-facing recording-playback base URL.
    pub playback_base: String,
    /// Optional server-side playback probe base; falls back to `playback_base`
    /// at access time via [`MediaMtxConfig::playback_probe_base`].
    pub playback_probe_base: Option<String>,
}

impl Default for MediaMtxConfig {
    fn default() -> Self {
        Self {
            api_base: default_mediamtx_api_base(),
            rtmp_base: default_mediamtx_rtmp_base(),
            hls_base: default_mediamtx_hls_base(),
            hls_probe_base: None,
            webrtc_base: default_mediamtx_webrtc_base(),
            playback_base: default_mediamtx_playback_base(),
            playback_probe_base: None,
        }
    }
}

impl MediaMtxConfig {
    /// Effective server-side HLS probe base — `hls_probe_base` when set,
    /// otherwise the browser-facing `hls_base`.
    #[must_use]
    pub fn hls_probe_base(&self) -> &str {
        self.hls_probe_base.as_deref().unwrap_or(&self.hls_base)
    }

    /// Effective server-side playback probe base — `playback_probe_base` when
    /// set, otherwise the browser-facing `playback_base`.
    #[must_use]
    pub fn playback_probe_base(&self) -> &str {
        self.playback_probe_base
            .as_deref()
            .unwrap_or(&self.playback_base)
    }
}

/// `FFmpeg` binary location for encode work (later slices).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FfmpegConfig {
    /// Path to (or name on `PATH` of) the `FFmpeg` binary.
    pub bin: String,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
        }
    }
}

/// Which storage backend completed media objects are written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MediaStorageBackend {
    /// Keep media on the local filesystem.
    #[default]
    Local,
    /// Upload media to an S3-compatible object store.
    S3,
}

impl MediaStorageBackend {
    /// Stable lowercase label for logs/summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
        }
    }
}

/// Media object storage settings (local filesystem or S3-compatible).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MediaStorageConfig {
    /// Selected backend. Defaults to [`MediaStorageBackend::Local`].
    pub backend: MediaStorageBackend,
    /// Filesystem root used by the `local` backend.
    pub root: String,
    /// S3 bucket name (required for `s3`).
    pub bucket: Option<String>,
    /// S3 endpoint URL (blank → the SDK default).
    pub endpoint_url: Option<String>,
    /// S3 region.
    pub region: Option<String>,
    /// S3 access key id (paired with `secret_access_key`).
    pub access_key_id: Option<String>,
    /// S3 secret access key (paired with `access_key_id`).
    pub secret_access_key: Option<String>,
    /// Public base URL served objects resolve under.
    pub public_base_url: Option<String>,
    /// Object key prefix (namespace) for stored media.
    pub key_prefix: String,
    /// Whether to force S3 path-style addressing.
    pub force_path_style: bool,
}

impl Default for MediaStorageConfig {
    fn default() -> Self {
        Self {
            backend: MediaStorageBackend::Local,
            root: default_local_root(),
            bucket: None,
            endpoint_url: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            public_base_url: None,
            key_prefix: default_s3_key_prefix(),
            force_path_style: false,
        }
    }
}

/// Recording retention policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// Days after a broadcast ends before its recording is swept. `0` disables
    /// the retention sweep.
    pub retention_days: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            retention_days: default_recording_retention_days(),
        }
    }
}

/// The full `[media]` configuration surface.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    /// `MediaMTX` origin URLs.
    pub mediamtx: MediaMtxConfig,
    /// `FFmpeg` binary location.
    pub ffmpeg: FfmpegConfig,
    /// Media object storage settings.
    pub storage: MediaStorageConfig,
    /// Recording retention policy.
    pub recording: RecordingConfig,
    /// Hard cap on mesh-room participants (no SFU).
    pub room_max_participants: usize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            mediamtx: MediaMtxConfig::default(),
            ffmpeg: FfmpegConfig::default(),
            storage: MediaStorageConfig::default(),
            recording: RecordingConfig::default(),
            room_max_participants: default_room_max_participants(),
        }
    }
}

/// The `[media]` table wrapper used to deserialize a full `autumn.toml`.
#[derive(Debug, Default, Deserialize)]
struct AutumnTomlRoot {
    #[serde(default)]
    media: MediaConfig,
}

impl MediaConfig {
    /// Validate cross-field invariants.
    ///
    /// # Errors
    ///
    /// Returns [`MediaConfigError::MissingBucket`] when the S3 backend is
    /// selected without a bucket, and [`MediaConfigError::MissingS3Credential`]
    /// when exactly one of the access-key / secret-key pair is set.
    pub fn validate(&self) -> Result<(), MediaConfigError> {
        if self.storage.backend != MediaStorageBackend::S3 {
            return Ok(());
        }
        if non_empty(self.storage.bucket.as_deref()).is_none() {
            return Err(MediaConfigError::MissingBucket);
        }
        let has_key = non_empty(self.storage.access_key_id.as_deref()).is_some();
        let has_secret = non_empty(self.storage.secret_access_key.as_deref()).is_some();
        if has_key != has_secret {
            return Err(MediaConfigError::MissingS3Credential);
        }
        Ok(())
    }

    /// Load a [`MediaConfig`] from an `autumn.toml`-shaped file, then apply
    /// `AUTUMN_MEDIA__*` environment overrides and `${VAR}` interpolation.
    ///
    /// A missing file, or a file without a `[media]` table, yields
    /// [`MediaConfig::default`] (still with env overrides applied). This is a
    /// focused loader — it is **not** Autumn's full five-layer config loader.
    ///
    /// # Errors
    ///
    /// Returns [`MediaConfigError::Io`] on a read error other than
    /// "not found", and [`MediaConfigError::Toml`] when the file does not parse.
    pub fn from_autumn_toml(path: impl AsRef<Path>) -> Result<Self, MediaConfigError> {
        let contents = match std::fs::read_to_string(path.as_ref()) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(MediaConfigError::Io(err)),
        };
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_toml_str_with_env(&contents, &env)
    }

    /// Pure core of [`from_autumn_toml`](Self::from_autumn_toml): parse the TOML
    /// string, then apply `AUTUMN_MEDIA__*` overrides and `${VAR}` interpolation
    /// from the supplied environment map. Keeps parsing testable without
    /// touching process-global environment.
    ///
    /// # Errors
    ///
    /// Returns [`MediaConfigError::Toml`] when the string does not parse.
    pub fn from_toml_str_with_env(
        toml_str: &str,
        env: &HashMap<String, String>,
    ) -> Result<Self, MediaConfigError> {
        let root: AutumnTomlRoot = toml::from_str(toml_str)?;
        let mut config = root.media;
        config.interpolate_env(env);
        config.apply_env_overrides(env);
        Ok(config)
    }

    /// Resolve every `${VAR}` placeholder in string fields from `env`, leaving
    /// the value empty (`None` for optionals) when the variable is unset.
    fn interpolate_env(&mut self, env: &HashMap<String, String>) {
        interpolate(&mut self.mediamtx.api_base, env);
        interpolate(&mut self.mediamtx.rtmp_base, env);
        interpolate(&mut self.mediamtx.hls_base, env);
        interpolate_opt(&mut self.mediamtx.hls_probe_base, env);
        interpolate(&mut self.mediamtx.webrtc_base, env);
        interpolate(&mut self.mediamtx.playback_base, env);
        interpolate_opt(&mut self.mediamtx.playback_probe_base, env);
        interpolate(&mut self.ffmpeg.bin, env);
        interpolate(&mut self.storage.root, env);
        interpolate_opt(&mut self.storage.bucket, env);
        interpolate_opt(&mut self.storage.endpoint_url, env);
        interpolate_opt(&mut self.storage.region, env);
        interpolate_opt(&mut self.storage.access_key_id, env);
        interpolate_opt(&mut self.storage.secret_access_key, env);
        interpolate_opt(&mut self.storage.public_base_url, env);
        interpolate(&mut self.storage.key_prefix, env);
    }

    /// Apply `AUTUMN_MEDIA__<TABLE>__<FIELD>` scalar leaf overrides
    /// (double-underscore = table nesting).
    fn apply_env_overrides(&mut self, env: &HashMap<String, String>) {
        if let Some(value) = env.get("AUTUMN_MEDIA__ROOM_MAX_PARTICIPANTS")
            && let Ok(parsed) = value.parse::<usize>()
        {
            self.room_max_participants = parsed;
        }

        override_string(
            &mut self.mediamtx.api_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__API_BASE",
        );
        override_string(
            &mut self.mediamtx.rtmp_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__RTMP_BASE",
        );
        override_string(
            &mut self.mediamtx.hls_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__HLS_BASE",
        );
        override_opt(
            &mut self.mediamtx.hls_probe_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__HLS_PROBE_BASE",
        );
        override_string(
            &mut self.mediamtx.webrtc_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__WEBRTC_BASE",
        );
        override_string(
            &mut self.mediamtx.playback_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__PLAYBACK_BASE",
        );
        override_opt(
            &mut self.mediamtx.playback_probe_base,
            env,
            "AUTUMN_MEDIA__MEDIAMTX__PLAYBACK_PROBE_BASE",
        );

        override_string(&mut self.ffmpeg.bin, env, "AUTUMN_MEDIA__FFMPEG__BIN");

        if let Some(value) = env.get("AUTUMN_MEDIA__STORAGE__BACKEND") {
            self.storage.backend = match value.trim().to_ascii_lowercase().as_str() {
                "s3" => MediaStorageBackend::S3,
                _ => MediaStorageBackend::Local,
            };
        }
        override_string(&mut self.storage.root, env, "AUTUMN_MEDIA__STORAGE__ROOT");
        override_opt(
            &mut self.storage.bucket,
            env,
            "AUTUMN_MEDIA__STORAGE__BUCKET",
        );
        override_opt(
            &mut self.storage.endpoint_url,
            env,
            "AUTUMN_MEDIA__STORAGE__ENDPOINT_URL",
        );
        override_opt(
            &mut self.storage.region,
            env,
            "AUTUMN_MEDIA__STORAGE__REGION",
        );
        override_opt(
            &mut self.storage.access_key_id,
            env,
            "AUTUMN_MEDIA__STORAGE__ACCESS_KEY_ID",
        );
        override_opt(
            &mut self.storage.secret_access_key,
            env,
            "AUTUMN_MEDIA__STORAGE__SECRET_ACCESS_KEY",
        );
        override_opt(
            &mut self.storage.public_base_url,
            env,
            "AUTUMN_MEDIA__STORAGE__PUBLIC_BASE_URL",
        );
        override_string(
            &mut self.storage.key_prefix,
            env,
            "AUTUMN_MEDIA__STORAGE__KEY_PREFIX",
        );
        if let Some(value) = env.get("AUTUMN_MEDIA__STORAGE__FORCE_PATH_STYLE") {
            self.storage.force_path_style = matches_bool(value);
        }

        if let Some(value) = env.get("AUTUMN_MEDIA__RECORDING__RETENTION_DAYS")
            && let Ok(parsed) = value.parse::<u32>()
        {
            self.recording.retention_days = parsed;
        }
    }

    /// The ratified compatibility shim: map an Arroyo operator's existing
    /// `ARROYO_*` (and `AWS_*` / `BUCKET_NAME`) environment onto a
    /// [`MediaConfig`], so migrating to `autumn-media-plugin` changes nothing.
    ///
    /// Where Arroyo has no equivalent variable, the [`MediaConfig`] default is
    /// kept. This reads the process environment; the pure mapping lives in
    /// [`from_arroyo_env_pairs`](Self::from_arroyo_env_pairs) for testability.
    #[must_use]
    pub fn from_arroyo_env() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_arroyo_env_pairs(&env)
    }

    /// Pure core of [`from_arroyo_env`](Self::from_arroyo_env): map the supplied
    /// `ARROYO_*` environment map onto a [`MediaConfig`] without touching
    /// process-global environment (mirrors Arroyo's
    /// `VideoStorageConfig::from_env_pairs`).
    #[must_use]
    pub fn from_arroyo_env_pairs(env: &HashMap<String, String>) -> Self {
        let mut config = Self::default();

        // MediaMTX origins.
        if let Some(value) = arroyo_value(env, "ARROYO_MEDIAMTX_API_BASE") {
            config.mediamtx.api_base = value;
        }
        if let Some(value) = arroyo_value(env, "ARROYO_RTMP_BASE_URL") {
            config.mediamtx.rtmp_base = value;
        }
        if let Some(value) = arroyo_value(env, "ARROYO_HLS_BASE_URL") {
            config.mediamtx.hls_base = value;
        }
        config.mediamtx.hls_probe_base = arroyo_value(env, "ARROYO_HLS_PROBE_BASE_URL");
        if let Some(value) = arroyo_value(env, "ARROYO_WEBRTC_BASE_URL") {
            config.mediamtx.webrtc_base = value;
        }
        if let Some(value) = arroyo_value(env, "ARROYO_MEDIAMTX_PLAYBACK_BASE") {
            config.mediamtx.playback_base = value;
        }
        config.mediamtx.playback_probe_base =
            arroyo_value(env, "ARROYO_MEDIAMTX_PLAYBACK_PROBE_BASE");

        // FFmpeg.
        if let Some(value) = arroyo_value(env, "ARROYO_FFMPEG_BIN") {
            config.ffmpeg.bin = value;
        }

        // Storage backend + S3 settings.
        let bucket =
            arroyo_value(env, "ARROYO_S3_BUCKET").or_else(|| arroyo_value(env, "BUCKET_NAME"));
        let access_key_id = arroyo_value(env, "ARROYO_S3_ACCESS_KEY_ID")
            .or_else(|| arroyo_value(env, "AWS_ACCESS_KEY_ID"));
        let secret_access_key = arroyo_value(env, "ARROYO_S3_SECRET_ACCESS_KEY")
            .or_else(|| arroyo_value(env, "AWS_SECRET_ACCESS_KEY"));

        let backend = arroyo_value(env, "ARROYO_VIDEO_STORAGE_BACKEND").map_or_else(
            || {
                if bucket.is_some() && access_key_id.is_some() && secret_access_key.is_some() {
                    MediaStorageBackend::S3
                } else {
                    MediaStorageBackend::Local
                }
            },
            |raw| match raw.trim().to_ascii_lowercase().as_str() {
                "s3" | "tigris" => MediaStorageBackend::S3,
                _ => MediaStorageBackend::Local,
            },
        );

        config.storage.backend = backend;
        config.storage.bucket = bucket;
        config.storage.access_key_id = access_key_id;
        config.storage.secret_access_key = secret_access_key;
        config.storage.endpoint_url = arroyo_value(env, "ARROYO_S3_ENDPOINT_URL")
            .or_else(|| arroyo_value(env, "AWS_ENDPOINT_URL_S3"));
        config.storage.region =
            arroyo_value(env, "ARROYO_S3_REGION").or_else(|| arroyo_value(env, "AWS_REGION"));
        config.storage.public_base_url = arroyo_value(env, "ARROYO_PUBLIC_HIGHLIGHTS_BASE")
            .or_else(|| arroyo_value(env, "ARROYO_S3_PUBLIC_BASE_URL"));
        if let Some(value) = arroyo_value(env, "ARROYO_S3_KEY_PREFIX") {
            config.storage.key_prefix = value;
        }
        if let Some(value) = arroyo_value(env, "ARROYO_S3_FORCE_PATH_STYLE") {
            config.storage.force_path_style = matches_bool(&value);
        }

        // Recording retention.
        if let Some(value) = arroyo_value(env, "ARROYO_RECORDING_RETENTION_DAYS")
            && let Ok(parsed) = value.parse::<u32>()
        {
            config.recording.retention_days = parsed;
        }

        config
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Return `Some(trimmed-non-empty)` view of an optional string.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Read a non-blank env var value (blank values are treated as unset).
fn arroyo_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Interpret a truthy string toggle.
fn matches_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Resolve a single `${VAR}` placeholder in-place, or leave the string as-is
/// when it is not a placeholder. An unset variable resolves to an empty string.
fn interpolate(value: &mut String, env: &HashMap<String, String>) {
    if let Some(resolved) = resolve_placeholder(value, env) {
        *value = resolved;
    }
}

/// Placeholder interpolation for an optional string; a resolved-empty value
/// becomes `None`.
fn interpolate_opt(value: &mut Option<String>, env: &HashMap<String, String>) {
    if let Some(inner) = value.as_ref()
        && let Some(resolved) = resolve_placeholder(inner, env)
    {
        *value = if resolved.is_empty() {
            None
        } else {
            Some(resolved)
        };
    }
}

/// If `value` is exactly a `${VAR}` placeholder, return its resolved value
/// (empty string when unset); otherwise `None` (leave the literal untouched).
fn resolve_placeholder(value: &str, env: &HashMap<String, String>) -> Option<String> {
    let inner = value.strip_prefix("${")?.strip_suffix('}')?;
    Some(env.get(inner).cloned().unwrap_or_default())
}

/// Overwrite a required string field from an env override key when present.
fn override_string(target: &mut String, env: &HashMap<String, String>, key: &str) {
    if let Some(value) = env.get(key) {
        target.clone_from(value);
    }
}

/// Overwrite an optional string field from an env override key when present
/// (a blank override clears the field to `None`).
fn override_opt(target: &mut Option<String>, env: &HashMap<String, String>, key: &str) {
    if let Some(value) = env.get(key) {
        *target = if value.trim().is_empty() {
            None
        } else {
            Some(value.clone())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_match_spec() {
        let config = MediaConfig::default();
        assert_eq!(config.mediamtx.api_base, "http://127.0.0.1:9997");
        assert_eq!(config.mediamtx.rtmp_base, "rtmp://127.0.0.1:1935/live");
        assert_eq!(config.mediamtx.hls_base, "http://127.0.0.1:8888");
        assert_eq!(config.mediamtx.webrtc_base, "http://127.0.0.1:8889");
        assert_eq!(config.mediamtx.playback_base, "http://127.0.0.1:9996");
        assert_eq!(config.ffmpeg.bin, "/usr/bin/ffmpeg");
        assert_eq!(config.storage.backend, MediaStorageBackend::Local);
        assert_eq!(config.storage.root, "media");
        assert_eq!(config.storage.key_prefix, "media");
        assert!(!config.storage.force_path_style);
        assert_eq!(config.recording.retention_days, 14);
        assert_eq!(config.room_max_participants, DEFAULT_ROOM_MAX_PARTICIPANTS);
    }

    #[test]
    fn probe_bases_fall_back_to_browser_facing() {
        let mut config = MediaConfig::default();
        assert_eq!(config.mediamtx.hls_probe_base(), "http://127.0.0.1:8888");
        assert_eq!(
            config.mediamtx.playback_probe_base(),
            "http://127.0.0.1:9996"
        );

        config.mediamtx.hls_probe_base = Some("http://mediamtx:8888".to_owned());
        config.mediamtx.playback_probe_base = Some("http://mediamtx:9996".to_owned());
        assert_eq!(config.mediamtx.hls_probe_base(), "http://mediamtx:8888");
        assert_eq!(
            config.mediamtx.playback_probe_base(),
            "http://mediamtx:9996"
        );
    }

    #[test]
    fn validate_default_local_is_ok() {
        assert!(MediaConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_s3_without_bucket() {
        let mut config = MediaConfig::default();
        config.storage.backend = MediaStorageBackend::S3;
        assert!(matches!(
            config.validate(),
            Err(MediaConfigError::MissingBucket)
        ));
    }

    #[test]
    fn validate_rejects_partial_credentials() {
        let mut config = MediaConfig::default();
        config.storage.backend = MediaStorageBackend::S3;
        config.storage.bucket = Some("b".to_owned());
        config.storage.access_key_id = Some("key".to_owned());
        assert!(matches!(
            config.validate(),
            Err(MediaConfigError::MissingS3Credential)
        ));
    }

    #[test]
    fn validate_accepts_s3_with_bucket_and_no_creds() {
        let mut config = MediaConfig::default();
        config.storage.backend = MediaStorageBackend::S3;
        config.storage.bucket = Some("b".to_owned());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_s3_with_full_creds() {
        let mut config = MediaConfig::default();
        config.storage.backend = MediaStorageBackend::S3;
        config.storage.bucket = Some("b".to_owned());
        config.storage.access_key_id = Some("key".to_owned());
        config.storage.secret_access_key = Some("secret".to_owned());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn from_toml_parses_media_table() {
        let toml_str = r#"
            [media]
            room_max_participants = 4

            [media.mediamtx]
            api_base = "http://mtx:9997"
            hls_probe_base = "http://mediamtx:8888"

            [media.storage]
            backend = "s3"
            bucket = "my-bucket"
            key_prefix = "vids"

            [media.recording]
            retention_days = 30
        "#;
        let config = MediaConfig::from_toml_str_with_env(toml_str, &HashMap::new()).unwrap();
        assert_eq!(config.room_max_participants, 4);
        assert_eq!(config.mediamtx.api_base, "http://mtx:9997");
        assert_eq!(config.mediamtx.hls_probe_base(), "http://mediamtx:8888");
        assert_eq!(config.storage.backend, MediaStorageBackend::S3);
        assert_eq!(config.storage.bucket.as_deref(), Some("my-bucket"));
        assert_eq!(config.storage.key_prefix, "vids");
        assert_eq!(config.recording.retention_days, 30);
    }

    #[test]
    fn from_toml_missing_media_table_is_default() {
        let config = MediaConfig::from_toml_str_with_env("", &HashMap::new()).unwrap();
        assert_eq!(config.mediamtx.api_base, "http://127.0.0.1:9997");
        assert_eq!(config.storage.backend, MediaStorageBackend::Local);
    }

    #[test]
    fn env_override_applies_scalar_leaves() {
        let overrides = env(&[
            ("AUTUMN_MEDIA__MEDIAMTX__API_BASE", "http://override:9997"),
            ("AUTUMN_MEDIA__STORAGE__BACKEND", "s3"),
            ("AUTUMN_MEDIA__STORAGE__BUCKET", "env-bucket"),
            ("AUTUMN_MEDIA__RECORDING__RETENTION_DAYS", "7"),
            ("AUTUMN_MEDIA__ROOM_MAX_PARTICIPANTS", "3"),
        ]);
        let config = MediaConfig::from_toml_str_with_env("[media]\n", &overrides).unwrap();
        assert_eq!(config.mediamtx.api_base, "http://override:9997");
        assert_eq!(config.storage.backend, MediaStorageBackend::S3);
        assert_eq!(config.storage.bucket.as_deref(), Some("env-bucket"));
        assert_eq!(config.recording.retention_days, 7);
        assert_eq!(config.room_max_participants, 3);
    }

    #[test]
    fn dollar_var_interpolation_resolves_and_clears() {
        let toml_str = r#"
            [media.storage]
            backend = "s3"
            bucket = "${MEDIA_BUCKET}"
            secret_access_key = "${MEDIA_S3_SECRET}"
        "#;
        let vars = env(&[("MEDIA_BUCKET", "resolved-bucket")]);
        let config = MediaConfig::from_toml_str_with_env(toml_str, &vars).unwrap();
        assert_eq!(config.storage.bucket.as_deref(), Some("resolved-bucket"));
        // Unset placeholder resolves to None.
        assert_eq!(config.storage.secret_access_key, None);
    }

    #[test]
    fn from_autumn_toml_missing_file_is_default() {
        let config =
            MediaConfig::from_autumn_toml("/nonexistent/definitely/not/here/autumn.toml").unwrap();
        assert_eq!(config.mediamtx.api_base, "http://127.0.0.1:9997");
    }

    #[test]
    fn arroyo_env_maps_mediamtx_and_ffmpeg() {
        let vars = env(&[
            ("ARROYO_MEDIAMTX_API_BASE", "http://arroyo:9997"),
            ("ARROYO_RTMP_BASE_URL", "rtmp://arroyo:1935/live"),
            ("ARROYO_HLS_BASE_URL", "http://arroyo:8888"),
            ("ARROYO_HLS_PROBE_BASE_URL", "http://mediamtx:8888"),
            ("ARROYO_WEBRTC_BASE_URL", "http://arroyo:8889"),
            ("ARROYO_MEDIAMTX_PLAYBACK_BASE", "http://arroyo:9996"),
            (
                "ARROYO_MEDIAMTX_PLAYBACK_PROBE_BASE",
                "http://mediamtx:9996",
            ),
            ("ARROYO_FFMPEG_BIN", "/opt/ffmpeg"),
            ("ARROYO_RECORDING_RETENTION_DAYS", "21"),
        ]);
        let config = MediaConfig::from_arroyo_env_pairs(&vars);
        assert_eq!(config.mediamtx.api_base, "http://arroyo:9997");
        assert_eq!(config.mediamtx.rtmp_base, "rtmp://arroyo:1935/live");
        assert_eq!(config.mediamtx.hls_base, "http://arroyo:8888");
        assert_eq!(config.mediamtx.hls_probe_base(), "http://mediamtx:8888");
        assert_eq!(config.mediamtx.webrtc_base, "http://arroyo:8889");
        assert_eq!(config.mediamtx.playback_base, "http://arroyo:9996");
        assert_eq!(
            config.mediamtx.playback_probe_base(),
            "http://mediamtx:9996"
        );
        assert_eq!(config.ffmpeg.bin, "/opt/ffmpeg");
        assert_eq!(config.recording.retention_days, 21);
    }

    #[test]
    fn arroyo_env_maps_s3_storage_via_aws_vars() {
        let vars = env(&[
            ("ARROYO_VIDEO_STORAGE_BACKEND", "s3"),
            ("BUCKET_NAME", "arroyo-bucket"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_REGION", "auto"),
            ("AWS_ENDPOINT_URL_S3", "https://t3.storage.dev"),
            (
                "ARROYO_PUBLIC_HIGHLIGHTS_BASE",
                "https://cdn.example.com/media",
            ),
        ]);
        let config = MediaConfig::from_arroyo_env_pairs(&vars);
        assert_eq!(config.storage.backend, MediaStorageBackend::S3);
        assert_eq!(config.storage.bucket.as_deref(), Some("arroyo-bucket"));
        assert_eq!(config.storage.access_key_id.as_deref(), Some("AKIA"));
        assert_eq!(config.storage.secret_access_key.as_deref(), Some("secret"));
        assert_eq!(config.storage.region.as_deref(), Some("auto"));
        assert_eq!(
            config.storage.endpoint_url.as_deref(),
            Some("https://t3.storage.dev")
        );
        assert_eq!(
            config.storage.public_base_url.as_deref(),
            Some("https://cdn.example.com/media")
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn arroyo_env_infers_s3_from_full_creds() {
        let vars = env(&[
            ("BUCKET_NAME", "b"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]);
        let config = MediaConfig::from_arroyo_env_pairs(&vars);
        assert_eq!(config.storage.backend, MediaStorageBackend::S3);
    }

    #[test]
    fn arroyo_env_empty_is_local_default() {
        let config = MediaConfig::from_arroyo_env_pairs(&HashMap::new());
        assert_eq!(config.storage.backend, MediaStorageBackend::Local);
        assert_eq!(config.mediamtx.api_base, "http://127.0.0.1:9997");
        assert_eq!(config.ffmpeg.bin, "/usr/bin/ffmpeg");
    }
}
