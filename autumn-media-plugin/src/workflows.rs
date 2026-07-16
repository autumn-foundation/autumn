//! Generic, harvest-free media workflows built on autumn-web's `#[job]` system.
//!
//! Each pipeline Arroyo drove through Autumn Harvest activities — poster
//! extraction, seek-preview sprite + `WebVTT`, and clip/highlight transcode —
//! is generalized here into a durable background job that:
//!
//! 1. resolves [`MediaStorage`](crate::storage::MediaStorage) and the `FFmpeg`
//!    binary from `AppState`,
//! 2. runs the matching [`crate::encode`] command **off** the async runtime via
//!    [`tokio::task::spawn_blocking`],
//! 3. persists the output through `MediaStorage`, and
//! 4. hands a [`MediaArtifact`] to the app-supplied
//!    [`MediaArtifactSink`](crate::sink::MediaArtifactSink).
//!
//! # The extension seam
//!
//! Dispatch flows through [`MediaWorkflows`], whose `queue_*` methods mirror
//! autumn-web's outbound-webhook seam exactly: if a
//! [`MediaWorkflowDelegateExt`] is installed on `AppState`, the request is
//! handed to that runtime delegate (an external Harvest adapter, maintained
//! outside this workspace); otherwise the **built-in** `#[job]` default is
//! enqueued. There is **no** `autumn-harvest` dependency here — the `#[job]`
//! path is the default and the delegate is the override.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use autumn_web::job::JobInfo;
use autumn_web::{AppState, AutumnError, AutumnResult, job};
use serde::{Deserialize, Serialize};

use crate::encode::{
    FfmpegHighlightCommand, FfmpegPosterCommand, FfmpegPreviewSpriteCommand, PREVIEW_CELL_HEIGHT,
    PREVIEW_CELL_WIDTH, PREVIEW_FRAME_INTERVAL_SECONDS, PREVIEW_SPRITE_COLUMNS,
    build_preview_webvtt,
};
use crate::error::MediaError;
use crate::sink::{MediaArtifact, MediaArtifactFile, MediaArtifactKind, MediaArtifactSinkExt};
use crate::storage::MediaStorage;

// ── Job argument structs ─────────────────────────────────────────────────────

/// Arguments for the poster/thumbnail extraction job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailJobArgs {
    /// Recording segment(s) to sample the poster frame from. Multiple segments
    /// are concatenated first (see [`FfmpegPosterCommand`]).
    pub source_paths: Vec<PathBuf>,
    /// Caller-relative storage key for the poster JPEG.
    pub output_key: String,
    /// Seconds back from end-of-file to sample the poster frame.
    pub tail_offset_seconds: u32,
    /// Caller-supplied workflow id echoed back to the sink.
    pub workflow_id: String,
    /// Caller-supplied source identifier (e.g. a broadcast id) echoed back.
    #[serde(default)]
    pub source_id: Option<String>,
}

/// Arguments for the seek-preview sprite + `WebVTT` job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewJobArgs {
    /// Recording segment(s) to sample preview frames from.
    pub source_paths: Vec<PathBuf>,
    /// Caller-relative storage key for the sprite-mosaic JPEG.
    pub sprite_key: String,
    /// Caller-relative storage key for the companion `WebVTT` track.
    pub vtt_key: String,
    /// Total number of preview frames to sample and tile.
    pub frame_count: u32,
    /// Caller-supplied workflow id echoed back to the sink.
    pub workflow_id: String,
    /// Caller-supplied source identifier echoed back.
    #[serde(default)]
    pub source_id: Option<String>,
}

/// Arguments for the transcode-window (clip/highlight) job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscodeJobArgs {
    /// Recording to transcode a window of.
    pub source_path: PathBuf,
    /// Caller-relative storage key for the encoded MP4.
    pub output_key: String,
    /// Start offset (seconds) into the source.
    pub start_seconds: u32,
    /// Length (seconds) of the window to encode.
    pub duration_seconds: u32,
    /// MIME content type to persist the output with.
    pub content_type: String,
    /// Caller-supplied workflow id echoed back to the sink.
    pub workflow_id: String,
    /// Caller-supplied source identifier echoed back.
    #[serde(default)]
    pub source_id: Option<String>,
}

