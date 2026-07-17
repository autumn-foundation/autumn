//! Dependency-free `FFmpeg` encode primitives: command builders, concat-list
//! plumbing, seek-preview `WebVTT`, and recording-segment discovery.
//!
//! Each command struct is a pure description of an `FFmpeg` invocation — its
//! [`args`](FfmpegHighlightCommand::args)/`*_args` builders produce the exact
//! argument vector, and its `run` method shells out and returns the number of
//! bytes written. Argument construction never touches the filesystem, so the
//! arg vectors are unit-testable without an `FFmpeg` binary on `PATH`; only the
//! `run`/`run_with_deadline` methods spawn a process.
//!
//! Terminology: a *recording segment directory* is wherever the ingest layer
//! writes its rolling video segments (one file per segment). The clip/highlight
//! encoders read those segments; the poster/sprite encoders sample frames from
//! them; the live-thumbnail encoder reads a live playlist URL instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{MediaError, stderr_tail};

/// Tail-of-file clip/highlight encoder that transcodes a fixed window of a
/// recording to an MP4.
///
/// Seeks `start_seconds` into the source, then writes `duration_seconds` of
/// H.264/AAC with `+faststart` so the result streams progressively.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegHighlightCommand {
    ffmpeg_bin: String,
    input_path: PathBuf,
    output_path: PathBuf,
    start_seconds: u32,
    duration_seconds: u32,
}

impl FfmpegHighlightCommand {
    /// Build a highlight-encode command.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_path: impl Into<PathBuf>,
        output_path: impl Into<PathBuf>,
        start_seconds: u32,
        duration_seconds: u32,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_path: input_path.into(),
            output_path: output_path.into(),
            start_seconds,
            duration_seconds,
        }
    }

    /// The exact `FFmpeg` argument vector this command runs.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        vec![
            "-y".to_owned(),
            "-ss".to_owned(),
            self.start_seconds.to_string(),
            "-i".to_owned(),
            self.input_path.display().to_string(),
            "-t".to_owned(),
            self.duration_seconds.to_string(),
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-movflags".to_owned(),
            "+faststart".to_owned(),
            self.output_path.display().to_string(),
        ]
    }

    /// Run `FFmpeg` and return the encoded file size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the input recording does not exist, the output
    /// directory cannot be created, `FFmpeg` exits unsuccessfully, or the
    /// encoded output cannot be inspected.
    pub fn run(&self) -> Result<u64, MediaError> {
        if !self.input_path.exists() {
            return Err(MediaError::FfmpegSourceMissing {
                path: self.input_path.display().to_string(),
            });
        }
        create_output_parent(&self.output_path)?;

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.args())
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }
}

/// One-frame poster extraction used to give clip/highlight permalinks a visual
/// unfurl.
///
/// Seeks `tail_offset_seconds` back from end-of-file so the poster always lands
/// inside the captured window regardless of how long the source recording
/// segment is. When multiple segments are provided they are concatenated first
/// so the poster does not degrade to a slice of the freshly-opened segment
/// after a segment rollover. Best-effort by convention: callers log and ignore
/// failures so a flaky poster never demotes an otherwise-good clip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegPosterCommand {
    ffmpeg_bin: String,
    input_paths: Vec<PathBuf>,
    output_path: PathBuf,
    tail_offset_seconds: u32,
}

impl FfmpegPosterCommand {
    /// Build a poster-extraction command.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_paths: Vec<PathBuf>,
        output_path: impl Into<PathBuf>,
        tail_offset_seconds: u32,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_paths,
            output_path: output_path.into(),
            tail_offset_seconds,
        }
    }

    /// The `FFmpeg` argument vector for this command. `concat_list` is `Some`
    /// when multiple source segments are demuxed through a concat list.
    #[must_use]
    pub(crate) fn poster_args(&self, concat_list: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "-y".to_owned(),
            "-sseof".to_owned(),
            format!("-{}", self.tail_offset_seconds),
        ];
        if let Some(list) = concat_list {
            args.extend([
                "-f".to_owned(),
                "concat".to_owned(),
                "-safe".to_owned(),
                "0".to_owned(),
                "-i".to_owned(),
                list.display().to_string(),
            ]);
        } else {
            args.extend(["-i".to_owned(), self.input_paths[0].display().to_string()]);
        }
        args.extend([
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-q:v".to_owned(),
            "4".to_owned(),
            self.output_path.display().to_string(),
        ]);
        args
    }

    /// Render the poster JPEG and return its size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when there are no source recordings, a source is
    /// missing, the output directory or concat list cannot be written,
    /// `FFmpeg` fails, or the output cannot be inspected after a successful run.
    pub fn run(&self) -> Result<u64, MediaError> {
        require_sources(&self.input_paths)?;
        create_output_parent(&self.output_path)?;

        let _concat_guard;
        let concat_list_path = if self.input_paths.len() > 1 {
            let path = write_concat_list(&self.input_paths, &self.output_path)?;
            _concat_guard = ConcatListGuard::new(path.clone());
            Some(path)
        } else {
            None
        };

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.poster_args(concat_list_path.as_deref()))
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }
}

/// Seconds of runtime represented by each seek-preview frame.
///
/// One frame per 10 seconds satisfies a "≥ 1 frame / 10 s" preview density.
/// This value drives the `FFmpeg` `fps=1/N` sampling filter, the frame-count
/// computation, and the `WebVTT` cue timings — keep them aligned.
pub const PREVIEW_FRAME_INTERVAL_SECONDS: u32 = 10;

/// Number of thumbnail columns in the seek-preview sprite mosaic.
/// Rows are derived as `ceil(frame_count / PREVIEW_SPRITE_COLUMNS)`.
pub const PREVIEW_SPRITE_COLUMNS: u32 = 10;

/// Width, in pixels, of a single seek-preview cell (16:9, letterboxed).
pub const PREVIEW_CELL_WIDTH: u32 = 160;

/// Height, in pixels, of a single seek-preview cell (16:9, letterboxed).
pub const PREVIEW_CELL_HEIGHT: u32 = 90;

/// Format a whole-second offset as a `WebVTT` `HH:MM:SS.mmm` timestamp.
#[must_use]
fn format_vtt_timestamp(total_seconds: u32) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.000")
}

