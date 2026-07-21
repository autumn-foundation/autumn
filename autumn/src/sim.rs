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
//! - **W3** exposes deterministic id / `Uuid` generation through [`SimRng`] and
//!   the [`crate::entropy::Rng`] extractor / [`crate::entropy::Entropy`] seam,
//!   and routes the framework's high-value id sites through it. Bridge a seeded
//!   source into a mounted app with [`Sim::seeded_entropy`].
//! - **W5** turns [`Chaos`] into a public fault-injection builder.
//!
//! Everything here is designed to grow additively (builder-style) without
//! breaking the frozen surface — hence the `#[non_exhaustive]` markers.

// The placeholder handles are intentionally thin in W1; their methods and
// docs fill in over later waves. These narrowly-scoped allows keep the
// skeleton clean under the workspace's pedantic lint set without masking real
// issues in the behavioral code that lands later.
#![allow(clippy::missing_const_for_fn)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use uuid::Uuid;

use crate::entropy::{Entropy, SeededEntropy, uuid_v4_from_bytes, uuid_v7_from_parts};
use crate::time::TickingClock;

// The per-sim SQLite DB lane (W4, issue #1797): a fresh, migrated, in-process
// SQLite substrate the sim builds its app on. Self-contained and additive — W2
// consumes its pool when it mounts the app on `SimApp`. Only meaningful under the
// `sqlite` feature (the sim's DB substrate is SQLite by design).
//
// Exposed as `#[doc(hidden)] pub` — unstable test/sim plumbing, not the stable
// surface — mirroring this module's `__seed_from_env` / `__replay_line` hidden
// hooks. It is `pub` (not `pub(crate)`) purely so the W4 DoD consolidated
// integration test, which is an external crate, can drive the end-to-end drain;
// the stable mount API remains W2's `Sim::build`.
#[cfg(feature = "sqlite")]
#[doc(hidden)]
pub mod substrate;

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
    /// (`2020-01-01T00:00:00Z`). Stepped by [`Sim::advance`] in lockstep with
    /// tokio's paused timer.
    clock: SimClock,

    /// Fault-injection configuration. Becomes a public builder in W5.
    #[allow(dead_code)] // fault-injection behavior lands in W5
    chaos: Chaos,

    /// Built [`crate::test::TestClient`] handle, mounted by [`Sim::build`] on
    /// the paused runtime with the virtual clock installed.
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
    /// Draw deterministic values and UUIDs through it — e.g.
    /// [`SimRng::uuid_v4`] / [`SimRng::next_u64`]. The same seed always yields
    /// the same draw sequence.
    #[must_use]
    pub fn rng(&mut self) -> &mut SimRng {
        &mut self.rng
    }

    /// Build a shared, seeded [`Entropy`] source for this simulation's seed,
    /// ready to inject into a mounted app via
    /// [`crate::state::AppState::with_entropy`].
    ///
    /// This is the bridge W3 provides for W2's app mounting: the app the
    /// simulation drives resolves the [`crate::entropy::Rng`] extractor and
    /// every framework-minted identifier (job ids, request ids, idempotency
    /// lock owners, session ids) from this source, so a fixed seed replays the
    /// whole identifier stream byte-for-byte.
    ///
    /// The returned source is seeded independently from the [`rng`](Self::rng)
    /// handle (both from [`seed`](Self::seed)), so drawing from one does not
    /// perturb the other's sequence.
    #[must_use]
    pub fn seeded_entropy(&self) -> Arc<dyn Entropy> {
        SeededEntropy::shared(self.seed)
    }
  
    /// Mount `app` on the paused runtime with the simulation's virtual clock
    /// installed, and return the resulting [`crate::test::TestClient`].
    ///
    /// The simulation's [`SimClock`] is threaded in via
    /// [`crate::test::TestApp::with_clock`], so every handler that reads a
    /// [`crate::time::Clock`] extractor sees the virtual instant — starting at
    /// the fixed sim epoch (`2020-01-01T00:00:00Z`) and moving only when
    /// [`Sim::advance`] steps it. The built app also starts the in-process job
    /// runtime (the in-memory backend), so [`run_to_idle`](Sim::run_to_idle)
    /// can drain enqueued jobs deterministically.
    ///
    /// Configure `app` fully before handing it over — routes, jobs, and (in a
    /// later wave) a sim database are attached to the [`crate::test::TestApp`]
    /// prior to this call. Do **not** call [`crate::test::TestApp::with_clock`]
    /// yourself; `build` owns the clock so time stays in lockstep.
    ///
    /// The returned borrow is convenient for an immediate request; to interleave
    /// requests with [`advance`](Sim::advance) / [`run_to_idle`](Sim::run_to_idle)
    /// calls, reach the client through [`client`](Sim::client) instead.
    ///
    /// ```rust,ignore
    /// let client = sim.build(TestApp::new().routes(routes![hello]).jobs(jobs![work]));
    /// client.get("/hello").send().await.assert_ok();
    /// ```
    pub fn build(&mut self, app: crate::test::TestApp) -> &crate::test::TestClient {
        let client = app.with_clock(self.clock.ticking()).build();
        self.app.client = Some(client);
        self.app.client()
    }

    /// Borrow the [`crate::test::TestClient`] mounted by [`build`](Sim::build).
    ///
    /// # Panics
    ///
    /// Panics if [`build`](Sim::build) has not been called yet.
    #[must_use]
    pub fn client(&self) -> &crate::test::TestClient {
        self.app.client()
    }

    /// Borrow the mounted [`crate::test::TestClient`], or `None` before
    /// [`build`](Sim::build).
    #[must_use]
    pub const fn try_client(&self) -> Option<&crate::test::TestClient> {
        self.app.try_client()
    }

    /// Advance virtual time by `duration`, stepping the injected wall clock and
    /// tokio's paused timer wheel **together**.
    ///
    /// The framework [`crate::time::Clock`] extractor (backed by the
    /// simulation's [`SimClock`]) and tokio's virtual timer (`tokio::time::sleep`,
    /// job backoff delays, delayed enqueues) move by exactly the same amount, so
    /// `Utc::now()`-via-extractor and a sleeping task stay in lockstep — a job
    /// whose retry backs off 24 hours fires the instant this advances 24 hours,
    /// with zero wall-clock delay. Timers that come due within the window fire
    /// and their tasks are polled before this returns.
    ///
    /// Pair with [`run_to_idle`](Sim::run_to_idle) to then drain the work the
    /// fired timers enqueued.
    pub async fn advance(&self, duration: std::time::Duration) {
        // Step the framework clock first so any task woken by the tokio timer
        // that reads the clock observes the already-advanced instant.
        self.clock.advance(duration);
        // Advance tokio's paused timer wheel; this fires due timers and yields
        // so their tasks are polled before returning.
        tokio::time::advance(duration).await;
    }

    /// Drain all ready work — enqueued jobs the in-process runtime can run now,
    /// plus tasks woken by timers that have already come due — until the runtime
    /// is quiescent.
    ///
    /// The sim runtime is a single-threaded, current-thread runtime with the
    /// clock paused, so background tasks (the job worker consuming its queue, a
    /// retry timer that just fired, a delayed enqueue delivering) make progress
    /// only when the running task yields. This cooperatively yields until no
    /// further ready progress is observed (bounded by `MAX_DRAIN_STEPS` so a
    /// pathological busy task can never hang the drain).
    ///
    /// It does **not** fast-forward to a *future* timer — advancing the clock to
    /// reach a not-yet-due backoff/sleep is [`advance`](Sim::advance)'s job
    /// (tokio exposes no next-deadline hook, and a blind auto-advance would
    /// break the clock lockstep). The idiom is therefore
    /// [`advance`](Sim::advance) to the next interesting instant, then
    /// `run_to_idle` to settle the work it released.
    pub async fn run_to_idle(&self) {
        // Resolve the mounted app's DB pool once (a cheap, cloned Arc-backed
        // handle). Durable repository commit hooks are rows, so there is
        // nothing to drain when the app was built without a database (e.g. the
        // in-memory job DoD path) — the lane is skipped entirely then.
        #[cfg(feature = "db")]
        let commit_hook_pool = self
            .app
            .try_client()
            .and_then(|client| crate::db::DbState::pool(client.state()).cloned());

        for _ in 0..MAX_DRAIN_STEPS {
            // One yield lets each currently-ready spawned task take a step; a
            // zero-duration timer advance flushes any timers registered for the
            // current instant and yields again, so a chain of ready timer/task
            // wakeups settles without advancing the clock. This quiesces the
            // in-process job runtime (TestJobRuntime / JobAdminMemoryBackend)
            // and any tasks woken by already-due scheduler ticks.
            tokio::task::yield_now().await;
            tokio::time::advance(std::time::Duration::ZERO).await;

            // Third ready-work source: durable repository commit hooks. Drain
            // the ready set through the public test-harness wrapper
            // (`crate::test::drain_ready_repository_commit_hooks`), which runs
            // the same claim → run → ack wiring the background commit-hook
            // worker uses, but deterministically and worker-free. A hook may
            // itself enqueue a job, so draining inside the settle loop lets a
            // subsequent iteration pick that job up, and the returned
            // hooks-drained count folds into the loop's quiescence: a nonzero
            // drain is progress that keeps this bounded settle running. Under
            // the paused runtime the job/timer sources expose no idle signal,
            // so the cooperative spin remains their settle mechanism and the
            // loop still exits when the step bound is hit.
            #[cfg(feature = "db")]
            if let Some(pool) = commit_hook_pool.as_ref() {
                let _hooks_drained =
                    crate::test::drain_ready_repository_commit_hooks(pool, MAX_DRAIN_STEPS).await;
            }
        }
    }
}