/// Arguments for the recording-finalize orchestration job, which fans a
/// finished recording out into a poster and a seek-preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeRecordingJobArgs {
    /// The poster job to enqueue for the finished recording.
    pub thumbnail: ThumbnailJobArgs,
    /// The seek-preview job to enqueue for the finished recording.
    pub preview: PreviewJobArgs,
}

// ── Built-in `#[job]` handlers (the default engine) ──────────────────────────

// `unique_by = "workflow_id"` + a TTL window make a re-enqueue of the same
// workflow_id a coalesced no-op (returns `Ok(())`, spawns no second job). This
// is what keeps `finalize_recording`'s partial-failure retry idempotent: if the
// thumbnail enqueue succeeds but the preview enqueue then fails, the retried
// orchestration re-enqueues the thumbnail as a dedup no-op (only the missing
// preview is filled), so the app-supplied `MediaArtifactSink::on_completed` is
// not invoked twice for the same artifact via that path. The TTL (5 min) simply
// has to outlast the finalize retry envelope (3 attempts, 1s exponential
// backoff); it is keyed on the caller's own `workflow_id`, so it only ever
// coalesces work the caller already labelled as one logical workflow.
#[job(
    name = "autumn_media_extract_thumbnail",
    max_attempts = 3,
    backoff_ms = 1000,
    queue = "media",
    unique,
    unique_by = "workflow_id",
    unique_for_ms = 300_000
)]
async fn extract_thumbnail(state: AppState, args: ThumbnailJobArgs) -> AutumnResult<()> {
    let (storage, ffmpeg) = resolve_engine(&state)?;
    let workflow_id = args.workflow_id.clone();
    let result = run_thumbnail(&ffmpeg, &storage, args).await;
    deliver_result(&state, MediaArtifactKind::Thumbnail, workflow_id, result).await
}

// See `extract_thumbnail`: the same workflow_id-keyed TTL dedup makes the
// finalize retry re-enqueue of the preview a coalesced no-op.
#[job(
    name = "autumn_media_extract_preview",
    max_attempts = 3,
    backoff_ms = 1000,
    queue = "media",
    unique,
    unique_by = "workflow_id",
    unique_for_ms = 300_000
)]
async fn extract_preview(state: AppState, args: PreviewJobArgs) -> AutumnResult<()> {
    let (storage, ffmpeg) = resolve_engine(&state)?;
    let workflow_id = args.workflow_id.clone();
    let result = run_preview(&ffmpeg, &storage, args).await;
    deliver_result(&state, MediaArtifactKind::Preview, workflow_id, result).await
}

#[job(
    name = "autumn_media_transcode_window",
    max_attempts = 3,
    backoff_ms = 1000,
    queue = "media"
)]
async fn transcode_window(state: AppState, args: TranscodeJobArgs) -> AutumnResult<()> {
    let (storage, ffmpeg) = resolve_engine(&state)?;
    let workflow_id = args.workflow_id.clone();
    let result = run_transcode(&ffmpeg, &storage, args).await;
    deliver_result(&state, MediaArtifactKind::Transcode, workflow_id, result).await
}

#[job(
    name = "autumn_media_finalize_recording",
    max_attempts = 3,
    backoff_ms = 1000,
    queue = "media"
)]
async fn finalize_recording(state: AppState, args: FinalizeRecordingJobArgs) -> AutumnResult<()> {
    // Orchestration: dispatch the two derived-artifact jobs through the same
    // seam, so an installed delegate governs the sub-work too and each gets its
    // own durable retry envelope.
    //
    // Partial-failure retry safety: if `queue_thumbnail` succeeds but
    // `queue_preview` then fails, this job returns `Err` and is retried. The two
    // sub-jobs are `unique`-keyed on their `workflow_id` (see `extract_thumbnail`
    // / `extract_preview`), so the retry re-enqueues the already-queued thumbnail
    // as a coalesced no-op and only fills the missing preview — no duplicate
    // sub-job, and therefore no duplicate `on_completed` for the same artifact
    // via this orchestration path. (The durable engine is still at-least-once, so
    // the app-side sink must remain idempotent by `workflow_id` + `kind` for the
    // general worker-crash-after-persist case, which no enqueue key can remove.)
    let workflows = state.extension::<MediaWorkflows>().ok_or_else(|| {
        AutumnError::internal_server_error_msg("MediaWorkflows extension is not installed")
    })?;
    workflows.queue_thumbnail(&state, args.thumbnail).await?;
    workflows.queue_preview(&state, args.preview).await?;
    Ok(())
}

