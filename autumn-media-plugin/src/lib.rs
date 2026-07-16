//! # autumn-media-plugin
//!
//! Live-streaming media plugin for `autumn-web` applications. It packages the
//! two primitives an interactive streaming product needs:
//!
//! - **Broadcast** — one creator ingests (RTMP / WHIP / browser WebRTC) and a
//!   fan-out audience watches over low-latency WebRTC/WHEP with HLS fallback,
//!   backed by `MediaMTX`; recordings become VODs and clip/highlight encodes.
//! - **Room** — a small (mesh, no SFU) multi-participant call capped at
//!   [`config::DEFAULT_ROOM_MAX_PARTICIPANTS`] participants.
//!
//! # Status: skeleton (slice 0)
//!
//! This slice ships the crate skeleton, the [`MediaPlugin`] builder, the
//! [`MediaConfig`](config::MediaConfig) surface + `[media]` parsing, and the
//! [`from_arroyo_env`](config::MediaConfig::from_arroyo_env) compatibility shim.
//! It has **no runtime behavior beyond configuration and registration** —
//! storage, encode, transport, and rooms land in later slices.
//!
//! Because Autumn resolves config *after* [`Plugin::build`] runs, the plugin
//! cannot read `[media]` from inside `build`; the application loads a
//! [`MediaConfig`](config::MediaConfig) up front and passes it in via
//! [`MediaPlugin::config`] (mirroring the `autumn-storage-s3`
//! `from_config(&cfg) -> …with_blob_store(store)` pattern).
//!
//! ```rust,ignore
//! use autumn_media_plugin::{prelude::*, MediaPlugin};
//!
//! let media = MediaConfig::from_autumn_toml("autumn.toml")?;
//! autumn_web::app()
//!     .plugin(MediaPlugin::new().config(media).with_broadcast())
//!     .run()
//!     .await;
//! ```
//!
//! # Naming convention
//!
//! First-party plugin: `autumn-<name>-plugin`.

pub mod config;
pub mod encode;
pub mod error;
pub mod retention;
pub mod sink;
pub mod storage;
pub mod transport;
pub mod workflows;

pub use config::{
    MediaConfig, MediaConfigError, MediaMtxConfig, MediaStorageBackend, MediaStorageConfig,
    RecordingConfig,
};
pub use encode::{
    FfmpegClipTailCommand, FfmpegHighlightCommand, FfmpegLiveThumbnailCommand, FfmpegPosterCommand,
    FfmpegPreviewSpriteCommand, PREVIEW_CELL_HEIGHT, PREVIEW_CELL_WIDTH,
    PREVIEW_FRAME_INTERVAL_SECONDS, PREVIEW_SPRITE_COLUMNS, build_preview_webvtt,
    newest_recording_file, newest_recording_files, newest_recording_files_since,
    recording_segments_covering_window, slugify,
};
pub use error::MediaError;
pub use retention::{
    RetentionDefer, RetentionReport, is_expired, recording_expires_at, spawn_retention_sweep_loop,
    sweep_recordings_root, within_root,
};
pub use sink::{
    MediaArtifact, MediaArtifactFile, MediaArtifactKind, MediaArtifactSink, MediaArtifactSinkExt,
    MediaSinkFuture,
};
pub use storage::{MediaStorage, S3MediaStorage, StoredObject};
pub use transport::{
    IngestStatus, MediaMtxClient, MediaUrls, StreamQualityStats, StreamStatus, ViewerCount,
    duration_seconds_param, ingest_statuses_from_paths_json, quality_stats_from_path_json,
    recording_available, recording_mediamtx_path, stream_status_from_path_json,
    viewer_count_from_path_json, viewer_counts_from_paths_json,
};
pub use workflows::{
    FinalizeRecordingJobArgs, MediaWorkflowDelegate, MediaWorkflowDelegateExt,
    MediaWorkflowRequest, MediaWorkflows, PreviewJobArgs, ThumbnailJobArgs, TranscodeJobArgs,
    media_job_infos,
};

