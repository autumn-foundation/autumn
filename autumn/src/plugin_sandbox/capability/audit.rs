//! "What did this plugin do in the last hour?" (issue #1632).
//!
//! An operator who granted five capabilities to an artifact they did not audit
//! needs one place to answer that. Not five: a cache metric, an upstream's
//! access log, a job table and a `grep` of the application log is four places
//! and no answer, because none of them knows which plugin was responsible.
//!
//! So every capability call — allowed, denied or over quota — is recorded at the
//! one point that knows all of it — `CapabilityRuntime::record`, the single exit
//! every `dispatch` takes — and the records aggregate here:
//!
//! ```text
//!   request ──► CapabilityEvent × n ──► SandboxOutcome.activity
//!                                            │
//!                                            ▼
//!                                    PluginActivityLog  ──► ActivitySummary
//!                                    (bounded, windowed)     hosts called,
//!                                                            kv/db/job counts,
//!                                                            denials, quota hits
//! ```
//!
//! # What a record carries, and what it deliberately does not
//!
//! A record names the capability, the operation, and the *logical* target — the
//! key, the table, the hostname, the job type. It never carries a value, a row,
//! a request body or a response. An audit surface that logged what a plugin
//! stored would be a second copy of the tenant data the capability system exists
//! to contain, in a place with different access rules — and an operator asking
//! "what did it do" is asking about shape, not contents.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::super::manifest::SandboxCapability;
use super::DenialReason;

/// How one capability call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CapabilityOutcome {
    /// The call was performed.
    Allowed,
    /// A quota refused it.
    QuotaExceeded,
    /// A grant refused it.
    Denied(DenialReason),
}

impl fmt::Display for CapabilityOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed => f.write_str("allowed"),
            Self::QuotaExceeded => f.write_str("quota-exceeded"),
            Self::Denied(reason) => write!(f, "denied:{reason}"),
        }
    }
}

/// One thing a plugin did, or was stopped from doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CapabilityEvent {
    /// Which capability the call needed.
    pub capability: SandboxCapability,
    /// The operation, as the wire spells it (`kv-get`, `db-query`, …).
    pub operation: &'static str,
    /// The logical thing it named: a key, a table, a hostname, a job type.
    pub target: String,
    /// How it ended.
    pub outcome: CapabilityOutcome,
}

/// The most events one plugin's log holds.
///
/// Bounded, because the log is fed by an untrusted plugin's call rate and an
/// unbounded one would be that plugin's memory-exhaustion channel — the same
/// argument the per-request ledger's ceiling rests on, one level up. Oldest
/// entries are dropped first: an operator asking "in the last hour" wants the
/// recent end.
pub const MAX_LOG_EVENTS: usize = 4096;

/// A bounded, time-windowed record of what each plugin has been doing.
///
/// One log per application, keyed by plugin, because the question is asked per
/// plugin but the ceiling has to hold across all of them.
#[derive(Debug, Default)]
pub struct PluginActivityLog {
    /// A ring, not a `Vec`. Dropping the oldest entry from a `Vec` is a
    /// memmove of every entry behind it, once per ingested event for as long as
    /// the log stays full — quadratic in the call rate, under one mutex shared
    /// by every plugin and every concurrent request. `VecDeque::pop_front` is
    /// the same eviction in constant time.
    entries: Mutex<VecDeque<(Instant, String, CapabilityEvent)>>,
    /// Events the per-request ledgers could not hold, so a summary can say it
    /// is a floor rather than a count.
    dropped: Mutex<BTreeMap<String, u64>>,
}

impl PluginActivityLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record everything one request's runtime gathered.
    pub fn ingest(&self, plugin: &str, events: impl IntoIterator<Item = CapabilityEvent>) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        for event in events {
            if entries.len() >= MAX_LOG_EVENTS {
                entries.pop_front();
            }
            entries.push_back((now, plugin.to_owned(), event));
        }
    }

    /// Record that one request made more calls than its ledger could hold.
    ///
    /// Separate from [`ingest`](Self::ingest) because the number is not an
    /// event: there is nothing to say about the calls beyond that they happened
    /// and are not below. An operator reading a summary needs to know that
    /// before they read the counts.
    pub fn ingest_dropped(&self, plugin: &str, dropped: u64) {
        if dropped == 0 {
            return;
        }
        {
            let mut counts = self.dropped.lock().unwrap_or_else(PoisonError::into_inner);
            let counter = counts.entry(plugin.to_owned()).or_insert(0);
            *counter = counter.saturating_add(dropped);
        }
    }

    /// What `plugin` did within `window`.
    #[must_use]
    pub fn summary(&self, plugin: &str, window: Duration) -> ActivitySummary {
        let cutoff = Instant::now();
        let mut summary = ActivitySummary {
            plugin: plugin.to_owned(),
            window,
            ..ActivitySummary::default()
        };
        {
            // Scoped so the entries lock is released before the `dropped` one
            // is taken. Holding both at once would make this the only site that
            // orders the two, and the next reader to take them the other way
            // round would deadlock the log the operator reads during an
            // incident.
            let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            for (at, who, event) in entries.iter() {
                if who != plugin || cutoff.saturating_duration_since(*at) > window {
                    continue;
                }
                summary.record(event);
            }
        }
        summary.dropped = self
            .dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(plugin)
            .copied()
            .unwrap_or(0);
        summary
    }

    /// Every plugin this log has heard from.
    #[must_use]
    pub fn plugins(&self) -> Vec<String> {
        let mut names: Vec<String> = {
            let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            entries.iter().map(|(_, who, _)| who.clone()).collect()
        };
        names.sort();
        names.dedup();
        names
    }
}