/// The [`JobInfo`] set for the built-in media jobs, with each job's queue
/// overridden to `queue`.
///
/// The `#[job(queue = "media")]` literal is a compile-time default; the plugin
/// lets an application override [`crate::MediaPlugin::queue`], so registration
/// rewrites `JobInfo.queue` here — the enqueue chokepoint routes by the
/// registered `JobInfo`, so this is what makes the override take effect (the
/// same technique autumn-web's outbound-webhook plugin uses).
#[must_use]
pub fn media_job_infos(queue: &str) -> Vec<JobInfo> {
    [
        __autumn_job_info_extract_thumbnail(),
        __autumn_job_info_extract_preview(),
        __autumn_job_info_transcode_window(),
        __autumn_job_info_finalize_recording(),
    ]
    .into_iter()
    .map(|mut info| {
        queue.clone_into(&mut info.queue);
        info
    })
    .collect()
}

// ── The dispatch seam ────────────────────────────────────────────────────────

/// A runtime delegation callback bridging the generic media workflow layer to
/// an external durable engine (e.g. an Autumn Harvest adapter).
///
/// When a [`MediaWorkflowDelegateExt`] is present on `AppState`, every
/// [`MediaWorkflows`] `queue_*` call routes through it instead of enqueuing the
/// built-in `#[job]`. Mirrors autumn-web's `WebhookDelegate`.
pub type MediaWorkflowDelegate = Arc<
    dyn Fn(
            &AppState,
            MediaWorkflowRequest,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>
        + Send
        + Sync,
>;

/// `AppState` extension carrying the runtime delegation hook. Present → the
/// delegate overrides the built-in `#[job]`; absent → the `#[job]` default runs.
#[derive(Clone)]
pub struct MediaWorkflowDelegateExt(pub MediaWorkflowDelegate);

/// A media-workflow request, as handed to a [`MediaWorkflowDelegate`].
///
/// Variants are boxed so the enum stays small regardless of the payload sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaWorkflowRequest {
    /// A poster/thumbnail extraction request.
    Thumbnail(Box<ThumbnailJobArgs>),
    /// A seek-preview sprite + `WebVTT` request.
    Preview(Box<PreviewJobArgs>),
    /// A transcode-window request.
    Transcode(Box<TranscodeJobArgs>),
    /// A recording-finalize orchestration request.
    Finalize(Box<FinalizeRecordingJobArgs>),
}

/// The service applications call to queue media work.
///
/// Installed on `AppState` by [`crate::MediaPlugin`]. Each `queue_*` method
/// runs the extension-seam branch: delegate if present, else enqueue the
/// built-in `#[job]`.
#[derive(Clone, Debug)]
pub struct MediaWorkflows {
    ffmpeg_bin: String,
    queue: String,
}

impl MediaWorkflows {
    /// Create a workflow service bound to an `FFmpeg` binary and job queue.
    #[must_use]
    pub fn new(ffmpeg_bin: impl Into<String>, queue: impl Into<String>) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            queue: queue.into(),
        }
    }

    /// The configured `FFmpeg` binary path the built-in jobs shell out to.
    #[must_use]
    pub fn ffmpeg_bin(&self) -> &str {
        &self.ffmpeg_bin
    }

    /// The job queue the built-in media jobs are registered on.
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// Queue a poster/thumbnail extraction.
    ///
    /// # Errors
    ///
    /// Returns any error from the runtime delegate or the fallback enqueue.
    pub async fn queue_thumbnail(
        &self,
        state: &AppState,
        args: ThumbnailJobArgs,
    ) -> AutumnResult<()> {
        self.dispatch(state, MediaWorkflowRequest::Thumbnail(Box::new(args)))
            .await
    }

    /// Queue a seek-preview sprite + `WebVTT` build.
    ///
    /// # Errors
    ///
    /// Returns any error from the runtime delegate or the fallback enqueue.
    pub async fn queue_preview(&self, state: &AppState, args: PreviewJobArgs) -> AutumnResult<()> {
        self.dispatch(state, MediaWorkflowRequest::Preview(Box::new(args)))
            .await
    }

    /// Queue a transcode-window (clip/highlight) encode.
    ///
    /// # Errors
    ///
    /// Returns any error from the runtime delegate or the fallback enqueue.
    pub async fn queue_transcode(
        &self,
        state: &AppState,
        args: TranscodeJobArgs,
    ) -> AutumnResult<()> {
        self.dispatch(state, MediaWorkflowRequest::Transcode(Box::new(args)))
            .await
    }

    /// Queue a recording-finalize orchestration (poster + seek-preview).
    ///
    /// # Errors
    ///
    /// Returns any error from the runtime delegate or the fallback enqueue.
    pub async fn queue_recording_finalize(
        &self,
        state: &AppState,
        args: FinalizeRecordingJobArgs,
    ) -> AutumnResult<()> {
        self.dispatch(state, MediaWorkflowRequest::Finalize(Box::new(args)))
            .await
    }

    /// The extension-seam branch, identical in shape to autumn-web's outbound
    /// webhook `dispatch`.
    async fn dispatch(&self, state: &AppState, request: MediaWorkflowRequest) -> AutumnResult<()> {
        if let Some(delegate) = state.extension::<MediaWorkflowDelegateExt>() {
            tracing::debug!("MediaWorkflows::dispatch: delegating media work via runtime hook");
            return (delegate.0)(state, request).await;
        }
        tracing::debug!("MediaWorkflows::dispatch: enqueuing built-in media job");
        match request {
            MediaWorkflowRequest::Thumbnail(args) => ExtractThumbnailJob::enqueue(*args).await,
            MediaWorkflowRequest::Preview(args) => ExtractPreviewJob::enqueue(*args).await,
            MediaWorkflowRequest::Transcode(args) => TranscodeWindowJob::enqueue(*args).await,
            MediaWorkflowRequest::Finalize(args) => FinalizeRecordingJob::enqueue(*args).await,
        }
    }
}