/// Common downstream imports for configuring and mounting the media plugin.
pub mod prelude {
    pub use crate::{
        FfmpegClipTailCommand, FfmpegHighlightCommand, FfmpegLiveThumbnailCommand,
        FfmpegPosterCommand, FfmpegPreviewSpriteCommand, FinalizeRecordingJobArgs, MediaArtifact,
        MediaArtifactFile, MediaArtifactKind, MediaArtifactSink, MediaArtifactSinkExt, MediaConfig,
        MediaConfigError, MediaError, MediaMtxConfig, MediaPlugin, MediaStorage,
        MediaStorageBackend, MediaStorageConfig, MediaWorkflowDelegate, MediaWorkflowDelegateExt,
        MediaWorkflowRequest, MediaWorkflows, PREVIEW_CELL_HEIGHT, PREVIEW_CELL_WIDTH,
        PREVIEW_FRAME_INTERVAL_SECONDS, PREVIEW_SPRITE_COLUMNS, PreviewJobArgs, RecordingConfig,
        RetentionDefer, S3MediaStorage, StoredObject, ThumbnailJobArgs, TranscodeJobArgs,
        build_preview_webvtt, media_job_infos, newest_recording_file, newest_recording_files,
        newest_recording_files_since, recording_segments_covering_window, slugify,
    };
    pub use crate::{
        IngestStatus, MediaMtxClient, MediaUrls, StreamQualityStats, StreamStatus, ViewerCount,
        duration_seconds_param, ingest_statuses_from_paths_json, quality_stats_from_path_json,
        recording_available, recording_mediamtx_path, stream_status_from_path_json,
        viewer_count_from_path_json, viewer_counts_from_paths_json,
    };
}

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

use crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS;
use crate::retention::RetentionDefer as RetentionDeferHook;
use crate::sink::MediaArtifactSink as MediaArtifactSinkTrait;
use crate::workflows::MediaWorkflowDelegate as MediaWorkflowDelegateHook;

/// The live-streaming media plugin.
///
/// Configure the two primitives with [`with_broadcast`](Self::with_broadcast)
/// and [`with_rooms`](Self::with_rooms), supply a
/// [`MediaConfig`](config::MediaConfig) with [`config`](Self::config), then
/// install with `app.plugin(...)`.
///
/// This is the slice-0 skeleton: [`build`](Plugin::build) currently declares no
/// routes and installs no extensions.
pub struct MediaPlugin {
    /// Resolved `[media]` configuration.
    config: MediaConfig,
    /// Whether the broadcast primitive is enabled.
    enable_broadcast: bool,
    /// Whether the rooms primitive is enabled.
    enable_rooms: bool,
    /// Hard cap on mesh-room participants.
    room_max_participants: usize,
    /// Job queue name the built-in media encode jobs are registered on.
    queue: String,
    /// URL prefix for the plugin's API routes (later slices).
    api_prefix: String,
    /// App-supplied artifact completion callback (parallels the
    /// `OutboundWebhookHandler` store).
    artifact_sink: Option<Arc<dyn MediaArtifactSinkTrait>>,
    /// Optional runtime delegate that overrides the built-in `#[job]` engine
    /// (e.g. an external Harvest adapter).
    workflow_delegate: Option<MediaWorkflowDelegateHook>,
    /// Retention window override (days). `None` uses `config.recording.retention_days`.
    retention_days: Option<u32>,
    /// Filesystem root the retention sweep operates on. `None` disables the
    /// sweep (there is nothing to sweep without a recordings root).
    recordings_root: Option<PathBuf>,
    /// App-overridable retention-defer predicate.
    retention_defer: Option<RetentionDeferHook>,
}

