//! Call-site facade for app-defined metrics (issue #1378).
//!
//! Recording an application metric should cost one line at the point where the
//! interesting thing happens — no traits to implement, no types to define, no
//! registry to wire into the app builder:
//!
//! ```rust,no_run
//! use autumn_web::metrics;
//!
//! # fn checkout() {
//! metrics::counter("checkout_completed_total")
//!     .with_label("status", "paid")
//!     .increment(1);
//! # }
//! ```
//!
//! Everything recorded here is exposed automatically by the actuator:
//! `/actuator/prometheus` renders the Prometheus text format and
//! `/actuator/metrics` renders the same data as JSON under a top-level `app`
//! key. Recording always works; only *exposure* is gated by
//! `actuator.prometheus`.
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
//! series (excess label sets are dropped and counted in
//! [`InstrumentSnapshot::dropped_series`](crate::metrics::InstrumentSnapshot::dropped_series)).
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

/// Maximum number of distinct label sets (series) retained per instrument.
///
/// Once an instrument holds this many series, further distinct label sets are
/// dropped and counted in [`InstrumentSnapshot::dropped_series`].
pub const MAX_SERIES_PER_METRIC: usize = 100;

/// Maximum number of distinct instruments the process-global registry holds.
pub const MAX_INSTRUMENTS: usize = 256;

/// Maximum number of labels retained on a single series; extras are dropped.
pub const MAX_LABELS_PER_SERIES: usize = 8;

/// Maximum length of a label value; longer values are truncated.
pub const MAX_LABEL_VALUE_LEN: usize = 128;

/// Default histogram bucket upper bounds, in seconds.
///
/// The implicit `+Inf` bucket is always present and is not listed here.
pub const DEFAULT_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Largest number of bucket upper bounds [`set_histogram_buckets`] accepts.
const MAX_BUCKET_BOUNDS: usize = 20;

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

