//! Deterministic simulation testing (sim-testing, issue #1797).
//!
//! This module is the public 0.7.0 developer-experience surface for writing
//! **deterministic** simulation tests: a single [`#[sim_test]`](crate::sim_test)
//! attribute gives you a seeded [`Sim`] handle and a paused runtime, so a test
//! runs identically on every machine and every run, and a failure prints a
//! copy-pasteable line that reproduces it exactly.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use autumn_web::sim::Sim;
//! use autumn_web::sim_test;
//!
//! #[sim_test]
//! async fn deterministic(mut sim: Sim) {
//!     // The seed comes from `AUTUMN_SIM_SEED` (hex `0x..` or decimal,
//!     // default 0). Everything derived from `sim` is seed-driven and
//!     // reproducible.
//!     assert_eq!(sim.seed, 0);
//!     let _rng = sim.rng();
//! }
//! ```
//!
//! Reproduce a failing run by copying the replay line printed on panic, e.g.:
//!
//! ```text
//! AUTUMN_SIM_SEED=0x9f3a cargo test -p my-crate deterministic
//! ```
//!
//! # Scope (Wave 1)
//!
//! W1 ships the **deterministic executor, the seed / replay / injection
//! plumbing, and the public [`Sim`] skeleton** only. The handles hung off
//! [`Sim`] ([`SimRng`], [`SimClock`], [`Chaos`], [`SimApp`]) are frozen,
//! stability-minded placeholders whose behavior lands in later waves:
//!
//! - **W2** wires virtual-clock advancing / draining onto [`SimClock`] against
//!   [`crate::time::ClockSource`], and mounts an app on [`SimApp`].
//! - **W3** exposes deterministic id / `Uuid` generation through [`SimRng`].
//! - **W5** turns [`Chaos`] into a public fault-injection builder.
//!
//! Everything here is designed to grow additively (builder-style) without
//! breaking the frozen surface — hence the `#[non_exhaustive]` markers.

// The placeholder handles are intentionally thin in W1; their methods and
// docs fill in over later waves. These narrowly-scoped allows keep the
// skeleton clean under the workspace's pedantic lint set without masking real
// issues in the behavioral code that lands later.
#![allow(clippy::missing_const_for_fn)]

use chrono::{TimeZone, Utc};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::time::TickingClock;

/// The fixed, deterministic epoch the simulation clock starts at:
/// `2020-01-01T00:00:00Z`.
///
/// Every sim run starts its virtual clock here so wall-clock-derived values are
/// reproducible across machines and runs. W2 drives this clock forward via
/// [`SimClock`].
const SIM_EPOCH_UNIX_SECS: i64 = 1_577_836_800; // 2020-01-01T00:00:00Z

/// A deterministic simulation handle, constructed from a single `u64` seed.
///
/// `Sim` is the day-one **public, stability-frozen** entry point handed to a
/// [`#[sim_test]`](crate::sim_test) body. Its [`seed`](Sim::seed) is public so a
/// test can assert on or thread it; the injection handles it owns
/// ([`SimRng`] / [`SimClock`] / [`Chaos`] / [`SimApp`]) are private and reached
/// through accessors, so their internals can evolve wave-over-wave without a
/// breaking change.
///
/// Marked `#[non_exhaustive]` so future waves can add handles without breaking
/// construction — always build one via [`Sim::from_seed`].
#[non_exhaustive]
pub struct Sim {
    /// The seed this simulation was constructed from.
    ///
    /// Reproduce a run by exporting `AUTUMN_SIM_SEED=0x<seed>` (the replay line
    /// printed on panic does this for you).
    pub seed: u64,

    /// Seeded deterministic RNG. Generation helpers land in W3.
    rng: SimRng,

    /// Virtual clock, started at the fixed sim epoch
    /// (`2020-01-01T00:00:00Z`). Advancing / draining lands in W2.
    #[allow(dead_code)] // wired up (advance/run_to_idle) in W2
    clock: SimClock,

    /// Fault-injection configuration. Becomes a public builder in W5.
    #[allow(dead_code)] // fault-injection behavior lands in W5
    chaos: Chaos,

    /// Built router + `AppState` handle. Mounted on the paused runtime in W2.
    #[allow(dead_code)] // app mounting lands in W2
    app: SimApp,
}

impl Sim {
    /// Construct a simulation from `seed`.
    ///
    /// Infallible and cheap: it seeds the RNG and starts the virtual clock but
    /// does **not** boot a database or an app, so an empty
    /// [`#[sim_test]`](crate::sim_test) runs with zero setup. App mounting
    /// arrives in W2 via [`SimApp`].
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        let epoch = Utc
            .timestamp_opt(SIM_EPOCH_UNIX_SECS, 0)
            .single()
            .unwrap_or_else(|| Utc.timestamp_nanos(0));
        Self {
            seed,
            rng: SimRng::new(seed),
            clock: SimClock::new(TickingClock::starting_at(epoch)),
            chaos: Chaos::default(),
            app: SimApp::default(),
        }
    }

    /// The seed this simulation was constructed from.
    ///
    /// Provided for API symmetry alongside the public [`seed`](Sim::seed)
    /// field.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Borrow the seeded deterministic RNG handle.
    ///
    /// W1 exposes only the handle; the deterministic id / `Uuid` generation
    /// helpers built on top of it land in W3.
    #[must_use]
    pub fn rng(&mut self) -> &mut SimRng {
        &mut self.rng
    }
}