impl MediaPlugin {
    /// Create a media plugin with default configuration and both primitives
    /// disabled (opt in with [`with_broadcast`](Self::with_broadcast) /
    /// [`with_rooms`](Self::with_rooms)).
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: MediaConfig::default(),
            enable_broadcast: false,
            enable_rooms: false,
            room_max_participants: DEFAULT_ROOM_MAX_PARTICIPANTS,
            queue: "media".to_owned(),
            api_prefix: "/api/media".to_owned(),
            artifact_sink: None,
            workflow_delegate: None,
            retention_days: None,
            recordings_root: None,
            retention_defer: None,
        }
    }

    /// Build a fully-wired broadcast [`MediaPlugin`] from an Arroyo operator's
    /// existing `ARROYO_*` environment — the ratified migration shim (issue
    /// #1974, slice 5). One call maps the operator's env onto a plugin ready to
    /// install, so adopting `autumn-media-plugin` changes no ops config:
    ///
    /// - the `[media]` config surface — `MediaMTX` origins, `FFmpeg` bin, the
    ///   local/S3 storage selection (including Arroyo's Tigris `auto` region /
    ///   `t3.storage.dev` endpoint / `highlights` key-prefix defaults), and the
    ///   retention window — via
    ///   [`MediaConfig::from_arroyo_env`](config::MediaConfig::from_arroyo_env);
    /// - the **broadcast** primitive enabled (Arroyo is one-to-many and uses no
    ///   rooms), so the plugin is watch-path-ready without a further builder
    ///   call;
    /// - the retention sweep's recordings root from `ARROYO_RECORDINGS_ROOT`
    ///   (default `recordings`, matching Arroyo's own
    ///   `configured_recordings_root()` fallback), so the hourly retention loop
    ///   is wired exactly as before.
    ///
    /// The returned value is a normal builder: chain
    /// [`artifact_sink`](Self::artifact_sink) (the app writes its own tables on
    /// completion), [`workflow_delegate`](Self::workflow_delegate),
    /// [`retention_defer`](Self::retention_defer), etc. This reads the process
    /// environment; the pure mapping lives in
    /// [`from_arroyo_env_pairs`](Self::from_arroyo_env_pairs) for testability.
    #[must_use]
    pub fn from_arroyo_env() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_arroyo_env_pairs(&env)
    }

    /// Pure core of [`from_arroyo_env`](Self::from_arroyo_env): map the supplied
    /// `ARROYO_*` environment map onto a wired [`MediaPlugin`] without touching
    /// process-global environment (mirroring
    /// [`MediaConfig::from_arroyo_env_pairs`](config::MediaConfig::from_arroyo_env_pairs)).
    #[must_use]
    pub fn from_arroyo_env_pairs(env: &HashMap<String, String>) -> Self {
        let config = MediaConfig::from_arroyo_env_pairs(env);
        Self::new()
            .config(config)
            .with_broadcast()
            .recordings_root(arroyo_recordings_root(env))
    }

    /// Supply the resolved `[media]` configuration.
    ///
    /// The plugin cannot read `[media]` itself because Autumn loads config
    /// after [`Plugin::build`] runs; resolve it up front with
    /// [`MediaConfig::from_autumn_toml`](config::MediaConfig::from_autumn_toml)
    /// or [`MediaConfig::from_arroyo_env`](config::MediaConfig::from_arroyo_env)
    /// and pass it here. Adopts the config's `room_max_participants`.
    #[must_use]
    pub fn config(mut self, config: MediaConfig) -> Self {
        self.room_max_participants = config.room_max_participants;
        self.config = config;
        self
    }

    /// Enable the broadcast primitive (ingest → fan-out playback → VOD).
    #[must_use]
    pub const fn with_broadcast(mut self) -> Self {
        self.enable_broadcast = true;
        self
    }

    /// Enable the rooms primitive (small mesh calls).
    #[must_use]
    pub const fn with_rooms(mut self) -> Self {
        self.enable_rooms = true;
        self
    }

    /// Override the hard cap on mesh-room participants.
    #[must_use]
    pub const fn room_max_participants(mut self, max: usize) -> Self {
        self.room_max_participants = max;
        self
    }

    /// Override the job/Harvest queue name used for media encode work.
    #[must_use]
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = queue.into();
        self
    }

    /// Override the URL prefix for the plugin's API routes.
    #[must_use]
    pub fn api(mut self, prefix: impl Into<String>) -> Self {
        self.api_prefix = prefix.into();
        self
    }

    /// Override the `FFmpeg` binary path (shortcut for
    /// `config.ffmpeg.bin`).
    #[must_use]
    pub fn ffmpeg_bin(mut self, bin: impl Into<String>) -> Self {
        self.config.ffmpeg.bin = bin.into();
        self
    }

    /// Install the app-supplied [`MediaArtifactSink`](sink::MediaArtifactSink)
    /// the built-in workflow jobs invoke on completion.
    ///
    /// Without a sink, a completed workflow persists its output but logs that
    /// no sink recorded it (parallels leaving `OutboundWebhookHandler` unset).
    #[must_use]
    pub fn artifact_sink(mut self, sink: Arc<dyn MediaArtifactSinkTrait>) -> Self {
        self.artifact_sink = Some(sink);
        self
    }

    /// Install a runtime workflow delegate that overrides the built-in `#[job]`
    /// engine (e.g. an external Harvest adapter maintained outside this
    /// workspace). When set, every [`MediaWorkflows`] `queue_*` call routes
    /// through the delegate instead of enqueuing the built-in job.
    #[must_use]
    pub fn workflow_delegate(mut self, delegate: MediaWorkflowDelegateHook) -> Self {
        self.workflow_delegate = Some(delegate);
        self
    }

    /// Override the recording-retention window, in days (`0` disables the
    /// sweep). Defaults to `config.recording.retention_days`.
    #[must_use]
    pub const fn retention_days(mut self, days: u32) -> Self {
        self.retention_days = Some(days);
        self
    }

    /// Set the filesystem root the retention sweep deletes expired recordings
    /// from. Required to spawn the sweep — without it, no retention loop runs.
    #[must_use]
    pub fn recordings_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.recordings_root = Some(root.into());
        self
    }

    /// Install the app-overridable retention-defer predicate: return `true` for
    /// a path to hold it back this sweep (e.g. a still-encoding workflow
    /// references it).
    #[must_use]
    pub fn retention_defer(mut self, defer: RetentionDeferHook) -> Self {
        self.retention_defer = Some(defer);
        self
    }
}

