//! Named replicas on one sim clock (issue #3067).
//!
//! [`Sim::mount_replica`](crate::sim::Sim::mount_replica) mounts several apps
//! on one simulation. Each replica reads its own clock: the sim's elapsed time,
//! plus an offset, plus a drift rate. So two replicas can show different wall
//! times, as two hosts can. [`Sim::step_replica_clock`](crate::sim::Sim::step_replica_clock)
//! jumps one clock, as an NTP step does.
//!
//! A replica clock follows the sim's paused tokio clock, so it moves with
//! [`Sim::run_for`](crate::sim::Sim::run_for) and with every tokio
//! auto-advance. Tokio timers do not drift: a replica that sleeps 60 s wakes
//! 60 s of sim time later, and its drifted clock then reads a little more or
//! less.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::entropy::SeededEntropy;
use crate::time::{ClockSource, MonotonicInstant};

/// The drift limit, in parts per million. A clock at `-1_000_000` would stop.
const MAX_DRIFT_PPM: i64 = 999_999;

/// One app in a multi-replica simulation: its name and its clock.
///
/// A plain `&str` converts to a replica with the sim's own clock.
///
/// ```rust,ignore
/// use autumn_web::sim::Replica;
/// use std::time::Duration;
///
/// sim.mount_replica("a", app_a);
/// sim.mount_replica(Replica::named("b").clock_behind(Duration::from_secs(2)), app_b);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replica {
    name: String,
    offset: TimeDelta,
    drift_ppm: i64,
    seeded: Option<(Duration, u32)>,
}

impl Replica {
    /// A replica called `name`, with the sim's own clock.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            offset: TimeDelta::zero(),
            drift_ppm: 0,
            seeded: None,
        }
    }

    /// The replica's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Move the clock `by` ahead of the sim clock. Adds to earlier offsets.
    #[must_use]
    pub fn clock_ahead(mut self, by: Duration) -> Self {
        self.offset = add_delta(self.offset, delta(by));
        self
    }

    /// Move the clock `by` behind the sim clock. Adds to earlier offsets.
    #[must_use]
    pub fn clock_behind(mut self, by: Duration) -> Self {
        self.offset = add_delta(self.offset, -delta(by));
        self
    }

    /// Make the clock run `ppm` parts per million fast (negative: slow). The
    /// drift starts at the mount. The value is clamped to ±999 999.
    #[must_use]
    pub fn clock_drift_ppm(mut self, ppm: i64) -> Self {
        self.drift_ppm = ppm.clamp(-MAX_DRIFT_PPM, MAX_DRIFT_PPM);
        self
    }

    /// Add an offset in `[-max_offset, max_offset]` and a drift in
    /// `[-max_drift_ppm, max_drift_ppm]`, drawn from the sim seed and the
    /// replica name at the mount. The same seed gives the same clock. A second
    /// call replaces the first one.
    #[must_use]
    pub fn seeded_clock(mut self, max_offset: Duration, max_drift_ppm: u32) -> Self {
        self.seeded = Some((max_offset, max_drift_ppm));
        self
    }

    /// The clock this replica runs with under `seed`.
    pub(crate) fn resolve(&self, seed: u64) -> ReplicaClock {
        let mut clock = ReplicaClock {
            offset: self.offset,
            drift_ppm: self.drift_ppm,
        };
        if let Some((max_offset, max_drift_ppm)) = self.seeded {
            let id =
                SeededEntropy::new(seed).derive_uuid(format!("sim-replica-clock:{}", self.name));
            let (offset_bits, drift_bits) = id.as_u64_pair();
            let max_ms = i64::try_from(max_offset.as_millis()).unwrap_or(i64::MAX / 4);
            let offset_ms = symmetric_draw(offset_bits, max_ms);
            let drift = symmetric_draw(drift_bits, i64::from(max_drift_ppm));
            clock.offset = add_delta(clock.offset, TimeDelta::milliseconds(offset_ms));
            clock.drift_ppm = clock
                .drift_ppm
                .saturating_add(drift)
                .clamp(-MAX_DRIFT_PPM, MAX_DRIFT_PPM);
        }
        clock
    }
}

