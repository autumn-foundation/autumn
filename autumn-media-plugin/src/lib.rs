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
pub mod rooms;
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
    FfmpegPreviewSpriteCommand, FfmpegRoomCompositeCommand, PREVIEW_CELL_HEIGHT,
    PREVIEW_CELL_WIDTH, PREVIEW_FRAME_INTERVAL_SECONDS, PREVIEW_SPRITE_COLUMNS,
    ROOM_COMPOSITE_CELL_HEIGHT, ROOM_COMPOSITE_CELL_WIDTH, build_preview_webvtt,
    newest_recording_file, newest_recording_files, newest_recording_files_since,
    recording_segments_covering_window, slugify,
};
pub use error::MediaError;
pub use retention::{
    RetentionDefer, RetentionReport, is_expired, recording_expires_at, spawn_retention_sweep_loop,
    sweep_recordings_root, within_root,
};
pub use rooms::{
    InMemoryRoomStore, JoinRecord, JoinRequest, JoinResponse, LeaveRequest, ParticipantView,
    PublishTarget, ReapStats, RoomError, RoomLeaveResponse, RoomService, RoomSnapshot, RoomStore,
    SessionToken, SubscribeTarget, room_participant_path, room_route_infos, room_router,
    spawn_room_reaper_loop, validate_room_segment,
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
    MediaWorkflowRequest, MediaWorkflows, PreviewJobArgs, RoomCompositeJobArgs, ThumbnailJobArgs,
    TranscodeJobArgs, media_job_infos,
};