/// Upper bound on cooperative yield rounds [`Sim::run_to_idle`] performs before
/// returning, so a misbehaving always-ready task can never hang the drain.
/// Generous relative to the handful of hops a job takes from the queue through
/// its handler to completion under the single-threaded paused runtime.
const MAX_DRAIN_STEPS: usize = 1024;

/// A seeded, deterministic random number generator handle.
///
/// Wraps a `ChaCha8Rng` seeded from the simulation seed, so the same seed
/// always yields the same draw sequence. Draw deterministic bytes and UUIDs
/// through the generation helpers below; they share their `Uuid` bit-stamping
/// with the [`crate::entropy::Entropy`] source an app is seeded with, so a
/// `SimRng` draw and an equivalently-seeded app draw agree.
pub struct SimRng {
    seed: u64,
    inner: ChaCha8Rng,
}

impl SimRng {
    /// Seed a fresh deterministic RNG from `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed,
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    /// Derive a stable [`Uuid`] from this simulation's seed and a `purpose_tag`
    /// namespace, **independently of the draw stream** (seed-derived ids).
    ///
    /// Unlike [`uuid_v4`](Self::uuid_v4), this does **not** advance the RNG: the
    /// same seed and `purpose_tag` always produce the same UUID no matter how
    /// many other values have been drawn, so `derive_uuid("tenant:acme")` is a
    /// stable, byte-reproducible id for "acme" across runs and machines. Ideal
    /// for seeding multi-tenant fixtures without perturbing the deterministic id
    /// stream. See [`crate::entropy::SeededEntropy::derive_uuid`] for the shared
    /// mechanism and the version bits it sets (v4).
    #[must_use]
    pub fn derive_uuid(&self, purpose_tag: impl AsRef<[u8]>) -> Uuid {
        crate::entropy::derive_uuid_from(self.seed, purpose_tag.as_ref())
    }