impl From<&str> for Replica {
    fn from(name: &str) -> Self {
        Self::named(name)
    }
}

impl From<String> for Replica {
    fn from(name: String) -> Self {
        Self::named(name)
    }
}

/// A value in `[-max, max]` from `bits`.
fn symmetric_draw(bits: u64, max: i64) -> i64 {
    let max = max.max(0);
    let span = u64::try_from(max)
        .unwrap_or(0)
        .saturating_mul(2)
        .saturating_add(1);
    let value = i64::try_from(bits % span).unwrap_or(0);
    value - max
}

fn delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap_or(TimeDelta::MAX)
}

fn add_delta(a: TimeDelta, b: TimeDelta) -> TimeDelta {
    a.checked_add(&b).unwrap_or_else(|| {
        if b < TimeDelta::zero() {
            TimeDelta::MIN
        } else {
            TimeDelta::MAX
        }
    })
}

/// The clock settings a mounted replica runs with.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplicaClock {
    offset: TimeDelta,
    drift_ppm: i64,
}

impl ReplicaClock {
    /// How far the replica clock is ahead of the sim clock (negative: behind),
    /// before drift. Includes every step.
    #[must_use]
    pub const fn offset(&self) -> TimeDelta {
        self.offset
    }

    /// How fast the replica clock runs, in parts per million.
    #[must_use]
    pub const fn drift_ppm(&self) -> i64 {
        self.drift_ppm
    }
}

/// A replica's clock: sim elapsed time, scaled by the drift, plus the offset.
///
/// Clones share one clock, so a restarted replica keeps the clock of the host
/// it runs on.
#[derive(Clone)]
pub(crate) struct NodeClock {
    inner: Arc<NodeClockInner>,
}

struct NodeClockInner {
    /// The sim's elapsed-time source (its ambient monotonic clock).
    elapsed: Arc<dyn ClockSource>,
    /// The wall time at sim elapsed zero.
    epoch: DateTime<Utc>,
    /// Sim elapsed time at the mount. Drift counts from here.
    anchor: Duration,
    drift_ppm: i64,
    offset: Mutex<TimeDelta>,
}

impl NodeClock {
    pub(crate) fn new(
        elapsed: Arc<dyn ClockSource>,
        epoch: DateTime<Utc>,
        clock: ReplicaClock,
    ) -> Self {
        let anchor = elapsed.monotonic().since_origin();
        Self {
            inner: Arc::new(NodeClockInner {
                elapsed,
                epoch,
                anchor,
                drift_ppm: clock.drift_ppm,
                offset: Mutex::new(clock.offset),
            }),
        }
    }

    /// The current settings, including steps.
    pub(crate) fn settings(&self) -> ReplicaClock {
        ReplicaClock {
            offset: *self.offset(),
            drift_ppm: self.inner.drift_ppm,
        }
    }

    /// Jump the clock by `by`, as an NTP step does.
    pub(crate) fn step(&self, by: TimeDelta) {
        let mut offset = self.offset();
        *offset = add_delta(*offset, by);
    }

    fn offset(&self) -> std::sync::MutexGuard<'_, TimeDelta> {
        self.inner
            .offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sim elapsed time, and the part of it since the mount.
    fn elapsed(&self) -> (Duration, Duration) {
        let elapsed = self.inner.elapsed.monotonic().since_origin();
        (elapsed, elapsed.saturating_sub(self.inner.anchor))
    }
}

/// `since * ppm / 1e6`, in nanoseconds.
fn drift_nanos(since: Duration, ppm: i64) -> i128 {
    i128::try_from(since.as_nanos()).unwrap_or(i128::MAX) * i128::from(ppm) / 1_000_000
}

impl ClockSource for NodeClock {
    fn now(&self) -> DateTime<Utc> {
        let (elapsed, since) = self.elapsed();
        let drift = i64::try_from(drift_nanos(since, self.inner.drift_ppm)).unwrap_or(0);
        let wall = self.inner.epoch + delta(elapsed);
        wall.checked_add_signed(add_delta(TimeDelta::nanoseconds(drift), *self.offset()))
            .unwrap_or(wall)
    }

