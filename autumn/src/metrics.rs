//! Call-site facade for app-defined metrics (issue #1378).
//!
//! Recording an application metric should cost one line at the point where the
//! interesting thing happens — no traits to implement, no types to define, no
//! registry to wire into the app builder:
//!
//! ```rust
//! use autumn_web::metrics;
//!
//! metrics::counter("checkout_completed_total")
//!     .with_label("status", "paid")
//!     .increment(1);
//! ```
//!
//! Everything recorded here is exposed automatically by the actuator:
//! `/actuator/prometheus` renders the Prometheus text format and
//! `/actuator/metrics` renders the same data as JSON under a top-level `app`
//! key. Recording always works, and `actuator.prometheus = false` gates only
//! the Prometheus scrape endpoint — the JSON `/actuator/metrics` view still
//! carries the `app` key.
//!
//! ## Which instrument do I want?
//!
//! - [`counter`](crate::metrics::counter) — how many times something happened.
//!   Only goes up, name it `*_total`.
//! - [`gauge`](crate::metrics::gauge) — how many of something there are *right
//!   now*. Goes up and down.
//! - [`histogram`](crate::metrics::histogram) /
//!   [`timer`](crate::metrics::timer) — how long something took (or how big it
//!   was). Timers record seconds, so `histogram_quantile` works out of the box.
//!
//! ## Labels are a closed set
//!
//! Label values must come from a small, fixed set the code controls. Never
//! label with user input, IDs, or anything else unbounded: each distinct
//! combination of label values is a separate time series, and the facade caps
//! an instrument at [`MAX_SERIES_PER_METRIC`](crate::metrics::MAX_SERIES_PER_METRIC)
//! labeled series (samples carrying excess label sets are dropped and counted
//! in [`InstrumentSnapshot::dropped_series`](crate::metrics::InstrumentSnapshot::dropped_series)).
//!
//! ## Reserved names
//!
//! The `autumn_` prefix belongs to the framework's own built-in metric
//! families. Names starting with it — and names that are not valid Prometheus
//! metric names — are rejected with a warning, yielding an inert handle that
//! records nothing rather than panicking.
//!
//! A histogram additionally reserves its derived `_bucket`, `_sum` and
//! `_count` names, and the first registration of a name fixes its kind: asking
//! for the same name as a different kind is rejected the same way, so the
//! scrape output always carries exactly one `# TYPE` line per family.
//!
//! Narrative guide: `docs/guide/metrics.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Maximum number of distinct **labeled** series retained per instrument.
///
/// The unlabeled series — what a handle with no `with_label` call records
/// into — is separate and does not count towards this cap. Once an instrument
/// holds this many labeled series, samples carrying a label set it has not
/// seen before are dropped and counted in
/// [`InstrumentSnapshot::dropped_series`].
pub const MAX_SERIES_PER_METRIC: usize = 100;

/// Maximum number of distinct instruments the process-global registry holds.
pub const MAX_INSTRUMENTS: usize = 256;

/// Maximum number of labels retained on a single series; extras are dropped.
///
/// The retained subset is the one with the lexicographically smallest label
/// names, so which labels survive never depends on the order `with_label` was
/// called in.
pub const MAX_LABELS_PER_SERIES: usize = 8;

/// Maximum length of a label value, **in characters** (not bytes).
///
/// A value at this cap can occupy up to four times as many bytes when it holds
/// non-ASCII text. Longer values are truncated on a character boundary.
pub const MAX_LABEL_VALUE_LEN: usize = 128;

/// Maximum length of a metric name, in bytes.
///
/// Metric names are ASCII by construction (they must match Prometheus' own
/// `[a-zA-Z_][a-zA-Z0-9_]*` grammar to be accepted at all), so bytes and
/// characters coincide. An over-long name is **rejected**, never truncated:
/// truncation would silently merge two distinct metrics into one family.
pub const MAX_METRIC_NAME_LEN: usize = 128;

/// Maximum length of a label name, in bytes.
///
/// ASCII by construction, like [`MAX_METRIC_NAME_LEN`]. An over-long label
/// name drops that label — never the sample, and never by truncation.
pub const MAX_LABEL_NAME_LEN: usize = 128;

/// Maximum length of `# HELP` text, **in characters**; longer text is
/// truncated on a character boundary.
pub const MAX_HELP_LEN: usize = 512;

/// Default histogram bucket upper bounds, in seconds.
///
/// The implicit `+Inf` bucket is always present and is not listed here.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Largest number of bucket upper bounds [`set_histogram_buckets`] accepts.
pub const MAX_BUCKET_BOUNDS: usize = 20;

/// How much of a rejected name a log line reproduces, in characters.
const LOG_NAME_PREVIEW_LEN: usize = 64;

/// Label names Prometheus reserves for its own use; the facade drops them.
const RESERVED_LABEL_NAMES: [&str; 2] = ["le", "quantile"];

/// Suffixes of the families a histogram occupies besides its base name.
///
/// Shared with [`crate::actuator`], which claims the same derived names when it
/// renders a histogram so a plugin source cannot shadow one of them.
pub(crate) const HISTOGRAM_SUFFIXES: [&str; 3] = ["_bucket", "_sum", "_count"];

// ── Registry internals ─────────────────────────────────────────

/// The process-global instrument registry.
///
/// Global rather than per-app so a call site deep in a service module can
/// record without threading app state to it — the same shape as
/// [`crate::cache::read_through_metrics`].
static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::default);

/// Descriptions waiting for the instrument they name to be registered, keyed
/// by metric name and tagged with the kind they were written for.
type PendingHelp = HashMap<Box<str>, (InstrumentKind, Box<str>)>;

/// Every registered instrument, plus the latches that keep warnings from
/// turning into hot-path log floods.
///
/// **Lock order**: the two `pending_*` maps are only ever locked while the
/// `instruments` lock is already held, never the other way round. Resolving
/// them inside the critical section that publishes an instrument is what makes
/// [`describe_counter`] / [`set_histogram_buckets`] free of a
/// check-then-register race with a concurrent first use.
#[derive(Debug, Default)]
struct Registry {
    /// Registered instruments keyed by metric name.
    instruments: RwLock<HashMap<Box<str>, Arc<Instrument>>>,
    /// Bucket bounds configured before their histogram was registered.
    pending_buckets: RwLock<HashMap<Box<str>, Box<[f64]>>>,
    /// Help text recorded by `describe_*` before its instrument was
    /// registered, with the kind the description was written for.
    pending_help: RwLock<PendingHelp>,
    /// Names already warned about, so a rejected name warns exactly once.
    warned_names: RwLock<HashSet<Box<str>>>,
    /// Latches the single "too many instruments" warning.
    over_capacity_warned: AtomicBool,
    /// Latches the single "too many pending bucket overrides" warning.
    pending_buckets_full_warned: AtomicBool,
    /// Latches the single "too many pending descriptions" warning.
    pending_help_full_warned: AtomicBool,
}

impl Registry {
    /// Warn once per metric name, then stay quiet forever after.
    ///
    /// The set of warned names is capped like the registry itself so a call
    /// site generating unbounded names cannot grow it without bound.
    fn warn_once(&self, name: &str, reason: &'static str) {
        {
            let seen = self
                .warned_names
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if seen.contains(name) || seen.len() >= MAX_INSTRUMENTS {
                return;
            }
        }
        {
            let mut seen = self
                .warned_names
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if seen.len() >= MAX_INSTRUMENTS || !seen.insert(name.into()) {
                return;
            }
        }
        // Outside the lock: a `tracing` subscriber is user code.
        tracing::warn!(
            metric = %sanitize_for_log(name),
            reason,
            "app metric rejected; recording through an inert handle"
        );
    }

    /// Warn once, process-wide, that the instrument cap is exhausted.
    fn warn_over_capacity(&self, name: &str) {
        if !self.over_capacity_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %sanitize_for_log(name),
                cap = MAX_INSTRUMENTS,
                "app metric registry is at capacity; further new metric names are ignored"
            );
        }
    }

    /// Warn once that too many histograms have bucket overrides waiting for a
    /// first use that never came.
    fn warn_pending_buckets_full(&self, name: &str) {
        if !self
            .pending_buckets_full_warned
            .swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                metric = %sanitize_for_log(name),
                cap = MAX_INSTRUMENTS,
                "too many histograms have bucket bounds configured but were never registered; \
                 ignoring further `set_histogram_buckets` calls for unregistered names"
            );
        }
    }

    /// Warn once that too many descriptions are waiting for instruments that
    /// were never registered.
    fn warn_pending_help_full(&self, name: &str) {
        if !self.pending_help_full_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %sanitize_for_log(name),
                cap = MAX_INSTRUMENTS,
                "too many app metrics have been described but were never registered; \
                 ignoring further `describe_*` calls for unregistered names"
            );
        }
    }
}

/// Render an untrusted name for a log line.
///
/// A name only reaches a warning path *because* it was rejected, so it may
/// carry newlines, ANSI escapes or anything else that would let a call site
/// forge log records. [`str::escape_debug`] neutralizes those, and the length
/// cap keeps a pathological name from flooding the log.
fn sanitize_for_log(name: &str) -> String {
    let preview: String = name.escape_debug().take(LOG_NAME_PREVIEW_LEN).collect();
    if name.escape_debug().nth(LOG_NAME_PREVIEW_LEN).is_some() {
        return format!("{preview}…");
    }
    preview
}

/// Sorted, deduplicated label pairs identifying one series of an instrument.
type SeriesKey = Box<[(Box<str>, Box<str>)]>;

/// A single registered instrument: its identity, its help text, and every
/// series recorded through it.
#[derive(Debug)]
struct Instrument {
    /// The metric name, as registered.
    name: Box<str>,
    /// Which kind of instrument this is; fixed at registration.
    kind: InstrumentKind,
    /// `# HELP` text, replaceable through the `describe_*` functions.
    help: RwLock<Box<str>>,
    /// Histogram bucket upper bounds; empty for counters and gauges. Frozen at
    /// registration so bucket boundaries never move under a scrape target.
    bounds: Box<[f64]>,
    /// Allocation-free fast path for the no-label case: recording through it
    /// takes no lock and canonicalizes nothing.
    unlabeled: Series,
    /// Whether anything was ever recorded into [`Self::unlabeled`]; an
    /// untouched instrument must not report a phantom zero series.
    unlabeled_used: AtomicBool,
    /// Labeled series, keyed by their canonical label set.
    series: RwLock<HashMap<SeriesKey, Arc<Series>>>,
    /// Label sets rejected by the cardinality cap.
    dropped: AtomicU64,
    /// Latches the single cardinality-cap warning.
    cap_warned: AtomicBool,
    /// Latches the single kind-conflict warning.
    kind_warned: AtomicBool,
    /// Latches the single label-rejection warning.
    label_warned: AtomicBool,
    /// Latches the single rejected-observation warning.
    value_warned: AtomicBool,
}

