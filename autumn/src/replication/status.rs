//! Observable replication state and the health indicator that publishes it
//! (issue #1628, AC #2 and phase 3).
//!
//! Replication lag is only a durability guarantee if an operator can *see* it,
//! and a replica is only trustworthy if a failure to ship — or a failure to
//! restore what was shipped — is loud. Both come from one shared handle:
//!
//! * the replication thread writes into [`ReplicationStatus`] on every tick;
//! * [`ReplicationHealthIndicator`] reads it, so lag, the current generation, the
//!   last verification and the last error land in `/actuator/health`;
//! * an indicator that stays non-healthy past the alerter's grace period is
//!   escalated onto every channel configured under #1610 — the existing
//!   health-indicator alert path, with its dedup and its recovery notice, rather
//!   than a bespoke alert of our own.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus, IndicatorGroup};

/// The name the replication health indicator registers under.
pub const INDICATOR_NAME: &str = "sqlite-replication";

/// A point-in-time view of the replicator, cheap to clone and to serialize.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplicationSnapshot {
    /// Credential-free description of the destination.
    pub destination: String,
    /// The generation currently being shipped, once one has been opened.
    pub generation: Option<String>,
    /// Last tick that completed without an error — the point the replica is
    /// known to be current to.
    pub last_success_at: Option<DateTime<Utc>>,
    /// Last tick that actually shipped bytes.
    pub last_shipped_at: Option<DateTime<Utc>>,
    /// Segments shipped since the process started.
    pub segments_shipped: u64,
    /// Base snapshots taken since the process started.
    pub snapshots_taken: u64,
    /// Uncompressed WAL bytes shipped since the process started.
    pub bytes_shipped: u64,
    /// WAL bytes written but not yet shipped, as of the last tick.
    pub pending_bytes: u64,
    /// The most recent failure, if the replicator is not currently healthy.
    pub last_error: Option<String>,
    /// When that failure happened.
    pub last_error_at: Option<DateTime<Utc>>,
    /// Consecutive failing ticks. Reset by any successful tick.
    pub consecutive_failures: u32,
    /// Last time a **real restore** of this replica succeeded.
    pub last_verified_at: Option<DateTime<Utc>>,
    /// Why the last verification failed, when it did.
    pub last_verify_error: Option<String>,
}

impl ReplicationSnapshot {
    /// How far behind the replica is as of `now`: the time since the last tick
    /// that completed cleanly. `None` before the first successful tick.
    #[must_use]
    pub fn lag(&self, now: DateTime<Utc>) -> Option<Duration> {
        let last = self.last_success_at?;
        // A clock that moved backwards yields a negative span, which is not a
        // *stale* replica — report zero lag rather than `None`, which the health
        // indicator would read as "never shipped".
        Some(
            now.signed_duration_since(last)
                .to_std()
                .unwrap_or(Duration::ZERO),
        )
    }
}

/// Shared, mutable replication state.
#[derive(Debug)]
pub struct ReplicationStatus {
    inner: Mutex<ReplicationSnapshot>,
}