/// Build a `WebVTT` thumbnail track for a seek-preview sprite sheet.
///
/// Emits a `WEBVTT` header followed by one cue per frame. Cue `i` spans
/// `[i * interval_seconds, (i + 1) * interval_seconds)` and its payload is
/// `<sprite_url>#xywh=<x>,<y>,<w>,<h>` where `x = (i % cols) * cell_w` and
/// `y = (i / cols) * cell_h` — the `#xywh` media-fragment the de-facto standard
/// VOD players (Video.js, Plyr, JW Player) consume with no per-seek request.
#[must_use]
pub fn build_preview_webvtt(
    frame_count: u32,
    interval_seconds: u32,
    cols: u32,
    cell_w: u32,
    cell_h: u32,
    sprite_url: &str,
) -> String {
    use std::fmt::Write as _;

    let cols = cols.max(1);
    let mut vtt = String::from("WEBVTT\n\n");
    for i in 0..frame_count {
        let start = i * interval_seconds;
        let end = (i + 1) * interval_seconds;
        let x = (i % cols) * cell_w;
        let y = (i / cols) * cell_h;
        let _ = write!(
            vtt,
            "{} --> {}\n{sprite_url}#xywh={x},{y},{cell_w},{cell_h}\n\n",
            format_vtt_timestamp(start),
            format_vtt_timestamp(end),
        );
    }
    vtt
}

/// Seek-preview sprite-mosaic encoder.
///
/// Samples one frame per [`PREVIEW_FRAME_INTERVAL_SECONDS`] of the recording,
/// letterboxes each into a [`PREVIEW_CELL_WIDTH`]×[`PREVIEW_CELL_HEIGHT`] cell,
/// and packs them into a single `cols`×`rows` JPEG mosaic via `FFmpeg`'s `tile`
/// filter. Multi-segment recordings are concatenated first (mirroring
/// [`FfmpegPosterCommand`]) so a segment rollover mid-recording does not
/// truncate the mosaic. Best-effort by convention: callers log and ignore
/// failures so a flaky sprite never breaks VOD playback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegPreviewSpriteCommand {
    ffmpeg_bin: String,
    input_paths: Vec<PathBuf>,
    output_path: PathBuf,
    cols: u32,
    rows: u32,
}

impl FfmpegPreviewSpriteCommand {
    /// Build a sprite-mosaic command. `cols`/`rows` are clamped to at least 1.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_paths: Vec<PathBuf>,
        output_path: impl Into<PathBuf>,
        cols: u32,
        rows: u32,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_paths,
            output_path: output_path.into(),
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }

    /// The `FFmpeg` argument vector for this command. `concat_list` is `Some`
    /// when multiple source segments are demuxed through a concat list.
    #[must_use]
    pub(crate) fn sprite_args(&self, concat_list: Option<&Path>) -> Vec<String> {
        let mut args = vec!["-y".to_owned()];
        if let Some(list) = concat_list {
            args.extend([
                "-f".to_owned(),
                "concat".to_owned(),
                "-safe".to_owned(),
                "0".to_owned(),
                "-i".to_owned(),
                list.display().to_string(),
            ]);
        } else {
            args.extend(["-i".to_owned(), self.input_paths[0].display().to_string()]);
        }
        let filter = format!(
            "fps=1/{PREVIEW_FRAME_INTERVAL_SECONDS},scale={cw}:{ch}:force_original_aspect_ratio=decrease,pad={cw}:{ch}:(ow-iw)/2:(oh-ih)/2,tile={cols}x{rows}",
            cw = PREVIEW_CELL_WIDTH,
            ch = PREVIEW_CELL_HEIGHT,
            cols = self.cols,
            rows = self.rows,
        );
        args.extend([
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-vf".to_owned(),
            filter,
            "-q:v".to_owned(),
            "5".to_owned(),
            self.output_path.display().to_string(),
        ]);
        args
    }

    /// Render the sprite-sheet JPEG mosaic and return its size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when there are no source recordings, a source is
    /// missing, the output directory or concat list cannot be written,
    /// `FFmpeg` fails, or the output cannot be inspected after a successful run.
    pub fn run(&self) -> Result<u64, MediaError> {
        require_sources(&self.input_paths)?;
        create_output_parent(&self.output_path)?;

        let _concat_guard;
        let concat_list_path = if self.input_paths.len() > 1 {
            let path = write_concat_list(&self.input_paths, &self.output_path)?;
            _concat_guard = ConcatListGuard::new(path.clone());
            Some(path)
        } else {
            None
        };

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.sprite_args(concat_list_path.as_deref()))
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }
}

/// Write a temporary `FFmpeg` concat-list file next to `near_path` and return it.
///
/// Entries are canonicalized to absolute paths because `FFmpeg`'s concat
/// demuxer resolves list entries relative to the **list file's** directory,
/// which is generally not the recording-segment directory, so relative entries
/// would have `FFmpeg` looking in the wrong place.
fn write_concat_list(input_paths: &[PathBuf], near_path: &Path) -> Result<PathBuf, MediaError> {
    use std::io::Write as _;
    let parent = near_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent).map_err(|source| MediaError::ConcatList {
        path: parent.display().to_string(),
        source,
    })?;
    let stem = near_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    // Make the concat-list filename unique per invocation so two concurrent
    // multi-segment encodes that share an output stem (e.g. `clip.mp4` and its
    // `clip.jpg` poster, both `file_stem() == "clip"`) never collide on one
    // `.clip.concat.txt` — otherwise one would truncate/read the file while the
    // other's `ConcatListGuard` deletes it. Process id + a monotonic counter is
    // dependency-free (no `tempfile` runtime dep) and keeps the file hidden and
    // in the same parent dir, so relative concat resolution is unchanged.
    let seq = CONCAT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let list_path = parent.join(format!(".{stem}.{pid}-{seq}.concat.txt"));
    let mut list_file =
        std::fs::File::create(&list_path).map_err(|source| MediaError::ConcatList {
            path: list_path.display().to_string(),
            source,
        })?;
    for path in input_paths {
        let absolute = absolutize_for_concat(path)?;
        // FFmpeg's concat demuxer parses each entry as a single-quoted string:
        // a literal single quote is written as `'\''` (close-quote,
        // backslash-escaped quote, reopen-quote). Backslashes are literal
        // inside the quotes for the demuxer, so Windows paths need no escaping.
        writeln!(
            list_file,
            "file '{}'",
            concat_escape(&absolute.display().to_string())
        )
        .map_err(|source| MediaError::ConcatList {
            path: list_path.display().to_string(),
            source,
        })?;
    }
    Ok(list_path)
}

