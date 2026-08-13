//! Clock sources that make time reproducible across a capture/replay pair.
//!
//! Wall-clock time is an input like any other: a handler that stamps
//! `created_at` or expires a token behaves differently a second later, so a
//! capsule records every reading the request took and replay serves them back
//! in the same order.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
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

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::capsule::capture::current_scope;
use crate::time::ClockSource;

/// Wraps the application's clock and tees every reading into the capture scope
/// of the request that took it.
///
/// Installed over the configured clock when `[failure_capture] enabled =
/// true`; reads taken outside a request (schedulers, jobs) pass straight
/// through because no scope is active on those tasks.
pub struct RecordingClock {
    inner: Arc<dyn ClockSource>,
}

impl RecordingClock {
    /// Wrap an existing clock.
    #[must_use]
    pub fn new(inner: Arc<dyn ClockSource>) -> Self {
        Self { inner }
    }
}

impl ClockSource for RecordingClock {
    fn now(&self) -> DateTime<Utc> {
        let reading = self.inner.now();
        if let Some(scope) = current_scope() {
            scope.record_clock(reading);
        }
        reading
    }
}

/// Serves the readings a capsule recorded, in order.
///
/// When a replayed handler reads the clock more times than the capture did
/// (a code change since recording), the last reading is repeated and the
/// over-read is counted so the verdict can warn about it rather than silently
/// drifting to real time.
pub struct ReplayClock {
    readings: Mutex<VecDeque<DateTime<Utc>>>,
    last: Mutex<Option<DateTime<Utc>>>,
    over_reads: std::sync::atomic::AtomicUsize,
    fallback: DateTime<Utc>,
}

impl ReplayClock {
    /// Build a clock from a capsule's recorded readings.
    ///
    /// `fallback` is used only when the capsule recorded no readings at all.
    #[must_use]
    pub fn new(readings: Vec<DateTime<Utc>>, fallback: DateTime<Utc>) -> Self {
        Self {
            readings: Mutex::new(readings.into()),
            last: Mutex::new(None),
            over_reads: std::sync::atomic::AtomicUsize::new(0),
            fallback,
        }
    }

    /// How many reads went past the end of the recording.
    #[must_use]
    pub fn over_reads(&self) -> usize {
        self.over_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many recorded readings were never consumed.
    ///
    /// A replayed run that reads the clock fewer times than the recording did
    /// took a different path through time-dependent code — worth surfacing,
    /// even though (unlike the database tape) the counts cannot gate the
    /// verdict: the recording counts only request-scoped reads, while replay
    /// serves every read of the process clock from this queue.
    #[must_use]
    pub fn unconsumed(&self) -> usize {
        self.readings.lock().map_or(0, |readings| readings.len())
    }
}

impl ClockSource for ReplayClock {
    fn now(&self) -> DateTime<Utc> {
        if let Ok(mut readings) = self.readings.lock()
            && let Some(reading) = readings.pop_front()
        {
            if let Ok(mut last) = self.last.lock() {
                *last = Some(reading);
            }
            return reading;
        }
        self.over_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.last
            .lock()
            .ok()
            .and_then(|last| *last)
            .unwrap_or(self.fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, second)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn replay_clock_serves_recorded_readings_in_order() {
        let clock = ReplayClock::new(vec![at(1), at(2)], at(0));
        assert_eq!(clock.now(), at(1));
        assert_eq!(clock.now(), at(2));
        assert_eq!(clock.over_reads(), 0);
    }

    #[test]
    fn replay_clock_reports_readings_it_never_served() {
        let clock = ReplayClock::new(vec![at(1), at(2), at(3)], at(0));
        assert_eq!(clock.now(), at(1));
        assert_eq!(
            clock.unconsumed(),
            2,
            "readings the replayed code never asked for must be countable"
        );
    }

    #[test]
    fn replay_clock_over_read_reuses_the_last_reading() {
        let clock = ReplayClock::new(vec![at(1)], at(0));
        assert_eq!(clock.now(), at(1));
        assert_eq!(clock.now(), at(1), "an over-read must repeat, not drift");
        assert_eq!(clock.over_reads(), 1);
    }

    #[test]
    fn replay_clock_with_no_readings_uses_the_fallback() {
        let clock = ReplayClock::new(Vec::new(), at(9));
        assert_eq!(clock.now(), at(9));
        assert_eq!(clock.over_reads(), 1);
    }

    #[test]
    fn recording_clock_passes_through_outside_a_request() {
        let clock = RecordingClock::new(Arc::new(crate::time::FixedClock::at(at(5))));
        assert_eq!(clock.now(), at(5));
    }
}
