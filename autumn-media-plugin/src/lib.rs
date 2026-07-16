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
pub mod storage;
pub mod transport;

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
pub use storage::{MediaStorage, S3MediaStorage, StoredObject};
pub use transport::{
    IngestStatus, MediaMtxClient, MediaUrls, StreamQualityStats, StreamStatus, ViewerCount,
    duration_seconds_param, ingest_statuses_from_paths_json, quality_stats_from_path_json,
    recording_available, recording_mediamtx_path, stream_status_from_path_json,
    viewer_count_from_path_json, viewer_counts_from_paths_json,
};

/// Common downstream imports for configuring and mounting the media plugin.
pub mod prelude {
    pub use crate::{
        FfmpegClipTailCommand, FfmpegHighlightCommand, FfmpegLiveThumbnailCommand,
        FfmpegPosterCommand, FfmpegPreviewSpriteCommand, MediaConfig, MediaConfigError, MediaError,
        MediaMtxConfig, MediaPlugin, MediaStorage, MediaStorageBackend, MediaStorageConfig,
        PREVIEW_CELL_HEIGHT, PREVIEW_CELL_WIDTH, PREVIEW_FRAME_INTERVAL_SECONDS,
        PREVIEW_SPRITE_COLUMNS, RecordingConfig, S3MediaStorage, StoredObject,
        build_preview_webvtt, newest_recording_file, newest_recording_files,
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

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

use crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS;

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
    /// Harvest/job queue name used by media encode work (later slices).
    queue: String,
    /// URL prefix for the plugin's API routes (later slices).
    api_prefix: String,
    // slice N: no encode-sink / retention-defer wiring yet — those depend on
    // types introduced in later slices and are intentionally omitted here.
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
        }
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
        } = self;

        tracing::info!(
            broadcast = %enable_broadcast,
            rooms = %enable_rooms,
            room_max_participants,
            storage_backend = config.storage.backend.as_str(),
            queue = %queue,
            api_prefix = %api_prefix,
            "🍂 Autumn Media configured"
        );

        // slice 1 (storage): insert the resolved MediaProfile / MediaStorage
        //   extensions and register the recording-retention sweep.
        // slice 2 (encode): register the FFmpeg encode jobs on `queue`.
        // slice 3 (transport): the `transport` module (MediaMtxClient + MediaUrls
        //   + status parsers) now exists and is re-exported, but its AppState
        //   extension wiring lands with the consumer slice (the broadcast/room
        //   router) — no consumers yet, so nothing is inserted here.
        // slice 3+ (rooms): insert the MediaMtxClient / MediaUrls extensions and
        //   nest the broadcast + room routers under `api_prefix`.
        //
        // Slice 0 declares no routes and installs no extensions; the empty
        // declaration keeps `autumn routes audit` clean and explicit.
        app.declare_plugin_routes(media_route_infos(&api_prefix))
    }
}

/// The route metadata `MediaPlugin` declares for `autumn routes` listing.
///
/// **Slice 0: empty.** Later slices will fill this in with the broadcast and
/// room routers' routes under `api_prefix`, kept in sync with what `build`
/// nests. Extracted so the empty slice-0 declaration is directly testable.
const fn media_route_infos(_api_prefix: &str) -> Vec<autumn_web::route_listing::RouteInfo> {
    Vec::new()
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