/// A seeded, deterministic random number generator handle.
///
/// Wraps a `ChaCha8Rng` seeded from the simulation seed, so the same seed
/// always yields the same draw sequence. W3 will expose deterministic id and
/// `Uuid` generation through this handle; W1 only constructs and threads it.
pub struct SimRng {
    #[allow(dead_code)] // read by the deterministic generation helpers in W3
    inner: ChaCha8Rng,
}

impl SimRng {
    /// Seed a fresh deterministic RNG from `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    /// Borrow the underlying `ChaCha8Rng`.
    ///
    /// Internal for W1 (used by the determinism smoke test); the public
    /// generation surface lands in W3.
    #[cfg(test)]
    pub(crate) fn inner_mut(&mut self) -> &mut ChaCha8Rng {
        &mut self.inner
    }
}

/// A virtual clock handle for the simulation.
///
/// Wraps a [`TickingClock`] started at the fixed sim
/// epoch. W2 wires `advance` / `run_to_idle` here against
/// [`crate::time::ClockSource`], driving the paused tokio runtime's virtual
/// clock in lockstep. W1 only constructs it.
pub struct SimClock {
    #[allow(dead_code)] // advance/run_to_idle wiring lands in W2
    inner: TickingClock,
}

impl SimClock {
    /// Wrap a ticking clock as the simulation's virtual clock.
    pub(crate) fn new(inner: TickingClock) -> Self {
        Self { inner }
    }
}

/// Fault-injection configuration for a simulation.
///
/// An empty placeholder in W1. W5 makes this a public, `#[non_exhaustive]`
/// builder (e.g. `db_transient_errors`, `clock_skew`, …) that the executor
/// consults to deterministically inject faults.
#[non_exhaustive]
#[derive(Default, Debug, Clone)]
pub struct Chaos {}

/// The built application handle for a simulation.
///
/// A placeholder for the mounted router + `AppState` in W1. W2 mounts an
/// `AppBuilder` app on the paused runtime here so sim tests can drive real
/// requests deterministically.
#[non_exhaustive]
#[derive(Default)]
pub struct SimApp {}

/// Read and parse the simulation seed from the `AUTUMN_SIM_SEED` environment
/// variable.
///
/// Accepts a hex (`0x`-prefixed) or decimal `u64`; an absent or unparseable
/// value falls back to `0`. Called by the [`#[sim_test]`](crate::sim_test)
/// macro so the parsing is unit-tested in one place and the macro stays tiny.
#[doc(hidden)]
#[must_use]
pub fn __seed_from_env() -> u64 {
    std::env::var("AUTUMN_SIM_SEED").map_or(0, |raw| parse_seed(&raw))
}

/// Parse a seed string: hex (`0x`/`0X` prefixed) or decimal, defaulting to `0`
/// on any parse failure or empty input.
fn parse_seed(raw: &str) -> u64 {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map_or_else(
            || trimmed.parse::<u64>().unwrap_or(0),
            |hex| u64::from_str_radix(hex, 16).unwrap_or(0),
        )
}

/// Build the deterministic replay line printed on a sim-test panic.
///
/// Returns exactly
/// `AUTUMN_SIM_SEED=0x<seed-hex> cargo test -p <pkg> <test>` — copy-paste it to
/// reproduce the failing run bit-for-bit. Called by the
/// [`#[sim_test]`](crate::sim_test) macro.
#[doc(hidden)]
#[must_use]
pub fn __replay_line(seed: u64, pkg: &str, test: &str) -> String {
    format!("AUTUMN_SIM_SEED=0x{seed:x} cargo test -p {pkg} {test}")
}

#[cfg(test)]
mod tests {
    use super::{__replay_line, Sim, parse_seed};
    use rand::RngCore;

    #[test]
    fn replay_line_zero_seed_is_exact() {
        assert_eq!(
            __replay_line(0, "autumn-web", "my_test"),
            "AUTUMN_SIM_SEED=0x0 cargo test -p autumn-web my_test"
        );
    }

    #[test]
    fn replay_line_formats_seed_as_hex() {
        let line = __replay_line(0x9f3a, "autumn-web", "my_test");
        assert!(
            line.contains("0x9f3a"),
            "seed must be rendered in hex: {line}"
        );
        assert_eq!(
            line,
            "AUTUMN_SIM_SEED=0x9f3a cargo test -p autumn-web my_test"
        );
    }

    #[test]
    fn parse_seed_covers_hex_decimal_and_garbage() {
        assert_eq!(parse_seed("0"), 0);
        assert_eq!(parse_seed("0x9f3a"), 0x9f3a);
        assert_eq!(parse_seed("0X9F3A"), 0x9f3a);
        assert_eq!(parse_seed("42"), 42);
        assert_eq!(parse_seed("garbage"), 0);
        assert_eq!(parse_seed(""), 0);
        assert_eq!(parse_seed("  0x10  "), 0x10);
    }

    #[test]
    fn from_seed_exposes_the_seed() {
        assert_eq!(Sim::from_seed(0).seed, 0);
        assert_eq!(Sim::from_seed(7).seed(), 7);
    }

    #[test]
    fn same_seed_produces_identical_first_draw() {
        let mut a = Sim::from_seed(7);
        let mut b = Sim::from_seed(7);
        let da = a.rng().inner_mut().next_u64();
        let db = b.rng().inner_mut().next_u64();
        assert_eq!(da, db, "same seed must yield the same first RNG draw");
    }
}
