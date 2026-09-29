//! Deterministic crash injection for simulation crash-recovery tests
//! (sim-testing W5.c item 7, issue #1797).
//!
//! This is the **crash lane** of the sim harness: a seed-derived, reproducible
//! *crash schedule* that models a process dying mid-flight so a
//! [`#[sim_test]`](crate::sim_test) can prove its **durable** work survives a
//! crash and is recovered on restart — without a flaky, timing-dependent kill.
//!
//! # Kill / restart primitive
//!
//! The crash itself is driven through the [`Sim`](crate::sim::Sim) handle:
//! [`Sim::kill`](crate::sim::Sim::kill) drops the mounted app (cancelling the
//! in-process job runtime's in-flight work **without** completing it), and
//! [`Sim::restart`](crate::sim::Sim::restart) mounts a fresh app on the **same
//! durable database** (the caller re-passes `substrate.pool()`), modelling a
//! process restart on the same on-disk/in-memory store.
//! [`Sim::crash_and_restart`](crate::sim::Sim::crash_and_restart) is the
//! kill-then-restart convenience.
//!
//! # The seeded crash schedule (determinism contract)
//!
//! Every crash decision is drawn from a **dedicated seeded stream**, seeded from
//! `seed ^ CRASH_STREAM_SALT` so it is independent of both the app-facing
//! entropy source and the [`chaos`](crate::sim::chaos) decision stream. Two
//! same-seed runs therefore derive an **identical crash schedule byte-for-byte**
//! (the schedule is a pure function of the seed), which is what the W5.c
//! Definition-of-Done asserts; different seeds (overwhelmingly likely) diverge.
//! Read the schedule through
//! [`Sim::crash_schedule`](crate::sim::Sim::crash_schedule) /
//! [`Sim::crash_point`](crate::sim::Sim::crash_point).
//!
//! # Crash at any await
//!
//! [`crash_at`] runs an operation and drops it at its N-th suspension point, so
//! a test can crash between any two awaits. [`CrashPoint::await_index`] is the
//! seeded N. Pair it with [`Sim::kill`](crate::sim::Sim::kill) and
//! [`Sim::restart`](crate::sim::Sim::restart) to model the process dying there.
//!
//! ```rust,ignore
//! let point = sim.crash_point().unwrap();
//! let outcome = crash_at(point.await_index, sim.client().post("/pay").send()).await;
//! if outcome.is_crashed() {
//!     sim.crash_and_restart(app_on_same_db());
//!     sim.run_to_idle().await;
//! }
//! ```
//!
//! A suspension point is an await that returns `Pending`. An await whose value
//! is ready at once does not suspend, so it is not a crash point.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};

use crate::entropy::SeededEntropy;

/// Salt `XOR`ed into the sim seed to derive the crash **decision** stream, keeping
/// it independent of the app-facing entropy source and the
/// [`chaos`](crate::sim::chaos) decision stream (both seeded from other values).
/// An arbitrary fixed non-zero constant.
pub(crate) const CRASH_STREAM_SALT: u64 = 0xC7A5_4EAD_C7A5_4EAD;

/// The seeded `await_index` is drawn in `[0, CRASH_AWAIT_BOUNDARIES)`. A small
/// bound keeps the seeded crash inside short operations. To reach a later
/// await, pass a larger index to [`crash_at`].
pub(crate) const CRASH_AWAIT_BOUNDARIES: u64 = 4;

/// Default number of crash decisions the derived schedule records for a sim.
pub(crate) const DEFAULT_CRASH_SCHEDULE_LEN: usize = 8;

/// One seed-derived crash decision in a [`CrashSchedule`].
///
/// `#[non_exhaustive]` so fields can be added without breaking readers; the
/// derived [`PartialEq`] is what the W5.c Definition-of-Done compares two
/// same-seed runs on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashPoint {
    /// The crash sequence number, starting at 0.
    pub seq: u64,
    /// The seeded suspension point to crash at, in `[0, 4)`. Pass it to
    /// [`crash_at`].
    pub await_index: u64,
}

/// A reproducible crash schedule for one simulation, derived purely from
/// `seed ^ CRASH_STREAM_SALT`.
///
/// Constructed by [`Sim::from_seed`](crate::sim::Sim::from_seed)'s seed and read
/// through [`Sim::crash_schedule`](crate::sim::Sim::crash_schedule). Two
/// same-seed sims produce an equal schedule; that equality is the W5.c
/// determinism Definition-of-Done.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSchedule {
    /// The ordered crash decisions this seed produced.
    points: Vec<CrashPoint>,
}

impl CrashSchedule {
    /// Derive the crash schedule for `seed`: `len` decisions drawn in order from
    /// the dedicated `seed ^ CRASH_STREAM_SALT` stream. A pure function of the
    /// seed, so it replays byte-for-byte across runs and machines.
    #[must_use]
    pub(crate) fn derive(seed: u64, len: usize) -> Self {
        let stream = SeededEntropy::shared(seed ^ CRASH_STREAM_SALT);
        let points = (0..len as u64)
            .map(|seq| CrashPoint {
                seq,
                await_index: stream.next_u64() % CRASH_AWAIT_BOUNDARIES,
            })
            .collect();
        Self { points }
    }