// ── Internal run helpers (shared by the jobs) ────────────────────────────────

/// Resolve the storage backend and `FFmpeg` binary the jobs need from state.
fn resolve_engine(state: &AppState) -> AutumnResult<(Arc<MediaStorage>, String)> {
    let storage = state.extension::<MediaStorage>().ok_or_else(|| {
        AutumnError::internal_server_error_msg("MediaStorage extension is not installed")
    })?;
    let workflows = state.extension::<MediaWorkflows>().ok_or_else(|| {
        AutumnError::internal_server_error_msg("MediaWorkflows extension is not installed")
    })?;
    let ffmpeg = workflows.ffmpeg_bin.clone();
    Ok((storage, ffmpeg))
}

/// Deliver a workflow result to the app-supplied sink, mapping errors for the
/// durable job engine.
async fn deliver_result(
    state: &AppState,
    kind: MediaArtifactKind,
    workflow_id: String,
    result: Result<MediaArtifact, MediaError>,
) -> AutumnResult<()> {
    match result {
        Ok(artifact) => {
            if let Some(sink) = state.extension::<MediaArtifactSinkExt>() {
                sink.0
                    .on_completed(artifact)
                    .await
                    .map_err(|error| media_to_autumn(&error))?;
            } else {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    kind = kind.as_str(),
                    "media workflow completed but no MediaArtifactSink is installed; artifact not recorded"
                );
            }
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            if let Some(sink) = state.extension::<MediaArtifactSinkExt>()
                && let Err(sink_error) = sink.0.on_failed(kind, workflow_id.clone(), message).await
            {
                tracing::warn!(%sink_error, workflow_id = %workflow_id, "MediaArtifactSink::on_failed itself failed");
            }
            // Return the error so the durable job engine retries per the job's
            // max_attempts/backoff.
            Err(media_to_autumn(&error))
        }
    }
}

fn media_to_autumn(error: &MediaError) -> AutumnError {
    AutumnError::internal_server_error_msg(error.to_string())
}

async fn run_thumbnail(
    ffmpeg_bin: &str,
    storage: &MediaStorage,
    args: ThumbnailJobArgs,
) -> Result<MediaArtifact, MediaError> {
    let (output_path, ephemeral) = staged_output(storage, &args.output_key, "jpg")?;
    let command = FfmpegPosterCommand::new(
        ffmpeg_bin,
        args.source_paths.clone(),
        output_path.clone(),
        args.tail_offset_seconds,
    );
    let bytes = run_blocking(move || command.run()).await?;
    let file = persist_and_cleanup(
        storage,
        &args.output_key,
        &output_path,
        "image/jpeg",
        bytes,
        ephemeral,
    )
    .await?;
    Ok(MediaArtifact {
        kind: MediaArtifactKind::Thumbnail,
        workflow_id: args.workflow_id,
        source_id: args.source_id,
        primary: file,
        secondary: None,
        metadata: BTreeMap::new(),
    })
}