/// The answer to "what did this plugin do".
///
/// Counts and names, never values. See the module header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ActivitySummary {
    /// Calls that were performed, by operation.
    pub allowed: BTreeMap<&'static str, u64>,
    /// Calls a grant refused, by operation.
    pub denied: BTreeMap<&'static str, u64>,
    /// Calls a quota refused, by operation.
    pub quota_hits: BTreeMap<&'static str, u64>,
    /// Hostnames actually called, and how often.
    pub hosts: BTreeMap<String, u64>,
    /// Tables touched, and how often.
    pub tables: BTreeMap<String, u64>,
    /// Job types enqueued, and how often.
    pub job_types: BTreeMap<String, u64>,
    /// Hosts, tables and job types the plugin reached for and did not get.
    pub refused_targets: BTreeMap<String, u64>,
    /// The plugin this summarises and the window it covers.
    ///
    /// Carried on the summary rather than passed again to the renderer: taking
    /// them twice let a caller print a header naming a window the counts were
    /// never taken over, which is a report that lies about its own scope.
    pub plugin: String,
    /// The window the counts were taken over.
    pub window: Duration,
    /// Calls that happened and are not counted above, because a per-request
    /// ledger filled up.
    ///
    /// A plugin sets its own quotas — they are in its manifest — so a ledger
    /// ceiling is reachable by a plugin that wants to reach it. Non-zero here
    /// means every count above is a **floor**, and an operator must not have to
    /// infer that from a suspiciously round number.
    pub dropped: u64,
}

impl ActivitySummary {
    fn record(&mut self, event: &CapabilityEvent) {
        let bucket = match event.outcome {
            CapabilityOutcome::Allowed => &mut self.allowed,
            CapabilityOutcome::QuotaExceeded => &mut self.quota_hits,
            CapabilityOutcome::Denied(_) => &mut self.denied,
        };
        let counter = bucket.entry(event.operation).or_default();
        *counter = counter.saturating_add(1);

        // The by-target breakdowns cover the three surfaces an operator asks
        // about by name. KV keys are deliberately absent: a plugin's key space
        // is chosen by the plugin and is unbounded, so a per-key breakdown would
        // be both useless and the log's own growth channel. The per-operation
        // counts above already answer "how much KV".
        // "Reached" rather than "allowed": a call whose answer was discarded
        // for being over a byte ceiling, or whose backend errored, still *left
        // the host*. Filing those under "refused" would make "hosts called"
        // undercount exactly the calls an operator most wants to see.
        let allowed = matches!(
            event.outcome,
            CapabilityOutcome::Allowed
                | CapabilityOutcome::Denied(
                    DenialReason::ResponseTooLarge | DenialReason::BackendError
                )
        );
        let bucket = match (event.capability, allowed) {
            (SandboxCapability::HttpOutbound, true) => Some(&mut self.hosts),
            (SandboxCapability::Db, true) => Some(&mut self.tables),
            (SandboxCapability::Jobs, true) => Some(&mut self.job_types),
            (
                SandboxCapability::HttpOutbound | SandboxCapability::Db | SandboxCapability::Jobs,
                false,
            ) => Some(&mut self.refused_targets),
            _ => None,
        };
        if let Some(bucket) = bucket {
            let counter = bucket.entry(event.target.clone()).or_default();
            *counter = counter.saturating_add(1);
        }
    }