/// Process-global monotonic counter making concat-list filenames unique per
/// invocation (paired with the process id) so concurrent same-stem encodes
/// never share a list file.
static CONCAT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Escape a path for a single-quoted `FFmpeg` concat-demuxer `file` entry.
///
/// The demuxer treats everything inside the single quotes literally except the
/// closing quote, so only `'` needs escaping — as `'\''` (close-quote,
/// backslash-escaped quote, reopen-quote). Backslashes stay literal, so Windows
/// paths pass through unchanged.
fn concat_escape(path: &str) -> String {
    path.replace('\'', "'\\''")
}

fn absolutize_for_concat(path: &Path) -> Result<PathBuf, MediaError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    // Fall back to cwd-relative resolution when canonicalize fails (rare on
    // Linux for existing files, but keeps the error message attributable to
    // FFmpeg rather than us).
    let cwd = std::env::current_dir().map_err(|source| MediaError::ConcatList {
        path: path.display().to_string(),
        source,
    })?;
    Ok(cwd.join(path))
}

/// RAII guard that removes a concat-list file when it drops.
struct ConcatListGuard {
    path: PathBuf,
}

impl ConcatListGuard {
    const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ConcatListGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Tail-relative clip encoder.
///
/// Uses `FFmpeg`'s `-sseof -<tail_offset_seconds>` to seek backward from the
/// end of the (possibly-concatenated) source, then `-t <duration_seconds>` to
/// write a fixed-length clip. Callers pad `tail_offset_seconds` to compensate
/// for queue-to-encode drift so the captured window always ends at the viewer's
/// click time, not at `FFmpeg` execution time. When more than one segment is
/// provided the encoder writes a concat-list file and feeds it through `-f
/// concat -safe 0 -i <list>`, so a clip that spans a segment rollover still
/// captures the full window instead of getting truncated to the freshly-opened
/// segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegClipTailCommand {
    ffmpeg_bin: String,
    input_paths: Vec<PathBuf>,
    output_path: PathBuf,
    duration_seconds: u32,
    tail_offset_seconds: u32,
}

impl FfmpegClipTailCommand {
    /// Build a tail-relative clip-encode command.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_paths: Vec<PathBuf>,
        output_path: impl Into<PathBuf>,
        duration_seconds: u32,
        tail_offset_seconds: u32,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_paths,
            output_path: output_path.into(),
            duration_seconds,
            tail_offset_seconds,
        }
    }

    #[must_use]
    fn input_args(&self, concat_list: Option<&Path>) -> Vec<String> {
        concat_list.map_or_else(
            || vec!["-i".to_owned(), self.input_paths[0].display().to_string()],
            |list| {
                vec![
                    "-f".to_owned(),
                    "concat".to_owned(),
                    "-safe".to_owned(),
                    "0".to_owned(),
                    "-i".to_owned(),
                    list.display().to_string(),
                ]
            },
        )
    }

    /// The `FFmpeg` argument vector for this command. `concat_list` is `Some`
    /// when multiple source segments are demuxed through a concat list.
    #[must_use]
    pub(crate) fn encode_args(&self, concat_list: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "-y".to_owned(),
            "-sseof".to_owned(),
            format!("-{}", self.tail_offset_seconds),
        ];
        args.extend(self.input_args(concat_list));
        args.extend([
            "-t".to_owned(),
            self.duration_seconds.to_string(),
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-movflags".to_owned(),
            "+faststart".to_owned(),
            self.output_path.display().to_string(),
        ]);
        args
    }

    /// Run `FFmpeg` and return the encoded file size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when no input recording exists, the output directory
    /// cannot be created, the concat list file cannot be written, `FFmpeg`
    /// exits unsuccessfully, or the encoded output cannot be inspected.
    pub fn run(&self) -> Result<u64, MediaError> {
        require_sources(&self.input_paths)?;
        create_output_parent(&self.output_path)?;

        let _concat_guard;
        let concat_list_path = if self.input_paths.len() > 1 {
            let path = write_concat_list(&self.input_paths, &self.output_path)?;
            _concat_guard = ConcatListGuard::new(path.clone());
            Some(path)
        } else {
            None
        };

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.encode_args(concat_list_path.as_deref()))
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }
}

/// One-frame capture from a live HLS playlist, used to populate the directory
/// thumbnail for an active channel.
///
/// Network reads are bounded so a stalled HLS source can never block the
/// per-channel capture loop. `-rw_timeout` is microseconds in `FFmpeg`; it is
/// set to 5s, which is well below a typical capture cadence so a hung source
/// fails the tick and lets the next one run unimpeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegLiveThumbnailCommand {
    ffmpeg_bin: String,
    input_url: String,
    output_path: PathBuf,
}

impl FfmpegLiveThumbnailCommand {
    /// Build a live-thumbnail capture command.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_url: impl Into<String>,
        output_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_url: input_url.into(),
            output_path: output_path.into(),
        }
    }

    /// The exact `FFmpeg` argument vector this command runs.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        vec![
            "-y".to_owned(),
            // Bound HLS network reads so a stalled source can't wedge the
            // per-channel capture loop. 5s is well under a typical capture
            // cadence; a hung source fails this tick and the next one tries.
            "-rw_timeout".to_owned(),
            "5000000".to_owned(),
            "-i".to_owned(),
            self.input_url.clone(),
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-q:v".to_owned(),
            "4".to_owned(),
            "-an".to_owned(),
            self.output_path.display().to_string(),
        ]
    }

    /// Run `FFmpeg` and return the captured file size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the output directory cannot be created, `FFmpeg`
    /// exits unsuccessfully, or the output cannot be inspected.
    pub fn run(&self) -> Result<u64, MediaError> {
        create_output_parent(&self.output_path)?;

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.args())
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }

    /// Run `FFmpeg` under an async deadline, killing the child process on
    /// timeout so a hung source can never leak a detached process across
    /// capture ticks.
    ///
    /// # Errors
    ///
    /// Returns an error when the output directory cannot be created, the child
    /// cannot be spawned, the deadline elapses (after killing the child),
    /// `FFmpeg` exits unsuccessfully, or the output cannot be inspected.
    pub async fn run_with_deadline(
        &self,
        deadline: std::time::Duration,
    ) -> Result<u64, MediaError> {
        if let Some(parent) = self.output_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|source| {
                MediaError::FfmpegOutputIo {
                    path: parent.display().to_string(),
                    source,
                }
            })?;
        }

        let child = tokio::process::Command::new(&self.ffmpeg_bin)
            .args(self.args())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        let output = match tokio::time::timeout(deadline, child.wait_with_output()).await {
            Ok(result) => result.map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?,
            // `kill_on_drop` reaps the child as it drops on the early return.
            Err(_elapsed) => return Err(MediaError::FfmpegTimeout),
        };

        check_exit(&output)?;

        let metadata = tokio::fs::metadata(&self.output_path)
            .await
            .map_err(|source| MediaError::FfmpegOutputIo {
                path: self.output_path.display().to_string(),
                source,
            })?;
        Ok(metadata.len())
    }
}

