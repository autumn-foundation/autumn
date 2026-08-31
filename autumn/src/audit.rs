//! Structured audit logging with pluggable sinks.
//!
//! Audit logs are intentionally separate from regular application logs and
//! should capture security-sensitive, compliance-relevant actions.
//! Autumn models audit writes as append-only events sent to one or more
//! sinks (database, SIEM adapter, dedicated file, etc.).

use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::state::AppState;

/// Outcome of an audited action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    /// The action completed successfully.
    Success,
    /// The action was attempted but failed.
    Failure,
}

/// A structured audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    /// UTC timestamp recorded when the event was created.
    pub timestamp: DateTime<Utc>,
    /// Actor identifier (user ID, service account ID, or API key ID).
    pub actor_id: String,
    /// Canonical action name (for example, `"user.role.update"`).
    pub action: String,
    /// Target resource identifier affected by the action.
    pub target_resource_id: String,
    /// Caller IP address if known.
    pub ip_address: Option<IpAddr>,
    /// Final action status.
    pub status: AuditStatus,
    /// Additional, action-specific detail (issue #1605).
    ///
    /// Deliberately a flat `String → String` map rather than arbitrary JSON:
    /// it keeps [`AuditEvent`] `Eq` and its serialization key-ordered and
    /// deterministic, and it is the shape SIEM ingestion expects. Empty for
    /// most events; a retention sweep uses it to carry the dataset, the
    /// cutoff timestamp, and the number of rows removed (see
    /// [`crate::data_retention`]).
    ///
    /// `#[serde(default)]`, so audit archives written before this field
    /// existed still deserialize.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, String>,
}

impl AuditEvent {
    /// Create a new audit event with the current UTC timestamp.
    #[must_use]
    pub fn new(
        actor_id: impl Into<String>,
        action: impl Into<String>,
        target_resource_id: impl Into<String>,
        ip_address: Option<IpAddr>,
        status: AuditStatus,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            actor_id: actor_id.into(),
            action: action.into(),
            target_resource_id: target_resource_id.into(),
            ip_address,
            status,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    /// Attach one key/value detail to this event, builder-style.
    ///
    /// Chainable; a repeated key overwrites the earlier value.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Error returned by audit sinks.
#[derive(Debug, Error)]
#[error("audit sink write failed: {message}")]
pub struct AuditError {
    message: String,
}

impl AuditError {
    /// Create a new sink error with a human-readable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn message(&self) -> &str {
        &self.message
    }
}

type AuditWriteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), AuditError>> + Send + 'a>>;

/// What a retention purge did (or would do) to one audit sink (issue #1605).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditPurgeOutcome {
    /// Entries removed — or, for a dry run, that *would* be removed.
    pub entries_removed: u64,
    /// `false` when this sink has no notion of purging, so a caller can tell
    /// "nothing was old enough" apart from "this destination cannot be
    /// pruned from here" (a SIEM forwarder, a broadcast channel).
    pub supported: bool,
}

impl AuditPurgeOutcome {
    /// A sink that cannot purge: nothing removed, support not claimed.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            entries_removed: 0,
            supported: false,
        }
    }

    /// A sink that purged (possibly zero) entries.
    #[must_use]
    pub const fn purged(entries_removed: u64) -> Self {
        Self {
            entries_removed,
            supported: true,
        }
    }
}

/// What a purge across *every* sink of an [`AuditLogger`] did (issue #1605).
///
/// Separate from the per-sink [`AuditPurgeOutcome`] because a fan-out can
/// partly succeed: entries removed by one sink stay removed even when a later
/// sink fails, so both facts have to reach the caller together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditPurgeSummary {
    /// Entries removed across all sinks — or, for a dry run, that *would* be
    /// removed. Counts work that really happened, even alongside
    /// [`Self::errors`].
    pub entries_removed: u64,
    /// `true` when at least one sink could purge at all.
    pub supported: bool,
    /// One message per sink that failed. Empty on a fully successful purge.
    pub errors: Vec<String>,
}

impl AuditPurgeSummary {
    /// The failures joined into one human-readable message, or `None` when
    /// every sink succeeded.
    #[must_use]
    pub fn error_message(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        Some(format!(
            "{} audit sink(s) failed to purge: {}",
            self.errors.len(),
            self.errors.join(" | ")
        ))
    }
}

type AuditPurgeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AuditPurgeOutcome, AuditError>> + Send + 'a>>;