impl Instrument {
    /// Build an unregistered instrument with an empty (untouched) fast path.
    fn new(name: &str, kind: InstrumentKind, bounds: Box<[f64]>) -> Self {
        Self {
            name: name.into(),
            kind,
            help: RwLock::new(Box::default()),
            unlabeled: Series::new(kind, bounds.len()),
            bounds,
            unlabeled_used: AtomicBool::new(false),
            series: RwLock::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            cap_warned: AtomicBool::new(false),
            kind_warned: AtomicBool::new(false),
            label_warned: AtomicBool::new(false),
            value_warned: AtomicBool::new(false),
        }
    }

    /// Apply `update` to the series `labels` identifies, creating it if needed.
    ///
    /// No lock is held while `update` runs, and the no-label case touches no
    /// lock at all.
    fn record(&self, labels: &[(String, String)], update: impl FnOnce(&Series)) {
        if labels.is_empty() {
            self.record_unlabeled(update);
            return;
        }
        let key = self.canonical_key(labels);
        if key.is_empty() {
            // Every label was rejected; the sample still belongs somewhere.
            self.record_unlabeled(update);
            return;
        }
        if let Some(series) = self.series_for(&key) {
            update(&series);
        }
    }

    /// Apply `update` to the unlabeled fast-path series.
    ///
    /// The "used" flag is published *before* the update, never after: a scrape
    /// that sees the flag but not yet the value renders a zero-valued series,
    /// which is harmless, while the other order could hide a recorded sample
    /// from the scrape entirely.
    fn record_unlabeled(&self, update: impl FnOnce(&Series)) {
        self.unlabeled_used.store(true, Ordering::Relaxed);
        update(&self.unlabeled);
    }

    /// Look up (or register) the series for `key`, honouring the cardinality
    /// cap. Returns `None` when the cap dropped this label set.
    fn series_for(&self, key: &SeriesKey) -> Option<Arc<Series>> {
        {
            let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = series.get(key) {
                return Some(Arc::clone(existing));
            }
            if series.len() >= MAX_SERIES_PER_METRIC {
                drop(series);
                self.note_dropped();
                return None;
            }
        }
        let fresh = Arc::new(Series::new(self.kind, self.bounds.len()));
        {
            let mut series = self.series.write().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = series.get(key) {
                return Some(Arc::clone(existing));
            }
            if series.len() >= MAX_SERIES_PER_METRIC {
                drop(series);
                self.note_dropped();
                return None;
            }
            series.insert(key.clone(), Arc::clone(&fresh));
        }
        Some(fresh)
    }

    /// Count one dropped sample and warn about it at most once.
    fn note_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if !self.cap_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %self.name,
                cap = MAX_SERIES_PER_METRIC,
                "app metric hit its series cardinality cap; samples carrying a new label set \
                 are dropped. Label values must come from a small closed set — never user \
                 input or IDs"
            );
        }
    }

    /// Canonicalize a handle's pending labels into a series key: invalid,
    /// reserved and over-long names dropped, duplicates resolved first-wins,
    /// values sanitized, sorted by key, then cut to
    /// [`MAX_LABELS_PER_SERIES`].
    fn canonical_key(&self, labels: &[(String, String)]) -> SeriesKey {
        let mut kept: Vec<(Box<str>, Box<str>)> = Vec::with_capacity(labels.len());
        for (key, value) in labels {
            if !is_acceptable_label_name(key) {
                self.warn_labels("invalid, reserved or over-long label name");
                continue;
            }
            if kept.iter().any(|(existing, _)| **existing == **key) {
                self.warn_labels("duplicate label name");
                continue;
            }
            kept.push((key.as_str().into(), sanitize_label_value(value)));
        }
        // Sort *before* cutting to the cap. Truncating in insertion order would
        // make the surviving subset — and therefore the identity of the series
        // — depend on the order `with_label` happened to be called in, so the
        // same ten labels applied in two orders would land in two series.
        kept.sort_by(|(a, _), (b, _)| a.cmp(b));
        if kept.len() > MAX_LABELS_PER_SERIES {
            kept.truncate(MAX_LABELS_PER_SERIES);
            self.warn_labels("too many labels");
        }
        kept.into_boxed_slice()
    }

    /// Warn at most once that this instrument was handed unusable labels.
    fn warn_labels(&self, reason: &'static str) {
        if !self.label_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %self.name,
                reason,
                "app metric label dropped; the sample itself is still recorded"
            );
        }
    }

    /// Warn at most once that this instrument was handed an unusable value.
    fn warn_value(&self, value: f64, reason: &'static str) {
        if !self.value_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %self.name,
                value,
                reason,
                "app metric value rejected; dropping the sample"
            );
        }
    }

    /// Point-in-time view of this instrument.
    fn snapshot(&self) -> InstrumentSnapshot {
        let help = {
            let help = self.help.read().unwrap_or_else(PoisonError::into_inner);
            help.to_string()
        };
        // Build each series' owned label map while the read lock is held: the
        // alternative — cloning every `SeriesKey` and converting afterwards —
        // allocates each label twice for no less time under the lock.
        let labeled: Vec<(BTreeMap<String, String>, Arc<Series>)> = {
            let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
            series
                .iter()
                .map(|(key, value)| {
                    let labels = key
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    (labels, Arc::clone(value))
                })
                .collect()
        };

        let mut series = Vec::with_capacity(labeled.len() + 1);
        if self.unlabeled_used.load(Ordering::Relaxed) {
            series.push(SeriesSnapshot {
                labels: BTreeMap::new(),
                value: self.unlabeled.value(&self.bounds),
            });
        }
        for (labels, value) in labeled {
            series.push(SeriesSnapshot {
                labels,
                value: value.value(&self.bounds),
            });
        }
        series.sort_by(|a, b| a.labels.cmp(&b.labels));

        InstrumentSnapshot {
            name: self.name.to_string(),
            help,
            kind: self.kind,
            series,
            dropped_series: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// The storage behind one time series.
///
/// Every variant is a plain atomic so the hot path never takes a lock.
#[derive(Debug)]
enum Series {
    /// Monotonic total.
    Counter(AtomicU64),
    /// Current value, held as the bit pattern of an `f64`.
    Gauge(AtomicU64),
    /// Bucketed distribution.
    Histogram {
        /// One **non-cumulative** slot per bucket bound, plus a final overflow
        /// slot for observations above the last bound (the `+Inf` bucket). The
        /// observation count is derived by summing these at snapshot time, so
        /// `+Inf` structurally equals `_count` and no separate counter can
        /// drift away from them.
        slots: Box<[AtomicU64]>,
        /// Sum of observations, held as the bit pattern of an `f64`.
        sum_bits: AtomicU64,
    },
}

impl Series {
    /// Build an empty series for `kind`, sized for `bucket_count` bounds.
    fn new(kind: InstrumentKind, bucket_count: usize) -> Self {
        match kind {
            InstrumentKind::Counter => Self::Counter(AtomicU64::new(0)),
            InstrumentKind::Gauge => Self::Gauge(AtomicU64::new(0)),
            InstrumentKind::Histogram => Self::Histogram {
                slots: (0..=bucket_count).map(|_| AtomicU64::new(0)).collect(),
                sum_bits: AtomicU64::new(0),
            },
        }
    }

    /// Add `amount` to a counter series, saturating at [`u64::MAX`].
    ///
    /// Saturating rather than wrapping: a wrapped total looks to `PromQL`
    /// exactly like a counter reset, so `rate()` would report an enormous
    /// phantom spike. A pinned total is obviously broken instead.
    fn add(&self, amount: u64) {
        if let Self::Counter(total) = self {
            let _ = total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(amount))
            });
        }
    }

    /// Overwrite a gauge series.
    fn set(&self, value: f64) {
        if let Self::Gauge(bits) = self {
            bits.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// Add `delta` (which may be negative) to a gauge series.
    fn adjust(&self, delta: f64) {
        if let Self::Gauge(bits) = self {
            add_f64(bits, delta);
        }
    }

    /// Record one observation into a histogram series.
    fn observe(&self, value: f64, bounds: &[f64]) {
        if let Self::Histogram { slots, sum_bits } = self {
            // The first bound at or above `value`; `bounds.len()` (the
            // overflow slot) when the value exceeds every bound.
            let index = bounds.partition_point(|bound| *bound < value);
            if let Some(slot) = slots.get(index) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
            add_f64(sum_bits, value);
        }
    }

    /// Read this series' current value.
    fn value(&self, bounds: &[f64]) -> SeriesValue {
        match self {
            Self::Counter(total) => SeriesValue::Counter {
                value: total.load(Ordering::Relaxed),
            },
            Self::Gauge(bits) => SeriesValue::Gauge {
                value: f64::from_bits(bits.load(Ordering::Relaxed)),
            },
            Self::Histogram { slots, sum_bits } => {
                let mut cumulative: u64 = 0;
                let mut buckets = Vec::with_capacity(slots.len());
                for (index, slot) in slots.iter().enumerate() {
                    cumulative = cumulative.saturating_add(slot.load(Ordering::Relaxed));
                    let le = bounds
                        .get(index)
                        .map_or_else(|| "+Inf".to_string(), |bound| format_bound(*bound));
                    buckets.push((le, cumulative));
                }
                SeriesValue::Histogram {
                    count: cumulative,
                    sum: f64::from_bits(sum_bits.load(Ordering::Relaxed)),
                    buckets,
                }
            }
        }
    }
}

/// Add `delta` to the `f64` held as a bit pattern in `cell`, atomically.
fn add_f64(cell: &AtomicU64, delta: f64) {
    let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        // Both operands are finite (deltas and observations are validated,
        // and this clamp keeps the stored value finite), so the sum can
        // overflow to ±Inf but never be NaN. Clamp so a gauge near
        // `f64::MAX` nudged by a finite delta — or a `_sum` fed enormous
        // observations — saturates instead of permanently storing a
        // non-finite value the JSON view would render as `null`.
        Some(
            (f64::from_bits(bits) + delta)
                .clamp(f64::MIN, f64::MAX)
                .to_bits(),
        )
    });
}

/// Render a bucket bound the way it appears in an `le` label.
///
/// Byte-for-byte what `client_golang` writes, because a dashboard query
/// (`histogram_quantile`) and a recording rule both match `le` as a *string*:
/// Go formats it with shortest-`%g`, which caps the precision decision at 6
/// and therefore switches to exponential notation outside `[1e-4, 1e6)`
/// (`1000000.0` renders as `1e+06`), padding the exponent to at least two
/// digits with an explicit sign. Rust's `{}` never goes exponential and its
/// `{:e}` never pads, so the two are combined here.
fn format_bound(bound: f64) -> String {
    let magnitude = bound.abs();
    if magnitude == 0.0 || (1e-4..1e6).contains(&magnitude) {
        return bound.to_string();
    }
    let rendered = format!("{bound:e}");
    let Some((mantissa, exponent)) = rendered.split_once('e') else {
        return rendered;
    };
    let (sign, digits) = exponent
        .strip_prefix('-')
        .map_or(("+", exponent), |rest| ("-", rest));
    format!("{mantissa}e{sign}{digits:0>2}")
}

