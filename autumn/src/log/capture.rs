//! In-memory log capture layer and bounded ring buffer.
//!
//! When `log.capture.enabled = true`, a [`LogCaptureLayer`] is installed
//! alongside the rest of the tracing subscriber stack.  It writes every
//! `tracing` event into a [`LogBuffer`] — a bounded ring-buffer that evicts
//! the oldest entry when capacity is reached.  The buffer is exposed by the
//! `/actuator/logfile` endpoint so recent structured log entries are visible
//! over HTTP without SSH access or an external aggregator.
//!
//! Sensitive field values are scrubbed using the same [`ParameterFilter`] that
//! guards the rest of the logging pipeline.  The `request_id` field is read
//! from the current [`crate::log::context`] task-local, tying log entries to
//! the request that produced them.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::log::filter::{FILTERED_PLACEHOLDER, ParameterFilter};

// ── Config ─────────────────────────────────────────────────────

/// Configuration for the in-memory log capture buffer.
///
/// Nested under `[log.capture]` in `autumn.toml` or via
/// `AUTUMN_LOG__CAPTURE__*` environment variables.
///
/// # Examples
///
/// ```rust
/// use autumn_web::log::capture::LogCaptureConfig;
///
/// let cfg = LogCaptureConfig::default();
/// assert!(!cfg.enabled);
/// assert_eq!(cfg.capacity, 1000);
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct LogCaptureConfig {
    /// Enable the in-memory capture buffer.  Default: `false`.
    ///
    /// When disabled, no capture layer is installed and the buffer is never
    /// populated.  The `/actuator/logfile` endpoint returns an empty list.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum number of entries to retain.  Default: `1000`.
    ///
    /// Once capacity is reached the oldest entry is evicted to make room for
    /// the new one — the buffer never grows beyond this size.
    #[serde(default = "default_capacity")]
    pub capacity: usize,
}

const fn default_capacity() -> usize {
    1000
}

impl Default for LogCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capacity: default_capacity(),
        }
    }
}

// ── CapturedLogEntry ──────────────────────────────────────────

/// A single captured tracing event stored in the [`LogBuffer`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CapturedLogEntry {
    /// ISO 8601 timestamp with millisecond precision (`2024-01-15T12:34:56.789Z`).
    pub timestamp: String,
    /// Tracing level: `"TRACE"`, `"DEBUG"`, `"INFO"`, `"WARN"`, or `"ERROR"`.
    pub level: String,
    /// The `tracing` target (typically the module path, e.g. `"myapp::orders"`).
    pub target: String,
    /// The event message (the first positional argument to the macro).
    pub message: String,
    /// Structured key-value fields attached to the event.
    ///
    /// Values whose key is on the sensitive-key deny-list are replaced with
    /// `"[FILTERED]"` before storage.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Request correlation id, when the event was emitted inside a request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

// ── LogBuffer ─────────────────────────────────────────────────

struct LogBufferInner {
    capacity: usize,
    entries: VecDeque<CapturedLogEntry>,
}

/// Thread-safe bounded ring-buffer of recent log entries.
///
/// [`LogBuffer`] is `Clone`: clones share the same underlying storage via
/// `Arc`, so both the capture layer and the actuator endpoint refer to the
/// same buffer.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<std::sync::Mutex<LogBufferInner>>,
    filter: Arc<ParameterFilter>,
}

impl std::fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.len();
        f.debug_struct("LogBuffer").field("len", &len).finish()
    }
}

