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
//! # Representative crash point (scope, stated plainly)
//!
//! This wave injects a **single representative deterministic crash point** — the
//! `await` boundary **after** a repository write has enqueued its durable
//! commit-hook row but **before** that hook drains — rather than a fully-general
//! "between ANY two `await`s" injector. That representative boundary is the
//! sharpest test of durable recovery: the durable row is committed, the in-flight
//! work is not, so a correct recovery re-runs it exactly-once/idempotently on
//! restart. The schedule API is shaped generally (an ordered sequence of
//! [`CrashPoint`]s, each carrying a seed-derived `await_index`), so a later wave
//! can realize more of the enumerated boundaries without a breaking change; today
//! the harness realizes the first one. This is documented representativeness, not
//! faked generality.

use crate::entropy::SeededEntropy;

/// Salt `XOR`ed into the sim seed to derive the crash **decision** stream, keeping
/// it independent of the app-facing entropy source and the
/// [`chaos`](crate::sim::chaos) decision stream (both seeded from other values).
/// An arbitrary fixed non-zero constant.
pub(crate) const CRASH_STREAM_SALT: u64 = 0xC7A5_4EAD_C7A5_4EAD;

/// The number of enumerated `await` boundaries in the representative durable
/// write → enqueue → drain path a crash can target. A seed-derived draw is taken
/// `% CRASH_AWAIT_BOUNDARIES` to pick one. The realized representative crash
/// always fires at the "commit-hook enqueued, pre-drain" boundary; this modulus
/// only shapes the (general) recorded schedule so it stays a small, stable index
/// space across runs.
pub(crate) const CRASH_AWAIT_BOUNDARIES: u64 = 4;

/// Default number of crash decisions the derived schedule records for a sim.
/// Generous relative to the single representative crash the harness realizes; the
/// extra entries exist only so the recorded schedule is a non-trivial seeded
/// sequence the Definition-of-Done can compare across two same-seed runs.
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
    /// The seed-derived `await`-boundary index this crash targets, in
    /// `[0, CRASH_AWAIT_BOUNDARIES)`. Representative: the harness realizes the
    /// crash at the "commit-hook enqueued, pre-drain" boundary regardless of this
    /// index; it exists so the recorded schedule is a genuine seeded sequence.
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

    /// The first (representative, realized) crash point, or `None` for an empty
    /// schedule.
    #[must_use]
    pub fn first(&self) -> Option<&CrashPoint> {
        self.points.first()
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