/// Common downstream imports for configuring and mounting the media plugin.
pub mod prelude {
    pub use crate::{
        FfmpegClipTailCommand, FfmpegHighlightCommand, FfmpegLiveThumbnailCommand,
        FfmpegPosterCommand, FfmpegPreviewSpriteCommand, FfmpegRoomCompositeCommand,
        FinalizeRecordingJobArgs, MediaArtifact, MediaArtifactFile, MediaArtifactKind,
        MediaArtifactSink, MediaArtifactSinkExt, MediaConfig, MediaConfigError, MediaError,
        MediaMtxConfig, MediaPlugin, MediaStorage, MediaStorageBackend, MediaStorageConfig,
        MediaWorkflowDelegate, MediaWorkflowDelegateExt, MediaWorkflowRequest, MediaWorkflows,
        PREVIEW_CELL_HEIGHT, PREVIEW_CELL_WIDTH, PREVIEW_FRAME_INTERVAL_SECONDS,
        PREVIEW_SPRITE_COLUMNS, PreviewJobArgs, ROOM_COMPOSITE_CELL_HEIGHT,
        ROOM_COMPOSITE_CELL_WIDTH, RecordingConfig, RetentionDefer, RoomCompositeJobArgs,
        S3MediaStorage, StoredObject, ThumbnailJobArgs, TranscodeJobArgs, build_preview_webvtt,
        media_job_infos, newest_recording_file, newest_recording_files,
        newest_recording_files_since, recording_segments_covering_window, slugify,
    };
    pub use crate::{
        InMemoryRoomStore, JoinRecord, JoinResponse, ParticipantView, ReapStats, RoomError,
        RoomService, RoomSnapshot, RoomStore, SessionToken, room_participant_path,
        room_route_infos, room_router, spawn_room_reaper_loop, validate_room_segment,
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
/// When [`with_rooms`](Self::with_rooms) is enabled, [`build`](Plugin::build)
/// nests the [`rooms::room_router`] under the API prefix and installs a
/// [`rooms::RoomService`] extension; the broadcast surface installs the storage
/// / encode wiring.
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

    /// Override the mesh-room session-token lifetime, in seconds (shortcut for
    /// `config.room_token_ttl_seconds`).
    #[must_use]
    pub const fn room_token_ttl_seconds(mut self, seconds: u32) -> Self {
        self.config.room_token_ttl_seconds = seconds;
        self
    }

    /// Override the mesh-room `MediaMTX` path namespace (shortcut for
    /// `config.room_namespace`).
    #[must_use]
    pub fn room_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.config.room_namespace = Some(namespace.into());
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

    // `build` is a long, linear plugin-assembly routine (config validation,
    // room wiring + reaper, storage/workflow install, retention loop); it reads
    // best as one top-to-bottom sequence rather than fragmented across helpers.
    #[allow(clippy::too_many_lines)]
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

        // The mesh is O(N²), so `DEFAULT_ROOM_MAX_PARTICIPANTS` (6) is an
        // ABSOLUTE ceiling with no SFU: the `[media]` room config and the
        // `room_max_participants` builder may set a per-room seat count only
        // within `1..=6`. An out-of-range value (0 or >6) is a fatal
        // misconfiguration — fail fast and LOUD at boot, never a silent clamp.
        // `Plugin::build` returns an `AppBuilder` (not a `Result`), so the
        // specific error is surfaced from an `on_startup` hook, which aborts
        // boot with `process::exit(1)` exactly as a failed init would (see
        // autumn-web's `run_startup_hooks`). `room_max_participants` is the
        // effective seat count sourced from either `config(..)` (which copies
        // `room_max_participants` into it) or the `room_max_participants(..)`
        // builder, so this single check covers both the TOML and builder paths;
        // `InMemoryRoomStore::create_room` re-checks the fixed 6 ceiling as a
        // defense-in-depth backstop, and `MediaConfig::validate` stays the
        // opt-in strict check for consumers who validate config up front.
        //
        // A configured `room_namespace` is prepended to every room path, so an
        // invalid one (a slash, a dot segment, whitespace) would likewise mount
        // the router fine yet make every `POST /rooms` fail `InvalidSegment` —
        // another config that can never serve a request. It fails fast here too,
        // via the same `on_startup` abort but its own specific message.
        // The storage backend keeps its own degrade path below (unchanged), so
        // this only fails fast on the room cap and room namespace.
        if let Some(message) = room_max_participants_error(room_max_participants)
            .or_else(|| room_namespace_error(config.room_namespace.as_deref()))
        {
            tracing::error!(
                room_max_participants,
                ceiling = DEFAULT_ROOM_MAX_PARTICIPANTS,
                "🍂 Autumn Media: {message}"
            );
            return app.on_startup(move |_state| {
                let message = message.clone();
                async move { Err(autumn_web::AutumnError::internal_server_error_msg(message)) }
            });
        }

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

        // Rooms are storage-independent, so mount the room signaling router and
        // install the `RoomService` extension up front — they must stay
        // available even if the storage backend below fails to resolve. The
        // `nest` + `declare_plugin_routes` pair keeps the room routes both
        // served and audit-visible under `api_prefix`.
        let mut app = app;
        if enable_rooms {
            // Hold a handle to the store so the idle-room / stale-participant
            // reaper can sweep the same registry the `RoomService` serves.
            let room_store: Arc<dyn rooms::RoomStore> =
                Arc::new(rooms::InMemoryRoomStore::new(room_max_participants));
            let room_service = rooms::RoomService::new(
                room_store.clone(),
                transport::MediaUrls::from_config(&config.mediamtx),
                config.room_namespace.clone().unwrap_or_default(),
                chrono::Duration::seconds(i64::from(config.room_token_ttl_seconds)),
                room_max_participants,
            );
            app = app
                .nest(&api_prefix, rooms::room_router())
                .declare_plugin_routes(rooms::room_route_infos(&api_prefix))
                .state_initializer(move |state| {
                    state.insert_extension(room_service);
                })
                // Spawn from `on_startup` so the reaper shares the running app's
                // tokio runtime (matching the retention sweep below).
                .on_startup(move |_state| {
                    let store = room_store.clone();
                    async move {
                        rooms::spawn_room_reaper_loop(store);
                        Ok(())
                    }
                });
        }

        // Resolve the storage backend up front so a misconfiguration surfaces
        // as one error line here rather than inside a job. On failure the plugin
        // still serves any mounted room routes, but installs no encode wiring.
        let storage = match storage::MediaStorage::from_config(&config.storage) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::error!(
                    %error,
                    "🍂 Autumn Media: storage config invalid; encode workflows disabled"
                );
                return app;
            }
        };

        let ffmpeg_bin = config.ffmpeg.bin;
        let workflows = workflows::MediaWorkflows::new(ffmpeg_bin, queue.clone());

        // Extensions are installed via `state_initializer` (not `on_startup`)
        // so they exist BEFORE job workers start — mirroring autumn-web's
        // outbound-webhook plugin, whose manager must be present before the
        // first job runs.
        let app = app
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

/// Validate an effective mesh-room seat count against the ABSOLUTE ceiling.
///
/// Mesh WebRTC is O(N²), so [`DEFAULT_ROOM_MAX_PARTICIPANTS`] (6) is a hard cap
/// with no SFU, and a room must seat at least one participant. Returns the
/// specific, fail-fast error message — naming the offending value and the cap —
/// when `configured` is outside `1..=6`, or `None` when it is in range.
fn room_max_participants_error(configured: usize) -> Option<String> {
    if configured == 0 {
        Some(format!(
            "a room must allow at least 1 participant; got {configured}"
        ))
    } else if configured > DEFAULT_ROOM_MAX_PARTICIPANTS {
        Some(format!(
            "mesh rooms are capped at {DEFAULT_ROOM_MAX_PARTICIPANTS} participants (no SFU); got {configured}"
        ))
    } else {
        None
    }
}