/// Whether `c` must not survive into a label value or a `# HELP` line.
///
/// Covers the C0 range, `DEL` and the C1 range — everything that can move a
/// terminal cursor, start an ANSI escape, or split one exposition line into
/// two. Nothing legitimate in a label value or a description is a control
/// character.
fn is_forbidden_control(c: char) -> bool {
    c.is_control()
}

/// Canonicalize a label value: control characters removed, then truncated to
/// [`MAX_LABEL_VALUE_LEN`] characters.
fn sanitize_label_value(value: &str) -> Box<str> {
    // A string of at most `MAX_LABEL_VALUE_LEN` bytes holds at most that many
    // characters, so the common case walks the string once and copies it once.
    if value.len() <= MAX_LABEL_VALUE_LEN && !value.contains(is_forbidden_control) {
        return value.into();
    }
    sanitized_chars(value, MAX_LABEL_VALUE_LEN).into_boxed_str()
}

/// [`sanitize_label_value`] for a value the caller already owns, reusing the
/// allocation when nothing needs changing.
fn sanitize_owned_label_value(value: String) -> String {
    if value.len() <= MAX_LABEL_VALUE_LEN && !value.contains(is_forbidden_control) {
        return value;
    }
    sanitized_chars(&value, MAX_LABEL_VALUE_LEN)
}

/// Canonicalize `# HELP` text: control characters removed (a `# HELP` line is
/// one line, so an embedded newline cannot be kept), then truncated to
/// [`MAX_HELP_LEN`] characters.
fn sanitize_help(help: &str) -> Box<str> {
    if help.len() <= MAX_HELP_LEN && !help.contains(is_forbidden_control) {
        return help.into();
    }
    sanitized_chars(help, MAX_HELP_LEN).into_boxed_str()
}

/// Drop control characters from `text` and keep at most `limit` of what is
/// left — always on a character boundary.
fn sanitized_chars(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !is_forbidden_control(*c))
        .take(limit)
        .collect()
}

/// Whether `name` may be used as a label key on an app metric.
fn is_acceptable_label_name(name: &str) -> bool {
    name.len() <= MAX_LABEL_NAME_LEN
        && crate::actuator::is_valid_label_name(name)
        && !name.starts_with("__")
        && !RESERVED_LABEL_NAMES.contains(&name)
}

/// Whether `name` may be registered as an app metric.
///
/// Stricter than Prometheus' own grammar: `:` is reserved for recording rules
/// and the `autumn_` namespace belongs to the framework's built-in families.
fn registration_rejection(name: &str) -> Option<&'static str> {
    if name.len() > MAX_METRIC_NAME_LEN {
        // Checked before the grammar so a pathological name is not scanned in
        // full. Rejected, never truncated: two names sharing a 128-byte prefix
        // would otherwise silently become one family.
        return Some("longer than the metric name cap");
    }
    if !crate::actuator::is_valid_metric_name(name) {
        return Some("not a valid Prometheus metric name");
    }
    if name.contains(':') {
        return Some("`:` is reserved for recording rules");
    }
    if name.starts_with("autumn_") || crate::actuator::BUILTIN_METRIC_FAMILY_NAMES.contains(&name) {
        return Some("the `autumn_` namespace is reserved for built-in metrics");
    }
    None
}

/// Every family name an instrument of `kind` named `name` occupies on a scrape.
fn occupied_names(name: &str, kind: InstrumentKind) -> Vec<String> {
    let mut names = Vec::with_capacity(4);
    names.push(name.to_owned());
    if matches!(kind, InstrumentKind::Histogram) {
        names.extend(HISTOGRAM_SUFFIXES.iter().map(|s| format!("{name}{s}")));
    }
    names
}

/// Whether registering `name` as `kind` would collide with a name an already
/// registered instrument occupies — in either direction.
fn collides_with_registered(
    registered: &HashMap<Box<str>, Arc<Instrument>>,
    name: &str,
    kind: InstrumentKind,
) -> bool {
    occupied_names(name, kind).iter().any(|candidate| {
        // Another instrument's base name.
        if registered.contains_key(candidate.as_str()) {
            return true;
        }
        // Another *histogram's* derived name.
        HISTOGRAM_SUFFIXES.iter().any(|suffix| {
            candidate.strip_suffix(suffix).is_some_and(|stem| {
                registered
                    .get(stem)
                    .is_some_and(|owner| matches!(owner.kind, InstrumentKind::Histogram))
            })
        })
    })
}

/// Take the bucket bounds to give an instrument named `name` at registration.
///
/// The pending entry is removed whatever `kind` is: once *any* instrument owns
/// the name, a bucket override stored against it can never apply, and leaving
/// it behind would let the map grow past its cap with dead entries.
///
/// Called only with the `instruments` write lock held — see [`Registry`] for
/// the lock order and why resolving the override there closes the race with
/// [`set_histogram_buckets`].
fn take_pending_bounds(name: &str, kind: InstrumentKind) -> Box<[f64]> {
    let pending = REGISTRY
        .pending_buckets
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(name);
    if matches!(kind, InstrumentKind::Histogram) {
        pending.unwrap_or_else(|| Box::from(DEFAULT_BUCKETS))
    } else {
        Box::default()
    }
}

/// Take the help text stashed by a `describe_*` call for `name`.
///
/// `Ok(None)` when nothing was described, `Ok(Some(help))` when the stashed
/// description matches the kind being registered, and `Err(described_kind)`
/// when it does not — the caller warns about that once the lock is released.
///
/// Called only with the `instruments` write lock held, like
/// [`take_pending_bounds`].
fn take_pending_help(name: &str, kind: InstrumentKind) -> Result<Option<Box<str>>, InstrumentKind> {
    let entry = REGISTRY
        .pending_help
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(name);
    match entry {
        None => Ok(None),
        Some((described, help)) if described == kind => Ok(Some(help)),
        Some((described, _)) => Err(described),
    }
}

/// What happened under the registry's write lock, decided while the lock was
/// held and acted on (warnings included) only after it was released.
enum Registration {
    /// This call registered the instrument. The second field carries the kind
    /// of a stashed description that was discarded for not matching.
    Registered(Arc<Instrument>, Option<InstrumentKind>),
    /// Another thread won the race; this is its instrument.
    Existing(Arc<Instrument>),
    /// The registry is full.
    OverCapacity,
    /// The name collides with another instrument's family names.
    Collision,
}

/// Get (or register) the instrument named `name`.
///
/// Returns `None` — meaning "hand back an inert handle" — for every rejection:
/// an unusable name, a collision with another instrument's family names, a
/// kind that contradicts an existing registration, or an exhausted registry.
fn instrument(name: &str, kind: InstrumentKind) -> Option<Arc<Instrument>> {
    if let Some(reason) = registration_rejection(name) {
        REGISTRY.warn_once(name, reason);
        return None;
    }

    // Fast path: already registered. Clone the `Arc` out before releasing the
    // lock so nothing (including `tracing`) runs while it is held.
    {
        let registered = REGISTRY
            .instruments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = registered.get(name) {
            let existing = Arc::clone(existing);
            drop(registered);
            return matching_kind(existing, kind);
        }
        if registered.len() >= MAX_INSTRUMENTS {
            drop(registered);
            REGISTRY.warn_over_capacity(name);
            return None;
        }
        if collides_with_registered(&registered, name, kind) {
            drop(registered);
            REGISTRY.warn_once(name, "collides with another metric's family names");
            return None;
        }
    }

    // Slow path: register. Every check is repeated under the write lock
    // because another thread may have won the race to this point.
    let outcome = {
        let mut registered = REGISTRY
            .instruments
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Decided (never acted on) under the lock; the guard is released
        // before any warning is emitted.
        let existing = registered.get(name).map(Arc::clone);
        let at_capacity = registered.len() >= MAX_INSTRUMENTS;
        let collides = collides_with_registered(&registered, name, kind);
        let outcome = match existing {
            Some(existing) => Registration::Existing(existing),
            None if at_capacity => Registration::OverCapacity,
            None if collides => Registration::Collision,
            None => {
                // Both pending maps are resolved *inside* this critical
                // section, so a `describe_*` or `set_histogram_buckets` call
                // racing with a first use either lands before the instrument
                // exists (and applies) or finds it registered (and says so) —
                // never in between, silently losing the setting.
                let bounds = take_pending_bounds(name, kind);
                let fresh = Arc::new(Instrument::new(name, kind, bounds));
                let discarded = match take_pending_help(name, kind) {
                    Ok(Some(help)) => {
                        *fresh.help.write().unwrap_or_else(PoisonError::into_inner) = help;
                        None
                    }
                    Ok(None) => None,
                    Err(described) => Some(described),
                };
                registered.insert(name.into(), Arc::clone(&fresh));
                Registration::Registered(fresh, discarded)
            }
        };
        drop(registered);
        outcome
    };

    match outcome {
        Registration::Registered(instrument, discarded_description) => {
            if let Some(described) = discarded_description {
                tracing::warn!(
                    metric = %instrument.name,
                    described = ?described,
                    registered = ?kind,
                    "app metric was described as a different kind before it was registered; \
                     the description is ignored"
                );
            }
            Some(instrument)
        }
        Registration::Existing(existing) => matching_kind(existing, kind),
        Registration::OverCapacity => {
            REGISTRY.warn_over_capacity(name);
            None
        }
        Registration::Collision => {
            REGISTRY.warn_once(name, "collides with another metric's family names");
            None
        }
    }
}

/// Hand back `existing` when it is of `kind`, else warn once and go inert.
///
/// First registration wins, which is what keeps exactly one `# TYPE` line per
/// name in the scrape output.
fn matching_kind(existing: Arc<Instrument>, kind: InstrumentKind) -> Option<Arc<Instrument>> {
    if existing.kind == kind {
        return Some(existing);
    }
    warn_kind_conflict(&existing, kind);
    None
}

/// Warn once per instrument that it was asked for as the wrong kind.
fn warn_kind_conflict(existing: &Instrument, requested: InstrumentKind) {
    if !existing.kind_warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            metric = %existing.name,
            registered = ?existing.kind,
            requested = ?requested,
            "app metric is already registered as a different kind; recording through an inert handle"
        );
    }
}

// ── Handle constructors ────────────────────────────────────────

/// Get (or lazily register) the counter named `name`.
///
/// A counter only ever goes up; name it `*_total` by convention. An invalid or
/// reserved name yields an inert handle that records nothing.
pub fn counter(name: &str) -> Counter {
    Counter {
        instrument: instrument(name, InstrumentKind::Counter),
        labels: Vec::new(),
    }
}

/// Get (or lazily register) the gauge named `name`.
///
/// A gauge is a point-in-time value that moves up and down. An invalid or
/// reserved name yields an inert handle that records nothing.
pub fn gauge(name: &str) -> Gauge {
    Gauge {
        instrument: instrument(name, InstrumentKind::Gauge),
        labels: Vec::new(),
    }
}