async fn run_preview(
    ffmpeg_bin: &str,
    storage: &MediaStorage,
    args: PreviewJobArgs,
) -> Result<MediaArtifact, MediaError> {
    let cols = PREVIEW_SPRITE_COLUMNS;
    let rows = args.frame_count.div_ceil(cols).max(1);
    let (sprite_path, sprite_ephemeral) = staged_output(storage, &args.sprite_key, "jpg")?;
    let command = FfmpegPreviewSpriteCommand::new(
        ffmpeg_bin,
        args.source_paths.clone(),
        sprite_path.clone(),
        cols,
        rows,
    );
    let sprite_bytes = run_blocking(move || command.run()).await?;
    let sprite_file = persist_and_cleanup(
        storage,
        &args.sprite_key,
        &sprite_path,
        "image/jpeg",
        sprite_bytes,
        sprite_ephemeral,
    )
    .await?;

    // The WebVTT cues point at the sprite's *public* URL, so build it only once
    // the sprite has been persisted and its URL is known.
    let vtt = build_preview_webvtt(
        args.frame_count,
        PREVIEW_FRAME_INTERVAL_SECONDS,
        cols,
        PREVIEW_CELL_WIDTH,
        PREVIEW_CELL_HEIGHT,
        &sprite_file.public_url,
    );
    let (vtt_path, vtt_ephemeral) = staged_output(storage, &args.vtt_key, "vtt")?;
    write_text_file(&vtt_path, &vtt).await?;
    let vtt_bytes = u64::try_from(vtt.len()).unwrap_or(u64::MAX);
    let vtt_file = persist_and_cleanup(
        storage,
        &args.vtt_key,
        &vtt_path,
        "text/vtt",
        vtt_bytes,
        vtt_ephemeral,
    )
    .await?;

    let mut metadata = BTreeMap::new();
    metadata.insert("frame_count".to_owned(), args.frame_count.to_string());
    Ok(MediaArtifact {
        kind: MediaArtifactKind::Preview,
        workflow_id: args.workflow_id,
        source_id: args.source_id,
        primary: sprite_file,
        secondary: Some(vtt_file),
        metadata,
    })
}

async fn run_transcode(
    ffmpeg_bin: &str,
    storage: &MediaStorage,
    args: TranscodeJobArgs,
) -> Result<MediaArtifact, MediaError> {
    let (output_path, ephemeral) = staged_output(storage, &args.output_key, "mp4")?;
    let command = FfmpegHighlightCommand::new(
        ffmpeg_bin,
        args.source_path.clone(),
        output_path.clone(),
        args.start_seconds,
        args.duration_seconds,
    );
    let bytes = run_blocking(move || command.run()).await?;
    let file = persist_and_cleanup(
        storage,
        &args.output_key,
        &output_path,
        &args.content_type,
        bytes,
        ephemeral,
    )
    .await?;
    Ok(MediaArtifact {
        kind: MediaArtifactKind::Transcode,
        workflow_id: args.workflow_id,
        source_id: args.source_id,
        primary: file,
        secondary: None,
        metadata: BTreeMap::new(),
    })
}

/// Run a blocking encode closure off the async runtime.
async fn run_blocking<F>(f: F) -> Result<u64, MediaError>
where
    F: FnOnce() -> Result<u64, MediaError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| MediaError::EncodeTaskJoin {
            message: error.to_string(),
        })?
}

/// Resolve the on-disk path an encode command should write its output to.
///
/// The **local** backend serves objects straight from its root, so the output
/// is written to its final served location (`root/{object_key}`) and never
/// cleaned up. The **S3** backend stages to a unique temp file that is uploaded
/// and then removed — so the returned `bool` reports whether the path is
/// ephemeral.
///
/// # Errors
///
/// Returns [`MediaError::ArtifactWrite`] when a local output key would escape
/// the storage root (see [`join_within_root`]). The S3 arm cannot fail (it
/// stages under [`std::env::temp_dir`]).
fn staged_output(
    storage: &MediaStorage,
    key: &str,
    ext: &str,
) -> Result<(PathBuf, bool), MediaError> {
    match storage {
        MediaStorage::Local { root, .. } => {
            // `object_key` only slash-trims a local key — it does not reject a
            // `..`/absolute segment — so guard the join against traversal before
            // an encode writes to `root/{object_key}`.
            let path = join_within_root(root, &storage.object_key(key))?;
            Ok((path, false))
        }
        MediaStorage::S3(_) => Ok((
            std::env::temp_dir().join(format!("autumn-media-{}.{ext}", uuid::Uuid::new_v4())),
            true,
        )),
    }
}