/// A destination for append-only audit events.
pub trait AuditSink: Send + Sync + 'static {
    /// Persist one audit event. Implementations must treat events as immutable,
    /// append-only records.
    fn write(&self, event: AuditEvent) -> AuditWriteFuture<'_>;

    /// Remove every entry older than `cutoff`, for the `audit_archives`
    /// retention window (issue #1605).
    ///
    /// Append-only is the *write* contract; retention is a separate,
    /// operator-declared bound that GDPR's storage-limitation principle can
    /// require. A sweep that reaches here is itself audited, so the deletion
    /// is not silent.
    ///
    /// When `dry_run` is `true` the sink counts what it would remove and
    /// changes nothing.
    ///
    /// The default implementation reports
    /// [`AuditPurgeOutcome::unsupported`], so existing sinks keep compiling
    /// and destinations that genuinely cannot be pruned from inside the app
    /// (a SIEM forwarder, a WebSocket channel) say so rather than silently
    /// reporting zero.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError`] when the sink supports purging but the
    /// underlying storage could not be read or rewritten.
    fn purge_before(&self, cutoff: DateTime<Utc>, dry_run: bool) -> AuditPurgeFuture<'_> {
        let _ = (cutoff, dry_run);
        Box::pin(async { Ok(AuditPurgeOutcome::unsupported()) })
    }
}

/// Shared audit writer that fans out to multiple sinks.
#[derive(Clone, Default)]
pub struct AuditLogger {
    sinks: Vec<Arc<dyn AuditSink>>,
}

impl AuditLogger {
    /// Create an empty logger with no sinks.
    #[must_use]
    pub const fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    /// Register an audit sink.
    #[must_use]
    pub fn with_sink(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.sinks.push(sink);
        self
    }

    /// Append an event to all configured sinks.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError`] when one or more configured sinks fail. All
    /// sinks are still attempted; failures are aggregated into one error.
    pub async fn write(&self, event: AuditEvent) -> Result<(), AuditError> {
        let mut errors = Vec::new();
        for sink in &self.sinks {
            if let Err(error) = sink.write(event.clone()).await {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            let mut details = String::with_capacity(errors.len() * 64);
            let mut first = true;
            for error in &errors {
                if !first {
                    details.push_str(" | ");
                }
                first = false;
                details.push_str(error.message());
            }
            Err(AuditError::new(format!(
                "{} audit sink(s) failed: {details}",
                errors.len()
            )))
        }
    }

    /// Returns true when at least one sink is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.sinks.is_empty()
    }

    /// Purge every sink of entries older than `cutoff`, summing what each one
    /// removed (issue #1605).
    ///
    /// Infallible by design, unlike [`Self::write`]: a purge that partly
    /// succeeded has *already deleted rows*, and returning only an error
    /// would throw that count away — the retention report would then record
    /// `rows_removed = 0` for entries that are genuinely gone, which is
    /// exactly the kind of understatement a compliance trail must not
    /// contain. The summary therefore carries both what was removed and what
    /// failed, and the caller decides how to present them.
    ///
    /// `supported` is `true` when *at least one* sink could purge; a logger
    /// whose sinks all forward elsewhere reports unsupported so the retention
    /// report can say so instead of implying an empty archive.
    ///
    /// Every sink is attempted even after one fails, matching [`Self::write`].
    pub async fn purge_before(&self, cutoff: DateTime<Utc>, dry_run: bool) -> AuditPurgeSummary {
        let mut summary = AuditPurgeSummary::default();
        for sink in &self.sinks {
            match sink.purge_before(cutoff, dry_run).await {
                Ok(outcome) => {
                    summary.entries_removed = summary
                        .entries_removed
                        .saturating_add(outcome.entries_removed);
                    summary.supported |= outcome.supported;
                }
                Err(error) => summary.errors.push(error.message().to_owned()),
            }
        }
        summary
    }
}

/// Tracing-based sink that emits JSON fields on a dedicated `autumn.audit` target.
#[derive(Debug, Default)]
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn write(&self, event: AuditEvent) -> AuditWriteFuture<'_> {
        Box::pin(async move {
            tracing::info!(
                target: "autumn.audit",
                timestamp = %event.timestamp,
                actor_id = %event.actor_id,
                action = %event.action,
                target_resource_id = %event.target_resource_id,
                ip_address = ?event.ip_address,
                status = ?event.status,
                "audit_event"
            );
            Ok(())
        })
    }
}

