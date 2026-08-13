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

tokio::task_local! {
    /// Marks the task on which the replay driver is running the rebuilt
    /// request. Deliberately a task-local, because that is exactly what the
    /// capture side's scope is: `tokio::spawn` inherits neither, so a clock
    /// read in request-spawned work is unrecorded at capture time **and**
    /// non-consuming at replay time — symmetric, instead of silently eating
    /// queue entries the recorded handler is still owed.
    static REPLAY_REQUEST: ();
}

/// Run `future` as *the replayed request*: recorded clock readings are served
/// (and consumed) only inside this scope.
///
/// The replay driver wraps its router call in this. Everything else that
/// reads the process clock during a replay — app boot, state initializers,
/// work the handler `tokio::spawn`s — sees a stable non-consuming timestamp
/// instead, mirroring what the recording did (those reads were never
/// recorded, because they carried no capture scope).
pub async fn with_replay_request_scope<F: Future>(future: F) -> F::Output {
    REPLAY_REQUEST.scope((), future).await
}

/// Whether the current task is the replay driver's request task.
fn in_replay_request() -> bool {
    REPLAY_REQUEST.try_with(|()| ()).is_ok()
}

/// Serves the readings a capsule recorded, in order.
///
/// When a replayed handler reads the clock more times than the capture did
/// (a code change since recording), the last reading is repeated and the
/// over-read is counted so the verdict can warn about it rather than silently
/// drifting to real time.
///
/// Only reads made inside [`with_replay_request_scope`] consume the queue;
/// any other read (boot-time, or from a task the handler spawned) gets the
/// last-served reading — or the fallback — without consuming, because the
/// recording never attributed those reads either.
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
        if !in_replay_request() {
            // A read the recording never attributed (boot, or a spawned
            // task): stable and non-consuming, so it can neither shift the
            // handler's readings nor count as the handler drifting.
            return self
                .last
                .lock()
                .ok()
                .and_then(|last| *last)
                .unwrap_or(self.fallback);
        }
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

    #[tokio::test]
    async fn replay_clock_serves_recorded_readings_in_order() {
        let clock = ReplayClock::new(vec![at(1), at(2)], at(0));
        with_replay_request_scope(async {
            assert_eq!(clock.now(), at(1));
            assert_eq!(clock.now(), at(2));
        })
        .await;
        assert_eq!(clock.over_reads(), 0);
    }

    #[tokio::test]
    async fn replay_clock_reports_readings_it_never_served() {
        let clock = ReplayClock::new(vec![at(1), at(2), at(3)], at(0));
        with_replay_request_scope(async {
            assert_eq!(clock.now(), at(1));
        })
        .await;
        assert_eq!(
            clock.unconsumed(),
            2,
            "readings the replayed code never asked for must be countable"
        );
    }

    #[tokio::test]
    async fn replay_clock_over_read_reuses_the_last_reading() {
        let clock = ReplayClock::new(vec![at(1)], at(0));
        with_replay_request_scope(async {
            assert_eq!(clock.now(), at(1));
            assert_eq!(clock.now(), at(1), "an over-read must repeat, not drift");
        })
        .await;
        assert_eq!(clock.over_reads(), 1);
    }

    #[tokio::test]
    async fn replay_clock_with_no_readings_uses_the_fallback() {
        let clock = ReplayClock::new(Vec::new(), at(9));
        with_replay_request_scope(async {
            assert_eq!(clock.now(), at(9));
        })
        .await;
        assert_eq!(clock.over_reads(), 1);
    }

    /// A clock read from outside the replay-request scope — app boot, or a
    /// task the handler spawned — must not consume the queue: at capture time
    /// those reads carried no scope and were never recorded, so letting them
    /// eat entries at replay time would shift every later reading the
    /// recorded handler is still owed and manufacture a false mismatch.
    #[tokio::test]
    async fn reads_outside_the_replay_request_scope_do_not_consume_the_queue() {
        let clock = Arc::new(ReplayClock::new(vec![at(1), at(2)], at(0)));

        // Boot-shaped read: before any request scope exists.
        assert_eq!(clock.now(), at(0), "an unscoped read serves the fallback");
        assert_eq!(clock.unconsumed(), 2, "…without consuming");

        with_replay_request_scope({
            let clock = Arc::clone(&clock);
            async move {
                assert_eq!(clock.now(), at(1));
                // Spawned-task-shaped read: `tokio::spawn` does not inherit
                // the task-local, exactly as it does not inherit the capture
                // scope at recording time.
                let spawned = {
                    let clock = Arc::clone(&clock);
                    tokio::spawn(async move { clock.now() })
                };
                let seen = spawned.await.expect("spawned read");
                assert_eq!(
                    seen,
                    at(1),
                    "a spawned read repeats the last served reading, non-consuming"
                );
                assert_eq!(
                    clock.now(),
                    at(2),
                    "the handler still gets its next reading"
                );
            }
        })
        .await;
        assert_eq!(clock.over_reads(), 0, "unscoped reads are not drift");
    }

    #[test]
    fn recording_clock_passes_through_outside_a_request() {
        let clock = RecordingClock::new(Arc::new(crate::time::FixedClock::at(at(5))));
        assert_eq!(clock.now(), at(5));
    }
}