/// Join an already-slash-trimmed caller key onto the local backend `root`,
/// rejecting any key that could escape it.
///
/// Slice-1's key resolution ([`MediaStorage::object_key`]) only slash-trims a
/// local key; its dot-segment handling is URL-only (`encode_key_for_url`), so
/// nothing upstream stops a `..`/absolute/drive-prefix segment from reaching the
/// on-disk join here — and the local encode output is written straight to
/// `root/{object_key}`. This guard rejects such keys before the join (defense in
/// depth), keeping every staged local output inside `root`. Only
/// [`std::path::Component::Normal`] segments are allowed; a `ParentDir` (`..`),
/// a `RootDir`/`Prefix` (absolute), a leading `CurDir` (`.`), or an
/// empty/dot-only key is rejected.
fn join_within_root(root: &Path, key: &str) -> Result<PathBuf, MediaError> {
    use std::path::Component;
    let relative = Path::new(key);
    let mut has_normal = false;
    for component in relative.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            _ => return Err(rejected_output_key(key)),
        }
    }
    if !has_normal {
        // Empty or dot-only key: nothing legitimate to write under `root`.
        return Err(rejected_output_key(key));
    }
    Ok(root.join(relative))
}

/// Build the traversal-rejection error for an unsafe local output key, reusing
/// [`MediaError::ArtifactWrite`] (the crate's "could not write this artifact
/// path" error) rather than introducing a new variant.
fn rejected_output_key(key: &str) -> MediaError {
    MediaError::ArtifactWrite {
        path: key.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "media output key must stay within the storage root \
             (no `..`, absolute, or dot-only path segments)",
        ),
    }
}

/// Persist a staged output under `key`, cleaning up an ephemeral (S3) staging
/// file afterwards, and describe the stored file.
async fn persist_and_cleanup(
    storage: &MediaStorage,
    key: &str,
    local_path: &Path,
    content_type: &str,
    bytes: u64,
    ephemeral: bool,
) -> Result<MediaArtifactFile, MediaError> {
    let stored = storage
        .persist_file_with_content_type(key, local_path, content_type)
        .await;
    if ephemeral {
        // Best-effort: the upload already succeeded or failed; the temp file is
        // never the source of truth.
        let _ = tokio::fs::remove_file(local_path).await;
    }
    let stored = stored?;
    Ok(MediaArtifactFile {
        key: stored.key,
        storage_uri: stored.uri,
        public_url: storage.public_url(key),
        content_type: content_type.to_owned(),
        bytes,
    })
}