/// Get (or lazily register) the histogram named `name`.
///
/// An invalid or reserved name — or one colliding with an existing
/// instrument's `_bucket`/`_sum`/`_count` derived names — yields an inert
/// handle that records nothing.
pub fn histogram(name: &str) -> Histogram {
    Histogram {
        instrument: instrument(name, InstrumentKind::Histogram),
        labels: Vec::new(),
    }
}

/// Get (or lazily register) the timer named `name`.
///
/// A timer is a histogram of durations measured in seconds; name it
/// `*_seconds` by convention. The usual shape is a guard bound to a **named**
/// variable, which records when the scope ends — including on an early `?`
/// return or an unwinding panic:
///
/// ```rust
/// use autumn_web::metrics;
///
/// fn render_report() -> Result<String, std::fmt::Error> {
///     let _timing = metrics::timer("report_render_seconds").start();
///     Ok("report".to_string())
/// }
/// # assert!(render_report().is_ok());
/// ```
///
/// `let _ = ...` would drop the guard immediately and record roughly zero.
pub fn timer(name: &str) -> Timer {
    Timer {
        histogram: histogram(name),
    }
}

/// Set the `# HELP` text of the counter named `name`.
///
/// Describing a metric does **not** register it: the description is stashed
/// and applied when the first `counter(name)` call registers the instrument,
/// so `describe_counter` and the first use may come in either order. An
/// instrument that is described but never used stays out of the scrape
/// output entirely.
///
/// Help text is stripped of control characters and truncated to
/// [`MAX_HELP_LEN`] characters.
pub fn describe_counter(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Counter, &help.into());
}

/// Set the `# HELP` text of the gauge named `name`.
///
/// Like [`describe_counter`], this does not register the instrument.
pub fn describe_gauge(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Gauge, &help.into());
}

/// Set the `# HELP` text of the histogram (or timer) named `name`.
///
/// Like [`describe_counter`], this does not register the instrument — which is
/// what lets it be combined with [`set_histogram_buckets`] in either order.
pub fn describe_histogram(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Histogram, &help.into());
}

/// What [`describe`] decided under the registry lock.
enum Description {
    /// Applied to an already-registered instrument, or stashed for one.
    Recorded,
    /// The name is registered as a different kind.
    Mismatch(Arc<Instrument>),
    /// Too many descriptions are already waiting for unregistered names.
    Full,
}

/// Apply `help` to the instrument named `name`, or stash it until one is
/// registered under that name.
///
/// Deliberately does **not** register the instrument. Registering here would
/// freeze a histogram's bucket bounds, so `describe_histogram` followed by
/// `set_histogram_buckets` would silently discard the custom bounds — a trap
/// with no diagnostic at all, since both calls "succeed".
fn describe(name: &str, kind: InstrumentKind, help: &str) {
    if let Some(reason) = registration_rejection(name) {
        REGISTRY.warn_once(name, reason);
        return;
    }
    let help = sanitize_help(help);

    let outcome = {
        // Lock order: `pending_help` nested under `instruments` (see
        // `Registry`). Holding the read lock across both branches is what
        // keeps a concurrent first use from registering between the lookup
        // and the stash.
        let registered = REGISTRY
            .instruments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match registered.get(name) {
            Some(existing) if existing.kind == kind => {
                *existing
                    .help
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = help;
                Description::Recorded
            }
            Some(existing) => Description::Mismatch(Arc::clone(existing)),
            None => {
                let mut pending = REGISTRY
                    .pending_help
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                if pending.len() >= MAX_INSTRUMENTS && !pending.contains_key(name) {
                    Description::Full
                } else {
                    pending.insert(name.into(), (kind, help));
                    Description::Recorded
                }
            }
        }
    };

    // Warnings only once the guards are gone: a `tracing` subscriber is user
    // code and must never run under a facade lock.
    match outcome {
        Description::Recorded => {}
        Description::Mismatch(existing) => warn_kind_conflict(&existing, kind),
        Description::Full => REGISTRY.warn_pending_help_full(name),
    }
}

/// What [`set_histogram_buckets`] decided under the registry lock.
enum BucketOverride {
    /// Stashed for the histogram's registration.
    Stored,
    /// The name is already registered; its bounds are frozen.
    AlreadyRegistered,
    /// Too many overrides are already waiting for unregistered names.
    Full,
}

/// Override the bucket upper bounds of the histogram (or timer) named `name`.
///
/// Only effective *before* that histogram is registered — that is, before the
/// first `histogram(name)` / `timer(name)` call. The bounds are frozen into the
/// instrument at registration, so a later call is ignored with a warning rather
/// than moving bucket boundaries under a running scrape target.
///
/// [`describe_histogram`] does not register anything, so the two can be called
/// in either order:
///
/// ```rust
/// use autumn_web::metrics;
///
/// metrics::describe_histogram("upload_duration_seconds", "How long an upload took");
/// metrics::set_histogram_buckets("upload_duration_seconds", &[0.1, 1.0, 10.0, 60.0]);
///
/// // The first use registers the histogram with both settings applied.
/// metrics::timer("upload_duration_seconds").record(std::time::Duration::from_secs(2));
/// ```
///
/// `upper_bounds` must hold 1..=[`MAX_BUCKET_BOUNDS`] finite, strictly
/// ascending, positive values; anything else is ignored with a warning and the
/// defaults are kept.
pub fn set_histogram_buckets(name: &str, upper_bounds: &[f64]) {
    if let Some(reason) = registration_rejection(name) {
        REGISTRY.warn_once(name, reason);
        return;
    }
    if !are_valid_bounds(upper_bounds) {
        tracing::warn!(
            metric = %sanitize_for_log(name),
            bounds = ?upper_bounds,
            max = MAX_BUCKET_BOUNDS,
            "histogram bucket bounds must be finite, positive, strictly ascending values; \
             keeping the defaults"
        );
        return;
    }

    let outcome = {
        // Lock order: `pending_buckets` nested under `instruments`. Holding the
        // read lock across the check *and* the insert is what stops a
        // concurrent first use from registering the histogram with the default
        // bounds in between, which would drop this override on the floor.
        let registered = REGISTRY
            .instruments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if registered.contains_key(name) {
            BucketOverride::AlreadyRegistered
        } else {
            let mut pending = REGISTRY
                .pending_buckets
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if pending.len() >= MAX_INSTRUMENTS && !pending.contains_key(name) {
                BucketOverride::Full
            } else {
                pending.insert(name.into(), upper_bounds.into());
                BucketOverride::Stored
            }
        }
    };

    match outcome {
        BucketOverride::Stored => {}
        BucketOverride::AlreadyRegistered => tracing::warn!(
            metric = %sanitize_for_log(name),
            "histogram bucket bounds are frozen once the histogram is registered; ignoring"
        ),
        BucketOverride::Full => REGISTRY.warn_pending_buckets_full(name),
    }
}

/// Whether `bounds` is a usable set of histogram bucket upper bounds.
fn are_valid_bounds(bounds: &[f64]) -> bool {
    !bounds.is_empty()
        && bounds.len() <= MAX_BUCKET_BOUNDS
        && bounds.iter().all(|b| b.is_finite() && *b > 0.0)
        && bounds.windows(2).all(|w| w[0] < w[1])
}

// ── Accepted numeric values ────────────────────────────────────

/// Implementation detail of the [`IntoMetricValue`] seal.
mod sealed {
    /// Keeps [`super::IntoMetricValue`] closed to downstream implementations.
    pub trait Sealed {}
}

/// A number a gauge or histogram will accept.
///
/// Covers every primitive that converts into `f64` losslessly, plus the three
/// integer types call sites actually reach for — `usize`, `u64` and `i64`,
/// almost always from a `len()` or a count — which
/// [`impl Into<f64>`](Into) refuses. `gauge("queue_depth").set(queue.len())`
/// compiles because of this trait.
///
/// Those three are **lossy above 2<sup>53</sup>**: the value is rounded to the
/// nearest representable `f64`, the same way Prometheus itself would store it
/// (its wire format is `f64` throughout), so nothing is lost that a scrape
/// could have carried anyway.
///
/// Sealed: only this crate implements it.
pub trait IntoMetricValue: sealed::Sealed {
    /// Convert into the `f64` the facade stores.
    #[must_use]
    fn into_metric_value(self) -> f64;
}

/// Implement [`IntoMetricValue`] for types with a lossless `f64` conversion.
macro_rules! impl_lossless_metric_value {
    ($($ty:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl IntoMetricValue for $ty {
            fn into_metric_value(self) -> f64 {
                f64::from(self)
            }
        }
    )+};
}

/// Implement [`IntoMetricValue`] for integer types wider than `f64`'s exact
/// range; see the trait docs for the precision this gives up.
macro_rules! impl_wide_metric_value {
    ($($ty:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl IntoMetricValue for $ty {
            #[allow(
                clippy::cast_precision_loss,
                reason = "documented on IntoMetricValue: exact to 2^53, rounded above it"
            )]
            fn into_metric_value(self) -> f64 {
                self as f64
            }
        }
    )+};
}

impl_lossless_metric_value!(f32, f64, i8, i16, i32, u8, u16, u32);
impl_wide_metric_value!(i64, u64, isize, usize);

// ── Counter ────────────────────────────────────────────────────

/// Handle to a monotonically increasing counter.
///
/// Cheap to clone and safe to share across threads.
#[derive(Clone, Debug)]
#[must_use = "a metric handle does nothing until you record through it"]
pub struct Counter {
    instrument: Option<Arc<Instrument>>,
    labels: Vec<(String, String)>,
}

impl Counter {
    /// Attach a label to the series this handle records into.
    ///
    /// Label keys are canonicalized (sorted, deduplicated first-wins); an
    /// invalid or reserved key drops that label, never the sample. Values are
    /// stripped of control characters and truncated to
    /// [`MAX_LABEL_VALUE_LEN`] characters here, so an over-long value is never
    /// carried around whole.
    ///
    /// An inert handle (one whose name was rejected) skips the work entirely.
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        if self.instrument.is_some() {
            self.labels
                .push((key.to_owned(), sanitize_owned_label_value(value.into())));
        }
        self
    }

    /// Add `amount` to this counter's series.
    ///
    /// The total saturates at [`u64::MAX`] rather than wrapping: a wrapped
    /// total is indistinguishable from a counter reset, which would give
    /// `PromQL`'s `rate()` an enormous phantom spike.
    pub fn increment(&self, amount: u64) {
        if let Some(instrument) = self.instrument.as_ref() {
            instrument.record(&self.labels, |series| series.add(amount));
        }
    }
}

// ── Gauge ──────────────────────────────────────────────────────

/// Handle to a gauge: a value that goes up and down.
#[derive(Clone, Debug)]
#[must_use = "a metric handle does nothing until you record through it"]
pub struct Gauge {
    instrument: Option<Arc<Instrument>>,
    labels: Vec<(String, String)>,
}