    /// Whether nothing at all was recorded.
    ///
    /// Says nothing about [`dropped`](Self::dropped): an empty summary with a
    /// non-zero drop count is a plugin that was busy, not one that was idle.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.denied.is_empty() && self.quota_hits.is_empty()
    }

    /// One operator-readable block.
    ///
    /// `Display` rather than a `render(plugin, window)` method: the summary
    /// already knows both, and taking them again is how a header comes to name
    /// a window the counts were not taken over.
    fn write_report(&self, out: &mut String) {
        use std::fmt::Write as _;

        // `write!` to a `String` is infallible; results are dropped rather than
        // unwrapped so this stays panic-free by construction.
        let _ = writeln!(
            out,
            "sandboxed plugin `{plugin}` — last {secs}s",
            plugin = self.plugin,
            secs = self.window.as_secs()
        );
        if self.is_empty() && self.dropped == 0 {
            out.push_str("  no capability calls\n");
            return;
        }
        if self.dropped > 0 {
            let _ = writeln!(
                out,
                "  \u{26A0} {dropped} further call(s) were made and are NOT counted below: \
                 every number here is a floor",
                dropped = self.dropped
            );
        }
        for (label, bucket) in [
            ("performed", &self.allowed),
            ("denied", &self.denied),
            ("over quota", &self.quota_hits),
        ] {
            if bucket.is_empty() {
                continue;
            }
            let _ = writeln!(out, "  {label}:");
            for (operation, count) in bucket {
                let _ = writeln!(out, "    {operation} × {count}");
            }
        }
        for (label, bucket) in [
            ("hosts called", &self.hosts),
            ("tables touched", &self.tables),
            ("jobs enqueued", &self.job_types),
            ("targets refused", &self.refused_targets),
        ] {
            if bucket.is_empty() {
                continue;
            }
            let _ = writeln!(out, "  {label}:");
            for (target, count) in bucket {
                let _ = writeln!(out, "    {target} × {count}");
            }
        }
    }
}

impl fmt::Display for ActivitySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write_report(&mut out);
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::manifest::SandboxCapability;

    fn event(operation: &'static str, outcome: CapabilityOutcome) -> CapabilityEvent {
        CapabilityEvent {
            capability: SandboxCapability::Kv,
            operation,
            target: "cart".to_owned(),
            outcome,
        }
    }

    #[test]
    fn the_ring_evicts_the_oldest_rather_than_growing() {
        let log = PluginActivityLog::new();
        log.ingest(
            "shop",
            (0..MAX_LOG_EVENTS + 100).map(|_| event("kv-get", CapabilityOutcome::Allowed)),
        );
        let summary = log.summary("shop", Duration::from_secs(3600));
        assert_eq!(
            summary.allowed.get("kv-get").copied(),
            Some(MAX_LOG_EVENTS as u64),
            "the log holds its ceiling and no more"
        );
    }

    #[test]
    fn a_summary_covers_one_plugin_and_one_window() {
        let log = PluginActivityLog::new();
        log.ingest("shop", [event("kv-get", CapabilityOutcome::Allowed)]);
        log.ingest("other", [event("kv-set", CapabilityOutcome::Allowed)]);

        let shop = log.summary("shop", Duration::from_secs(3600));
        assert_eq!(shop.allowed.get("kv-get").copied(), Some(1));
        assert!(shop.allowed.get("kv-set").is_none(), "{shop:?}");

        // A zero-length window covers nothing, which is the same filter the
        // "last hour" in the acceptance criterion rests on.
        assert!(log.summary("shop", Duration::ZERO).is_empty());
        assert!(log.summary("nobody", Duration::from_secs(3600)).is_empty());
        assert_eq!(log.plugins(), vec!["other".to_owned(), "shop".to_owned()]);
    }

    #[test]
    fn a_truncated_summary_says_so_before_it_says_anything_else() {
        let log = PluginActivityLog::new();
        log.ingest("shop", [event("kv-get", CapabilityOutcome::Allowed)]);
        log.ingest_dropped("shop", 0);
        assert_eq!(log.summary("shop", Duration::from_secs(3600)).dropped, 0);

        log.ingest_dropped("shop", 7);
        log.ingest_dropped("shop", 5);
        let summary = log.summary("shop", Duration::from_secs(3600));
        assert_eq!(summary.dropped, 12);
        let rendered = summary.to_string();
        assert!(rendered.contains("floor"), "{rendered}");
        // The warning is above the counts, because an operator who reads the
        // numbers first has already been misled.
        let warn = rendered.find("floor").unwrap_or(usize::MAX);
        let count = rendered.find("kv-get").unwrap_or(0);
        assert!(warn < count, "{rendered}");
    }

    #[test]
    fn a_summary_names_the_window_it_was_taken_over() {
        let log = PluginActivityLog::new();
        log.ingest("shop", [event("kv-get", CapabilityOutcome::Allowed)]);
        let rendered = log.summary("shop", Duration::from_secs(60)).to_string();
        assert!(rendered.contains("last 60s"), "{rendered}");
        assert!(rendered.contains("`shop`"), "{rendered}");
    }
}