/// JSON-lines file sink suitable for immutable append-only audit archives.
#[derive(Debug)]
pub struct JsonlFileAuditSink {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlFileAuditSink {
    /// Create a JSONL sink writing to `path` in append-only mode.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            write_lock: Mutex::new(()),
        }
    }
}

impl AuditSink for JsonlFileAuditSink {
    fn write(&self, event: AuditEvent) -> AuditWriteFuture<'_> {
        Box::pin(async move {
            let mut encoded = serde_json::to_vec(&event).map_err(|error| {
                AuditError::new(format!("failed to encode audit event: {error}"))
            })?;
            encoded.push(b'\n');
            let _guard = self.write_lock.lock().await;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
                .map_err(|error| {
                    AuditError::new(format!("failed to open audit log file: {error}"))
                })?;
            file.write_all(&encoded).await.map_err(|error| {
                AuditError::new(format!("failed to write audit event: {error}"))
            })?;
            file.sync_data()
                .await
                .map_err(|error| AuditError::new(format!("failed to sync audit file: {error}")))?;
            Ok(())
        })
    }

    /// Rewrite the archive without entries older than `cutoff`.
    ///
    /// Reads the archive, filters it into a sibling temp file, `fsync`s that
    /// file, then atomically renames it over the archive — so a crash
    /// mid-purge leaves the original intact rather than a truncated archive.
    /// The temp file inherits the archive's permissions before the rename, so
    /// a hardened archive (`chmod 600` on a file carrying `actor_id` and
    /// `ip_address`) does not come back world-readable.
    ///
    /// A line that does not decode as an [`AuditEvent`] is **kept**: a
    /// retention sweep must never be the thing that silently discards a
    /// record it merely failed to parse (a future schema, or a partial line
    /// left by an earlier crash).
    ///
    /// # Concurrency and memory limits
    ///
    /// The sink's write lock serializes this against [`AuditSink::write`]
    /// **within one process only**. It is not a file lock: a second process
    /// appending to the same path (notably `autumn db retention --purge`,
    /// which boots a second copy of the app, run against a live server) can
    /// append between this read and this rename, and those events are lost.
    /// Prefer the in-process scheduled sweep for a live deployment; see
    /// `docs/guide/data-retention.md`.
    ///
    /// The archive is read into memory in full, and the kept lines are
    /// buffered alongside it — peak usage is roughly twice the archive size.
    /// Keep the JSONL archive rotated by external tooling if it can grow to a
    /// size that matters on your host.
    fn purge_before(&self, cutoff: DateTime<Utc>, dry_run: bool) -> AuditPurgeFuture<'_> {
        Box::pin(async move {
            let _guard = self.write_lock.lock().await;
            let existing = match tokio::fs::read_to_string(&self.path).await {
                Ok(contents) => contents,
                // No archive yet is a successful no-op, not a failure: the
                // sink creates the file lazily on first write.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(AuditPurgeOutcome::purged(0));
                }
                Err(error) => {
                    return Err(AuditError::new(format!(
                        "failed to read audit log file for purge: {error}"
                    )));
                }
            };

            let mut kept = String::with_capacity(existing.len());
            let mut removed: u64 = 0;
            for line in existing.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let stale = serde_json::from_str::<AuditEvent>(line)
                    .is_ok_and(|event| event.timestamp < cutoff);
                if stale {
                    removed = removed.saturating_add(1);
                } else {
                    kept.push_str(line);
                    kept.push('\n');
                }
            }

            if dry_run || removed == 0 {
                return Ok(AuditPurgeOutcome::purged(removed));
            }

            // A unique temp name per process and per call. A fixed one (e.g.
            // `with_extension("…tmp")`) collides between two processes purging
            // the same archive — and, because `with_extension` replaces the
            // extension, between `audit.log` and `audit.jsonl` in one
            // directory — and `File::create` truncates, so the loser's
            // partial write gets renamed over the archive.
            let temp_path = self.path.with_file_name(format!(
                "{}.{}.{}.autumn-retention.tmp",
                self.path.file_name().map_or_else(
                    || "audit".to_owned(),
                    |name| name.to_string_lossy().into_owned()
                ),
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_nanos()),
            ));
            let mut temp = tokio::fs::File::create(&temp_path).await.map_err(|error| {
                AuditError::new(format!("failed to create audit purge temp file: {error}"))
            })?;
            // Carry the archive's own permissions across the rename. Best
            // effort: a platform or filesystem that cannot report or apply
            // them must not fail the purge.
            if let Ok(metadata) = tokio::fs::metadata(&self.path).await {
                let _ = tokio::fs::set_permissions(&temp_path, metadata.permissions()).await;
            }
            temp.write_all(kept.as_bytes()).await.map_err(|error| {
                AuditError::new(format!("failed to write audit purge temp file: {error}"))
            })?;
            temp.sync_data().await.map_err(|error| {
                AuditError::new(format!("failed to sync audit purge temp file: {error}"))
            })?;
            drop(temp);
            if let Err(error) = tokio::fs::rename(&temp_path, &self.path).await {
                // Leaving the temp file behind would accumulate one orphan per
                // failed sweep in the archive's own directory.
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(AuditError::new(format!(
                    "failed to replace audit log file: {error}"
                )));
            }
            Ok(AuditPurgeOutcome::purged(removed))
        })
    }
}