/// Width, in pixels, of one participant cell in a room-composite grid (16:9).
pub const ROOM_COMPOSITE_CELL_WIDTH: u32 = 640;

/// Height, in pixels, of one participant cell in a room-composite grid (16:9).
pub const ROOM_COMPOSITE_CELL_HEIGHT: u32 = 360;

/// Maximum participant recordings a room composite accepts.
///
/// Tied to the mesh-room seat cap
/// ([`crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS`], 6): a mesh call never
/// exceeds 6 participants, so a composite never tiles more. Kept as a local
/// const so this dependency-free encode module needs no `config` import.
const ROOM_COMPOSITE_MAX_INPUTS: usize = 6;

/// Grid-composite encoder for a finished mesh-room recording.
///
/// Tiles each participant's recording into a grid — `2x1` for two participants,
/// `2x2` for three–four, `3x2` for five–six — via `FFmpeg`'s `xstack` filter
/// (each cell letterboxed to
/// [`ROOM_COMPOSITE_CELL_WIDTH`]×[`ROOM_COMPOSITE_CELL_HEIGHT`]) and mixes every
/// participant's audio into one track with `amix`, producing a single
/// composited MP4. Best-effort by convention, like the other recording
/// encoders. The composite needs `2..=6` participant recordings (the mesh-room
/// cap); one input per participant, so — unlike the clip/poster encoders — it
/// never concatenates.
///
/// # Audio-stream assumption
///
/// This composite assumes **each participant recording carries an audio
/// stream**: the `-filter_complex` graph references every input's audio and
/// mixes them with `amix`, so a **video-only participant recording (no audio
/// stream) is not supported for composition this slice** — `FFmpeg` would fail
/// on the missing stream. The **per-participant recordings themselves are
/// unaffected**; this limitation is only about the composited grid output.
/// Robust handling (per-input audio detection / silence synthesis) is recorded
/// future work on the epic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegRoomCompositeCommand {
    ffmpeg_bin: String,
    input_paths: Vec<PathBuf>,
    output_path: PathBuf,
}

impl FfmpegRoomCompositeCommand {
    /// Build a room-composite command. One `input_path` per participant.
    #[must_use]
    pub fn new(
        ffmpeg_bin: impl Into<String>,
        input_paths: Vec<PathBuf>,
        output_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ffmpeg_bin: ffmpeg_bin.into(),
            input_paths,
            output_path: output_path.into(),
        }
    }

    /// The exact `FFmpeg` argument vector this command runs.
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        let n = self.input_paths.len();
        let mut args = vec!["-y".to_owned()];
        for path in &self.input_paths {
            args.push("-i".to_owned());
            args.push(path.display().to_string());
        }
        args.push("-filter_complex".to_owned());
        args.push(room_composite_filtergraph(n));
        args.extend([
            "-map".to_owned(),
            "[vout]".to_owned(),
            "-map".to_owned(),
            "[aout]".to_owned(),
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-movflags".to_owned(),
            "+faststart".to_owned(),
            self.output_path.display().to_string(),
        ]);
        args
    }

    /// Render the composited MP4 and return its size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when there are fewer than two or more than six source
    /// recordings (the `2..=6` mesh-room band), a source is missing, the output
    /// directory cannot be created, `FFmpeg` fails, or the output cannot be
    /// inspected after a successful run.
    pub fn run(&self) -> Result<u64, MediaError> {
        require_sources(&self.input_paths)?;
        if self.input_paths.len() < 2 {
            return Err(MediaError::FfmpegSourceMissing {
                path: "<room composite needs at least two participant recordings>".to_owned(),
            });
        }
        if self.input_paths.len() > ROOM_COMPOSITE_MAX_INPUTS {
            return Err(MediaError::FfmpegSourceMissing {
                path: "<room composite accepts at most six participant recordings>".to_owned(),
            });
        }
        create_output_parent(&self.output_path)?;

        let output = Command::new(&self.ffmpeg_bin)
            .args(self.args())
            .output()
            .map_err(|source| MediaError::FfmpegSpawn {
                bin: self.ffmpeg_bin.clone(),
                source,
            })?;

        check_exit(&output)?;
        output_size(&self.output_path)
    }
}

/// Grid dimensions `(cols, rows)` for an `n`-participant composite: `2x1` for
/// two, `2x2` for three–four, `3x2` for five–six.
const fn room_composite_grid_dims(n: usize) -> (usize, usize) {
    if n <= 2 {
        (2, 1)
    } else if n <= 4 {
        (2, 2)
    } else {
        (3, 2)
    }
}

/// Build the `-filter_complex` graph: one letterboxed `scale`/`pad` per input,
/// an `xstack` grid of them, and an `amix` of every audio track.
fn room_composite_filtergraph(n: usize) -> String {
    use std::fmt::Write as _;

    let (cols, _rows) = room_composite_grid_dims(n);
    let (w, h) = (ROOM_COMPOSITE_CELL_WIDTH, ROOM_COMPOSITE_CELL_HEIGHT);
    let mut parts = Vec::new();
    let mut video_labels = String::new();
    let mut audio_labels = String::new();
    for i in 0..n {
        parts.push(format!(
            "[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1[v{i}]"
        ));
        let _ = write!(video_labels, "[v{i}]");
        let _ = write!(audio_labels, "[{i}:a]");
    }
    let layout = room_composite_layout(cols, n);
    parts.push(format!(
        "{video_labels}xstack=inputs={n}:layout={layout}[vout]"
    ));
    parts.push(format!("{audio_labels}amix=inputs={n}:normalize=1[aout]"));
    parts.join(";")
}

/// The `xstack` `layout` string. Every cell is the same size, so each input at
/// `(row, col)` is placed at `X_Y` where the column offset is a sum of the
/// first cell's width (`w0`) and the row offset a sum of its height (`h0`).
fn room_composite_layout(cols: usize, n: usize) -> String {
    (0..n)
        .map(|i| {
            let col = i % cols;
            let row = i / cols;
            let x = if col == 0 {
                "0".to_owned()
            } else {
                vec!["w0"; col].join("+")
            };
            let y = if row == 0 {
                "0".to_owned()
            } else {
                vec!["h0"; row].join("+")
            };
            format!("{x}_{y}")
        })
        .collect::<Vec<_>>()
        .join("|")
}