    fn monotonic(&self) -> MonotonicInstant {
        let (_, since) = self.elapsed();
        let drift = drift_nanos(since, self.inner.drift_ppm);
        let since_nanos = i128::try_from(since.as_nanos()).unwrap_or(i128::MAX);
        let scaled = u64::try_from((since_nanos + drift).max(0)).unwrap_or(u64::MAX);
        MonotonicInstant::from_origin_elapsed(
            self.inner
                .anchor
                .saturating_add(Duration::from_nanos(scaled)),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{TimeDelta, TimeZone, Utc};

    use super::{NodeClock, Replica, ReplicaClock, symmetric_draw};
    use crate::time::{ClockSource, TickingClock};

    fn node(spec: &Replica) -> (TickingClock, NodeClock) {
        let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let base = TickingClock::starting_at(epoch);
        let elapsed: Arc<dyn ClockSource> = Arc::new(base.clone());
        (base, NodeClock::new(elapsed, epoch, spec.resolve(0)))
    }

    #[test]
    fn offset_and_drift_shape_the_wall_clock() {
        let spec = Replica::named("a")
            .clock_ahead(Duration::from_secs(5))
            .clock_behind(Duration::from_secs(2))
            .clock_drift_ppm(500_000);
        let (base, clock) = node(&spec);
        assert_eq!(clock.now() - base.now(), TimeDelta::seconds(3));
        base.advance(Duration::from_secs(10));
        assert_eq!(
            clock.now() - base.now(),
            TimeDelta::seconds(8),
            "3 s offset + 5 s drift"
        );
        assert_eq!(
            clock.monotonic().since_origin(),
            Duration::from_secs(15),
            "monotonic time runs fast too"
        );
    }

    #[test]
    fn a_slow_clock_still_moves_forward() {
        let (base, clock) = node(&Replica::named("a").clock_drift_ppm(-5_000_000));
        let before = clock.now();
        base.advance(Duration::from_secs(1_000_000));
        assert!(clock.now() > before);
        assert_eq!(clock.settings().drift_ppm(), -999_999, "clamped");
    }

    #[test]
    fn a_step_moves_the_wall_clock_not_the_monotonic_clock() {
        let (_base, clock) = node(&Replica::named("a"));
        let wall = clock.now();
        let mono = clock.monotonic();
        clock.step(TimeDelta::seconds(-30));
        assert_eq!(wall - clock.now(), TimeDelta::seconds(30));
        assert_eq!(clock.monotonic(), mono);
        assert_eq!(clock.settings().offset(), TimeDelta::seconds(-30));
    }

    #[test]
    fn seeded_clocks_are_bounded_and_replay() {
        let spec = Replica::named("a").seeded_clock(Duration::from_millis(250), 40);
        assert_eq!(spec.resolve(3), spec.resolve(3));
        let other = Replica::named("b").seeded_clock(Duration::from_millis(250), 40);
        let mut differs = false;
        for seed in 0..200 {
            let ReplicaClock { offset, drift_ppm } = spec.resolve(seed);
            assert!(offset.abs() <= TimeDelta::milliseconds(250), "{offset}");
            assert!(drift_ppm.abs() <= 40, "{drift_ppm}");
            differs |= spec.resolve(seed) != other.resolve(seed);
        }
        assert!(differs, "the name is part of the draw");
    }

    #[test]
    fn symmetric_draw_covers_both_ends() {
        assert_eq!(symmetric_draw(0, 3), -3);
        assert_eq!(symmetric_draw(6, 3), 3);
        assert_eq!(symmetric_draw(7, 3), -3);
        assert_eq!(symmetric_draw(u64::MAX, 0), 0);
        assert_eq!(symmetric_draw(5, -1), 0);
    }

    #[test]
    fn a_str_is_a_plain_replica() {
        let replica: Replica = "x".into();
        assert_eq!(replica, Replica::named("x"));
        assert_eq!(replica.resolve(1), ReplicaClock::default());
    }
}