impl LogBuffer {
    /// Create a new buffer with the given capacity and sensitive-key filter.
    #[must_use]
    pub fn new(capacity: usize, filter: ParameterFilter) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(LogBufferInner {
                capacity,
                entries: VecDeque::with_capacity(capacity.min(1024)),
            })),
            filter: Arc::new(filter),
        }
    }

    /// Push a new entry, evicting the oldest if at capacity.
    pub fn push(&self, entry: CapturedLogEntry) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.capacity > 0 && guard.entries.len() >= guard.capacity {
            guard.entries.pop_front();
        }
        if guard.capacity > 0 {
            guard.entries.push_back(entry);
        }
    }

    /// Snapshot recent entries, optionally filtered by minimum level and/or count.
    ///
    /// `min_level` keeps entries whose severity is ≥ the given level (e.g.
    /// `Level::WARN` keeps only WARN and ERROR entries).  `limit` caps the
    /// result to the *N* most-recent matching entries.  The returned slice is
    /// always in chronological order (oldest first).
    #[must_use]
    pub fn snapshot(
        &self,
        min_level: Option<Level>,
        limit: Option<usize>,
    ) -> Vec<CapturedLogEntry> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let iter = guard.entries.iter().filter(|e| {
            min_level.is_none_or(|filter| level_from_str(&e.level).is_some_and(|lvl| lvl <= filter))
        });

        if let Some(n) = limit {
            // Take the last N matching entries (newest-last in the original order).
            let mut result: Vec<_> = iter.rev().take(n).cloned().collect();
            drop(guard);
            result.reverse();
            result
        } else {
            let result = iter.cloned().collect();
            drop(guard);
            result
        }
    }

    /// Number of entries currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }

    /// `true` when no entries are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Expose the shared parameter filter for field scrubbing.
    pub(crate) fn filter(&self) -> &ParameterFilter {
        &self.filter
    }
}

// ── Level helpers ──────────────────────────────────────────────

/// Parse a level string (case-insensitive) into a `tracing::Level`.
///
/// Returns `None` for unrecognised strings so callers can handle gracefully.
#[must_use]
pub fn level_from_str(s: &str) -> Option<Level> {
    match s {
        "ERROR" | "error" => Some(Level::ERROR),
        "WARN" | "warn" => Some(Level::WARN),
        "INFO" | "info" => Some(Level::INFO),
        "DEBUG" | "debug" => Some(Level::DEBUG),
        "TRACE" | "trace" => Some(Level::TRACE),
        _ => {
            if s.eq_ignore_ascii_case("ERROR") {
                Some(Level::ERROR)
            } else if s.eq_ignore_ascii_case("WARN") {
                Some(Level::WARN)
            } else if s.eq_ignore_ascii_case("INFO") {
                Some(Level::INFO)
            } else if s.eq_ignore_ascii_case("DEBUG") {
                Some(Level::DEBUG)
            } else if s.eq_ignore_ascii_case("TRACE") {
                Some(Level::TRACE)
            } else {
                None
            }
        }
    }
}

// ── LogCaptureLayer ───────────────────────────────────────────

/// `tracing_subscriber` layer that captures events into a [`LogBuffer`].
///
/// Install this via [`crate::telemetry::init`] by enabling
/// `log.capture.enabled`.  It sits in the subscriber stack alongside the
/// existing stdout/JSON and OTLP layers and does not affect their output.
#[derive(Clone)]
pub struct LogCaptureLayer {
    buffer: LogBuffer,
}

impl LogCaptureLayer {
    /// Wrap `buffer` in a new capture layer.
    #[must_use]
    pub const fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }

    /// Return the underlying buffer (for wiring into `AppState`).
    #[must_use]
    pub const fn buffer(&self) -> &LogBuffer {
        &self.buffer
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for LogCaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor {
            message: None,
            fields: serde_json::Map::new(),
        };
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_default();
        let level = event.metadata().level().as_str().to_owned();
        let target = event.metadata().target().to_owned();
        let timestamp = crate::time::ambient_now().to_rfc3339_opts(SecondsFormat::Millis, true);

        // Scrub sensitive field values in-place to avoid re-allocating the map.
        let filter = self.buffer.filter();
        let mut fields = visitor.fields;
        for (k, v) in &mut fields {
            if filter.matches_key(k) {
                *v = serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned());
            }
        }

        // Pull full request context (request_id, user_id, tenant_id, custom fields)
        // from the task-local log context.  Event-level fields take priority;
        // context fields are only inserted when the key does not already exist.
        // All values are run through the same sensitive-key filter.
        let request_id;
        if let Some(ctx) = crate::log::context::snapshot() {
            request_id = ctx.request_id;
            if let Some(uid) = ctx.user_id {
                let val = if filter.matches_key("user_id") {
                    serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned())
                } else {
                    serde_json::Value::String(uid)
                };
                fields.entry("user_id".to_owned()).or_insert(val);
            }
            if let Some(tid) = ctx.tenant_id {
                let val = if filter.matches_key("tenant_id") {
                    serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned())
                } else {
                    serde_json::Value::String(tid)
                };
                fields.entry("tenant_id".to_owned()).or_insert(val);
            }
            for (k, v) in ctx.fields {
                let val = if filter.matches_key(&k) {
                    serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned())
                } else {
                    serde_json::Value::String(v)
                };
                fields.entry(k).or_insert(val);
            }
        } else {
            request_id = None;
        }

        let entry = CapturedLogEntry {
            timestamp,
            level,
            target,
            message,
            fields,
            request_id,
        };

        self.buffer.push(entry);
    }
}