// ── Shared run() helpers ────────────────────────────────────────────────────

/// Reject an empty or missing set of source recordings before spawning.
fn require_sources(input_paths: &[PathBuf]) -> Result<(), MediaError> {
    if input_paths.is_empty() {
        return Err(MediaError::FfmpegSourceMissing {
            path: "<no source recordings>".to_owned(),
        });
    }
    if let Some(missing) = input_paths.iter().find(|path| !path.exists()) {
        return Err(MediaError::FfmpegSourceMissing {
            path: missing.display().to_string(),
        });
    }
    Ok(())
}

/// Create the parent directory of `output_path` if it has one.
fn create_output_parent(output_path: &Path) -> Result<(), MediaError> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MediaError::FfmpegOutputIo {
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Map a non-zero `FFmpeg` exit into [`MediaError::FfmpegNonZeroExit`].
fn check_exit(output: &std::process::Output) -> Result<(), MediaError> {
    if output.status.success() {
        return Ok(());
    }
    Err(MediaError::FfmpegNonZeroExit {
        code: output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
        stderr_tail: stderr_tail(&output.stderr),
    })
}

/// Inspect the encoded output and return its size in bytes.
fn output_size(output_path: &Path) -> Result<u64, MediaError> {
    let metadata = std::fs::metadata(output_path).map_err(|source| MediaError::FfmpegOutputIo {
        path: output_path.display().to_string(),
        source,
    })?;
    Ok(metadata.len())
}

/// Turn arbitrary text into a lowercase, dash-separated slug.
///
/// ASCII alphanumerics are lowercased; every other character becomes a dash,
/// runs of dashes collapse, and leading/trailing dashes are trimmed. An empty
/// or all-punctuation input yields the literal fallback `"clip"`.
#[must_use]
pub fn slugify(input: &str) -> String {
    let slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "clip".to_owned()
    } else {
        slug
    }
}

/// Return the single newest recording file in `recording_dir`, if any.
#[must_use]
pub fn newest_recording_file(recording_dir: &Path) -> Option<PathBuf> {
    newest_recording_files(recording_dir, 1).into_iter().next()
}

/// Return up to `max_count` newest recording files in chronological order
/// (oldest first), suitable for feeding to `FFmpeg`'s concat demuxer.
#[must_use]
pub fn newest_recording_files(recording_dir: &Path, max_count: usize) -> Vec<PathBuf> {
    newest_recording_files_since(recording_dir, max_count, None)
}

/// Variant of [`newest_recording_files`] that ignores entries modified before
/// `min_modified`.
///
/// Used as a backstop when callers genuinely want "the newest N segments". The
/// clip pipeline prefers [`recording_segments_covering_window`] so a long
/// encode backlog cannot pick later segments than the ones that actually
/// contain the captured moment.
#[must_use]
pub fn newest_recording_files_since(
    recording_dir: &Path,
    max_count: usize,
    min_modified: Option<std::time::SystemTime>,
) -> Vec<PathBuf> {
    if max_count == 0 {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(recording_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            if let Some(cutoff) = min_modified
                && modified < cutoff
            {
                return None;
            }
            Some((modified, path))
        })
        .collect();
    // Sort newest-first, then take the requested window, then re-sort
    // chronologically so the concat list plays in order.
    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    candidates.truncate(max_count);
    candidates.sort_by_key(|a| a.0);
    candidates.into_iter().map(|(_, path)| path).collect()
}