    /// Draw the next deterministic `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// Fill `dest` with deterministic bytes.
    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest);
    }

    /// Draw a deterministic version-4 (fully random) [`Uuid`].
    ///
    /// The same seed and the same number of prior draws always yield the same
    /// UUID.
    #[must_use]
    pub fn uuid_v4(&mut self) -> Uuid {
        let mut bytes = [0u8; 16];
        self.inner.fill_bytes(&mut bytes);
        uuid_v4_from_bytes(bytes)
    }

    /// Draw a version-7 (time-ordered) [`Uuid`] whose 48-bit timestamp is
    /// `unix_millis` and whose remaining bits are drawn deterministically.
    #[must_use]
    pub fn uuid_v7(&mut self, unix_millis: u64) -> Uuid {
        let mut rand_bytes = [0u8; 10];
        self.inner.fill_bytes(&mut rand_bytes);
        uuid_v7_from_parts(unix_millis, rand_bytes)
    }

    /// Borrow the underlying `ChaCha8Rng`.
    ///
    /// Internal escape hatch for the determinism smoke test.
    #[cfg(test)]
    pub(crate) fn inner_mut(&mut self) -> &mut ChaCha8Rng {
        &mut self.inner
    }
}

/// A virtual clock handle for the simulation.
///
/// Wraps a [`TickingClock`] started at the fixed sim epoch. [`Sim::advance`]
/// steps this clock (the wall-clock time a [`crate::time::Clock`] extractor
/// reports) in lockstep with tokio's paused virtual timer, so
/// `Utc::now()`-via-extractor and `tokio::time::sleep` never drift apart.
pub struct SimClock {
    inner: TickingClock,
}

