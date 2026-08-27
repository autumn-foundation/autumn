//! In-memory shadow-run bookkeeping: counters plus a bounded ring of the most
//! recent divergences (issue #1653).
//!
//! Modelled on [`crate::actuator::TaskRegistry`] — a cheaply cloneable handle
//! over shared state that the actuator reads and the request path writes. Two
//! properties matter more here than they do for tasks:
//!
//! - **Bounded.** A mirror run against a badly-regressed candidate diverges on
//!   *every* request. The counters are unbounded (they saturate rather than
//!   wrap), but the stored records are capped at `max_records` and identical
//!   divergences collapse onto one record by
//!   [`fingerprint`](crate::shadow::diff::Divergence::fingerprint) — so a
//!   thousand copies of one regression occupy one slot, and the ring keeps
//!   showing distinct problems rather than a thousand copies of the loudest.
//! - **Clock-free.** The observation time is a parameter, never read from a
//!   clock in here, so the whole recording path stays a pure function of its
//!   inputs and a captured request replays to a byte-identical record.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::shadow::diff::{Comparison, Divergence};

/// Aggregate counters for a shadow run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ShadowStats {
    /// Requests selected for mirroring.
    pub mirrored: u64,
    /// Mirrored requests whose two responses were successfully compared.
    pub compared: u64,
    /// Comparisons where the builds agreed.
    pub matched: u64,
    /// Comparisons where they did not.
    pub diverged: u64,
    /// Shadow requests that failed at the transport (connection refused, DNS,
    /// TLS, a malformed response).
    pub shadow_errors: u64,
    /// Shadow requests abandoned at the configured deadline.
    pub shadow_timeouts: u64,
    /// Requests dropped without being mirrored because the in-flight ceiling
    /// was already reached.
    pub dropped_at_capacity: u64,
    /// Mirrored requests not compared because a response body exceeded
    /// `shadow.max_body_bytes`.
    pub skipped_oversize: u64,
}

/// The request a divergence was observed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestContext {
    /// HTTP method (always `GET` or `HEAD` in this slice).
    pub method: String,
    /// Request target with sensitive query parameters already redacted — see
    /// [`crate::shadow::diff::redact_path_and_query`].
    pub target: String,
    /// The bounded route label this request maps to (a configured pattern, or
    /// `"*"`). Used as the metric dimension too.
    pub route: String,
}

/// One stored divergence: the request it happened on, the comparison detail,
/// and how often this exact divergence has recurred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DivergenceRecord {
    /// HTTP method.
    pub method: String,
    /// Redacted request target.
    pub target: String,
    /// Bounded route label.
    pub route: String,
    /// Times this fingerprint has been seen.
    pub occurrences: u64,
    /// Epoch milliseconds of the first occurrence.
    pub first_observed_at_ms: u64,
    /// Epoch milliseconds of the most recent occurrence.
    pub last_observed_at_ms: u64,
    /// The comparison detail (kind, statuses, digests, redacted samples).
    #[serde(flatten)]
    pub divergence: Divergence,
}

/// Everything `{actuator-prefix}/shadow` publishes.
#[derive(Clone, Debug, Serialize)]
pub struct ShadowSnapshot {
    /// Whether mirroring is switched on for this replica.
    pub enabled: bool,
    /// The configured candidate target, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Aggregate counters.
    pub stats: ShadowStats,
    /// Most recent distinct divergences, oldest first.
    pub divergences: Vec<DivergenceRecord>,
}

/// Shared, cheaply cloneable shadow-run state.
#[derive(Clone, Debug)]
pub struct ShadowRegistry {
    counters: Arc<Counters>,
    records: Arc<RwLock<VecDeque<DivergenceRecord>>>,
    max_records: usize,
}

#[derive(Debug, Default)]
struct Counters {
    mirrored: AtomicU64,
    compared: AtomicU64,
    matched: AtomicU64,
    diverged: AtomicU64,
    shadow_errors: AtomicU64,
    shadow_timeouts: AtomicU64,
    dropped_at_capacity: AtomicU64,
    skipped_oversize: AtomicU64,
}

/// Saturating increment: a long-lived replica must not wrap a counter back to
/// zero and report a healthy mirror that has in fact diverged 2^64 times.
fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

impl ShadowRegistry {
    /// Create a registry that keeps at most `max_records` distinct divergences.
    ///
    /// `max_records` of `0` is clamped to `1` so the ring can always hold the
    /// evidence for at least one divergence; config validation rejects `0`
    /// before this, so the clamp is belt-and-braces.
    #[must_use]
    pub fn new(max_records: usize) -> Self {
        Self {
            counters: Arc::new(Counters::default()),
            records: Arc::new(RwLock::new(VecDeque::new())),
            max_records: max_records.max(1),
        }
    }