/// Every registered instrument, plus the latches that keep warnings from
/// turning into hot-path log floods.
#[derive(Debug, Default)]
struct Registry {
    /// Registered instruments keyed by metric name.
    instruments: RwLock<HashMap<Box<str>, Arc<Instrument>>>,
    /// Bucket bounds configured before their histogram was first used.
    pending_buckets: RwLock<HashMap<Box<str>, Box<[f64]>>>,
    /// Names already warned about, so a rejected name warns exactly once.
    warned_names: RwLock<HashSet<Box<str>>>,
    /// Latches the single "too many instruments" warning.
    over_capacity_warned: AtomicBool,
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
            metric = %name,
            reason,
            "app metric rejected; recording through an inert handle"
        );
    }

    /// Warn once, process-wide, that the instrument cap is exhausted.
    fn warn_over_capacity(&self, name: &str) {
        if !self.over_capacity_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %name,
                cap = MAX_INSTRUMENTS,
                "app metric registry is at capacity; further new metric names are ignored"
            );
        }
    }
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
    /// Allocation-free fast path for the (overwhelmingly common) no-label case.
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
            update(&self.unlabeled);
            self.unlabeled_used.store(true, Ordering::Relaxed);
            return;
        }
        let key = self.canonical_key(labels);
        if key.is_empty() {
            // Every label was rejected; the sample still belongs somewhere.
            update(&self.unlabeled);
            self.unlabeled_used.store(true, Ordering::Relaxed);
            return;
        }
        if let Some(series) = self.series_for(&key) {
            update(&series);
        }
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

    /// Count one dropped label set and warn about it at most once.
    fn note_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if !self.cap_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %self.name,
                cap = MAX_SERIES_PER_METRIC,
                "app metric hit its series cardinality cap; further label sets are dropped. \
                 Label values must come from a small closed set — never user input or IDs"
            );
        }
    }

    /// Canonicalize a handle's pending labels into a series key: invalid and
    /// reserved names dropped, duplicates resolved first-wins, values
    /// truncated, at most [`MAX_LABELS_PER_SERIES`] kept, sorted by key.
    fn canonical_key(&self, labels: &[(String, String)]) -> SeriesKey {
        let mut kept: Vec<(Box<str>, Box<str>)> =
            Vec::with_capacity(labels.len().min(MAX_LABELS_PER_SERIES));
        for (key, value) in labels {
            if !is_acceptable_label_name(key) {
                self.warn_labels("invalid or reserved label name");
                continue;
            }
            if kept.iter().any(|(existing, _)| **existing == **key) {
                self.warn_labels("duplicate label name");
                continue;
            }
            if kept.len() >= MAX_LABELS_PER_SERIES {
                self.warn_labels("too many labels");
                continue;
            }
            kept.push((key.as_str().into(), truncate_label_value(value)));
        }
        kept.sort_by(|(a, _), (b, _)| a.cmp(b));
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
    fn warn_value(&self, value: f64) {
        if !self.value_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = %self.name,
                value,
                "histogram observation must be finite and non-negative; dropping it"
            );
        }
    }

    /// Point-in-time view of this instrument.
    fn snapshot(&self) -> InstrumentSnapshot {
        let help = {
            let help = self.help.read().unwrap_or_else(PoisonError::into_inner);
            help.to_string()
        };
        let labeled: Vec<(SeriesKey, Arc<Series>)> = {
            let series = self.series.read().unwrap_or_else(PoisonError::into_inner);
            series
                .iter()
                .map(|(key, value)| (key.clone(), Arc::clone(value)))
                .collect()
        };

        let mut series = Vec::with_capacity(labeled.len() + 1);
        if self.unlabeled_used.load(Ordering::Relaxed) {
            series.push(SeriesSnapshot {
                labels: BTreeMap::new(),
                value: self.unlabeled.value(&self.bounds),
            });
        }
        for (key, value) in labeled {
            series.push(SeriesSnapshot {
                labels: key
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
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
            buckets: self.bounds.to_vec(),
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

    /// Add `amount` to a counter series.
    fn add(&self, amount: u64) {
        if let Self::Counter(total) = self {
            total.fetch_add(amount, Ordering::Relaxed);
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
        Some((f64::from_bits(bits) + delta).to_bits())
    });
}

/// Render a bucket bound the way it appears in an `le` label.
fn format_bound(bound: f64) -> String {
    bound.to_string()
}

/// Truncate an over-long label value on a character boundary.
fn truncate_label_value(value: &str) -> Box<str> {
    // A string of at most `MAX_LABEL_VALUE_LEN` bytes can hold at most that
    // many characters, so the common case never walks the string.
    if value.len() <= MAX_LABEL_VALUE_LEN {
        return value.into();
    }
    value
        .chars()
        .take(MAX_LABEL_VALUE_LEN)
        .collect::<String>()
        .into_boxed_str()
}

/// Whether `name` may be used as a label key on an app metric.
fn is_acceptable_label_name(name: &str) -> bool {
    crate::actuator::is_valid_label_name(name)
        && !name.starts_with("__")
        && !RESERVED_LABEL_NAMES.contains(&name)
}

/// Whether `name` may be registered as an app metric.
///
/// Stricter than Prometheus' own grammar: `:` is reserved for recording rules
/// and the `autumn_` namespace belongs to the framework's built-in families.
fn registration_rejection(name: &str) -> Option<&'static str> {
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

/// Bucket bounds to give a histogram named `name` at registration.
fn bounds_for(name: &str) -> Box<[f64]> {
    let pending = REGISTRY
        .pending_buckets
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    pending
        .get(name)
        .map_or_else(|| Box::from(DEFAULT_BUCKETS.as_slice()), Clone::clone)
}

/// What happened under the registry's write lock, decided while the lock was
/// held and acted on (warnings included) only after it was released.
enum Registration {
    /// This call registered the instrument.
    Registered(Arc<Instrument>),
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

    let bounds = if matches!(kind, InstrumentKind::Histogram) {
        bounds_for(name)
    } else {
        Box::default()
    };
    let fresh = Arc::new(Instrument::new(name, kind, bounds));

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
                registered.insert(name.into(), Arc::clone(&fresh));
                Registration::Registered(fresh)
            }
        };
        drop(registered);
        outcome
    };

    match outcome {
        Registration::Registered(instrument) => {
            if matches!(kind, InstrumentKind::Histogram) {
                // The bounds are frozen into the instrument now; drop the
                // pending entry so the map cannot grow without bound.
                let mut pending = REGISTRY
                    .pending_buckets
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                pending.remove(name);
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
    if !existing.kind_warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            metric = %existing.name,
            registered = ?existing.kind,
            requested = ?kind,
            "app metric is already registered as a different kind; recording through an inert handle"
        );
    }
    None
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
/// `*_seconds` by convention.
pub fn timer(name: &str) -> Timer {
    Timer {
        histogram: histogram(name),
    }
}

/// Set the `# HELP` text of the counter named `name`.
pub fn describe_counter(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Counter, help.into());
}

/// Set the `# HELP` text of the gauge named `name`.
pub fn describe_gauge(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Gauge, help.into());
}

/// Set the `# HELP` text of the histogram (or timer) named `name`.
pub fn describe_histogram(name: &str, help: impl Into<String>) {
    describe(name, InstrumentKind::Histogram, help.into());
}

/// Register `name` if needed and replace its help text.
fn describe(name: &str, kind: InstrumentKind, help: String) {
    if let Some(instrument) = instrument(name, kind) {
        let mut slot = instrument
            .help
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = help.into_boxed_str();
    }
}