/// A sink that broadcasts audit events to a WebSocket/SSE channel.
///
/// Available when the `ws` feature is enabled. Useful for real-time admin dashboards.
#[cfg(feature = "ws")]
#[derive(Clone)]
pub struct ChannelAuditSink {
    sender: crate::channels::Sender,
}

#[cfg(feature = "ws")]
impl ChannelAuditSink {
    /// Create a new channel sink targeting the provided sender.
    #[must_use]
    pub const fn new(sender: crate::channels::Sender) -> Self {
        Self { sender }
    }
}

#[cfg(feature = "ws")]
impl AuditSink for ChannelAuditSink {
    fn write(&self, event: AuditEvent) -> AuditWriteFuture<'_> {
        let sender = self.sender.clone();
        Box::pin(async move {
            let json = serde_json::to_string(&event).map_err(|e| {
                AuditError::new(format!("failed to serialize audit event for channel: {e}"))
            })?;
            // Ignore send errors -- they just mean no one is currently subscribed to the channel.
            let _ = sender.send(json);
            Ok(())
        })
    }
}

/// Helper to write an audit event using the logger stored in [`AppState`].
///
/// # Errors
///
/// Returns [`AuditError`] when the installed logger fails to persist to one or
/// more sinks. If no logger is installed in state, this is a no-op and returns
/// `Ok(())`.
pub async fn write_from_state(state: &AppState, event: AuditEvent) -> Result<(), AuditError> {
    if let Some(logger) = state.extension::<AuditLogger>() {
        logger.write(event).await
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FailingSink;

    impl AuditSink for FailingSink {
        fn write(&self, _event: AuditEvent) -> AuditWriteFuture<'_> {
            Box::pin(async { Err(AuditError::new("boom")) })
        }
    }

    struct CountingSink {
        writes: Arc<AtomicUsize>,
    }

    impl AuditSink for CountingSink {
        fn write(&self, _event: AuditEvent) -> AuditWriteFuture<'_> {
            let writes = self.writes.clone();
            Box::pin(async move {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    // ── Retention purge (issue #1605) ─────────────────────────────────────

    /// Build a JSONL archive whose lines carry the given timestamps.
    async fn archive_with_timestamps(
        path: &std::path::Path,
        timestamps: &[DateTime<Utc>],
    ) -> JsonlFileAuditSink {
        let sink = JsonlFileAuditSink::new(path);
        for (index, timestamp) in timestamps.iter().enumerate() {
            let mut event = AuditEvent::new(
                format!("actor-{index}"),
                "auth.login",
                format!("session-{index}"),
                None,
                AuditStatus::Success,
            );
            event.timestamp = *timestamp;
            sink.write(event).await.expect("write archive line");
        }
        sink
    }

    #[test]
    fn audit_event_metadata_defaults_to_empty() {
        let event = AuditEvent::new("u1", "auth.login", "s1", None, AuditStatus::Success);
        assert!(
            event.metadata.is_empty(),
            "an ordinary audit event carries no metadata"
        );
    }

    #[test]
    fn audit_event_with_metadata_round_trips_through_json() {
        let event = AuditEvent::new(
            "autumn:retention",
            "retention.sweep",
            "job_history",
            None,
            AuditStatus::Success,
        )
        .with_metadata("dataset", "job_history")
        .with_metadata("cutoff", "2026-01-01T00:00:00Z")
        .with_metadata("rows_removed", "12");

        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: AuditEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
        assert_eq!(
            decoded.metadata.get("rows_removed").map(String::as_str),
            Some("12")
        );
    }

    #[test]
    fn audit_event_without_metadata_field_still_deserializes() {
        // Archives written before #1605 have no `metadata` key; reading one
        // back (for a purge, or by an operator's tooling) must still work.
        let legacy = r#"{"timestamp":"2026-01-01T00:00:00Z","actor_id":"u1",
            "action":"auth.login","target_resource_id":"s1","ip_address":null,
            "status":"success"}"#;
        let decoded: AuditEvent = serde_json::from_str(legacy).expect("legacy line must decode");
        assert!(decoded.metadata.is_empty());
    }

    #[tokio::test]
    async fn jsonl_sink_purge_removes_only_entries_older_than_the_cutoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("audit.log");
        let old = Utc::now() - chrono::Duration::days(400);
        let recent = Utc::now() - chrono::Duration::days(1);
        let sink = archive_with_timestamps(&path, &[old, recent, old]).await;

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let outcome = sink.purge_before(cutoff, false).await.expect("purge");

        assert!(outcome.supported);
        assert_eq!(outcome.entries_removed, 2);
        let content = tokio::fs::read_to_string(&path).await.expect("read back");
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "only the recent entry survives: {content}");
        assert!(lines[0].contains("session-1"), "{content}");
    }

    #[tokio::test]
    async fn jsonl_sink_purge_dry_run_counts_without_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("audit.log");
        let old = Utc::now() - chrono::Duration::days(400);
        let sink = archive_with_timestamps(&path, &[old, old]).await;
        let before = tokio::fs::read_to_string(&path).await.expect("read");

        let outcome = sink
            .purge_before(Utc::now(), true)
            .await
            .expect("dry-run purge");

        assert_eq!(outcome.entries_removed, 2);
        let after = tokio::fs::read_to_string(&path).await.expect("read");
        assert_eq!(before, after, "a dry run must not touch the archive");
    }

    #[tokio::test]
    async fn jsonl_sink_purge_keeps_unparseable_lines() {
        // Fail-safe: a line this build cannot decode (a future schema, a
        // partially-written line after a crash) is kept, never silently
        // discarded by a retention sweep.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("audit.log");
        let old = Utc::now() - chrono::Duration::days(400);
        let sink = archive_with_timestamps(&path, &[old]).await;
        tokio::fs::write(
            &path,
            format!(
                "{}not json at all
",
                tokio::fs::read_to_string(&path).await.unwrap()
            ),
        )
        .await
        .expect("append junk");

        let outcome = sink.purge_before(Utc::now(), false).await.expect("purge");

        assert_eq!(outcome.entries_removed, 1);
        let content = tokio::fs::read_to_string(&path).await.expect("read back");
        assert!(
            content.contains("not json at all"),
            "unparseable lines must survive: {content}"
        );
    }

    #[tokio::test]
    async fn jsonl_sink_purge_on_a_missing_archive_is_a_no_op() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = JsonlFileAuditSink::new(tmp.path().join("never-written.log"));

        let outcome = sink.purge_before(Utc::now(), false).await.expect("purge");

        assert!(outcome.supported);
        assert_eq!(outcome.entries_removed, 0);
    }

    #[tokio::test]
    async fn a_sink_that_cannot_purge_reports_unsupported() {
        let outcome = TracingAuditSink
            .purge_before(Utc::now(), false)
            .await
            .expect("default purge");
        assert!(!outcome.supported);
        assert_eq!(outcome.entries_removed, 0);
    }

    #[tokio::test]
    async fn logger_purge_sums_across_sinks_and_reports_support() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = Utc::now() - chrono::Duration::days(400);
        let first = tmp.path().join("a.log");
        let second = tmp.path().join("b.log");
        archive_with_timestamps(&first, &[old, old]).await;
        archive_with_timestamps(&second, &[old]).await;

        let logger = AuditLogger::new()
            .with_sink(Arc::new(TracingAuditSink))
            .with_sink(Arc::new(JsonlFileAuditSink::new(&first)))
            .with_sink(Arc::new(JsonlFileAuditSink::new(&second)));

        let summary = logger.purge_before(Utc::now(), false).await;

        assert_eq!(summary.entries_removed, 3);
        assert!(
            summary.supported,
            "at least one sink supports purging, so the logger reports support"
        );
        assert!(summary.errors.is_empty());
        assert_eq!(summary.error_message(), None);
    }

    #[tokio::test]
    async fn logger_purge_with_no_purgeable_sinks_reports_unsupported() {
        let logger = AuditLogger::new().with_sink(Arc::new(TracingAuditSink));
        let summary = logger.purge_before(Utc::now(), false).await;
        assert!(!summary.supported);
        assert_eq!(summary.entries_removed, 0);
        assert!(summary.errors.is_empty());
    }

    /// A sink that always fails to purge (but accepts writes).
    struct UnpurgeableSink;

    impl AuditSink for UnpurgeableSink {
        fn write(&self, _event: AuditEvent) -> AuditWriteFuture<'_> {
            Box::pin(async { Ok(()) })
        }

        fn purge_before(&self, _cutoff: DateTime<Utc>, _dry_run: bool) -> AuditPurgeFuture<'_> {
            Box::pin(async { Err(AuditError::new("sink offline")) })
        }
    }

    #[tokio::test]
    async fn logger_purge_keeps_what_succeeded_when_another_sink_fails() {
        // A partial purge has already deleted those entries. Throwing the
        // count away with the error would make the retention report record
        // `rows_removed = 0` for entries that are genuinely gone — an
        // understatement a compliance trail must not contain.
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = Utc::now() - chrono::Duration::days(400);
        let path = tmp.path().join("a.log");
        archive_with_timestamps(&path, &[old, old]).await;

        let logger = AuditLogger::new()
            .with_sink(Arc::new(JsonlFileAuditSink::new(&path)))
            .with_sink(Arc::new(UnpurgeableSink));

        let summary = logger.purge_before(Utc::now(), false).await;

        assert_eq!(
            summary.entries_removed, 2,
            "the successful sink's removals must survive the aggregated failure"
        );
        assert!(summary.supported);
        assert_eq!(summary.errors.len(), 1);
        let message = summary.error_message().expect("a failure is reported");
        assert!(message.contains("sink offline"), "{message}");
        assert!(message.contains("1 audit sink(s) failed"), "{message}");
    }

    #[tokio::test]
    async fn jsonl_sink_appends_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("audit.log");
        let sink = JsonlFileAuditSink::new(&path);

        sink.write(AuditEvent::new(
            "admin-1",
            "user.role.update",
            "user-99",
            None,
            AuditStatus::Success,
        ))
        .await
        .expect("write first event");

        sink.write(AuditEvent::new(
            "api-key-1",
            "export.create",
            "export-42",
            None,
            AuditStatus::Failure,
        ))
        .await
        .expect("write second event");

        let content = tokio::fs::read_to_string(&path)
            .await
            .expect("read audit file");
        let line_count = content.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(line_count, 2, "content:\n{content}");
    }

    #[tokio::test]
    async fn write_from_state_no_logger_is_noop() {
        let state = AppState::for_test();
        write_from_state(
            &state,
            AuditEvent::new("u1", "auth.login", "session-1", None, AuditStatus::Success),
        )
        .await
        .expect("no-op write should succeed");
    }

    #[tokio::test]
    async fn audit_logger_continues_fan_out_after_sink_failure() {
        let writes = Arc::new(AtomicUsize::new(0));
        let logger = AuditLogger::new()
            .with_sink(Arc::new(FailingSink))
            .with_sink(Arc::new(CountingSink {
                writes: writes.clone(),
            }));

        let error = logger
            .write(AuditEvent::new(
                "u1",
                "auth.login",
                "session-1",
                None,
                AuditStatus::Failure,
            ))
            .await
            .expect_err("first sink should fail");

        assert!(
            error.to_string().contains("1 audit sink(s) failed"),
            "unexpected error: {error}"
        );
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "second sink should still receive event"
        );
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn channel_sink_broadcasts_events() {
        let channels = crate::channels::Channels::new(16);
        let sender = channels.sender("audit-events");
        let mut rx = channels.subscribe("audit-events");

        let sink = ChannelAuditSink::new(sender);
        let event = AuditEvent::new("admin", "test.action", "target", None, AuditStatus::Success);

        sink.write(event.clone()).await.expect("channel sink write");

        let msg = rx.recv().await.expect("should receive message");
        let received_event: AuditEvent = serde_json::from_str(msg.as_str()).expect("valid json");
        assert_eq!(received_event.action, "test.action");
    }
}