    /// Count a request selected for mirroring.
    pub fn record_mirrored(&self) {
        bump(&self.counters.mirrored);
    }

    /// Count a request dropped because the in-flight ceiling was reached.
    pub fn record_dropped_at_capacity(&self) {
        bump(&self.counters.dropped_at_capacity);
    }

    /// Count a shadow request that failed at the transport.
    pub fn record_shadow_error(&self) {
        bump(&self.counters.shadow_errors);
    }

    /// Count a shadow request abandoned at its deadline.
    pub fn record_shadow_timeout(&self) {
        bump(&self.counters.shadow_timeouts);
    }

    /// Count a mirrored request whose body was too large to compare.
    pub fn record_skipped_oversize(&self) {
        bump(&self.counters.skipped_oversize);
    }

    /// Record the outcome of one comparison, observed at `observed_at_ms`.
    pub fn record_comparison(
        &self,
        context: &RequestContext,
        comparison: Comparison,
        observed_at_ms: u64,
    ) {
        bump(&self.counters.compared);
        let divergence = match comparison {
            Comparison::Match => {
                bump(&self.counters.matched);
                return;
            }
            Comparison::Diverged(divergence) => *divergence,
        };
        bump(&self.counters.diverged);

        let Ok(mut records) = self.records.write() else {
            // A poisoned lock means a previous writer panicked while holding
            // it. The counters above still tell the operator divergences are
            // happening; losing the sample is strictly better than propagating
            // a panic into a detached mirror task.
            return;
        };
        if let Some(existing) = records
            .iter_mut()
            .find(|record| record.divergence.fingerprint == divergence.fingerprint)
        {
            existing.occurrences = existing.occurrences.saturating_add(1);
            existing.last_observed_at_ms = observed_at_ms;
            return;
        }
        while records.len() >= self.max_records {
            records.pop_front();
        }
        records.push_back(DivergenceRecord {
            method: context.method.clone(),
            target: context.target.clone(),
            route: context.route.clone(),
            occurrences: 1,
            first_observed_at_ms: observed_at_ms,
            last_observed_at_ms: observed_at_ms,
            divergence,
        });
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> ShadowStats {
        ShadowStats {
            mirrored: self.counters.mirrored.load(Ordering::Relaxed),
            compared: self.counters.compared.load(Ordering::Relaxed),
            matched: self.counters.matched.load(Ordering::Relaxed),
            diverged: self.counters.diverged.load(Ordering::Relaxed),
            shadow_errors: self.counters.shadow_errors.load(Ordering::Relaxed),
            shadow_timeouts: self.counters.shadow_timeouts.load(Ordering::Relaxed),
            dropped_at_capacity: self.counters.dropped_at_capacity.load(Ordering::Relaxed),
            skipped_oversize: self.counters.skipped_oversize.load(Ordering::Relaxed),
        }
    }

    /// The stored divergences, oldest first.
    #[must_use]
    pub fn recent(&self) -> Vec<DivergenceRecord> {
        self.records
            .read()
            .map(|records| records.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Build the payload `{actuator-prefix}/shadow` returns.
    #[must_use]
    pub fn snapshot(&self, enabled: bool, target: Option<&str>) -> ShadowSnapshot {
        ShadowSnapshot {
            enabled,
            target: target.map(ToOwned::to_owned),
            stats: self.stats(),
            divergences: self.recent(),
        }
    }

    /// Drive the `mirrored` counter to its ceiling so a test can prove the next
    /// increment saturates instead of wrapping.
    #[cfg(test)]
    fn pin_mirrored_at_ceiling_for_tests(&self) {
        self.counters.mirrored.store(u64::MAX, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::filter::ParameterFilter;
    use crate::shadow::diff::{Comparison, ResponseFacts, compare};
    use bytes::Bytes;

    fn facts(status: u16, body: &str) -> ResponseFacts {
        ResponseFacts::new(
            status,
            Some("application/json".to_owned()),
            Bytes::from(body.to_owned()),
        )
    }

    fn context() -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            target: "/api/orders".to_owned(),
            route: "/api/*".to_owned(),
        }
    }

    fn diverging(primary: &str, shadow: &str) -> Comparison {
        compare(
            &facts(200, primary),
            &facts(200, shadow),
            &ParameterFilter::default(),
            2048,
        )
    }

    #[test]
    fn a_fresh_registry_is_empty() {
        let registry = ShadowRegistry::new(10);
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 0);
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.diverged, 0);
        assert!(registry.recent().is_empty());
    }

    #[test]
    fn a_match_is_counted_but_not_recorded() {
        let registry = ShadowRegistry::new(10);
        registry.record_comparison(&context(), diverging("{\"a\":1}", "{\"a\":1}"), 1_000);
        let stats = registry.stats();
        assert_eq!(stats.compared, 1);
        assert_eq!(stats.matched, 1);
        assert_eq!(stats.diverged, 0);
        assert!(registry.recent().is_empty());
    }

    #[test]
    fn a_divergence_is_counted_and_recorded() {
        let registry = ShadowRegistry::new(10);
        registry.record_comparison(&context(), diverging("{\"a\":1}", "{}"), 1_000);
        let stats = registry.stats();
        assert_eq!(stats.compared, 1);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.diverged, 1);
        let recent = registry.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].method, "GET");
        assert_eq!(recent[0].route, "/api/*");
        assert_eq!(recent[0].occurrences, 1);
        assert_eq!(recent[0].first_observed_at_ms, 1_000);
        assert_eq!(recent[0].last_observed_at_ms, 1_000);
    }

    #[test]
    fn repeat_divergences_collapse_by_fingerprint() {
        let registry = ShadowRegistry::new(10);
        for at in [1_000, 2_000, 3_000] {
            registry.record_comparison(&context(), diverging("{\"a\":1}", "{}"), at);
        }
        let recent = registry.recent();
        assert_eq!(
            recent.len(),
            1,
            "the same divergence must not fill the ring"
        );
        assert_eq!(recent[0].occurrences, 3);
        assert_eq!(recent[0].first_observed_at_ms, 1_000);
        assert_eq!(recent[0].last_observed_at_ms, 3_000);
        assert_eq!(registry.stats().diverged, 3);
    }

    #[test]
    fn the_record_ring_is_bounded() {
        let registry = ShadowRegistry::new(3);
        for n in 0_u64..10 {
            registry.record_comparison(
                &context(),
                diverging(&format!("{{\"a\":{n}}}"), "{}"),
                1_000 + n,
            );
        }
        let recent = registry.recent();
        assert_eq!(recent.len(), 3, "ring must be capped at max_records");
        assert_eq!(registry.stats().diverged, 10, "counters are not capped");
        // The newest survive; the oldest are evicted.
        assert_eq!(recent[2].last_observed_at_ms, 1_009);
    }

    #[test]
    fn operational_counters_move_independently() {
        let registry = ShadowRegistry::new(10);
        registry.record_mirrored();
        registry.record_mirrored();
        registry.record_dropped_at_capacity();
        registry.record_shadow_error();
        registry.record_shadow_timeout();
        registry.record_skipped_oversize();
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 2);
        assert_eq!(stats.dropped_at_capacity, 1);
        assert_eq!(stats.shadow_errors, 1);
        assert_eq!(stats.shadow_timeouts, 1);
        assert_eq!(stats.skipped_oversize, 1);
        assert_eq!(stats.compared, 0);
    }

    #[test]
    fn clones_share_one_registry() {
        let registry = ShadowRegistry::new(10);
        let clone = registry.clone();
        clone.record_mirrored();
        assert_eq!(registry.stats().mirrored, 1);
    }

    #[test]
    fn the_snapshot_serializes_the_shape_the_actuator_publishes() {
        let registry = ShadowRegistry::new(10);
        registry.record_mirrored();
        registry.record_comparison(&context(), diverging("{\"a\":1}", "{}"), 1_000);
        let snapshot = registry.snapshot(true, Some("http://127.0.0.1:9091"));
        let json = serde_json::to_value(&snapshot).expect("must serialize");
        assert_eq!(json["enabled"], serde_json::json!(true));
        assert_eq!(json["target"], serde_json::json!("http://127.0.0.1:9091"));
        assert_eq!(json["stats"]["diverged"], serde_json::json!(1));
        assert_eq!(json["divergences"][0]["route"], serde_json::json!("/api/*"));
        assert_eq!(
            json["divergences"][0]["kind"],
            serde_json::json!("body"),
            "the divergence detail must be flattened into the record"
        );
    }

    #[test]
    fn a_disabled_snapshot_reports_no_target() {
        let registry = ShadowRegistry::new(10);
        let json = serde_json::to_value(registry.snapshot(false, None)).expect("must serialize");
        assert_eq!(json["enabled"], serde_json::json!(false));
        assert!(json.get("target").is_none() || json["target"].is_null());
    }

    #[test]
    fn counters_saturate_rather_than_wrapping() {
        let registry = ShadowRegistry::new(10);
        registry.pin_mirrored_at_ceiling_for_tests();
        registry.record_mirrored();
        assert_eq!(registry.stats().mirrored, u64::MAX);
    }
}