    /// The ordered crash decisions this schedule recorded.
    #[must_use]
    pub fn points(&self) -> &[CrashPoint] {
        &self.points
    }

    /// The first crash point, or `None` for an empty schedule.
    #[must_use]
    pub fn first(&self) -> Option<&CrashPoint> {
        self.points.first()
    }
}

/// What happened to an operation run under [`crash_at`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashOutcome<T> {
    /// The operation finished before it reached the crash point.
    Completed(T),
    /// The operation was dropped at this suspension point.
    Crashed {
        /// The suspension point, counted from 0.
        await_index: u64,
    },
}

impl<T> CrashOutcome<T> {
    /// `true` when the operation was dropped before it finished.
    #[must_use]
    pub const fn is_crashed(&self) -> bool {
        matches!(self, Self::Crashed { .. })
    }

    /// The output, or `None` when the operation crashed.
    #[must_use]
    pub fn completed(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            Self::Crashed { .. } => None,
        }
    }
}

/// Run `op` and drop it at its `await_index`-th suspension point.
///
/// Index 0 drops `op` the first time it returns `Pending`. The work `op` did
/// before that point stays done; the work after it never runs. If `op` finishes
/// first, the result is [`CrashOutcome::Completed`].
///
/// A suspension counts once, however often the caller polls: `op` is polled
/// again only after it wakes itself. So `crash_at` inside
/// [`Sim::interleave`](crate::sim::Sim::interleave) or `join!` counts the same
/// awaits as it does alone.
///
/// This drops only `op`. Call [`Sim::kill`](crate::sim::Sim::kill) after a
/// crash to also stop the app's background work.
pub async fn crash_at<F: Future>(await_index: u64, op: F) -> CrashOutcome<F::Output> {
    let mut op = pin!(op);
    let wake = Arc::new(WakeFlag::default());
    let waker = Waker::from(Arc::clone(&wake));
    let mut suspensions = 0_u64;
    let mut polled = false;
    std::future::poll_fn(move |cx| {
        wake.set_parent(cx.waker());
        // Poll `op` first, and then only after its own waker fired.
        if polled && !wake.take() {
            return Poll::Pending;
        }
        polled = true;
        match op.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(value) => Poll::Ready(CrashOutcome::Completed(value)),
            Poll::Pending if suspensions == await_index => {
                Poll::Ready(CrashOutcome::Crashed { await_index })
            }
            Poll::Pending => {
                suspensions += 1;
                Poll::Pending
            }
        }
    })
    .await
}

/// The waker [`crash_at`] gives `op`: it records the wake and passes it on.
#[derive(Default)]
struct WakeFlag {
    woken: AtomicBool,
    parent: Mutex<Option<Waker>>,
}

impl WakeFlag {
    fn set_parent(&self, waker: &Waker) {
        let mut parent = self.parent.lock().unwrap_or_else(PoisonError::into_inner);
        if !parent
            .as_ref()
            .is_some_and(|current| current.will_wake(waker))
        {
            *parent = Some(waker.clone());
        }
    }

    /// Whether `op` woke since the last call.
    fn take(&self) -> bool {
        self.woken.swap(false, Ordering::AcqRel)
    }
}

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        let parent = self
            .parent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(parent) = parent {
            parent.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CRASH_AWAIT_BOUNDARIES, CrashSchedule, DEFAULT_CRASH_SCHEDULE_LEN};

    #[test]
    fn schedule_is_seed_deterministic() {
        let a = CrashSchedule::derive(42, DEFAULT_CRASH_SCHEDULE_LEN);
        let b = CrashSchedule::derive(42, DEFAULT_CRASH_SCHEDULE_LEN);
        assert_eq!(a, b, "same seed must replay an identical crash schedule");
    }

    #[test]
    fn different_seeds_diverge() {
        let a = CrashSchedule::derive(42, DEFAULT_CRASH_SCHEDULE_LEN);
        let b = CrashSchedule::derive(43, DEFAULT_CRASH_SCHEDULE_LEN);
        assert_ne!(
            a, b,
            "different seeds should (overwhelmingly likely) diverge"
        );
    }

    #[test]
    fn schedule_len_and_bounds_hold() {
        let s = CrashSchedule::derive(7, DEFAULT_CRASH_SCHEDULE_LEN);
        assert_eq!(s.points().len(), DEFAULT_CRASH_SCHEDULE_LEN);
        for (i, point) in s.points().iter().enumerate() {
            assert_eq!(point.seq, i as u64, "seq numbers are dense and ordered");
            assert!(
                point.await_index < CRASH_AWAIT_BOUNDARIES,
                "await index stays in range"
            );
        }
        assert_eq!(s.first(), s.points().first());
    }
}