/// Write text (the `WebVTT` track) to a staging path, creating parent dirs.
async fn write_text_file(path: &Path, contents: &str) -> Result<(), MediaError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| MediaError::ArtifactWrite {
                path: parent.display().to_string(),
                source,
            })?;
    }
    tokio::fs::write(path, contents)
        .await
        .map_err(|source| MediaError::ArtifactWrite {
            path: path.display().to_string(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        FinalizeRecordingJobArgs, MediaWorkflowDelegate, MediaWorkflowDelegateExt,
        MediaWorkflowRequest, MediaWorkflows, PreviewJobArgs, ThumbnailJobArgs, TranscodeJobArgs,
        extract_thumbnail, media_job_infos, persist_and_cleanup, staged_output,
    };
    use crate::sink::MediaArtifactSinkExt;
    use crate::sink::tests::CountingSink;
    use crate::storage::MediaStorage;
    use autumn_web::AppState;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn local_storage(root: &std::path::Path) -> MediaStorage {
        MediaStorage::Local {
            root: root.to_path_buf(),
            public_base_url: "/media".to_owned(),
        }
    }

    fn thumbnail_args(source: PathBuf) -> ThumbnailJobArgs {
        ThumbnailJobArgs {
            source_paths: vec![source],
            output_key: "thumbnails/7.jpg".to_owned(),
            tail_offset_seconds: 5,
            workflow_id: "wf-thumb".to_owned(),
            source_id: Some("stream-7".to_owned()),
        }
    }

    #[test]
    fn media_job_infos_override_declared_queue() {
        let infos = media_job_infos("custom-queue");
        assert_eq!(infos.len(), 4);
        assert!(infos.iter().all(|info| info.queue == "custom-queue"));
        let names: Vec<&str> = infos.iter().map(|info| info.name.as_str()).collect();
        assert!(names.contains(&"autumn_media_extract_thumbnail"));
        assert!(names.contains(&"autumn_media_extract_preview"));
        assert!(names.contains(&"autumn_media_transcode_window"));
        assert!(names.contains(&"autumn_media_finalize_recording"));
    }

    #[test]
    fn derived_sub_jobs_dedup_on_workflow_id() {
        use autumn_web::job::JobUniquenessWindow;
        // The thumbnail and preview jobs are `unique` on `workflow_id` with a TTL
        // window, so a `finalize_recording` partial-failure retry coalesces the
        // already-enqueued sub-job instead of spawning a duplicate. The transcode
        // and orchestration jobs carry no dedup.
        let infos = media_job_infos("media");
        let uniqueness = |name: &str| {
            infos
                .iter()
                .find(|info| info.name == name)
                .expect("job registered")
                .uniqueness
                .clone()
        };
        for name in [
            "autumn_media_extract_thumbnail",
            "autumn_media_extract_preview",
        ] {
            let u = uniqueness(name).unwrap_or_else(|| panic!("{name} must be unique"));
            assert_eq!(
                u.by,
                vec!["workflow_id".to_owned()],
                "{name} keys on workflow_id"
            );
            assert!(
                matches!(u.window, JobUniquenessWindow::TtlMs(ms) if ms > 0),
                "{name} must use a TTL dedup window, got {:?}",
                u.window
            );
        }
        assert!(uniqueness("autumn_media_transcode_window").is_none());
        assert!(uniqueness("autumn_media_finalize_recording").is_none());
    }

    #[test]
    fn job_args_round_trip_through_json() {
        let args = thumbnail_args(PathBuf::from("/rec/seg.mp4"));
        let json = serde_json::to_string(&args).expect("serialize");
        let back: ThumbnailJobArgs = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(args, back);

        let preview = PreviewJobArgs {
            source_paths: vec![PathBuf::from("/rec/a.mp4")],
            sprite_key: "previews/7.jpg".to_owned(),
            vtt_key: "previews/7.vtt".to_owned(),
            frame_count: 24,
            workflow_id: "wf-prev".to_owned(),
            source_id: None,
        };
        let back: PreviewJobArgs =
            serde_json::from_str(&serde_json::to_string(&preview).expect("ser")).expect("de");
        assert_eq!(preview, back);

        let transcode = TranscodeJobArgs {
            source_path: PathBuf::from("/rec/a.mp4"),
            output_key: "clips/7.mp4".to_owned(),
            start_seconds: 10,
            duration_seconds: 30,
            content_type: "video/mp4".to_owned(),
            workflow_id: "wf-clip".to_owned(),
            source_id: Some("clip-7".to_owned()),
        };
        let back: TranscodeJobArgs =
            serde_json::from_str(&serde_json::to_string(&transcode).expect("ser")).expect("de");
        assert_eq!(transcode, back);
    }

    #[test]
    fn staged_output_is_final_for_local_and_ephemeral_for_temp() {
        let storage = local_storage(std::path::Path::new("/srv/media"));
        let (path, ephemeral) =
            staged_output(&storage, "/thumbnails/7.jpg", "jpg").expect("normal key");
        assert!(!ephemeral, "local backend writes to its served location");
        assert_eq!(path, PathBuf::from("/srv/media/thumbnails/7.jpg"));
    }

    #[test]
    fn staged_output_rejects_local_traversal_keys() {
        let storage = local_storage(std::path::Path::new("/srv/media"));
        // A `..` segment would escape the media root — must be rejected, not
        // joined into `/srv/media/../../etc/...`. (A plain absolute key like
        // `/etc/passwd` is *not* traversal here: `object_key` slash-trims it to
        // the benign in-root relative key `etc/passwd`, so it is not in this set.)
        for key in [
            "../../etc/passwd",
            "clips/../../../etc/passwd",
            "..",
            ".",
            "",
        ] {
            let err = staged_output(&storage, key, "jpg")
                .expect_err("traversal/degenerate key must be rejected");
            assert!(
                matches!(err, super::MediaError::ArtifactWrite { .. }),
                "expected ArtifactWrite rejection for {key:?}, got {err:?}"
            );
        }
        // A normal nested key still resolves inside the root.
        let (path, _) = staged_output(&storage, "previews/7.vtt", "vtt").expect("normal key");
        assert_eq!(path, PathBuf::from("/srv/media/previews/7.vtt"));
    }

    #[tokio::test]
    async fn dispatch_uses_delegate_when_present() {
        let state = AppState::for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(None::<MediaWorkflowRequest>));
        let calls_c = calls.clone();
        let seen_c = seen.clone();
        let delegate: MediaWorkflowDelegate = Arc::new(move |_state, request| {
            calls_c.fetch_add(1, Ordering::SeqCst);
            *seen_c.lock().expect("lock") = Some(request);
            Box::pin(async { Ok(()) })
        });
        state.insert_extension(MediaWorkflowDelegateExt(delegate));

        let workflows = MediaWorkflows::new("ffmpeg", "media");
        workflows
            .queue_thumbnail(&state, thumbnail_args(PathBuf::from("/rec/seg.mp4")))
            .await
            .expect("delegate path returns Ok");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            seen.lock().expect("lock").as_ref(),
            Some(MediaWorkflowRequest::Thumbnail(_))
        ));
    }

    #[tokio::test]
    async fn dispatch_enqueues_builtin_job_when_no_delegate() {
        let _guard = autumn_web::job::global_job_runtime_test_lock().lock().await;
        autumn_web::job::clear_global_job_client();

        let state = AppState::for_test();
        let workflows = MediaWorkflows::new("ffmpeg", "media");
        // No delegate installed → dispatch takes the built-in enqueue branch,
        // which errors because no job runtime is initialized in this unit test.
        let err = workflows
            .queue_thumbnail(&state, thumbnail_args(PathBuf::from("/rec/seg.mp4")))
            .await
            .expect_err("enqueue branch should be taken without a delegate");
        assert!(
            err.to_string().contains("job runtime is not initialized"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn job_reports_failure_to_sink_when_source_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = AppState::for_test();
        state.insert_extension(local_storage(temp.path()));
        state.insert_extension(MediaWorkflows::new("ffmpeg", "media"));
        let sink = Arc::new(CountingSink::new());
        state.insert_extension(MediaArtifactSinkExt(sink.clone()));

        // A source path that does not exist → the poster command fails before it
        // ever spawns FFmpeg (require_sources), so this test needs no binary.
        let args = thumbnail_args(temp.path().join("does-not-exist.mp4"));
        let result = extract_thumbnail(state, args).await;

        assert!(result.is_err(), "missing source must fail the job");
        assert_eq!(sink.failed.load(Ordering::SeqCst), 1);
        assert_eq!(sink.completed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn persist_and_cleanup_describes_the_stored_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage = local_storage(temp.path());
        // Stage a real "encoded" file at the served location.
        let (output_path, ephemeral) =
            staged_output(&storage, "clips/7.mp4", "mp4").expect("normal key");
        std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&output_path, b"encoded-bytes").expect("write");

        let file = persist_and_cleanup(
            &storage,
            "clips/7.mp4",
            &output_path,
            "video/mp4",
            13,
            ephemeral,
        )
        .await
        .expect("persist");

        assert_eq!(file.key, "clips/7.mp4");
        assert_eq!(file.content_type, "video/mp4");
        assert_eq!(file.bytes, 13);
        assert_eq!(file.public_url, "/media/clips/7.mp4");
        assert!(
            std::path::Path::new(&file.storage_uri).exists(),
            "local backend keeps the served file in place"
        );
    }

    #[test]
    fn finalize_args_carry_both_sub_requests() {
        let args = FinalizeRecordingJobArgs {
            thumbnail: thumbnail_args(PathBuf::from("/rec/seg.mp4")),
            preview: PreviewJobArgs {
                source_paths: vec![PathBuf::from("/rec/seg.mp4")],
                sprite_key: "previews/7.jpg".to_owned(),
                vtt_key: "previews/7.vtt".to_owned(),
                frame_count: 12,
                workflow_id: "wf-final".to_owned(),
                source_id: Some("stream-7".to_owned()),
            },
        };
        let back: FinalizeRecordingJobArgs =
            serde_json::from_str(&serde_json::to_string(&args).expect("ser")).expect("de");
        assert_eq!(args, back);
    }
}