/// Validate a configured mesh-room `MediaMTX` path namespace at boot.
///
/// The namespace is prepended to every room / participant `MediaMTX` path, so a
/// value [`rooms::validate_room_segment`] would reject (a slash, a `.` / `..`
/// dot segment, whitespace, or other non-`[A-Za-z0-9_-]` label) mounts the
/// router fine but makes every `POST /rooms` fail with `InvalidSegment` — a
/// config that can never serve a request must instead refuse to boot. Returns
/// the specific, fail-fast error message — naming the offending value and the
/// allowed charset — when the namespace is present-and-invalid, or `None` when
/// it is absent, empty, or a valid segment.
fn room_namespace_error(namespace: Option<&str>) -> Option<String> {
    let ns = namespace?;
    if ns.is_empty() || rooms::validate_room_segment(ns).is_ok() {
        return None;
    }
    Some(format!(
        "media.room_namespace {ns:?} is not a valid room path segment: use only ASCII letters, digits, '_' or '-' (no '/', '.', or empty)"
    ))
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

// ── Absolute room-cap enforcement (Fix 1) ───────────────────────────────────

#[cfg(test)]
mod room_cap_tests {
    use super::{DEFAULT_ROOM_MAX_PARTICIPANTS, MediaPlugin, room_max_participants_error};
    use crate::config::MediaConfig;

    #[test]
    fn out_of_range_room_cap_is_a_specific_fail_fast_error() {
        // >6 fails loud, naming the offending value and the ceiling — never a
        // clamp.
        let over = room_max_participants_error(50).expect("50 > 6 must be rejected");
        assert!(over.contains("50"), "names the offending value: {over}");
        assert!(over.contains('6'), "names the ceiling: {over}");
        assert!(over.contains("no SFU"), "explains why: {over}");
        // 0 fails loud with its own message.
        let zero = room_max_participants_error(0).expect("0 must be rejected");
        assert!(zero.contains("at least 1 participant"), "0 message: {zero}");
        assert!(zero.contains('0'), "names the value: {zero}");
    }

    #[test]
    fn in_range_room_cap_is_accepted() {
        assert!(room_max_participants_error(1).is_none());
        assert!(room_max_participants_error(4).is_none());
        assert!(room_max_participants_error(DEFAULT_ROOM_MAX_PARTICIPANTS).is_none());
    }

    #[test]
    fn builder_and_config_paths_feed_the_same_effective_cap_that_boot_rejects() {
        // The `room_max_participants(50)` builder stores what it was given;
        // `build()` is what rejects it fail-fast (no clamp), so an out-of-range
        // builder value produces the specific error at boot.
        let over = MediaPlugin::new().with_rooms().room_max_participants(50);
        assert_eq!(over.room_max_participants, 50);
        assert!(room_max_participants_error(over.room_max_participants).is_some());

        // `room_max_participants(4)` stays in range → a real 4-seat room.
        let ok = MediaPlugin::new().with_rooms().room_max_participants(4);
        assert_eq!(ok.room_max_participants, 4);
        assert!(room_max_participants_error(ok.room_max_participants).is_none());

        // The `[media] room_max_participants = 50` config path flows into the
        // same effective field via `config(..)`, so it is rejected identically.
        let config = MediaConfig {
            room_max_participants: 50,
            ..MediaConfig::default()
        };
        let from_config = MediaPlugin::new().config(config);
        assert_eq!(from_config.room_max_participants, 50);
        assert!(room_max_participants_error(from_config.room_max_participants).is_some());
    }
}

// ── Room-namespace fail-fast (Fix P2-a) ─────────────────────────────────────

#[cfg(test)]
mod room_namespace_tests {
    use super::{MediaPlugin, room_namespace_error};

    #[test]
    fn absent_empty_and_valid_namespaces_are_accepted() {
        // Absent and empty mean "no namespace" → no boot error; a valid segment
        // passes through untouched.
        assert!(room_namespace_error(None).is_none());
        assert!(room_namespace_error(Some("")).is_none());
        assert!(room_namespace_error(Some("tenant-a")).is_none());
        assert!(room_namespace_error(Some("room_1")).is_none());
    }

    #[test]
    fn invalid_namespace_is_a_specific_fail_fast_error() {
        // A slash, either dot segment, or embedded whitespace fails loud, naming
        // the offending value and the allowed charset — never a silent mount
        // that 500s every request.
        for bad in ["tenant/a", ".", "..", "a b"] {
            let message = room_namespace_error(Some(bad))
                .unwrap_or_else(|| panic!("{bad:?} must be rejected"));
            assert!(
                message.contains(bad),
                "names the offending value: {message}"
            );
            assert!(
                message.contains("letters") && message.contains('-'),
                "mentions the allowed charset: {message}"
            );
        }
    }

    #[test]
    fn room_namespace_builder_feeds_the_effective_value_that_boot_rejects() {
        // The `room_namespace("tenant/a")` builder stores what it was given;
        // `build()` is what rejects it fail-fast, so an invalid builder value
        // produces the specific error at boot.
        let bad = MediaPlugin::new().with_rooms().room_namespace("tenant/a");
        assert_eq!(bad.config.room_namespace.as_deref(), Some("tenant/a"));
        assert!(room_namespace_error(bad.config.room_namespace.as_deref()).is_some());

        // A valid namespace flows through untouched → a real namespaced room.
        let ok = MediaPlugin::new().with_rooms().room_namespace("tenant-a");
        assert_eq!(ok.config.room_namespace.as_deref(), Some("tenant-a"));
        assert!(room_namespace_error(ok.config.room_namespace.as_deref()).is_none());
    }
}

// ── Conformance reference tests ─────────────────────────────────────────────
//
// The room signaling routes `MediaPlugin::build` declares under `/api/media`
// (via `rooms::room_route_infos`) are run through autumn-web's plugin
// conformance harness — the same checks `autumn-admin-plugin`'s
// `conformance_tests` uses — so the plugin's naming / prefix / attribution
// conventions stay clean and a future slice that touches the room routes must
// consciously keep them conformant.

#[cfg(test)]
mod conformance_tests {
    use autumn_web::plugin_conformance::{
        CheckStatus, ConformanceConfig, check_collisions, check_duplicate_registration,
        check_route_attribution, check_route_prefix, check_sensitive_surfaces, run_conformance,
    };
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    const PLUGIN_NAME: &str = "autumn-media-plugin";
    const API_PREFIX: &str = "/api/media";

    /// The real room routes `build` declares, attributed to the plugin exactly
    /// as `declare_plugin_routes` attributes them at runtime.
    fn declared_room_routes() -> Vec<RouteInfo> {
        super::rooms::room_route_infos(API_PREFIX)
            .into_iter()
            .map(|mut route| {
                route.source = RouteSource::Plugin(PLUGIN_NAME.to_owned());
                route
            })
            .collect()
    }

    #[test]
    fn build_declares_the_four_room_routes_when_rooms_enabled() {
        let routes = super::rooms::room_route_infos(API_PREFIX);
        assert_eq!(routes.len(), 4, "rooms declare exactly four routes");
        assert!(
            routes
                .iter()
                .all(|route| route.path.starts_with(API_PREFIX)),
            "every declared room route lives under the API prefix"
        );
    }

    #[test]
    fn room_routes_are_attributed_to_plugin_name() {
        let result = check_route_attribution(PLUGIN_NAME, &declared_room_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "attribution failed: {}",
            result.message
        );
    }

    #[test]
    fn room_routes_live_under_api_prefix() {
        let result = check_route_prefix(PLUGIN_NAME, API_PREFIX, &[], &declared_room_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "prefix check failed: {}",
            result.message
        );
    }

    #[test]
    fn room_routes_have_no_collisions_in_isolation() {
        let (result, _) = check_collisions(&declared_room_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "unexpected collision: {}",
            result.message
        );
    }

    #[test]
    fn room_routes_have_no_undeclared_sensitive_surfaces() {
        let result = check_sensitive_surfaces(PLUGIN_NAME, &declared_room_routes(), &[]);
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "sensitive-surfaces check failed: {}",
            result.message
        );
    }

    #[test]
    fn room_single_registration_passes_duplicate_check() {
        let result = check_duplicate_registration(PLUGIN_NAME, &declared_room_routes());
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "single registration should pass: {}",
            result.message
        );
    }

    #[test]
    fn room_routes_pass_full_conformance() {
        let config = ConformanceConfig::new(PLUGIN_NAME).prefix(API_PREFIX);
        let report = run_conformance(&config, &declared_room_routes());
        assert!(
            report.passed(),
            "MediaPlugin conformance failed:\n{}",
            report.to_text_report()
        );
    }

    #[test]
    fn duplicate_registration_detected() {
        // Installing the plugin twice would double its routes → FAIL.
        let mut routes = declared_room_routes();
        routes.extend(declared_room_routes());
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
        let mut routes = declared_room_routes();
        routes.push(RouteInfo {
            method: "GET".to_owned(),
            path: format!("{API_PREFIX}/rooms/{{room_id}}"),
            handler: "host::roster".to_owned(),
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