impl ReplicationStatus {
    /// A status for a replicator shipping to `destination`.
    #[must_use]
    pub fn new(destination: impl Into<String>) -> Self {
        Self {
            inner: Mutex::new(ReplicationSnapshot {
                destination: destination.into(),
                ..ReplicationSnapshot::default()
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut ReplicationSnapshot) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    /// Read the current state.
    #[must_use]
    pub fn snapshot(&self) -> ReplicationSnapshot {
        self.with(|s| s.clone())
    }

    /// Record that a base snapshot opened `generation`.
    pub fn record_generation(&self, generation: &str, at: DateTime<Utc>) {
        self.with(|s| {
            s.generation = Some(generation.to_owned());
            s.snapshots_taken = s.snapshots_taken.saturating_add(1);
            s.last_success_at = Some(at);
            s.last_error = None;
            s.last_error_at = None;
            s.consecutive_failures = 0;
        });
    }

    /// Record a segment landing on the destination.
    pub fn record_segment(&self, bytes: u64, at: DateTime<Utc>) {
        self.with(|s| {
            s.segments_shipped = s.segments_shipped.saturating_add(1);
            s.bytes_shipped = s.bytes_shipped.saturating_add(bytes);
            s.last_shipped_at = Some(at);
        });
    }

    /// Record a tick that succeeded but left committed data un-shipped.
    ///
    /// Deliberately does not touch `last_success_at`: lag is measured from the
    /// last moment the destination was actually caught up, so a run of slow
    /// uploads that never drains the tail keeps lag growing instead of resetting
    /// it on every tick.
    pub fn record_tick_behind(&self, pending_bytes: u64) {
        self.with(|s| s.pending_bytes = pending_bytes);
    }

    /// Record a tick that completed with nothing left un-shipped.
    pub fn record_tick_ok(&self, pending_bytes: u64, at: DateTime<Utc>) {
        self.with(|s| {
            s.pending_bytes = pending_bytes;
            s.last_success_at = Some(at);
            s.last_error = None;
            s.last_error_at = None;
            s.consecutive_failures = 0;
        });
    }

    /// Record a failed tick. Does **not** advance `last_success_at`, so lag keeps
    /// growing for as long as shipping is broken.
    pub fn record_tick_error(&self, error: impl Into<String>, at: DateTime<Utc>) {
        self.with(|s| {
            s.last_error = Some(error.into());
            s.last_error_at = Some(at);
            s.consecutive_failures = s.consecutive_failures.saturating_add(1);
        });
    }

    /// Record the outcome of a periodic restore verification.
    pub fn record_verification(&self, result: Result<(), String>, at: DateTime<Utc>) {
        self.with(|s| match result {
            Ok(()) => {
                s.last_verified_at = Some(at);
                s.last_verify_error = None;
            }
            Err(detail) => s.last_verify_error = Some(detail),
        });
    }
}

/// Thresholds beyond which the replicator is considered unhealthy.
#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    /// Lag beyond which the indicator reports `Down`. Derived from the
    /// configured RPO.
    pub lag_alert_after: Duration,
    /// How long after startup the replicator is allowed to have shipped nothing
    /// before that counts as a failure.
    pub startup_grace: Duration,
}

/// Publishes [`ReplicationStatus`] as an `/actuator/health` indicator.
///
/// Registered in the `HealthOnly` group: replication falling behind is an
/// operator emergency, but it does not make the process unable to serve
/// requests, so it must not pull the pod out of the load balancer via `/ready`.
#[derive(Debug)]
pub struct ReplicationHealthIndicator {
    status: Arc<ReplicationStatus>,
    thresholds: HealthThresholds,
    started_at: DateTime<Utc>,
}

impl ReplicationHealthIndicator {
    /// Build an indicator over `status`.
    #[must_use]
    pub const fn new(
        status: Arc<ReplicationStatus>,
        thresholds: HealthThresholds,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            status,
            thresholds,
            started_at,
        }
    }

    /// Evaluate `snapshot` against the thresholds, as of `now`.
    ///
    /// Pure, so the decision table is unit-testable without a clock or a running
    /// replicator.
    #[must_use]
    pub fn evaluate(
        &self,
        snapshot: &ReplicationSnapshot,
        now: DateTime<Utc>,
    ) -> HealthCheckOutput {
        let mut details: HashMap<String, serde_json::Value> = HashMap::new();
        details.insert(
            "destination".to_owned(),
            serde_json::Value::String(snapshot.destination.clone()),
        );
        if let Some(generation) = &snapshot.generation {
            details.insert(
                "generation".to_owned(),
                serde_json::Value::String(generation.clone()),
            );
        }
        details.insert(
            "segments_shipped".to_owned(),
            serde_json::Value::from(snapshot.segments_shipped),
        );
        details.insert(
            "pending_bytes".to_owned(),
            serde_json::Value::from(snapshot.pending_bytes),
        );
        // Named for what it is. This is `lag_alert_after` — the threshold at
        // which lag turns this check unhealthy, which `replication::build` sets
        // to a multiple of the RPO. Publishing it as `rpo_seconds` reported 30
        // for the default 10-second objective, telling operators and their
        // monitoring the wrong number for the one thing this check exists to
        // guard. (`autumn db replica status` reports the configured RPO itself.)
        details.insert(
            "lag_alert_after_seconds".to_owned(),
            serde_json::Value::from(self.thresholds.lag_alert_after.as_secs()),
        );
        if let Some(at) = snapshot.last_verified_at {
            details.insert(
                "last_verified_at".to_owned(),
                serde_json::Value::String(at.to_rfc3339()),
            );
        }

        let lag = snapshot.lag(now).or_else(|| {
            // Before the first successful tick, measure from process start so a
            // replicator that never got going still trips the threshold.
            now.signed_duration_since(self.started_at).to_std().ok()
        });
        if let Some(lag) = lag {
            details.insert(
                "lag_seconds".to_owned(),
                serde_json::Value::from(lag.as_secs()),
            );
        }

        // A verification failure means the bytes offsite are not restorable —
        // the single most important thing this indicator can say.
        if let Some(detail) = &snapshot.last_verify_error {
            details.insert(
                "verification_error".to_owned(),
                serde_json::Value::String(detail.clone()),
            );
            return HealthCheckOutput {
                status: HealthStatus::Down,
                details,
            };
        }
        if let Some(error) = &snapshot.last_error {
            details.insert("error".to_owned(), serde_json::Value::String(error.clone()));
        }

        let grace = if snapshot.last_success_at.is_some() {
            Duration::ZERO
        } else {
            self.thresholds.startup_grace
        };
        let over_lag =
            lag.is_some_and(|lag| lag > self.thresholds.lag_alert_after.saturating_add(grace));
        if over_lag {
            HealthCheckOutput {
                status: HealthStatus::Down,
                details,
            }
        } else {
            HealthCheckOutput {
                status: HealthStatus::Up,
                details,
            }
        }
    }
}

impl HealthIndicator for ReplicationHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            let snapshot = self.status.snapshot();
            self.evaluate(&snapshot, Utc::now())
        })
    }

    fn group(&self) -> IndicatorGroup {
        IndicatorGroup::HealthOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indicator(
        status: &Arc<ReplicationStatus>,
        started_at: DateTime<Utc>,
    ) -> ReplicationHealthIndicator {
        ReplicationHealthIndicator::new(
            Arc::clone(status),
            HealthThresholds {
                lag_alert_after: Duration::from_secs(30),
                startup_grace: Duration::from_secs(60),
            },
            started_at,
        )
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("timestamp")
    }

    #[test]
    fn a_freshly_started_replicator_is_up_during_the_startup_grace() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        let health = indicator(&status, at(0));
        let output = health.evaluate(&status.snapshot(), at(10));
        assert_eq!(output.status, HealthStatus::Up);
        assert_eq!(
            output.details.get("destination").and_then(|v| v.as_str()),
            Some("file:///replicas")
        );
    }

    #[test]
    fn a_replicator_that_never_ships_goes_down_after_the_grace() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        let health = indicator(&status, at(0));
        assert_eq!(
            health.evaluate(&status.snapshot(), at(200)).status,
            HealthStatus::Down
        );
    }

    #[test]
    fn lag_past_the_threshold_is_down_and_recovers_on_the_next_success() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        let health = indicator(&status, at(0));
        status.record_tick_ok(0, at(10));

        assert_eq!(
            health.evaluate(&status.snapshot(), at(20)).status,
            HealthStatus::Up
        );
        let down = health.evaluate(&status.snapshot(), at(100));
        assert_eq!(down.status, HealthStatus::Down);
        assert_eq!(
            down.details
                .get("lag_seconds")
                .and_then(serde_json::Value::as_u64),
            Some(90)
        );

        status.record_tick_ok(0, at(100));
        assert_eq!(
            health.evaluate(&status.snapshot(), at(105)).status,
            HealthStatus::Up
        );
    }

    #[test]
    fn a_failing_tick_does_not_advance_the_replication_point() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        status.record_tick_ok(0, at(10));
        status.record_tick_error("endpoint unreachable", at(20));
        status.record_tick_error("endpoint unreachable", at(25));

        let snapshot = status.snapshot();
        assert_eq!(snapshot.last_success_at, Some(at(10)));
        assert_eq!(snapshot.consecutive_failures, 2);
        assert_eq!(snapshot.lag(at(40)), Some(Duration::from_secs(30)));

        let health = indicator(&status, at(0));
        let output = health.evaluate(&snapshot, at(60));
        assert_eq!(output.status, HealthStatus::Down);
        assert_eq!(
            output.details.get("error").and_then(|v| v.as_str()),
            Some("endpoint unreachable")
        );
    }

    #[test]
    fn a_failed_verification_is_down_even_when_shipping_is_current() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        let health = indicator(&status, at(0));
        status.record_tick_ok(0, at(100));
        status.record_verification(Err("integrity check failed".to_owned()), at(100));

        let output = health.evaluate(&status.snapshot(), at(101));
        assert_eq!(
            output.status,
            HealthStatus::Down,
            "uploaded is not the same as restorable"
        );
        assert_eq!(
            output
                .details
                .get("verification_error")
                .and_then(|v| v.as_str()),
            Some("integrity check failed")
        );

        status.record_tick_ok(0, at(200));
        status.record_verification(Ok(()), at(200));
        let recovered = health.evaluate(&status.snapshot(), at(201));
        assert_eq!(recovered.status, HealthStatus::Up);
        assert!(recovered.details.contains_key("last_verified_at"));
    }

    #[test]
    fn counters_accumulate_across_ticks() {
        let status = Arc::new(ReplicationStatus::new("s3://bucket"));
        status.record_generation("0000000001000-0000000000000001", at(1));
        status.record_segment(4_120, at(2));
        status.record_segment(8_240, at(3));
        let snapshot = status.snapshot();
        assert_eq!(snapshot.snapshots_taken, 1);
        assert_eq!(snapshot.segments_shipped, 2);
        assert_eq!(snapshot.bytes_shipped, 12_360);
        assert_eq!(snapshot.last_shipped_at, Some(at(3)));
        assert_eq!(
            snapshot.generation.as_deref(),
            Some("0000000001000-0000000000000001")
        );
    }

    #[test]
    fn the_indicator_is_health_only_so_it_never_pulls_a_replica_out_of_service() {
        let status = Arc::new(ReplicationStatus::new("file:///replicas"));
        assert_eq!(
            indicator(&status, at(0)).group(),
            IndicatorGroup::HealthOnly
        );
    }
}