impl Gauge {
    /// Attach a label to the series this handle records into.
    ///
    /// Canonicalized exactly like [`Counter::with_label`].
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        if self.instrument.is_some() {
            self.labels
                .push((key.to_owned(), sanitize_owned_label_value(value.into())));
        }
        self
    }

    /// Set this gauge's series to `value`.
    ///
    /// Accepts any [`IntoMetricValue`], so the `usize` a `len()` hands you
    /// needs no cast:
    ///
    /// ```rust
    /// use autumn_web::metrics;
    ///
    /// let queue: Vec<u8> = vec![1, 2, 3];
    /// metrics::gauge("worker_queue_depth").set(queue.len());
    /// ```
    ///
    /// Non-finite values (`NaN`, `±Inf`) are rejected with a warning and the
    /// gauge keeps its previous reading: they have no meaning to render, and
    /// JSON has no way to express them, so accepting one would make the
    /// scrape and the `/actuator/metrics` view disagree.
    pub fn set(&self, value: impl IntoMetricValue) {
        let value = value.into_metric_value();
        let Some(instrument) = self.instrument.as_ref() else {
            return;
        };
        if !value.is_finite() {
            instrument.warn_value(value, "a gauge value must be finite");
            return;
        }
        instrument.record(&self.labels, |series| series.set(value));
    }

    /// Add `delta` to this gauge's series.
    ///
    /// A non-finite `delta` is rejected, as in [`Gauge::set`].
    pub fn increment(&self, delta: impl IntoMetricValue) {
        self.adjust(delta.into_metric_value());
    }

    /// Subtract `delta` from this gauge's series.
    ///
    /// A non-finite `delta` is rejected, as in [`Gauge::set`].
    pub fn decrement(&self, delta: impl IntoMetricValue) {
        self.adjust(-delta.into_metric_value());
    }

    /// Shared body of [`Gauge::increment`] and [`Gauge::decrement`].
    fn adjust(&self, delta: f64) {
        let Some(instrument) = self.instrument.as_ref() else {
            return;
        };
        if !delta.is_finite() {
            instrument.warn_value(delta, "a gauge delta must be finite");
            return;
        }
        instrument.record(&self.labels, |series| series.adjust(delta));
    }
}

// ── Histogram ──────────────────────────────────────────────────

/// Handle to a histogram of observed values.
#[derive(Clone, Debug)]
#[must_use = "a metric handle does nothing until you record through it"]
pub struct Histogram {
    instrument: Option<Arc<Instrument>>,
    labels: Vec<(String, String)>,
}

impl Histogram {
    /// Attach a label to the series this handle records into.
    ///
    /// Canonicalized exactly like [`Counter::with_label`].
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        if self.instrument.is_some() {
            self.labels
                .push((key.to_owned(), sanitize_owned_label_value(value.into())));
        }
        self
    }

    /// Record one observation.
    ///
    /// Accepts any [`IntoMetricValue`], so `record(body.len())` compiles.
    /// Non-finite and negative values are rejected with a warning so `_sum`
    /// can never become `NaN` and buckets stay meaningful. `_sum` is still an
    /// `f64` accumulator: enough enormous observations will saturate it to
    /// `+Inf`, permanently, since a histogram is never reset.
    pub fn record(&self, value: impl IntoMetricValue) {
        let value = value.into_metric_value();
        let Some(instrument) = self.instrument.as_ref() else {
            return;
        };
        if !value.is_finite() || value < 0.0 {
            instrument.warn_value(
                value,
                "a histogram observation must be finite and non-negative",
            );
            return;
        }
        instrument.record(&self.labels, |series| {
            series.observe(value, &instrument.bounds);
        });
    }
}

// ── Timer ──────────────────────────────────────────────────────

/// Handle to a duration histogram, recorded in seconds.
#[derive(Clone, Debug)]
#[must_use = "a metric handle does nothing until you record through it"]
pub struct Timer {
    histogram: Histogram,
}

impl Timer {
    /// Attach a label to the series this handle records into.
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.histogram = self.histogram.with_label(key, value);
        self
    }

    /// Start measuring; the elapsed time is recorded when the guard drops.
    ///
    /// The guard records on every exit path — including early `?` returns and
    /// unwinding panics.
    pub fn start(&self) -> TimerGuard {
        TimerGuard {
            started: Instant::now(),
            timer: Some(self.clone()),
        }
    }

    /// Record an already-measured duration.
    pub fn record(&self, elapsed: Duration) {
        self.histogram.record(elapsed.as_secs_f64());
    }

    /// Time a synchronous closure, returning its value.
    pub fn time<T>(&self, f: impl FnOnce() -> T) -> T {
        let _guard = self.start();
        f()
    }

    /// Time a future, returning its output.
    pub async fn time_async<F: Future>(&self, fut: F) -> F::Output {
        let _guard = self.start();
        fut.await
    }
}

/// Records the elapsed time of the scope it lives in when dropped.
#[derive(Debug)]
#[must_use = "the timer only records for as long as the guard is alive"]
pub struct TimerGuard {
    started: Instant,
    timer: Option<Timer>,
}

impl TimerGuard {
    /// Discard the measurement: nothing is recorded.
    pub fn cancel(mut self) {
        self.timer = None;
    }

    /// Stop timing, record the observation, and return the elapsed duration.
    // Not `#[must_use]`: recording the observation is the point of the call and
    // `guard.stop();` — stop timing here, before the scope ends — is a correct,
    // complete use. The returned duration is a convenience for callers that
    // also want to log or assert on it.
    #[allow(clippy::must_use_candidate)]
    pub fn stop(mut self) -> Duration {
        let elapsed = self.started.elapsed();
        // Taking the timer latches the measurement: `Drop` finds `None` and
        // will not record it a second time.
        if let Some(timer) = self.timer.take() {
            timer.record(elapsed);
        }
        elapsed
    }
}

impl Drop for TimerGuard {
    fn drop(&mut self) {
        // `Instant::elapsed` is monotonic and saturating, and nothing below
        // unwraps, indexes or divides. It is not *provably* panic-free: a
        // labelled guard canonicalizes its labels, which allocates, and a
        // rejected value calls into the `tracing` subscriber — either can
        // panic in principle. Neither is on the path a guard normally takes,
        // which records a finite duration into an already-registered series.
        if let Some(timer) = self.timer.take() {
            timer.record(self.started.elapsed());
        }
    }
}

// ── Snapshot ───────────────────────────────────────────────────

/// Point-in-time view of every registered instrument.
///
/// Instruments are sorted by name and each instrument's series are sorted by
/// their canonical label key, so the rendered output is byte-stable.
#[must_use]
pub fn snapshot() -> Vec<InstrumentSnapshot> {
    let instruments: Vec<Arc<Instrument>> = {
        let registered = REGISTRY
            .instruments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        registered.values().map(Arc::clone).collect()
    };
    let mut snapshot: Vec<InstrumentSnapshot> = instruments.iter().map(|i| i.snapshot()).collect();
    snapshot.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot
}

/// Which kind of instrument a snapshot describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InstrumentKind {
    /// A monotonically increasing counter.
    Counter,
    /// A value that moves up and down.
    Gauge,
    /// A bucketed distribution of observations.
    Histogram,
}

/// One instrument's data at snapshot time.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct InstrumentSnapshot {
    /// The instrument's metric name.
    pub name: String,
    /// The `# HELP` text, empty when never described.
    pub help: String,
    /// Which kind of instrument this is.
    pub kind: InstrumentKind,
    /// Every retained series, sorted by canonical label key.
    pub series: Vec<SeriesSnapshot>,
    /// How many **samples** this instrument dropped because they carried a
    /// label set it had no room for.
    ///
    /// Counts samples, not distinct label sets: a hot call site hammering an
    /// over-cap label set is exactly what an operator needs to see, and a
    /// distinct-set count would need unbounded memory to compute.
    pub dropped_series: u64,
}
// A histogram's bucket bounds are deliberately *not* a field here: they appear
// in every histogram series already, as the `le` strings of
// [`SeriesValue::Histogram`], in the exact form the scrape renders.

/// One labeled series of an instrument.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SeriesSnapshot {
    /// Canonical (sorted, deduplicated) labels of this series.
    pub labels: BTreeMap<String, String>,
    /// The series' value.
    pub value: SeriesValue,
}

/// The value carried by a [`SeriesSnapshot`].
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum SeriesValue {
    /// A counter total.
    Counter {
        /// The accumulated total.
        value: u64,
    },
    /// A gauge reading.
    Gauge {
        /// The current value.
        value: f64,
    },
    /// A histogram distribution.
    Histogram {
        /// Number of observations; equals the `+Inf` bucket.
        count: u64,
        /// Sum of all observed values.
        sum: f64,
        /// Cumulative bucket counts keyed by their canonical `le` string; the
        /// last entry is always `("+Inf", count)`.
        buckets: Vec<(String, u64)>,
    },
}

// ── Test support ───────────────────────────────────────────────

/// Clear the process-global registry.
///
/// **Not** for use in the consolidated test binary: the registry is
/// process-global and tests run concurrently, so clearing it would race with
/// unrelated tests. Prefer [`testing::unique_name`] instead.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_tests() {
    REGISTRY
        .instruments
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    REGISTRY
        .pending_buckets
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    REGISTRY
        .pending_help
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    REGISTRY
        .warned_names
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    REGISTRY
        .over_capacity_warned
        .store(false, Ordering::Relaxed);
    REGISTRY
        .pending_buckets_full_warned
        .store(false, Ordering::Relaxed);
    REGISTRY
        .pending_help_full_warned
        .store(false, Ordering::Relaxed);
}