// ── Field visitor ─────────────────────────────────────────────

struct FieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields.insert(
                field.name().to_owned(),
                serde_json::Value::String(value.to_owned()),
            );
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(
            field.name().to_owned(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if let Some(n) = serde_json::Number::from_u128(u128::from(value)) {
            self.fields
                .insert(field.name().to_owned(), serde_json::Value::Number(n));
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields
                .insert(field.name().to_owned(), serde_json::Value::Number(n));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), serde_json::Value::Bool(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(s);
        } else {
            self.fields
                .insert(field.name().to_owned(), serde_json::Value::String(s));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(level: &str, msg: &str) -> CapturedLogEntry {
        CapturedLogEntry {
            timestamp: "2024-01-01T00:00:00.000Z".to_owned(),
            level: level.to_owned(),
            target: "test".to_owned(),
            message: msg.to_owned(),
            fields: serde_json::Map::new(),
            request_id: None,
        }
    }

    // ── RED: LogBuffer bounded capacity ──────────────────────

    #[test]
    fn red_buffer_evicts_oldest_at_capacity() {
        let buf = LogBuffer::new(3, ParameterFilter::default());
        buf.push(make_entry("INFO", "first"));
        buf.push(make_entry("INFO", "second"));
        buf.push(make_entry("INFO", "third"));
        buf.push(make_entry("INFO", "fourth")); // should evict "first"

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "second");
        assert_eq!(snap[2].message, "fourth");
    }

    #[test]
    fn red_buffer_zero_capacity_stores_nothing() {
        let buf = LogBuffer::new(0, ParameterFilter::default());
        buf.push(make_entry("INFO", "msg"));
        assert_eq!(buf.len(), 0);
        assert!(buf.snapshot(None, None).is_empty());
    }

    #[test]
    fn red_buffer_snapshot_respects_limit() {
        let buf = LogBuffer::new(100, ParameterFilter::default());
        for i in 0..10u32 {
            buf.push(make_entry("INFO", &format!("msg-{i}")));
        }
        let snap = buf.snapshot(None, Some(3));
        assert_eq!(snap.len(), 3);
        // limit takes the most recent N entries in chronological order
        assert_eq!(snap[0].message, "msg-7");
        assert_eq!(snap[2].message, "msg-9");
    }

    #[test]
    fn red_buffer_snapshot_level_filter_excludes_debug_when_min_info() {
        let buf = LogBuffer::new(100, ParameterFilter::default());
        buf.push(make_entry("DEBUG", "debug-msg"));
        buf.push(make_entry("INFO", "info-msg"));
        buf.push(make_entry("WARN", "warn-msg"));

        let snap = buf.snapshot(Some(Level::INFO), None);
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|e| e.level != "DEBUG"));
    }

    #[test]
    fn red_buffer_snapshot_level_filter_error_only() {
        let buf = LogBuffer::new(100, ParameterFilter::default());
        buf.push(make_entry("INFO", "info"));
        buf.push(make_entry("WARN", "warn"));
        buf.push(make_entry("ERROR", "error"));

        let snap = buf.snapshot(Some(Level::ERROR), None);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].level, "ERROR");
    }

    #[test]
    fn red_buffer_snapshot_no_filter_returns_all() {
        let buf = LogBuffer::new(100, ParameterFilter::default());
        buf.push(make_entry("TRACE", "t"));
        buf.push(make_entry("ERROR", "e"));
        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 2);
    }

    // ── RED: LogBuffer sensitive field scrubbing ──────────────

    #[test]
    fn red_buffer_push_does_not_scrub_fields_directly() {
        // Scrubbing happens in LogCaptureLayer, not in push; push is raw.
        let buf = LogBuffer::new(10, ParameterFilter::default());
        let mut entry = make_entry("INFO", "login");
        entry.fields.insert(
            "password".to_owned(),
            serde_json::Value::String("hunter2".to_owned()),
        );
        buf.push(entry.clone());
        let snap = buf.snapshot(None, None);
        // push stores whatever it's given (scrubbing is the layer's job)
        assert_eq!(snap[0].fields["password"], "hunter2");
    }

    // ── RED: LogBuffer clone shares storage ────────────────────

    #[test]
    fn red_buffer_clone_shares_storage() {
        let buf = LogBuffer::new(10, ParameterFilter::default());
        let clone = buf.clone();
        buf.push(make_entry("INFO", "shared"));
        assert_eq!(clone.len(), 1);
    }

    // ── RED: level_from_str ────────────────────────────────────

    #[test]
    fn red_level_from_str_parses_case_insensitive() {
        assert_eq!(level_from_str("error"), Some(Level::ERROR));
        assert_eq!(level_from_str("WARN"), Some(Level::WARN));
        assert_eq!(level_from_str("Info"), Some(Level::INFO));
        assert_eq!(level_from_str("debug"), Some(Level::DEBUG));
        assert_eq!(level_from_str("TRACE"), Some(Level::TRACE));
        assert_eq!(level_from_str("bogus"), None);
    }

    // ── RED: LogCaptureConfig defaults ────────────────────────

    #[test]
    fn red_capture_config_default_is_disabled_with_1000_capacity() {
        let cfg = LogCaptureConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.capacity, 1000);
    }

    // ── RED: LogBuffer snapshot newest-last ordering ──────────

    #[test]
    fn red_snapshot_returns_entries_in_insertion_order() {
        let buf = LogBuffer::new(10, ParameterFilter::default());
        buf.push(make_entry("INFO", "a"));
        buf.push(make_entry("INFO", "b"));
        buf.push(make_entry("INFO", "c"));

        let snap = buf.snapshot(None, None);
        assert_eq!(snap[0].message, "a");
        assert_eq!(snap[1].message, "b");
        assert_eq!(snap[2].message, "c");
    }

    // ── GREEN: LogCaptureLayer via tracing subscriber ─────────

    #[tokio::test]
    async fn green_layer_captures_event_with_structured_fields_and_scrubs_sensitive_keys() {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());

        // Install into a *dispatch* (not global) so the test doesn't fight
        // with other tests for the global subscriber slot.
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        tracing::info!(order_id = "A-1001", password = "hunter2", "order placed");

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.message, "order placed");
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.fields["order_id"].as_str().unwrap(), "A-1001");
        // sensitive key scrubbed
        assert_eq!(
            entry.fields["password"].as_str().unwrap(),
            crate::log::filter::FILTERED_PLACEHOLDER
        );
    }

    #[tokio::test]
    async fn green_layer_captures_multiple_levels() {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        tracing::warn!("something went wrong");
        tracing::error!("fatal error");

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].level, "WARN");
        assert_eq!(snap[1].level, "ERROR");
    }

    #[tokio::test]
    async fn green_layer_is_additive_does_not_affect_other_layers() {
        // This test verifies the layer is truly additive by ensuring the
        // buffer receives events even when multiple layers are stacked.
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let capture = LogCaptureLayer::new(buf.clone());

        // Stack capture + a no-op fmt layer (simulating the existing pipeline).
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink))
            .with(capture);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        tracing::info!("additive test");

        // Both the fmt layer and the capture layer ran; capture has the entry.
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.snapshot(None, None)[0].message, "additive test");
    }

    // ── GREEN: LogContext fields captured in on_event ─────────

    #[tokio::test]
    async fn green_layer_captures_request_context_user_tenant_and_custom_fields() {
        use crate::log::context::{LogContext, scope};
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        let ctx = LogContext::new(Some("req-context-test".to_owned()));
        ctx.set_user_id("user-42");
        ctx.set_tenant_id("tenant-99");
        ctx.insert_field("region", "eu-west-1");

        scope(ctx, async {
            tracing::info!("context fields test");
        })
        .await;

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.request_id.as_deref(), Some("req-context-test"));
        assert_eq!(entry.fields["user_id"].as_str().unwrap(), "user-42");
        assert_eq!(entry.fields["tenant_id"].as_str().unwrap(), "tenant-99");
        assert_eq!(entry.fields["region"].as_str().unwrap(), "eu-west-1");
    }

    #[tokio::test]
    async fn green_layer_event_field_takes_priority_over_context_field() {
        use crate::log::context::{LogContext, scope};
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        let ctx = LogContext::new(None);
        ctx.set_user_id("context-user");

        scope(ctx, async {
            // Event-level field takes priority over the context field.
            tracing::info!(user_id = "event-user", "priority test");
        })
        .await;

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].fields["user_id"].as_str().unwrap(), "event-user");
    }

    #[tokio::test]
    async fn green_layer_no_context_sets_request_id_to_none() {
        // When no LogContext is active, request_id must be None.
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        tracing::info!("no context");

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].request_id.is_none());
    }

    // ── GREEN: FieldVisitor numeric and bool types ─────────────

    #[tokio::test]
    async fn green_layer_field_visitor_records_numeric_and_bool_types() {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = LogBuffer::new(10, ParameterFilter::default());
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        tracing::info!(
            count = 42i64,
            size = 100u64,
            ratio = 0.5f64,
            active = true,
            "typed fields"
        );

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.fields["count"].as_i64().unwrap(), 42);
        assert_eq!(entry.fields["size"].as_u64().unwrap(), 100);
        assert!((entry.fields["ratio"].as_f64().unwrap() - 0.5).abs() < f64::EPSILON);
        assert!(entry.fields["active"].as_bool().unwrap());
    }

    // ── GREEN: level_from_str mixed-case fallback paths ────────

    #[test]
    fn green_level_from_str_mixed_case_hits_fallback_branches() {
        // "Error", "Warn", "Debug", "Trace" are not in the fast-path arms;
        // they fall through to the eq_ignore_ascii_case branches.
        assert_eq!(level_from_str("Error"), Some(Level::ERROR));
        assert_eq!(level_from_str("Warn"), Some(Level::WARN));
        assert_eq!(level_from_str("Debug"), Some(Level::DEBUG));
        assert_eq!(level_from_str("Trace"), Some(Level::TRACE));
    }

    // ── GREEN: LogBuffer Debug impl and is_empty ───────────────

    #[test]
    fn green_buffer_debug_format_shows_len() {
        let buf = LogBuffer::new(10, ParameterFilter::default());
        buf.push(make_entry("INFO", "a"));
        let s = format!("{buf:?}");
        assert!(s.contains("len"), "Debug output should contain 'len': {s}");
    }

    #[test]
    fn green_buffer_is_empty_false_when_has_entries() {
        let buf = LogBuffer::new(10, ParameterFilter::default());
        assert!(buf.is_empty());
        buf.push(make_entry("INFO", "a"));
        assert!(!buf.is_empty());
    }

    // ── GREEN: context field scrubbing (filter matches) ────────

    #[tokio::test]
    async fn green_layer_scrubs_sensitive_context_user_id_and_tenant_id() {
        use crate::log::context::{LogContext, scope};
        use tracing_subscriber::layer::SubscriberExt;

        // Build a filter that considers user_id and tenant_id sensitive.
        let filter = ParameterFilter::new(
            &[
                "user_id".to_owned(),
                "tenant_id".to_owned(),
                "region".to_owned(),
            ],
            &[],
        );
        let buf = LogBuffer::new(10, filter);
        let layer = LogCaptureLayer::new(buf.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

        let ctx = LogContext::new(None);
        ctx.set_user_id("secret-user");
        ctx.set_tenant_id("secret-tenant");
        ctx.insert_field("region", "eu-west-1");

        scope(ctx, async {
            tracing::info!("context scrub test");
        })
        .await;

        let snap = buf.snapshot(None, None);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(
            entry.fields["user_id"].as_str().unwrap(),
            FILTERED_PLACEHOLDER,
            "user_id from context must be scrubbed"
        );
        assert_eq!(
            entry.fields["tenant_id"].as_str().unwrap(),
            FILTERED_PLACEHOLDER,
            "tenant_id from context must be scrubbed"
        );
        assert_eq!(
            entry.fields["region"].as_str().unwrap(),
            FILTERED_PLACEHOLDER,
            "custom context field matching filter must be scrubbed"
        );
    }
}