/// Pick the recording segments that contain the window
/// `[anchored_at - lookback, anchored_at]`, chronologically.
///
/// The encode step uses this to anchor the source list on the captured moment
/// instead of the latest writes — if the encode backlog grew long enough that
/// more than two segments rolled since the click, "newest two segments" would
/// no longer include the captured moment.
///
/// Algorithm: find the first segment whose mtime is at or after `anchored_at`
/// (that's the segment that was being written when the viewer clicked — its
/// mtime advances past the click) and walk backward until the included
/// segments' mtimes drop below `anchored_at - lookback`. Segments older than
/// `broadcast_started_at` are dropped so a stream key reused for a later
/// broadcast does not drag the prior broadcast's tail in. When no segment
/// satisfies the anchor (e.g. a click that just barely outran the first segment
/// write), falls back to the newest remaining segment so a retrying caller
/// still has something to work with.
#[must_use]
pub fn recording_segments_covering_window(
    recording_dir: &Path,
    broadcast_started_at: std::time::SystemTime,
    anchored_at: std::time::SystemTime,
    lookback: std::time::Duration,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(recording_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            if modified < broadcast_started_at {
                return None;
            }
            Some((modified, path))
        })
        .collect();
    candidates.sort_by_key(|a| a.0);
    if candidates.is_empty() {
        return Vec::new();
    }

    let anchor_idx = candidates
        .iter()
        .position(|(mtime, _)| *mtime >= anchored_at)
        .unwrap_or(candidates.len() - 1);
    let cutoff = anchored_at
        .checked_sub(lookback)
        .unwrap_or(broadcast_started_at);

    let mut indices = vec![anchor_idx];
    let mut i = anchor_idx;
    while i > 0 {
        i -= 1;
        if candidates[i].0 > cutoff {
            indices.push(i);
        } else {
            break;
        }
    }
    indices.sort_unstable();
    indices
        .into_iter()
        .map(|idx| candidates[idx].1.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{
        FfmpegClipTailCommand, FfmpegHighlightCommand, FfmpegLiveThumbnailCommand,
        FfmpegPosterCommand, FfmpegPreviewSpriteCommand, FfmpegRoomCompositeCommand,
        PREVIEW_CELL_HEIGHT, PREVIEW_CELL_WIDTH, PREVIEW_FRAME_INTERVAL_SECONDS,
        PREVIEW_SPRITE_COLUMNS, build_preview_webvtt, newest_recording_files,
        newest_recording_files_since, recording_segments_covering_window, slugify,
    };

    // ── Arg-vector contracts (no FFmpeg binary required) ────────────────────

    #[test]
    fn highlight_args_have_expected_flags() {
        let cmd = FfmpegHighlightCommand::new("ffmpeg", "/rec/in.mp4", "/out/clip.mp4", 12, 30);
        let args = cmd.args();
        assert_eq!(args.first().map(String::as_str), Some("-y"));
        // -ss <start> comes before -i (an input-seek).
        let ss = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[ss + 1], "12");
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert!(ss < i, "-ss must precede -i");
        assert_eq!(args[i + 1], "/rec/in.mp4");
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "30");
        assert_windowed(&args, "-c:v", "libx264");
        assert_windowed(&args, "-preset", "veryfast");
        assert_windowed(&args, "-c:a", "aac");
        assert_windowed(&args, "-movflags", "+faststart");
        assert_eq!(args.last().map(String::as_str), Some("/out/clip.mp4"));
    }

    #[test]
    fn clip_tail_single_source_uses_plain_input() {
        let cmd = FfmpegClipTailCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/seg0.mp4")],
            "/out/clip.mp4",
            20,
            25,
        );
        let args = cmd.encode_args(None);
        // -sseof -<tail>
        let sseof = args.iter().position(|a| a == "-sseof").unwrap();
        assert_eq!(args[sseof + 1], "-25");
        // single -i, no concat demuxer
        assert!(!args.iter().any(|a| a == "concat"));
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "/rec/seg0.mp4");
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "20");
    }

    #[test]
    fn clip_tail_multi_source_uses_concat_list() {
        let cmd = FfmpegClipTailCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/a.mp4"), PathBuf::from("/rec/b.mp4")],
            "/out/clip.mp4",
            20,
            25,
        );
        let list = PathBuf::from("/out/.clip.concat.txt");
        let args = cmd.encode_args(Some(&list));
        assert_windowed(&args, "-f", "concat");
        assert_windowed(&args, "-safe", "0");
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "/out/.clip.concat.txt");
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "20");
    }

    #[test]
    fn poster_single_vs_concat() {
        let single = FfmpegPosterCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/seg.mp4")],
            "/out/poster.jpg",
            4,
        );
        let sa = single.poster_args(None);
        let sseof = sa.iter().position(|a| a == "-sseof").unwrap();
        assert_eq!(sa[sseof + 1], "-4");
        assert!(!sa.iter().any(|a| a == "concat"));
        assert_windowed(&sa, "-frames:v", "1");
        assert_windowed(&sa, "-q:v", "4");
        assert_eq!(sa.last().map(String::as_str), Some("/out/poster.jpg"));

        let list = PathBuf::from("/out/.poster.concat.txt");
        let multi = FfmpegPosterCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/a.mp4"), PathBuf::from("/rec/b.mp4")],
            "/out/poster.jpg",
            4,
        );
        let ma = multi.poster_args(Some(&list));
        assert_windowed(&ma, "-f", "concat");
        assert_windowed(&ma, "-safe", "0");
        let i = ma.iter().position(|a| a == "-i").unwrap();
        assert_eq!(ma[i + 1], "/out/.poster.concat.txt");
    }

    #[test]
    fn sprite_args_have_exact_filter() {
        let cmd = FfmpegPreviewSpriteCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/vod.mp4")],
            "/out/sprite.jpg",
            10,
            3,
        );
        let args = cmd.sprite_args(None);
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(
            args[vf + 1],
            "fps=1/10,scale=160:90:force_original_aspect_ratio=decrease,pad=160:90:(ow-iw)/2:(oh-ih)/2,tile=10x3"
        );
        assert_windowed(&args, "-frames:v", "1");
        assert_windowed(&args, "-q:v", "5");
        assert_eq!(args.last().map(String::as_str), Some("/out/sprite.jpg"));
    }

    #[test]
    fn sprite_new_clamps_cols_rows_to_one() {
        let cmd = FfmpegPreviewSpriteCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/vod.mp4")],
            "/out/sprite.jpg",
            0,
            0,
        );
        let args = cmd.sprite_args(None);
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert!(args[vf + 1].ends_with("tile=1x1"));
    }

    #[test]
    fn live_thumbnail_args_bound_reads_and_drop_audio() {
        let cmd = FfmpegLiveThumbnailCommand::new(
            "ffmpeg",
            "http://mediamtx/live/key/index.m3u8",
            "/out/thumb.jpg",
        );
        let args = cmd.args();
        assert_windowed(&args, "-rw_timeout", "5000000");
        assert_windowed(&args, "-frames:v", "1");
        assert!(args.iter().any(|a| a == "-an"), "must drop audio");
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "http://mediamtx/live/key/index.m3u8");
        assert_eq!(args.last().map(String::as_str), Some("/out/thumb.jpg"));
    }

    #[test]
    fn room_composite_two_participants_side_by_side() {
        let cmd = FfmpegRoomCompositeCommand::new(
            "ffmpeg",
            vec![PathBuf::from("/rec/a.mp4"), PathBuf::from("/rec/b.mp4")],
            "/out/room.mp4",
        );
        let args = cmd.args();
        assert_eq!(args.first().map(String::as_str), Some("-y"));
        // One -i per participant.
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2);
        // Exact filter graph: two letterboxed cells, a 2x1 xstack, an amix.
        let fc = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(
            args[fc + 1],
            "[0:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v0];\
             [1:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v1];\
             [v0][v1]xstack=inputs=2:layout=0_0|w0_0[vout];\
             [0:a][1:a]amix=inputs=2:normalize=1[aout]"
        );
        assert_windowed(&args, "-map", "[vout]");
        assert_windowed(&args, "-c:v", "libx264");
        assert_windowed(&args, "-c:a", "aac");
        assert_windowed(&args, "-movflags", "+faststart");
        assert_eq!(args.last().map(String::as_str), Some("/out/room.mp4"));
    }

    #[test]
    fn room_composite_four_participants_two_by_two_grid() {
        let cmd = FfmpegRoomCompositeCommand::new(
            "ffmpeg",
            vec![
                PathBuf::from("/rec/a.mp4"),
                PathBuf::from("/rec/b.mp4"),
                PathBuf::from("/rec/c.mp4"),
                PathBuf::from("/rec/d.mp4"),
            ],
            "/out/room.mp4",
        );
        let args = cmd.args();
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 4);
        let fc = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(
            args[fc + 1],
            "[0:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v0];\
             [1:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v1];\
             [2:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v2];\
             [3:v]scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v3];\
             [v0][v1][v2][v3]xstack=inputs=4:layout=0_0|w0_0|0_h0|w0_h0[vout];\
             [0:a][1:a][2:a][3:a]amix=inputs=4:normalize=1[aout]"
        );
    }

    #[test]
    fn room_composite_grid_dims_match_participant_bands() {
        use super::room_composite_grid_dims;
        assert_eq!(room_composite_grid_dims(2), (2, 1));
        assert_eq!(room_composite_grid_dims(3), (2, 2));
        assert_eq!(room_composite_grid_dims(4), (2, 2));
        assert_eq!(room_composite_grid_dims(5), (3, 2));
        assert_eq!(room_composite_grid_dims(6), (3, 2));
    }

    #[test]
    fn room_composite_run_rejects_single_participant() {
        let dir = tempfile::tempdir().unwrap();
        let only = dir.path().join("solo.mp4");
        File::create(&only).unwrap();
        let cmd =
            FfmpegRoomCompositeCommand::new("ffmpeg", vec![only], dir.path().join("room.mp4"));
        // A one-participant composite is meaningless; run() must reject it
        // before spawning FFmpeg.
        assert!(matches!(
            cmd.run(),
            Err(super::MediaError::FfmpegSourceMissing { .. })
        ));
    }

    #[test]
    fn room_composite_run_rejects_more_than_six_participants() {
        let dir = tempfile::tempdir().unwrap();
        // Seven present sources: `require_sources` passes, then the `2..=6`
        // upper bound rejects before spawning FFmpeg (mesh rooms cap at six).
        let sources: Vec<PathBuf> = (0..7)
            .map(|i| {
                let path = dir.path().join(format!("p{i}.mp4"));
                File::create(&path).unwrap();
                path
            })
            .collect();
        let cmd = FfmpegRoomCompositeCommand::new("ffmpeg", sources, dir.path().join("room.mp4"));
        assert!(matches!(
            cmd.run(),
            Err(super::MediaError::FfmpegSourceMissing { .. })
        ));
    }

    /// Assert `flag` appears immediately followed by `value`.
    fn assert_windowed(args: &[String], flag: &str, value: &str) {
        let idx = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag} not found in {args:?}"));
        assert_eq!(
            args[idx + 1],
            value,
            "flag {flag} should be followed by {value}"
        );
    }

    // ── build_preview_webvtt ────────────────────────────────────────────────

    #[test]
    fn webvtt_header_and_cue_per_frame() {
        let vtt = build_preview_webvtt(
            23,
            PREVIEW_FRAME_INTERVAL_SECONDS,
            PREVIEW_SPRITE_COLUMNS,
            PREVIEW_CELL_WIDTH,
            PREVIEW_CELL_HEIGHT,
            "https://example.test/previews/7.jpg",
        );
        assert!(
            vtt.starts_with("WEBVTT\n\n"),
            "must start with WEBVTT header"
        );
        assert_eq!(vtt.matches(" --> ").count(), 23, "one cue per frame");
        // Frame 0: [0,10) at origin.
        assert!(vtt.contains("00:00:00.000 --> 00:00:10.000\n"));
        assert!(vtt.contains("https://example.test/previews/7.jpg#xywh=0,0,160,90\n"));
        // Frame 10 wraps to the second row (cols=10): x=0, y=90; time 100..110s.
        assert!(vtt.contains("00:01:40.000 --> 00:01:50.000\n"));
        assert!(vtt.contains("https://example.test/previews/7.jpg#xywh=0,90,160,90\n"));
        // Frame 11: x=160 (second column), y=90.
        assert!(vtt.contains("https://example.test/previews/7.jpg#xywh=160,90,160,90\n"));
    }

    #[test]
    fn webvtt_cols_guard_treats_zero_as_one() {
        // cols=0 → clamped to 1, so every frame is its own row (x stays 0).
        let vtt = build_preview_webvtt(3, 10, 0, 160, 90, "s.jpg");
        assert!(vtt.contains("s.jpg#xywh=0,0,160,90\n"));
        assert!(vtt.contains("s.jpg#xywh=0,90,160,90\n"));
        assert!(vtt.contains("s.jpg#xywh=0,180,160,90\n"));
    }

    #[test]
    fn webvtt_empty_is_header_only() {
        assert_eq!(
            build_preview_webvtt(0, 10, 10, 160, 90, "sprite.jpg"),
            "WEBVTT\n\n"
        );
    }

    // ── slugify ─────────────────────────────────────────────────────────────

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Foo_Bar.Baz!"), "foo-bar-baz");
        assert_eq!(slugify("--Trim--This--"), "trim-this");
        assert_eq!(slugify("a   b"), "a-b");
    }

    #[test]
    fn slugify_empty_and_all_punct_fall_back_to_clip() {
        assert_eq!(slugify(""), "clip");
        assert_eq!(slugify("!!!"), "clip");
        assert_eq!(slugify("---"), "clip");
    }

    #[test]
    fn slugify_drops_non_ascii() {
        // Unicode letters aren't ASCII-alphanumeric → become dashes → dropped.
        assert_eq!(slugify("café"), "caf");
        assert_eq!(slugify("naïve rocks"), "na-ve-rocks");
    }

    // ── Concat list + guard ─────────────────────────────────────────────────

    #[test]
    fn write_concat_list_writes_absolute_entries_and_guard_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        File::create(&a).unwrap();
        File::create(&b).unwrap();
        let near = dir.path().join("clip.mp4");

        let list = super::write_concat_list(&[a.clone(), b.clone()], &near).unwrap();
        let name = list.file_name().unwrap().to_str().unwrap();
        // Filename is unique per invocation: `.{stem}.{pid}-{seq}.concat.txt`.
        assert!(name.starts_with(".clip."), "name: {name}");
        assert!(name.ends_with(".concat.txt"), "name: {name}");

        let body = std::fs::read_to_string(&list).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each entry is `file '<absolute>'`, and the absolutized path exists.
        for (line, src) in lines.iter().zip([&a, &b]) {
            assert!(line.starts_with("file '"), "line: {line}");
            assert!(line.ends_with('\''));
            let inner = &line["file '".len()..line.len() - 1];
            assert!(
                Path::new(inner).is_absolute(),
                "entry not absolute: {inner}"
            );
            assert!(inner.ends_with(src.file_name().unwrap().to_str().unwrap()));
        }

        {
            let _guard = super::ConcatListGuard::new(list.clone());
            assert!(list.exists());
        }
        assert!(!list.exists(), "guard drop must remove the concat list");
    }

    #[test]
    fn concat_escape_handles_single_quotes_and_leaves_others_literal() {
        // A single quote becomes the demuxer's close/escape/reopen sequence.
        assert_eq!(
            super::concat_escape("creator's-stream/seg.mp4"),
            "creator'\\''s-stream/seg.mp4"
        );
        // Multiple quotes each get the treatment.
        assert_eq!(super::concat_escape("a'b'c"), "a'\\''b'\\''c");
        // Backslashes are literal for the demuxer (Windows paths untouched).
        assert_eq!(super::concat_escape(r"C:\rec\seg.mp4"), r"C:\rec\seg.mp4");
        // A normal path is unchanged.
        assert_eq!(super::concat_escape("/rec/seg.mp4"), "/rec/seg.mp4");
    }

    #[test]
    fn write_concat_list_escapes_apostrophes_in_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("creator's-stream");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("seg.mp4");
        File::create(&a).unwrap();
        let near = dir.path().join("clip.mp4");

        let list = super::write_concat_list(std::slice::from_ref(&a), &near).unwrap();
        let _guard = super::ConcatListGuard::new(list.clone());
        let body = std::fs::read_to_string(&list).unwrap();
        let line = body.lines().next().unwrap();

        // The quoted path contains the exact `'\''` escape for the apostrophe.
        assert!(line.contains("'\\''"), "line: {line}");
        // And it round-trips: applying the demuxer's unescape (replace `'\''`
        // back with `'`) on the quoted inner yields the original absolute path.
        let inner = &line["file '".len()..line.len() - 1];
        let unescaped = inner.replace("'\\''", "'");
        let expected = super::absolutize_for_concat(&a).unwrap();
        assert_eq!(unescaped, expected.display().to_string());
    }

    #[test]
    fn write_concat_list_uses_unique_filenames_per_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        File::create(&a).unwrap();
        File::create(&b).unwrap();
        // Same output stem for both calls (e.g. `clip.mp4` and `clip.jpg`).
        let near_mp4 = dir.path().join("clip.mp4");
        let near_jpg = dir.path().join("clip.jpg");

        let list1 = super::write_concat_list(&[a.clone(), b.clone()], &near_mp4).unwrap();
        let g1 = super::ConcatListGuard::new(list1.clone());
        let list2 = super::write_concat_list(&[a, b], &near_jpg).unwrap();
        let g2 = super::ConcatListGuard::new(list2.clone());

        assert_ne!(
            list1, list2,
            "same-stem concurrent encodes must not collide"
        );
        assert!(list1.exists());
        assert!(list2.exists());

        drop(g1);
        assert!(!list1.exists(), "guard removes its own file");
        assert!(list2.exists(), "the other list is untouched");
        drop(g2);
        assert!(!list2.exists());
    }

    // ── Segment discovery ───────────────────────────────────────────────────

    /// Create `name` in `dir` with mtime `base + offset_secs`.
    fn touch_at(dir: &Path, name: &str, base: SystemTime, offset_secs: u64) -> PathBuf {
        let path = dir.join(name);
        let f = File::create(&path).unwrap();
        f.set_modified(base + Duration::from_secs(offset_secs))
            .unwrap();
        path
    }

    #[test]
    fn newest_files_returns_k_newest_in_chronological_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        touch_at(dir.path(), "s0.mp4", base, 0);
        touch_at(dir.path(), "s1.mp4", base, 10);
        touch_at(dir.path(), "s2.mp4", base, 20);
        touch_at(dir.path(), "s3.mp4", base, 30);

        let got = newest_recording_files(dir.path(), 2);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        // Two newest (s2, s3) returned oldest-first for concat.
        assert_eq!(names, vec!["s2.mp4", "s3.mp4"]);
    }

    #[test]
    fn newest_files_zero_count_and_missing_dir_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(newest_recording_files(dir.path(), 0).is_empty());
        assert!(newest_recording_files(Path::new("/no/such/dir/xyz"), 3).is_empty());
    }

    #[test]
    fn newest_files_since_honors_min_modified() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        touch_at(dir.path(), "old.mp4", base, 0);
        touch_at(dir.path(), "new.mp4", base, 100);

        let cutoff = base + Duration::from_secs(50);
        let got = newest_recording_files_since(dir.path(), 10, Some(cutoff));
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec!["new.mp4"], "old segment must be filtered out");
    }

    #[test]
    fn covering_window_anchors_on_first_mtime_at_or_after_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);
        // seg0@0, seg1@10, seg2@20, seg3@30
        touch_at(dir.path(), "seg0.mp4", base, 0);
        touch_at(dir.path(), "seg1.mp4", base, 10);
        touch_at(dir.path(), "seg2.mp4", base, 20);
        touch_at(dir.path(), "seg3.mp4", base, 30);

        let broadcast_started = base;
        let anchored = base + Duration::from_secs(20); // seg2 is first >= anchor
        let lookback = Duration::from_secs(15); // cutoff = anchor - 15 = @5

        let got =
            recording_segments_covering_window(dir.path(), broadcast_started, anchored, lookback);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        // Anchor seg2, walk back while mtime > @5: seg1@10 included, seg0@0 excluded.
        assert_eq!(names, vec!["seg1.mp4", "seg2.mp4"]);
    }

    #[test]
    fn covering_window_drops_pre_broadcast_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);
        // A stale prior-broadcast segment older than broadcast_started.
        touch_at(dir.path(), "stale.mp4", base, 0);
        touch_at(dir.path(), "live.mp4", base, 100);

        let broadcast_started = base + Duration::from_secs(50);
        let anchored = base + Duration::from_secs(100);
        let got = recording_segments_covering_window(
            dir.path(),
            broadcast_started,
            anchored,
            Duration::from_secs(30),
        );
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            vec!["live.mp4"],
            "pre-broadcast segment must be dropped"
        );
    }

    #[test]
    fn covering_window_falls_back_to_newest_when_no_anchor_match() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);
        touch_at(dir.path(), "a.mp4", base, 0);
        touch_at(dir.path(), "b.mp4", base, 10);

        let broadcast_started = base;
        // Anchor far in the future — no segment mtime reaches it.
        let anchored = base + Duration::from_secs(10_000);
        let got = recording_segments_covering_window(
            dir.path(),
            broadcast_started,
            anchored,
            Duration::from_secs(5),
        );
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        // Falls back to the newest remaining segment only (cutoff excludes a@0).
        assert_eq!(names, vec!["b.mp4"]);
    }

    #[test]
    fn covering_window_missing_dir_is_empty() {
        let got = recording_segments_covering_window(
            Path::new("/no/such/dir/xyz"),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            Duration::from_secs(1),
        );
        assert!(got.is_empty());
    }
}