impl Default for MediaPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MediaPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-media-plugin")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let Self {
            config,
            enable_broadcast,
            enable_rooms,
            room_max_participants,
            queue,
            api_prefix,
            artifact_sink,
            workflow_delegate,
            retention_days,
            recordings_root,
            retention_defer,
        } = self;

        let retention_days = retention_days.unwrap_or(config.recording.retention_days);

        tracing::info!(
            broadcast = %enable_broadcast,
            rooms = %enable_rooms,
            room_max_participants,
            storage_backend = config.storage.backend.as_str(),
            queue = %queue,
            api_prefix = %api_prefix,
            has_artifact_sink = artifact_sink.is_some(),
            has_workflow_delegate = workflow_delegate.is_some(),
            retention_days,
            "🍂 Autumn Media configured"
        );

        // Resolve the storage backend up front so a misconfiguration surfaces
        // as one error line here rather than inside a job. On failure we still
        // register the (empty) route surface, but install no encode wiring.
        let storage = match storage::MediaStorage::from_config(&config.storage) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::error!(
                    %error,
                    "🍂 Autumn Media: storage config invalid; encode workflows disabled"
                );
                return app.declare_plugin_routes(media_route_infos(&api_prefix));
            }
        };

        let ffmpeg_bin = config.ffmpeg.bin;
        let workflows = workflows::MediaWorkflows::new(ffmpeg_bin, queue.clone());

        // slice 3 (transport/rooms): the `transport` module (MediaMtxClient +
        //   MediaUrls + status parsers) now exists and is re-exported; its
        //   AppState extension wiring lands with the consumer slice — insert the
        //   MediaMtxClient / MediaUrls extensions and
        //   nest the broadcast + room routers under `api_prefix`.
        //
        // Extensions are installed via `state_initializer` (not `on_startup`)
        // so they exist BEFORE job workers start — mirroring autumn-web's
        // outbound-webhook plugin, whose manager must be present before the
        // first job runs.
        let app = app
            .declare_plugin_routes(media_route_infos(&api_prefix))
            .state_initializer(move |state| {
                state.insert_extension(storage.clone());
                state.insert_extension(workflows.clone());
                if let Some(sink) = &artifact_sink {
                    state.insert_extension(sink::MediaArtifactSinkExt(sink.clone()));
                }
                if let Some(delegate) = &workflow_delegate {
                    state.insert_extension(workflows::MediaWorkflowDelegateExt(delegate.clone()));
                }
            })
            // Register the built-in encode jobs with the queue overridden to
            // `queue` (the JobInfo's `queue` field, not the `#[job]` literal, is
            // what the enqueue chokepoint routes on). autumn-web's worker
            // auto-registers any declared-but-unconfigured queue at lowest
            // priority, so the media queue drains without extra config.
            .jobs(workflows::media_job_infos(&queue));

        // Recording-retention sweep: spawn only when a recordings root is
        // configured and retention is enabled. Spawned from `on_startup` so it
        // shares the running app's runtime, matching how the thumbnail/retention
        // loops are spawned in an Autumn app.
        if let Some(root) = recordings_root {
            app.on_startup(move |_state| {
                let root = root.clone();
                let defer = retention_defer.clone();
                async move {
                    retention::spawn_retention_sweep_loop(root, retention_days, defer);
                    Ok(())
                }
            })
        } else {
            if retention_days > 0 {
                tracing::debug!(
                    "🍂 Autumn Media: retention window set but no recordings_root; sweep not spawned"
                );
            }
            app
        }
    }
}