/// Helpers for testing code that records metrics.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Process-wide sequence backing [`unique_name`].
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// Build a metric name no other test in this process will use.
    ///
    /// The registry is process-global, so every test must record into its own
    /// instrument names; this makes exact-value assertions safe under
    /// concurrent test execution. `prefix` must itself be a valid Prometheus
    /// metric name fragment.
    ///
    /// Those names are never reclaimed, and they share the process-wide
    /// [`MAX_INSTRUMENTS`](super::MAX_INSTRUMENTS) budget with every other
    /// test in the same binary. A test that registers a *handful* of unique
    /// names is fine; one that registers hundreds in a loop will exhaust the
    /// registry for whatever runs after it. Cap the loop, or reuse one name
    /// with different labels (which is capped separately, per instrument).
    #[must_use]
    pub fn unique_name(prefix: &str) -> String {
        format!("{prefix}_{}", SEQUENCE.fetch_add(1, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::unique_name;
    use super::*;

    /// Look up one instrument in the process-global snapshot by exact name.
    fn find(name: &str) -> Option<InstrumentSnapshot> {
        snapshot().into_iter().find(|i| i.name == name)
    }

    /// Look up one instrument, failing the test when it is absent.
    fn expect_instrument(name: &str) -> InstrumentSnapshot {
        find(name).unwrap_or_else(|| panic!("instrument {name} missing from snapshot"))
    }

    /// The single series of an unlabeled instrument.
    fn only_series(instrument: &InstrumentSnapshot) -> &SeriesSnapshot {
        assert_eq!(
            instrument.series.len(),
            1,
            "expected exactly one series on {}",
            instrument.name
        );
        &instrument.series[0]
    }

    fn counter_value(series: &SeriesSnapshot) -> u64 {
        match series.value {
            SeriesValue::Counter { value } => value,
            ref other => panic!("expected a counter value, got {other:?}"),
        }
    }

    fn gauge_value(series: &SeriesSnapshot) -> f64 {
        match series.value {
            SeriesValue::Gauge { value } => value,
            ref other => panic!("expected a gauge value, got {other:?}"),
        }
    }

    /// The `le` strings of an instrument's buckets, `+Inf` excluded — the
    /// observable form of its configured upper bounds.
    fn bucket_bounds(instrument: &InstrumentSnapshot) -> Vec<String> {
        let (_count, _sum, buckets) = histogram_parts(only_series(instrument));
        buckets
            .into_iter()
            .map(|(le, _)| le)
            .filter(|le| le != "+Inf")
            .collect()
    }

    /// [`DEFAULT_BUCKETS`] as the `le` strings a scrape would show.
    fn default_bucket_strings() -> Vec<String> {
        DEFAULT_BUCKETS.iter().map(|b| format_bound(*b)).collect()
    }

    fn histogram_parts(series: &SeriesSnapshot) -> (u64, f64, Vec<(String, u64)>) {
        match series.value {
            SeriesValue::Histogram {
                count,
                sum,
                ref buckets,
            } => (count, sum, buckets.clone()),
            ref other => panic!("expected a histogram value, got {other:?}"),
        }
    }

    // ── Counters ───────────────────────────────────────────────

    #[test]
    fn counter_increment_accumulates_in_snapshot() {
        let name = unique_name("facade_counter_accumulates");
        counter(&name).increment(1);
        counter(&name).increment(1);
        counter(&name).increment(1);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.kind, InstrumentKind::Counter);
        assert_eq!(counter_value(only_series(&instrument)), 3);
    }

    #[test]
    fn counter_increment_by_amount_accumulates() {
        let name = unique_name("facade_counter_by_amount");
        let handle = counter(&name);
        handle.increment(7);
        handle.increment(35);

        assert_eq!(counter_value(only_series(&expect_instrument(&name))), 42);
    }

    #[test]
    fn counter_help_text_is_exposed_in_snapshot() {
        let name = unique_name("facade_counter_help");
        describe_counter(&name, "how many widgets shipped");
        counter(&name).increment(1);

        assert_eq!(expect_instrument(&name).help, "how many widgets shipped");
    }

    // ── Label canonicalization ─────────────────────────────────

    #[test]
    fn label_order_canonicalizes_to_a_single_series() {
        let name = unique_name("facade_label_order");
        counter(&name)
            .with_label("region", "eu")
            .with_label("status", "paid")
            .increment(1);
        counter(&name)
            .with_label("status", "paid")
            .with_label("region", "eu")
            .increment(1);

        let instrument = expect_instrument(&name);
        assert_eq!(
            instrument.series.len(),
            1,
            "label order must not create a second series: {:?}",
            instrument.series
        );
        let series = only_series(&instrument);
        assert_eq!(counter_value(series), 2);
        assert_eq!(series.labels.get("region").map(String::as_str), Some("eu"));
        assert_eq!(
            series.labels.get("status").map(String::as_str),
            Some("paid")
        );
    }

    #[test]
    fn duplicate_label_key_keeps_the_first_value() {
        let name = unique_name("facade_label_dup");
        counter(&name)
            .with_label("status", "paid")
            .with_label("status", "refunded")
            .increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(
            series.labels.get("status").map(String::as_str),
            Some("paid")
        );
        assert_eq!(series.labels.len(), 1);
    }

    #[test]
    fn invalid_label_name_drops_the_label_not_the_sample() {
        let name = unique_name("facade_label_invalid");
        counter(&name)
            .with_label("not-a-valid-name", "x")
            .with_label("status", "paid")
            .increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(
            counter_value(series),
            1,
            "the sample must still be recorded"
        );
        assert!(
            !series.labels.contains_key("not-a-valid-name"),
            "invalid label must be dropped: {:?}",
            series.labels
        );
        assert_eq!(
            series.labels.get("status").map(String::as_str),
            Some("paid")
        );
    }

    #[test]
    fn reserved_label_names_are_dropped() {
        let name = unique_name("facade_label_reserved");
        counter(&name)
            .with_label("le", "0.5")
            .with_label("quantile", "0.99")
            .with_label("__private", "x")
            .with_label("status", "paid")
            .increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(counter_value(series), 1);
        assert_eq!(
            series.labels.len(),
            1,
            "only `status` survives: {:?}",
            series.labels
        );
        assert_eq!(
            series.labels.get("status").map(String::as_str),
            Some("paid")
        );
    }

    #[test]
    fn long_label_values_are_truncated() {
        let name = unique_name("facade_label_truncate");
        let long = "x".repeat(MAX_LABEL_VALUE_LEN * 2);
        counter(&name).with_label("status", long).increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(
            series.labels.get("status").map(String::len),
            Some(MAX_LABEL_VALUE_LEN)
        );
    }

    // ── Gauges ─────────────────────────────────────────────────

    #[test]
    fn gauge_set_increment_and_decrement_track_the_current_value() {
        let name = unique_name("facade_gauge_basic");
        let handle = gauge(&name);
        handle.set(10.0);
        handle.increment(5.0);
        handle.decrement(2.5);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.kind, InstrumentKind::Gauge);
        assert!(
            (gauge_value(only_series(&instrument)) - 12.5).abs() < f64::EPSILON,
            "expected 12.5, got {}",
            gauge_value(only_series(&instrument))
        );
    }

    #[test]
    fn gauge_concurrent_increments_sum_exactly() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 500;

        let name = unique_name("facade_gauge_concurrent");
        let handle = gauge(&name);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let handle = handle.clone();
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        handle.increment(1.0);
                    }
                });
            }
        });

        let instrument = expect_instrument(&name);
        #[allow(clippy::cast_precision_loss)]
        let expected = (THREADS * PER_THREAD) as f64;
        assert!(
            (gauge_value(only_series(&instrument)) - expected).abs() < f64::EPSILON,
            "lost updates: expected {expected}, got {}",
            gauge_value(only_series(&instrument))
        );
    }

    // ── Histograms ─────────────────────────────────────────────

    #[test]
    fn histogram_record_fills_cumulative_buckets() {
        let name = unique_name("facade_histogram_buckets");
        let handle = histogram(&name);
        handle.record(0.003);
        handle.record(0.03);
        handle.record(100.0);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.kind, InstrumentKind::Histogram);
        let (count, sum, buckets) = histogram_parts(only_series(&instrument));

        assert_eq!(count, 3);
        assert!(
            (sum - 100.033).abs() < 1e-9,
            "sum must be the exact total, got {sum}"
        );
        assert_eq!(
            buckets,
            vec![
                ("0.005".to_string(), 1),
                ("0.01".to_string(), 1),
                ("0.025".to_string(), 1),
                ("0.05".to_string(), 2),
                ("0.1".to_string(), 2),
                ("0.25".to_string(), 2),
                ("0.5".to_string(), 2),
                ("1".to_string(), 2),
                ("2.5".to_string(), 2),
                ("5".to_string(), 2),
                ("10".to_string(), 2),
                ("+Inf".to_string(), 3),
            ],
            "buckets must be cumulative with canonical `le` strings"
        );
    }

    #[test]
    fn histogram_inf_bucket_equals_count_and_buckets_never_decrease() {
        let name = unique_name("facade_histogram_invariants");
        let handle = histogram(&name);
        for value in [0.001, 0.2, 0.75, 3.0, 42.0] {
            handle.record(value);
        }

        let instrument = expect_instrument(&name);
        let (count, _sum, buckets) = histogram_parts(only_series(&instrument));

        let (last_le, last_value) = buckets.last().expect("at least the +Inf bucket");
        assert_eq!(last_le, "+Inf", "the final bucket must be +Inf");
        assert_eq!(*last_value, count, "+Inf must equal the derived count");
        assert_eq!(count, 5);
        assert!(
            buckets.windows(2).all(|w| w[0].1 <= w[1].1),
            "cumulative buckets must be non-decreasing: {buckets:?}"
        );
    }

    #[test]
    fn histogram_rejects_non_finite_and_negative_observations() {
        let name = unique_name("facade_histogram_rejects");
        let handle = histogram(&name);
        handle.record(1.0);
        handle.record(f64::NAN);
        handle.record(f64::INFINITY);
        handle.record(f64::NEG_INFINITY);
        handle.record(-1.0);

        let instrument = expect_instrument(&name);
        let (count, sum, buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1, "only the one valid observation counts");
        assert!(sum.is_finite(), "sum must never become NaN or infinite");
        assert!(
            (sum - 1.0).abs() < f64::EPSILON,
            "sum should be 1.0, got {sum}"
        );
        assert_eq!(buckets.last().map(|(_, v)| *v), Some(1));
    }

    #[test]
    fn histogram_concurrent_records_are_exact() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 250;

        let name = unique_name("facade_histogram_concurrent");
        let handle = histogram(&name);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let handle = handle.clone();
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        handle.record(0.02);
                    }
                });
            }
        });

        let instrument = expect_instrument(&name);
        let (count, _sum, buckets) = histogram_parts(only_series(&instrument));
        // `try_from` rather than `count as usize`: a truncating cast in an
        // assertion could hide the very miscount this test exists to catch.
        assert_eq!(usize::try_from(count).unwrap(), THREADS * PER_THREAD);
        assert_eq!(buckets.last().map(|(_, v)| *v), Some(count));
    }

    // ── Timers ─────────────────────────────────────────────────

    #[test]
    fn timer_guard_records_on_drop() {
        let name = unique_name("facade_timer_drop_seconds");
        {
            let _guard = timer(&name).start();
        }

        let instrument = expect_instrument(&name);
        let (count, sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1);
        assert!(sum >= 0.0 && sum.is_finite(), "sum must be sane, got {sum}");
    }

    #[test]
    fn timer_guard_records_on_the_error_path() {
        fn fallible(t: &Timer) -> Result<(), &'static str> {
            let _guard = t.start();
            Err("boom")?;
            Ok(())
        }

        let name = unique_name("facade_timer_error_seconds");
        let t = timer(&name);
        assert!(fallible(&t).is_err());

        let instrument = expect_instrument(&name);
        let (count, _sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1, "an early `?` return must still record");
    }

    #[test]
    fn timer_guard_records_when_dropped_during_panic_unwind() {
        let name = unique_name("facade_timer_unwind_seconds");
        let t = timer(&name);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = t.start();
            panic!("handler blew up");
        }));
        assert!(unwound.is_err(), "the closure must actually panic");

        let instrument = expect_instrument(&name);
        let (count, _sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(
            count, 1,
            "a guard dropped while unwinding must still record"
        );
    }

    #[test]
    fn timer_time_records_the_closure_duration() {
        let name = unique_name("facade_timer_time_seconds");
        let value = timer(&name).time(|| 21 * 2);
        assert_eq!(value, 42);

        let instrument = expect_instrument(&name);
        let (count, _sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn timer_time_async_records_the_future_duration() {
        let name = unique_name("facade_timer_async_seconds");
        let value = timer(&name).time_async(async { 42 }).await;
        assert_eq!(value, 42);

        let instrument = expect_instrument(&name);
        let (count, _sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1);
    }

    #[test]
    fn timer_guard_cancel_discards_the_measurement() {
        let name = unique_name("facade_timer_cancel_seconds");
        let t = timer(&name);
        t.start().cancel();

        assert!(
            find(&name).is_none_or(|i| i.series.is_empty()),
            "a cancelled guard must record nothing"
        );
    }

    #[test]
    fn timer_guard_stop_returns_elapsed_and_records_once() {
        let name = unique_name("facade_timer_stop_seconds");
        let elapsed = timer(&name).start().stop();
        assert!(elapsed < Duration::from_secs(60), "elapsed looks wrong");

        let instrument = expect_instrument(&name);
        let (count, _sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1, "stop() must record exactly once, not twice");
    }

    #[test]
    fn timer_record_accepts_a_measured_duration() {
        let name = unique_name("facade_timer_record_seconds");
        timer(&name).record(Duration::from_millis(250));

        let instrument = expect_instrument(&name);
        let (count, sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 1);
        assert!(
            (sum - 0.25).abs() < 1e-6,
            "timers record seconds; got {sum}"
        );
    }

    // ── Caps ───────────────────────────────────────────────────

    #[test]
    fn cardinality_cap_drops_series_beyond_the_limit() {
        let name = unique_name("facade_cardinality_cap");
        for i in 0..=MAX_SERIES_PER_METRIC {
            counter(&name)
                .with_label("shard", i.to_string())
                .increment(1);
        }

        let instrument = expect_instrument(&name);
        assert_eq!(
            instrument.series.len(),
            MAX_SERIES_PER_METRIC,
            "the cap must hold the series count at {MAX_SERIES_PER_METRIC}"
        );
        assert_eq!(
            instrument.dropped_series, 1,
            "the one over-cap label set must be counted as dropped"
        );
    }

    #[test]
    fn dropped_series_counts_samples_not_distinct_label_sets() {
        // The counter and its `# HELP` line both promise *samples*: a hot call
        // site hammering one over-cap label set is the signal an operator
        // needs, and counting distinct sets would mean remembering exactly the
        // label sets the cap exists to stop remembering.
        let name = unique_name("facade_dropped_counts_samples");
        for i in 0..MAX_SERIES_PER_METRIC {
            counter(&name)
                .with_label("shard", i.to_string())
                .increment(1);
        }
        for _ in 0..3 {
            counter(&name).with_label("shard", "over-cap").increment(1);
        }

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.series.len(), MAX_SERIES_PER_METRIC);
        assert_eq!(
            instrument.dropped_series, 3,
            "three samples for one over-cap label set must count as three"
        );
    }

    #[test]
    fn label_count_beyond_the_cap_is_dropped_not_the_sample() {
        let name = unique_name("facade_label_cap");
        let mut handle = counter(&name);
        for i in 0..=MAX_LABELS_PER_SERIES {
            handle = handle.with_label(&format!("k{i}"), i.to_string());
        }
        handle.increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(counter_value(series), 1, "the sample is still recorded");
        assert_eq!(series.labels.len(), MAX_LABELS_PER_SERIES);
    }

    #[test]
    fn label_subset_beyond_the_cap_does_not_depend_on_call_order() {
        // Which labels survive the cap must be a function of the label *set*,
        // never of the order `with_label` happened to be called in — otherwise
        // the same ten labels applied two ways land in two distinct series.
        let name = unique_name("facade_label_cap_order");
        let keys: Vec<String> = (0..10).map(|i| format!("k{i}")).collect();

        let mut ascending = counter(&name);
        for key in &keys {
            ascending = ascending.with_label(key, "v");
        }
        ascending.increment(1);

        let mut descending = counter(&name);
        for key in keys.iter().rev() {
            descending = descending.with_label(key, "v");
        }
        descending.increment(1);

        let instrument = expect_instrument(&name);
        assert_eq!(
            instrument.series.len(),
            1,
            "label order must not split the sample across two series: {:?}",
            instrument.series
        );
        let series = only_series(&instrument);
        assert_eq!(counter_value(series), 2);
        let kept: Vec<&str> = series.labels.keys().map(String::as_str).collect();
        assert_eq!(
            kept,
            vec!["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7"],
            "the retained subset is the lexicographically smallest {MAX_LABELS_PER_SERIES}"
        );
    }

    // ── Name validation and kind conflicts ─────────────────────

    #[test]
    fn kind_conflict_yields_an_inert_handle_and_one_instrument() {
        let name = unique_name("facade_kind_conflict");
        counter(&name).increment(1);
        gauge(&name).set(99.0);

        let matching: Vec<_> = snapshot().into_iter().filter(|i| i.name == name).collect();
        assert_eq!(
            matching.len(),
            1,
            "first registration wins: exactly one instrument per name"
        );
        assert_eq!(matching[0].kind, InstrumentKind::Counter);
        assert_eq!(
            counter_value(only_series(&matching[0])),
            1,
            "the conflicting gauge write must be a no-op"
        );
    }

    #[test]
    fn autumn_prefixed_names_are_rejected() {
        let name = format!("autumn_{}", unique_name("facade_reserved"));
        counter(&name).increment(1);
        assert!(
            find(&name).is_none(),
            "the `autumn_` namespace is reserved for framework metrics"
        );
    }

    #[test]
    fn builtin_metric_family_names_are_rejected() {
        counter("autumn_http_requests_total").increment(1);
        gauge("autumn_http_requests_active").set(1.0);
        assert!(
            snapshot()
                .iter()
                .all(|i| !i.name.starts_with("autumn_http_")),
            "built-in family names must never be registrable through the facade"
        );
    }

    #[test]
    fn invalid_metric_names_are_rejected() {
        for name in ["", "0leading_digit", "has-hyphen", "has.dot", "has space"] {
            counter(name).increment(1);
            assert!(
                find(name).is_none(),
                "invalid metric name {name:?} must yield an inert handle"
            );
        }
    }

    #[test]
    fn colon_in_metric_name_is_rejected() {
        // `:` is reserved for recording rules even though Prometheus' own
        // grammar allows it in a metric name.
        let name = format!("ns:{}", unique_name("facade_colon"));
        counter(&name).increment(1);
        assert!(find(&name).is_none());
    }

    #[test]
    fn counter_colliding_with_an_existing_histograms_derived_name_is_rejected() {
        let base = unique_name("facade_collide_forward");
        histogram(&base).record(1.0);

        let derived = format!("{base}_count");
        counter(&derived).increment(1);
        assert!(
            find(&derived).is_none(),
            "`{derived}` is the histogram's derived count family"
        );

        let bucket = format!("{base}_bucket");
        counter(&bucket).increment(1);
        assert!(find(&bucket).is_none());

        let sum = format!("{base}_sum");
        gauge(&sum).set(1.0);
        assert!(find(&sum).is_none());
    }

    #[test]
    fn histogram_colliding_with_an_existing_derived_name_is_rejected() {
        let base = unique_name("facade_collide_reverse");
        counter(&format!("{base}_bucket")).increment(1);

        histogram(&base).record(1.0);
        assert!(
            find(&base).is_none(),
            "the histogram would emit `{base}_bucket`, which already exists"
        );
    }

    // ── Bucket configuration ───────────────────────────────────

    #[test]
    fn set_histogram_buckets_before_the_first_record_applies() {
        let name = unique_name("facade_buckets_before");
        set_histogram_buckets(&name, &[1.0, 2.0, 3.0]);
        histogram(&name).record(2.5);

        let instrument = expect_instrument(&name);
        assert_eq!(bucket_bounds(&instrument), vec!["1", "2", "3"]);
        let (_count, _sum, buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(
            buckets,
            vec![
                ("1".to_string(), 0),
                ("2".to_string(), 0),
                ("3".to_string(), 1),
                ("+Inf".to_string(), 1),
            ]
        );
    }

    #[test]
    fn set_histogram_buckets_after_registration_is_ignored() {
        let name = unique_name("facade_buckets_after");
        histogram(&name).record(0.03);
        set_histogram_buckets(&name, &[1.0, 2.0, 3.0]);

        assert_eq!(
            bucket_bounds(&expect_instrument(&name)),
            default_bucket_strings(),
            "bucket boundaries must not move once a scrape target has seen them"
        );
    }

    #[test]
    fn set_histogram_buckets_rejects_invalid_bounds() {
        for bounds in [
            vec![],
            vec![3.0, 2.0, 1.0],
            vec![1.0, 1.0],
            vec![0.0, 1.0],
            vec![-1.0, 1.0],
            vec![f64::NAN],
            vec![f64::INFINITY],
            (1..=21).map(f64::from).collect::<Vec<_>>(),
        ] {
            let name = unique_name("facade_buckets_invalid");
            set_histogram_buckets(&name, &bounds);
            histogram(&name).record(1.0);

            assert_eq!(
                bucket_bounds(&expect_instrument(&name)),
                default_bucket_strings(),
                "invalid bounds {bounds:?} must be ignored in favour of the defaults"
            );
        }
    }

    #[test]
    fn describe_histogram_then_set_histogram_buckets_keeps_both() {
        // The trap this ordering used to spring: `describe_*` registered the
        // instrument, freezing its bounds, so the custom buckets vanished with
        // no diagnostic at all.
        let name = unique_name("facade_describe_then_buckets");
        describe_histogram(&name, "how long a thing took");
        set_histogram_buckets(&name, &[1.0, 2.0, 3.0]);
        histogram(&name).record(2.5);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.help, "how long a thing took");
        assert_eq!(
            bucket_bounds(&instrument),
            vec!["1", "2", "3"],
            "the custom bounds must survive an earlier describe_histogram"
        );
    }

    #[test]
    fn set_histogram_buckets_then_describe_histogram_keeps_both() {
        let name = unique_name("facade_buckets_then_describe");
        set_histogram_buckets(&name, &[1.0, 2.0, 3.0]);
        describe_histogram(&name, "how long a thing took");
        histogram(&name).record(2.5);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.help, "how long a thing took");
        assert_eq!(bucket_bounds(&instrument), vec!["1", "2", "3"]);
    }

    // ── Describing ─────────────────────────────────────────────

    #[test]
    fn describe_alone_does_not_register_the_instrument() {
        let name = unique_name("facade_describe_only");
        describe_counter(&name, "never actually used");

        assert!(
            find(&name).is_none(),
            "describing a metric must not register it — a described-but-unused \
             metric stays out of the scrape entirely"
        );
    }

    #[test]
    fn describe_after_registration_still_applies() {
        let name = unique_name("facade_describe_after");
        counter(&name).increment(1);
        describe_counter(&name, "described late");

        assert_eq!(expect_instrument(&name).help, "described late");
    }

    #[test]
    fn describing_as_the_wrong_kind_does_not_apply_the_help() {
        let name = unique_name("facade_describe_wrong_kind");
        describe_gauge(&name, "described as a gauge");
        counter(&name).increment(1);

        let instrument = expect_instrument(&name);
        assert_eq!(instrument.kind, InstrumentKind::Counter);
        assert_eq!(
            instrument.help, "",
            "a description written for another kind must not be applied"
        );
    }

    #[test]
    fn help_text_is_stripped_of_control_characters_and_truncated() {
        let name = unique_name("facade_help_sanitized");
        let help = format!("first\nsecond\x1b[31m{}", "x".repeat(MAX_HELP_LEN));
        describe_counter(&name, help);
        counter(&name).increment(1);

        let help = expect_instrument(&name).help;
        assert!(
            !help.contains('\n') && !help.contains('\x1b'),
            "control characters must not survive into a HELP line: {help:?}"
        );
        assert_eq!(
            help.chars().count(),
            MAX_HELP_LEN,
            "help must be truncated to the cap"
        );
        assert!(help.starts_with("firstsecond[31m"));
    }

    // ── Snapshot shape ─────────────────────────────────────────

    #[test]
    fn snapshot_is_sorted_by_instrument_name() {
        let base = unique_name("facade_sorted");
        counter(&format!("{base}_zulu")).increment(1);
        counter(&format!("{base}_alpha")).increment(1);
        gauge(&format!("{base}_mike")).set(1.0);

        let names: Vec<String> = snapshot().into_iter().map(|i| i.name).collect();
        assert!(
            names.windows(2).all(|w| w[0] <= w[1]),
            "snapshot must be sorted by name: {names:?}"
        );
        let ours: Vec<&String> = names.iter().filter(|n| n.starts_with(&base)).collect();
        assert_eq!(
            ours,
            vec![
                &format!("{base}_alpha"),
                &format!("{base}_mike"),
                &format!("{base}_zulu")
            ]
        );
    }

    #[test]
    fn snapshot_series_are_sorted_by_canonical_label_key() {
        let name = unique_name("facade_series_sorted");
        for shard in ["c", "a", "b"] {
            counter(&name).with_label("shard", shard).increment(1);
        }

        let instrument = expect_instrument(&name);
        let shards: Vec<&str> = instrument
            .series
            .iter()
            .map(|s| s.labels.get("shard").map_or("", String::as_str))
            .collect();
        assert_eq!(shards, vec!["a", "b", "c"]);
    }

    // ── Length caps and sanitization ───────────────────────────

    #[test]
    fn over_long_metric_names_are_rejected_not_truncated() {
        let stem = unique_name("facade_long_name");
        let name = format!("{stem}_{}", "x".repeat(MAX_METRIC_NAME_LEN));
        assert!(name.len() > MAX_METRIC_NAME_LEN);

        counter(&name).increment(1);
        assert!(find(&name).is_none(), "an over-long name must be rejected");
        assert!(
            snapshot().iter().all(|i| !i.name.starts_with(&stem)),
            "a rejected name must not be truncated into a registration either"
        );
    }

    #[test]
    fn metric_names_at_the_cap_are_accepted() {
        let stem = unique_name("facade_cap_name");
        let name = format!("{stem}{}", "x".repeat(MAX_METRIC_NAME_LEN - stem.len()));
        assert_eq!(name.len(), MAX_METRIC_NAME_LEN);

        counter(&name).increment(1);
        assert_eq!(counter_value(only_series(&expect_instrument(&name))), 1);
    }

    #[test]
    fn over_long_label_names_are_dropped_not_the_sample() {
        let name = unique_name("facade_long_label_name");
        let long_key = "k".repeat(MAX_LABEL_NAME_LEN + 1);
        counter(&name)
            .with_label(&long_key, "x")
            .with_label("status", "paid")
            .increment(1);

        let instrument = expect_instrument(&name);
        let series = only_series(&instrument);
        assert_eq!(
            counter_value(series),
            1,
            "the sample must still be recorded"
        );
        assert_eq!(
            series.labels.len(),
            1,
            "only `status` survives: {:?}",
            series.labels
        );
    }

    #[test]
    fn control_characters_are_stripped_from_label_values() {
        let name = unique_name("facade_label_control_chars");
        counter(&name)
            .with_label("status", "pa\rid\x1b[31m\nnext")
            .increment(1);

        let instrument = expect_instrument(&name);
        let value = only_series(&instrument)
            .labels
            .get("status")
            .expect("the label survives")
            .clone();
        assert_eq!(
            value, "paid[31mnext",
            "C0 control characters must not reach the exposition format"
        );
    }

    #[test]
    fn sanitize_for_log_escapes_and_truncates() {
        let injected = sanitize_for_log("evil\nWARN forged log line");
        assert!(
            !injected.contains('\n'),
            "a rejected name must not be able to forge a log line: {injected}"
        );
        assert_eq!(injected, "evil\\nWARN forged log line");

        let long = sanitize_for_log(&"x".repeat(LOG_NAME_PREVIEW_LEN * 4));
        assert_eq!(long.chars().count(), LOG_NAME_PREVIEW_LEN + 1);
        assert!(long.ends_with('…'));
    }

    // ── Value validation and numeric ergonomics ────────────────

    #[test]
    fn gauge_rejects_non_finite_values() {
        let name = unique_name("facade_gauge_non_finite");
        let handle = gauge(&name);
        handle.set(7.0);
        handle.set(f64::NAN);
        handle.set(f64::INFINITY);
        handle.set(f64::NEG_INFINITY);
        handle.increment(f64::NAN);
        handle.decrement(f64::INFINITY);

        let instrument = expect_instrument(&name);
        let value = gauge_value(only_series(&instrument));
        assert!(
            (value - 7.0).abs() < f64::EPSILON,
            "a rejected value must leave the gauge untouched, got {value}"
        );
        // The JSON view maps non-finite floats to `null`, so a gauge that
        // accepted one would disagree with its own prometheus rendering.
        let json = serde_json::to_value(&instrument).unwrap();
        assert_eq!(json["series"][0]["value"]["value"].as_f64(), Some(7.0));
    }

    #[test]
    fn gauge_adjustments_saturate_instead_of_overflowing_to_infinity() {
        let name = unique_name("facade_gauge_saturating");
        let handle = gauge(&name);
        handle.set(f64::MAX);
        handle.increment(f64::MAX); // finite delta, but MAX + MAX == +Inf

        let instrument = expect_instrument(&name);
        let value = gauge_value(only_series(&instrument));
        assert!(
            value.is_finite(),
            "a finite adjustment must never store a non-finite gauge, got {value}"
        );
        assert!((value - f64::MAX).abs() < f64::EPSILON * f64::MAX);

        handle.set(f64::MIN);
        handle.decrement(f64::MAX);
        let instrument = expect_instrument(&name);
        let value = gauge_value(only_series(&instrument));
        assert!(value.is_finite(), "saturation must hold downward too");
    }

    #[test]
    fn histogram_sum_saturates_instead_of_overflowing_to_infinity() {
        let name = unique_name("facade_histogram_sum_saturating");
        let handle = histogram(&name);
        handle.record(f64::MAX);
        handle.record(f64::MAX);

        let instrument = expect_instrument(&name);
        let (count, sum, _buckets) = histogram_parts(only_series(&instrument));
        assert_eq!(count, 2);
        assert!(
            sum.is_finite(),
            "_sum must saturate at f64::MAX, not poison to +Inf, got {sum}"
        );
    }

    #[test]
    fn gauge_and_histogram_accept_integer_types() {
        let queue: Vec<u8> = vec![1, 2, 3];
        let gauge_name = unique_name("facade_integer_gauge");
        gauge(&gauge_name).set(queue.len()); // usize
        gauge(&gauge_name).increment(2_u64);
        gauge(&gauge_name).decrement(1_i64);

        let value = gauge_value(only_series(&expect_instrument(&gauge_name)));
        assert!((value - 4.0).abs() < f64::EPSILON, "got {value}");

        let hist_name = unique_name("facade_integer_histogram");
        histogram(&hist_name).record(2_048_usize);
        let (count, sum, _buckets) = histogram_parts(only_series(&expect_instrument(&hist_name)));
        assert_eq!(count, 1);
        assert!((sum - 2048.0).abs() < f64::EPSILON, "got {sum}");
    }

    #[test]
    fn counter_saturates_instead_of_wrapping() {
        let name = unique_name("facade_counter_saturates");
        let handle = counter(&name);
        handle.increment(u64::MAX);
        handle.increment(3);

        assert_eq!(
            counter_value(only_series(&expect_instrument(&name))),
            u64::MAX,
            "a wrapped counter would look like a reset and blow up rate()"
        );
    }

    // ── Bucket bound formatting ────────────────────────────────

    #[test]
    fn format_bound_matches_go_g_formatting() {
        // client_golang renders `le` with Go's shortest %g: exponential
        // outside [1e-4, 1e6), exponent signed and padded to two digits.
        // (Go caps the %g precision decision at 6 for shortest formatting,
        // which is why `1000000.0` famously prints as `1e+06`.)
        assert_eq!(format_bound(0.000_05), "5e-05");
        assert_eq!(format_bound(0.000_025), "2.5e-05");
        assert_eq!(format_bound(1e6), "1e+06");
        assert_eq!(format_bound(2_500_000.0), "2.5e+06");
        assert_eq!(format_bound(1e20), "1e+20");
        assert_eq!(format_bound(1e21), "1e+21");
        assert_eq!(format_bound(1.5e22), "1.5e+22");
        assert_eq!(format_bound(1e-7), "1e-07");
        assert_eq!(format_bound(1e-100), "1e-100");
        // Just inside the window at both edges: plain decimal, no exponent.
        assert_eq!(format_bound(0.000_1), "0.0001");
        assert_eq!(format_bound(999_999.0), "999999");
    }

    #[test]
    fn default_bucket_strings_are_unchanged() {
        assert_eq!(
            default_bucket_strings(),
            vec![
                "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10"
            ],
            "the default `le` strings are a scrape-visible contract"
        );
    }

    #[test]
    fn tiny_and_huge_bounds_render_in_exponential_form() {
        let name = unique_name("facade_exponential_bounds");
        set_histogram_buckets(&name, &[0.000_025, 0.000_05, 1e21]);
        histogram(&name).record(1.0);

        assert_eq!(
            bucket_bounds(&expect_instrument(&name)),
            vec!["2.5e-05", "5e-05", "1e+21"]
        );
    }

    #[test]
    fn unique_name_produces_distinct_valid_metric_names() {
        let a = unique_name("facade_unique");
        let b = unique_name("facade_unique");
        assert_ne!(a, b);
        for name in [&a, &b] {
            let mut chars = name.chars();
            assert!(matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_'));
            assert!(chars.all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }
}
