//! "What did this plugin do in the last hour?" (issue #1632).
//!
//! An operator who granted five capabilities to an artifact they did not audit
//! needs one place to answer that. Not five: a cache metric, an upstream's
//! access log, a job table and a `grep` of the application log is four places
//! and no answer, because none of them knows which plugin was responsible.
//!
//! So every capability call — allowed, denied or over quota — is recorded at the
//! one point that knows all of it ([`CapabilityRuntime::record`]), and the
//! records aggregate here:
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

use std::collections::BTreeMap;
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
    entries: Mutex<Vec<(Instant, String, CapabilityEvent)>>,
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
                // Dropping from the front is O(n) per event, which is why the
                // ceiling is checked before the push rather than after: at most
                // one drain happens per ingested event, and only once the log is
                // already full.
                entries.remove(0);
            }
            entries.push((now, plugin.to_owned(), event));
        }
    }

    /// What `plugin` did within `window`.
    #[must_use]
    pub fn summary(&self, plugin: &str, window: Duration) -> ActivitySummary {
        let cutoff = Instant::now();
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut summary = ActivitySummary::default();
        for (at, who, event) in entries.iter() {
            if who != plugin || cutoff.saturating_duration_since(*at) > window {
                continue;
            }
            summary.record(event);
        }
        summary
    }

    /// Every plugin this log has heard from.
    #[must_use]
    pub fn plugins(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut names: Vec<String> = entries.iter().map(|(_, who, _)| who.clone()).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// The answer to "what did this plugin do".
///
/// Counts and names, never values. See the module header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
        let allowed = matches!(event.outcome, CapabilityOutcome::Allowed);
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

    /// Whether nothing at all happened.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.denied.is_empty() && self.quota_hits.is_empty()
    }

    /// One operator-readable block.
    #[must_use]
    pub fn render(&self, plugin: &str, window: Duration) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        // `write!` to a `String` is infallible; results are dropped rather than
        // unwrapped so this stays panic-free by construction.
        let _ = writeln!(
            out,
            "sandboxed plugin `{plugin}` — last {secs}s",
            secs = window.as_secs()
        );
        if self.is_empty() {
            out.push_str("  no capability calls\n");
            return out;
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
        out
    }
}