/// Recordings root an Arroyo deployment uses when `ARROYO_RECORDINGS_ROOT` is
/// unset — mirrors Arroyo's own `configured_recordings_root()` fallback, so the
/// migration shim wires the retention sweep the same way with no ops change.
const DEFAULT_ARROYO_RECORDINGS_ROOT: &str = "recordings";

/// Resolve the Arroyo recordings root from an env map: a non-blank
/// `ARROYO_RECORDINGS_ROOT`, else [`DEFAULT_ARROYO_RECORDINGS_ROOT`]. Blank /
/// whitespace-only values are treated as unset (parity with
/// [`MediaConfig::from_arroyo_env_pairs`](config::MediaConfig::from_arroyo_env_pairs)).
fn arroyo_recordings_root(env: &HashMap<String, String>) -> PathBuf {
    env.get("ARROYO_RECORDINGS_ROOT")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map_or_else(
            || PathBuf::from(DEFAULT_ARROYO_RECORDINGS_ROOT),
            PathBuf::from,
        )
}

/// The route metadata `MediaPlugin` declares for `autumn routes` listing.
///
/// **Slice 0: empty.** Later slices will fill this in with the broadcast and
/// room routers' routes under `api_prefix`, kept in sync with what `build`
/// nests. Extracted so the empty slice-0 declaration is directly testable.
const fn media_route_infos(_api_prefix: &str) -> Vec<autumn_web::route_listing::RouteInfo> {
    Vec::new()
}

// ── Arroyo migration shim (slice 5) ─────────────────────────────────────────