/// Override the bucket upper bounds of the histogram (or timer) named `name`.
///
/// Only effective *before* that histogram is first used: the bounds are frozen
/// into the instrument when it is registered, so a later call is ignored with a
/// warning rather than moving bucket boundaries under a running scrape target.
/// Call it during startup, next to the matching [`describe_histogram`].
///
/// `upper_bounds` must hold 1..=20 finite, strictly ascending, positive values;
/// anything else is ignored with a warning and the defaults are kept.
pub fn set_histogram_buckets(name: &str, upper_bounds: &[f64]) {
    if let Some(reason) = registration_rejection(name) {
        REGISTRY.warn_once(name, reason);
        return;
    }
    if !are_valid_bounds(upper_bounds) {
        tracing::warn!(
            metric = %name,
            bounds = ?upper_bounds,
            "histogram bucket bounds must be 1..=20 finite, positive, strictly ascending values; \
             keeping the defaults"
        );
        return;
    }

    let already_registered = {
        let registered = REGISTRY
            .instruments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        registered.contains_key(name)
    };
    if already_registered {
        tracing::warn!(
            metric = %name,
            "histogram bucket bounds are frozen once the histogram is first used; ignoring"
        );
        return;
    }

    let full = {
        let mut pending = REGISTRY
            .pending_buckets
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if pending.len() >= MAX_INSTRUMENTS && !pending.contains_key(name) {
            true
        } else {
            pending.insert(name.into(), upper_bounds.into());
            false
        }
    };
    if full {
        REGISTRY.warn_over_capacity(name);
    }
}

/// Whether `bounds` is a usable set of histogram bucket upper bounds.
fn are_valid_bounds(bounds: &[f64]) -> bool {
    !bounds.is_empty()
        && bounds.len() <= MAX_BUCKET_BOUNDS
        && bounds.iter().all(|b| b.is_finite() && *b > 0.0)
        && bounds.windows(2).all(|w| w[0] < w[1])
}

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
    /// invalid or reserved key drops that label, never the sample.
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.labels.push((key.to_owned(), value.into()));
        self
    }

    /// Add `amount` to this counter's series.
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
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.labels.push((key.to_owned(), value.into()));
        self
    }

    /// Set this gauge's series to `value`.
    pub fn set(&self, value: impl Into<f64>) {
        let value = value.into();
        if let Some(instrument) = self.instrument.as_ref() {
            instrument.record(&self.labels, |series| series.set(value));
        }
    }

    /// Add `delta` to this gauge's series.
    pub fn increment(&self, delta: impl Into<f64>) {
        let delta = delta.into();
        if let Some(instrument) = self.instrument.as_ref() {
            instrument.record(&self.labels, |series| series.adjust(delta));
        }
    }

    /// Subtract `delta` from this gauge's series.
    pub fn decrement(&self, delta: impl Into<f64>) {
        let delta = delta.into();
        if let Some(instrument) = self.instrument.as_ref() {
            instrument.record(&self.labels, |series| series.adjust(-delta));
        }
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
    pub fn with_label(mut self, key: &str, value: impl Into<String>) -> Self {
        self.labels.push((key.to_owned(), value.into()));
        self
    }

    /// Record one observation.
    ///
    /// Non-finite and negative values are rejected with a warning so `_sum`
    /// can never become `NaN` and buckets stay meaningful.
    pub fn record(&self, value: impl Into<f64>) {
        let value = value.into();
        let Some(instrument) = self.instrument.as_ref() else {
            return;
        };
        if !value.is_finite() || value < 0.0 {
            instrument.warn_value(value);
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
        // `Instant::elapsed` is monotonic and saturating, and every step below
        // is allocation-light and panic-free, so a guard dropped while
        // unwinding cannot turn a panic into an abort.
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
pub struct InstrumentSnapshot {
    /// The instrument's metric name.
    pub name: String,
    /// The `# HELP` text, empty when never described.
    pub help: String,
    /// Which kind of instrument this is.
    pub kind: InstrumentKind,
    /// Every retained series, sorted by canonical label key.
    pub series: Vec<SeriesSnapshot>,
    /// How many distinct label sets were dropped by the cardinality cap.
    pub dropped_series: u64,
    /// Histogram bucket upper bounds (empty for counters and gauges).
    #[serde(skip)]
    pub buckets: Vec<f64>,
}

/// One labeled series of an instrument.
#[derive(Debug, Clone, Serialize)]
pub struct SeriesSnapshot {
    /// Canonical (sorted, deduplicated) labels of this series.
    pub labels: BTreeMap<String, String>,
    /// The series' value.
    pub value: SeriesValue,
}

/// The value carried by a [`SeriesSnapshot`].
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
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
        .warned_names
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    REGISTRY
        .over_capacity_warned
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
        assert_eq!(instrument.buckets, vec![1.0, 2.0, 3.0]);
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
    fn set_histogram_buckets_after_the_first_record_is_ignored() {
        let name = unique_name("facade_buckets_after");
        histogram(&name).record(0.03);
        set_histogram_buckets(&name, &[1.0, 2.0, 3.0]);

        let instrument = expect_instrument(&name);
        assert_eq!(
            instrument.buckets,
            DEFAULT_BUCKETS.to_vec(),
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

            let instrument = expect_instrument(&name);
            assert_eq!(
                instrument.buckets,
                DEFAULT_BUCKETS.to_vec(),
                "invalid bounds {bounds:?} must be ignored in favour of the defaults"
            );
        }
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