impl SimClock {
    /// Wrap a ticking clock as the simulation's virtual clock.
    pub(crate) fn new(inner: TickingClock) -> Self {
        Self { inner }
    }

    /// Step the injected wall clock forward by `duration`.
    ///
    /// This moves only the framework clock (the [`crate::time::Clock`]
    /// extractor / `ClockSource`); [`Sim::advance`] pairs it with
    /// `tokio::time::advance` so the tokio timer wheel moves the same amount.
    pub(crate) fn advance(&self, duration: std::time::Duration) {
        self.inner.advance(duration);
    }

    /// A [`TickingClock`] handle sharing this clock's instant.
    ///
    /// Handed to [`crate::test::TestApp::with_clock`] at [`Sim::build`] time so
    /// mounted handlers read the same virtual instant [`Sim::advance`] steps.
    pub(crate) fn ticking(&self) -> TickingClock {
        self.inner.clone()
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
/// Holds the [`crate::test::TestClient`] mounted by [`Sim::build`] on the paused
/// runtime with the simulation's virtual clock installed. Empty until
/// [`Sim::build`] is called (an empty [`#[sim_test]`](crate::sim_test) that only
/// drives time / RNG never mounts an app).
///
/// Marked `#[non_exhaustive]` so later waves (e.g. W4's sim-DB substrate) can
/// hang additional handles here without a breaking change.
#[non_exhaustive]
#[derive(Default)]
pub struct SimApp {
    /// The mounted test client, or `None` before [`Sim::build`].
    client: Option<crate::test::TestClient>,
}

impl SimApp {
    /// Borrow the mounted [`crate::test::TestClient`].
    ///
    /// # Panics
    ///
    /// Panics if no app has been mounted yet — call [`Sim::build`] first.
    #[must_use]
    pub fn client(&self) -> &crate::test::TestClient {
        self.try_client()
            .expect("no app mounted: call `sim.build(TestApp::new()...)` before `client()`")
    }

    /// Borrow the mounted [`crate::test::TestClient`], or `None` before
    /// [`Sim::build`].
    #[must_use]
    pub const fn try_client(&self) -> Option<&crate::test::TestClient> {
        self.client.as_ref()
    }
}

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