#[cfg(test)]
mod arroyo_shim_tests {
    use super::{DEFAULT_ARROYO_RECORDINGS_ROOT, MediaPlugin, arroyo_recordings_root};
    use crate::config::MediaStorageBackend;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn empty_env_is_wired_broadcast_local_plugin() {
        let plugin = MediaPlugin::from_arroyo_env_pairs(&HashMap::new());
        // Broadcast is enabled (Arroyo is one-to-many); rooms are not.
        assert!(plugin.enable_broadcast, "broadcast must be enabled");
        assert!(!plugin.enable_rooms, "rooms must stay disabled");
        // Config maps through with neutral defaults for an empty env.
        assert_eq!(plugin.config.storage.backend, MediaStorageBackend::Local);
        assert_eq!(plugin.config.mediamtx.api_base, "http://127.0.0.1:9997");
        assert_eq!(plugin.config.ffmpeg.bin, "/usr/bin/ffmpeg");
        // Retention sweep is wired to Arroyo's default recordings root, so the
        // hourly loop runs exactly as it did pre-migration.
        assert_eq!(
            plugin.recordings_root.as_deref(),
            Some(std::path::Path::new(DEFAULT_ARROYO_RECORDINGS_ROOT))
        );
        // The produced config is valid to hand to MediaStorage::from_config.
        assert!(plugin.config.validate().is_ok());
    }

    #[test]
    fn maps_full_s3_environment_and_validates() {
        let vars = env(&[
            ("ARROYO_VIDEO_STORAGE_BACKEND", "s3"),
            ("BUCKET_NAME", "arroyo-bucket"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("ARROYO_MEDIAMTX_API_BASE", "http://mtx:9997"),
            ("ARROYO_FFMPEG_BIN", "/opt/ffmpeg"),
            ("ARROYO_RECORDING_RETENTION_DAYS", "21"),
            ("ARROYO_RECORDINGS_ROOT", "/data/recordings"),
        ]);
        let plugin = MediaPlugin::from_arroyo_env_pairs(&vars);
        assert!(plugin.enable_broadcast);
        assert_eq!(plugin.config.storage.backend, MediaStorageBackend::S3);
        assert_eq!(
            plugin.config.storage.bucket.as_deref(),
            Some("arroyo-bucket")
        );
        // Arroyo's Tigris S3 defaults flow through the config-level shim.
        assert_eq!(plugin.config.storage.region.as_deref(), Some("auto"));
        assert_eq!(
            plugin.config.storage.endpoint_url.as_deref(),
            Some("https://t3.storage.dev")
        );
        assert_eq!(plugin.config.storage.key_prefix, "highlights");
        assert_eq!(plugin.config.mediamtx.api_base, "http://mtx:9997");
        assert_eq!(plugin.config.ffmpeg.bin, "/opt/ffmpeg");
        // Retention window flows from ARROYO_RECORDING_RETENTION_DAYS into the
        // config (the plugin resolves the effective days from it at build time).
        assert_eq!(plugin.config.recording.retention_days, 21);
        assert_eq!(
            plugin.recordings_root.as_deref(),
            Some(std::path::Path::new("/data/recordings"))
        );
        assert!(plugin.config.validate().is_ok());
    }

    #[test]
    fn recordings_root_honors_env_and_defaults() {
        assert_eq!(
            arroyo_recordings_root(&env(&[("ARROYO_RECORDINGS_ROOT", "/mnt/rec")])),
            PathBuf::from("/mnt/rec")
        );
        // Blank / whitespace-only is treated as unset → Arroyo's default.
        assert_eq!(
            arroyo_recordings_root(&env(&[("ARROYO_RECORDINGS_ROOT", "   ")])),
            PathBuf::from(DEFAULT_ARROYO_RECORDINGS_ROOT)
        );
        assert_eq!(
            arroyo_recordings_root(&HashMap::new()),
            PathBuf::from(DEFAULT_ARROYO_RECORDINGS_ROOT)
        );
    }

    #[test]
    fn returned_plugin_stays_chainable() {
        // The shim returns a normal builder, so an app can layer its own
        // overrides (queue name, retention window) on top of the mapped env.
        let plugin = MediaPlugin::from_arroyo_env_pairs(&HashMap::new())
            .queue("arroyo-media")
            .retention_days(30);
        assert!(plugin.enable_broadcast);
        assert_eq!(plugin.queue, "arroyo-media");
        assert_eq!(plugin.retention_days, Some(30));
    }
}

// ── Conformance reference tests ─────────────────────────────────────────────
//
// Slice 0's `build()` declares **zero** routes (asserted directly). The
// conformance harness, however, treats a plugin that declares no routes as a
// FAIL — it expects every plugin to eventually declare at least one route. So,
// mirroring `autumn-admin-plugin`'s `conformance_tests`, the harness checks run
// against a small **representative** future route set attributed to the plugin
// under `/api/media`: this proves the plugin's naming/prefix conventions are
// conformance-clean the moment routes land in a later slice, so a later slice
// that adds routes must consciously keep them conformant.

#[cfg(test)]
mod conformance_tests {
    use autumn_web::plugin_conformance::{
        CheckStatus, ConformanceConfig, check_collisions, check_duplicate_registration,
        check_route_attribution, check_route_prefix, check_sensitive_surfaces, run_conformance,
    };
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    const PLUGIN_NAME: &str = "autumn-media-plugin";

    /// Slice-0 `MediaPlugin` declares **no** routes.
    #[test]
    fn media_plugin_declares_no_routes_in_slice_0() {
        assert!(
            super::media_route_infos("/api/media").is_empty(),
            "slice 0 must declare no routes"
        );
    }

    /// A representative route set attributed to the plugin, all under the
    /// plugin's `/api/media` prefix. Stands in for the routes later slices will
    /// declare, so the conformance conventions are pinned now.
    fn representative_routes() -> Vec<RouteInfo> {
        ["/api/media/broadcasts", "/api/media/rooms"]
            .into_iter()
            .map(|path| RouteInfo {
                method: "GET".to_owned(),
                path: path.to_owned(),
                handler: format!("media::{}", path.rsplit('/').next().unwrap_or("handler")),
                source: RouteSource::Plugin(PLUGIN_NAME.to_owned()),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn representative_routes_are_attributed_to_plugin_name() {
        let result = check_route_attribution(PLUGIN_NAME, &representative_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "attribution failed: {}",
            result.message
        );
    }

    #[test]
    fn representative_routes_live_under_api_prefix() {
        let result = check_route_prefix(PLUGIN_NAME, "/api/media", &[], &representative_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "prefix check failed: {}",
            result.message
        );
    }

    #[test]
    fn representative_routes_have_no_collisions_in_isolation() {
        let (result, _) = check_collisions(&representative_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "unexpected collision: {}",
            result.message
        );
    }

    #[test]
    fn representative_routes_have_no_undeclared_sensitive_surfaces() {
        let result = check_sensitive_surfaces(PLUGIN_NAME, &representative_routes(), &[]);
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "sensitive-surfaces check failed: {}",
            result.message
        );
    }

    #[test]
    fn representative_single_registration_passes_duplicate_check() {
        let result = check_duplicate_registration(PLUGIN_NAME, &representative_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "single registration should pass: {}",
            result.message
        );
    }

    #[test]
    fn representative_routes_pass_full_conformance() {
        let config = ConformanceConfig::new(PLUGIN_NAME).prefix("/api/media");
        let report = run_conformance(&config, &representative_routes());
        assert!(
            report.passed(),
            "MediaPlugin conformance failed:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn duplicate_registration_detected() {
        // Installing the plugin twice would double its routes → FAIL.
        let mut routes = representative_routes();
        routes.extend(representative_routes());
        let result = check_duplicate_registration(PLUGIN_NAME, &routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "expected duplicate-registration FAIL when installed twice"
        );
    }

    #[test]
    fn collision_with_host_route_detected() {
        // Sanity check the harness is wired: a host route colliding with a
        // plugin route is flagged.
        let mut routes = representative_routes();
        routes.push(RouteInfo {
            method: "GET".to_owned(),
            path: "/api/media/broadcasts".to_owned(),
            handler: "host::list".to_owned(),
            source: RouteSource::User,
            ..Default::default()
        });
        let (result, _) = check_collisions(&routes);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "expected collision to be detected"
        );
    }
}
