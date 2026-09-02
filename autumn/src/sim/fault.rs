//! Authored, seed-deterministic fault-injection scenarios (issue #1680).
//!
//! [`FaultPlan`] is the **authored** lane of the sim harness's fault injection.
//! Instead of asking for a failure *rate*, you name the exact effects that must
//! fail — "the 3rd database checkout", "the 2nd execution of `send_invoice`" —
//! attach the plan to a [`TestApp`](crate::test::TestApp), and drive the app.
//! The harness fails exactly those effects through the existing
//! [`interceptor`](crate::interceptor) seams (no application code changes),
//! records what happened, and hands back a serializable [`FaultOutcome`] a test
//! can assert on — or compare byte-for-byte across a hundred replays.
//!
//! ```rust,ignore
//! # async fn scenario() {
//! use autumn_web::sim::FaultPlan;
//! use autumn_web::test::TestApp;
//!
//! let plan = FaultPlan::from_seed(0x5EED)
//!     .fail_db_checkout(3)                  // the 3rd checkout, any pool
//!     .fail_job("send_invoice", 2)          // the 2nd `send_invoice` execution
//!     .random_job_execution_faults(2, 1..=10); // 2 seed-picked ordinals in 1..=10
//!
//! let client = TestApp::new()
//!     .routes(routes![touch])
//!     .plugin(InvoiceJobs)                  // the plugin registering `send_invoice`
//!     .with_fault_plan(plan)
//!     .build();
//! // … drive requests / drain jobs …
//! let outcome = client.fault_outcome().await;
//! assert!(outcome.unfired.is_empty()); // every authored ordinal was reached
//! let json = outcome.to_json_string(); // canonical, byte-comparable
//! # }
//! ```
//!
//! # How this differs from [`Chaos`](crate::sim::Chaos)
//!
//! Both lanes are reproducible; they answer different questions.
//!
//! | | [`Chaos`](crate::sim::Chaos) | [`FaultPlan`] |
//! |---|---|---|
//! | What you author | a **probability** (`db_transient_errors(0.1)`) | an **ordinal** (`fail_db_checkout(3)`) |
//! | Where the seed lands | every hook draws from the seeded stream | only the `random_*` builders draw, once, at builder-call time |
//! | What you assert | the recorded decision log | a serializable [`FaultOutcome`] — fired / suppressed / unfired / 5xx / final state |
//! | Installed by | [`Sim::chaos`](crate::sim::Sim::chaos) | [`TestApp::with_fault_plan`](crate::test::TestApp::with_fault_plan) |
//! | Question it answers | "is my code resilient to *some* failures?" | "does *this* failure still reproduce?" |
//!
//! `Chaos` explores; a `FaultPlan` pins. The intended workflow is to find a bug
//! with the probabilistic lane (or in production), then freeze it as an authored
//! plan that fails before the fix and passes after — a real regression test.
//!
//! The two compose: a plan's interceptors are chained *inside* whatever
//! [`Sim::chaos`](crate::sim::Sim::chaos) or
//! [`with_job_interceptor`](crate::test::TestApp::with_job_interceptor) already
//! installed, never replacing them.
//!
//! # The determinism contract
//!
//! * **An explicit plan draws no entropy at all.** `fail_db_checkout(3)` is a
//!   fact about the 3rd checkout, not a coin flip, so the schedule is
//!   reproducible *by construction* — independent of the seed and of every other
//!   fault lane's draws.
//! * **The `random_*` builders draw once, at builder-call time**, from a
//!   dedicated stream seeded `seed ^ FAULT_STREAM_SALT ^ effect_salt` —
//!   distinct from the app-facing [`Entropy`] source and from every chaos salt,
//!   so a plan's draws neither perturb nor are perturbed by the application's
//!   own identifier stream — and per-effect, so asking for random job-execution
//!   ordinals and random checkout ordinals on one plan does not pick the same
//!   numbers twice. Each call re-derives that stream from the seed and the
//!   effect alone, so the picked ordinals are a pure function of
//!   `(seed, effect, count, within)`: where the call sits in the builder chain
//!   does not matter, and repeating the same call on one plan is a no-op (it
//!   picks the same ordinals, and duplicate entries collapse). The picks are
//!   resolved immediately into ordinary [`PlannedFault`] entries, so
//!   [`FaultPlan::planned`] always describes the whole schedule.
//! * **Fault timing reads only the app's injected [`ClockSource`]** —
//!   [`FiredFault::at`] and [`FiredFault::elapsed_ms`] come from the clock
//!   [`TestApp::build`](crate::test::TestApp::build) resolved, which under a
//!   [`Sim`](crate::sim::Sim) is the virtual clock. Nothing here reads
//!   `Utc::now()` or `Instant::now()`.
//! * **The outcome record is canonical.** [`FaultOutcome`] contains only
//!   integers, strings, RFC 3339 timestamps, structs and `Vec`s — no maps, no
//!   floats, no request ids (those are entropy-minted) — so
//!   [`FaultOutcome::to_json_string`] is byte-stable and
//!   [`FaultOutcome::fingerprint`] is a stable 64-bit digest of a whole run.
//!
//! # Determinism is scoped to a paused runtime + injected clock + seeded entropy
//!
//! Ordinals are counted in the order the effects actually reach the interceptor.
//! That order is only reproducible when the work is scheduled deterministically:
//!
//! * run under [`#[sim_test]`](crate::sim_test) (or
//!   `#[tokio::test(flavor = "current_thread", start_paused = true)]`) — on a
//!   multi-threaded runtime two job workers racing for the queue can swap
//!   execution #2 and #3 between runs;
//! * keep `jobs.workers = 1` (the default, and
//!   [`TestApp::build`](crate::test::TestApp::build) asserts it while a plan is
//!   attached);
//! * let the harness inject the clock and the entropy — attaching a plan
//!   defaults the app's [`Entropy`] to `SeededEntropy::shared(plan.seed())` so
//!   request ids and job-retry jitter replay from the same seed, unless the test
//!   supplied its own with
//!   [`with_entropy`](crate::test::TestApp::with_entropy).
//!
//! Outside those conditions the *plan* is still exact — the same effects are
//! still targeted — but the interleaving that decides which checkout is "the
//! 3rd" is the test's own responsibility.
//!
//! [`Entropy`]: crate::entropy::Entropy
//! [`ClockSource`]: crate::time::ClockSource

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::entropy::{Entropy, SeededEntropy};
use crate::time::{ClockSource, MonotonicInstant};

/// Salt `XOR`ed into a plan's seed to derive the stream the `random_*` builders
/// draw their ordinals from.
///
/// Kept distinct from the app-facing entropy seeding (the raw seed) and from
/// every [`chaos`](crate::sim::chaos) salt, so choosing random ordinals never
/// shifts another lane's schedule. `XOR`ed further with
/// [`FaultEffect::stream_salt`] so each effect class draws its own stream. An
/// arbitrary fixed constant; its only requirement is being non-zero and unlike
/// its neighbours.
const FAULT_STREAM_SALT: u64 = 0xFA17_0DEA_FA17_0DEA;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// ── Plan ─────────────────────────────────────────────────────────────────────

/// Which effect class a planned or fired fault targets.
///
/// `#[non_exhaustive]` so later waves can add classes (an outbound HTTP call, a
/// channel publish) without a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultEffect {
    /// A database connection checkout through the [`Db`](crate::db::Db)
    /// extractor, wrapped via
    /// [`DbConnectionInterceptor`](crate::interceptor::DbConnectionInterceptor).
    /// The fault target is the pool name (`"primary"`, a replica, a shard).
    DbCheckout,
    /// One execution of an already-enqueued job, wrapped via
    /// [`JobInterceptor::intercept_execute`](crate::interceptor::JobInterceptor::intercept_execute).
    /// The fault target is the job name.
    JobExecution,
}

impl FaultEffect {
    /// The `snake_case` label used in [`FaultPlan::describe`] and in the
    /// serialized form.
    const fn label(self) -> &'static str {
        match self {
            Self::DbCheckout => "db_checkout",
            Self::JobExecution => "job_execution",
        }
    }

    /// Per-effect salt folded into the `random_*` sampling stream.
    ///
    /// Without it `random_job_execution_faults(2, 1..=6)` and
    /// `random_db_checkout_faults(2, 1..=6)` on the same plan would draw from
    /// one stream seeded only by `seed ^ FAULT_STREAM_SALT` and pick *identical*
    /// ordinals — a correlation nobody authored. Arbitrary fixed constants;
    /// their only requirement is being non-zero and distinct from each other.
    const fn stream_salt(self) -> u64 {
        match self {
            Self::DbCheckout => 0xD8CE_4B07_D8CE_4B07,
            Self::JobExecution => 0x70B5_EC07_70B5_EC07,
        }
    }
}

/// One authored entry of a [`FaultPlan`]: "fail the `ordinal`-th `effect` on
/// `target`".
///
/// Entries are compared and ordered field-by-field in declaration order, which
/// is what makes [`FaultPlan::planned`] a stable, deterministic list.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlannedFault {
    /// The effect class this entry targets.
    pub effect: FaultEffect,
    /// `None` = any target, counted on the effect's global counter;
    /// `Some("primary")` / `Some("send_invoice")` = counted on that target's own
    /// counter.
    pub target: Option<String>,
    /// The **1-based** ordinal on the counter named by `(effect, target)`.
    pub ordinal: u32,
}

/// A reproducible, authored fault schedule, constructed from a single `u64`
/// seed.
///
/// A plan is **pure data**: it holds no counters and no runtime state, so
/// cloning one and attaching it to two apps yields two independent ledgers.
/// Attach it with
/// [`TestApp::with_fault_plan`](crate::test::TestApp::with_fault_plan); the
/// runtime handle it produces is a [`FaultLedger`], reachable from the built
/// client.
///
/// Ordinals are **1-based** — `fail_db_checkout(3)` fails the third checkout —
/// and an ordinal of `0` is ignored rather than silently faulting the first
/// effect, mirroring [`Chaos`](crate::sim::Chaos)'s `smtp_faults` schedule.
/// Duplicate `(effect, target, ordinal)` entries collapse to one.
///
/// `#[non_exhaustive]` with private fields: always build one through
/// [`from_seed`](Self::from_seed) and the chaining setters.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultPlan {
    /// The seed the `random_*` builders draw from.
    seed: u64,
    /// The authored schedule. A `BTreeSet` so duplicates collapse and the
    /// schedule is order-independent: two plans that author the same faults in
    /// a different order are equal.
    entries: BTreeSet<PlannedFault>,
    /// Optional half-open elapsed-time window `[from, to)` outside which a
    /// matched fault is suppressed instead of fired.
    window: Option<(Duration, Duration)>,
}

impl FaultPlan {
    /// Start an empty plan seeded from `seed`.
    ///
    /// The seed only matters for the `random_*` builders; an explicit-only plan
    /// is deterministic whatever the seed is. It is still recorded in
    /// [`FaultOutcome::seed`], and — when the test supplies no
    /// [`with_entropy`](crate::test::TestApp::with_entropy) of its own — it
    /// seeds the app's identifier stream too.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        Self {
            seed,
            entries: BTreeSet::new(),
            window: None,
        }
    }

    /// The seed this plan was constructed from.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Fail the `ordinal`-th job execution, counted across **every** job name.
    ///
    /// An `ordinal` of `0` is ignored.
    #[must_use]
    pub fn fail_job_execution(self, ordinal: u32) -> Self {
        self.push(FaultEffect::JobExecution, None, ordinal)
    }

    /// Fail the `ordinal`-th execution of the job named `job`, counted on that
    /// name's **own** counter.
    ///
    /// Independent of [`fail_job_execution`](Self::fail_job_execution)'s global
    /// counter: with two job names interleaved, `fail_job("b", 2)` fires on the
    /// second `b` execution whichever global position that lands on.
    ///
    /// An `ordinal` of `0` is ignored.
    #[must_use]
    pub fn fail_job(self, job: impl Into<String>, ordinal: u32) -> Self {
        self.push(FaultEffect::JobExecution, Some(job.into()), ordinal)
    }

    /// Pick `count` distinct job-execution ordinals uniformly from `within` (the
    /// global job-execution counter), derived from this plan's seed.
    ///
    /// This is the one builder where the seed does the choosing. The draw
    /// happens **now**, from a stream seeded
    /// `seed ^ FAULT_STREAM_SALT ^ effect_salt`, and is resolved immediately
    /// into ordinary entries — so the result shows up in
    /// [`planned`](Self::planned) and replays byte-for-byte for a given
    /// `(seed, effect, count, within)`, wherever in the chain the call sits.
    /// Because the picks depend on nothing else, calling this twice with the
    /// same arguments on one plan is a no-op: the second call picks the same
    /// ordinals and the duplicate entries collapse.
    ///
    /// `count` is clamped to the length of `within`; a descending range picks
    /// nothing, and any ordinal `0` in range is skipped.
    #[must_use]
    pub fn random_job_execution_faults(
        self,
        count: u32,
        within: std::ops::RangeInclusive<u32>,
    ) -> Self {
        self.push_random(FaultEffect::JobExecution, count, &within)
    }

    /// Fail the `ordinal`-th database connection checkout, counted across
    /// **every** pool.
    ///
    /// An `ordinal` of `0` is ignored.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn fail_db_checkout(self, ordinal: u32) -> Self {
        self.push(FaultEffect::DbCheckout, None, ordinal)
    }

    /// Fail the `ordinal`-th checkout from the pool named `pool` (`"primary"`, a
    /// replica, or a shard name), counted on that pool's **own** counter.
    ///
    /// An `ordinal` of `0` is ignored.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn fail_db_checkout_on(self, pool: impl Into<String>, ordinal: u32) -> Self {
        self.push(FaultEffect::DbCheckout, Some(pool.into()), ordinal)
    }

    /// Pick `count` distinct database-checkout ordinals uniformly from `within`
    /// (the global checkout counter), derived from this plan's seed.
    ///
    /// The database twin of
    /// [`random_job_execution_faults`](Self::random_job_execution_faults); the
    /// same determinism rules apply.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn random_db_checkout_faults(
        self,
        count: u32,
        within: std::ops::RangeInclusive<u32>,
    ) -> Self {
        self.push_random(FaultEffect::DbCheckout, count, &within)
    }

    /// Restrict every fault in this plan to the half-open elapsed-time window
    /// `[from, to)`.
    ///
    /// `elapsed` is measured as
    /// `clock.monotonic().saturating_duration_since(app_started_at)` on the
    /// app's **injected** clock — virtual under a [`Sim`](crate::sim::Sim) — so
    /// the window moves only when the test moves the clock.
    ///
    /// Outside the window a matched effect still **counts toward its ordinal**
    /// (the ordinal is consumed) but the fault is not injected: it is recorded
    /// in [`FaultOutcome::suppressed`] instead of [`FaultOutcome::fired`], and
    /// the operation proceeds normally. Calling this twice replaces the window;
    /// an empty or inverted window (`from >= to`) suppresses everything.
    #[must_use]
    pub fn only_between(mut self, from: Duration, to: Duration) -> Self {
        self.window = Some((from, to));
        self
    }

    /// The authored schedule, sorted by `(effect, target, ordinal)`.
    ///
    /// Deterministic and complete: `random_*` picks are already resolved into
    /// entries here, so this is the whole plan.
    #[must_use]
    pub fn planned(&self) -> Vec<PlannedFault> {
        self.entries.iter().cloned().collect()
    }

    /// A human-readable, one-line-per-fault rendering of the schedule.
    ///
    /// Intended for panic messages and debugging output.
    ///
    /// Stable for a given plan (the entries are sorted), so it is safe to assert
    /// on in a test.
    #[must_use]
    pub fn describe(&self) -> String {
        use std::fmt::Write as _;

        let mut out = format!("fault plan (seed {:#018x})", self.seed);
        if self.entries.is_empty() {
            out.push_str("\n  (no faults planned)");
        }
        for entry in &self.entries {
            let _ = match entry.target.as_deref() {
                Some(target) => write!(
                    out,
                    "\n  {} ordinal {} of `{target}`",
                    entry.effect.label(),
                    entry.ordinal
                ),
                None => write!(
                    out,
                    "\n  {} ordinal {} (any target)",
                    entry.effect.label(),
                    entry.ordinal
                ),
            };
        }
        if let Some((from, to)) = self.window {
            let _ = write!(
                out,
                "\n  window: fires only while elapsed is in [{}ms, {}ms)",
                from.as_millis(),
                to.as_millis()
            );
        }
        out
    }

    /// Whether at least one fault is planned.
    ///
    /// An inactive plan still installs its interceptors (so the counters in
    /// [`FaultOutcome::final_state`] are populated) but can never fire.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.entries.is_empty()
    }

    /// The configured elapsed-time window, if any.
    pub(crate) const fn window(&self) -> Option<(Duration, Duration)> {
        self.window
    }

    /// Insert one entry, ignoring a `0` ordinal and collapsing duplicates.
    fn push(mut self, effect: FaultEffect, target: Option<String>, ordinal: u32) -> Self {
        if ordinal >= 1 {
            self.entries.insert(PlannedFault {
                effect,
                target,
                ordinal,
            });
        }
        self
    }

    /// Resolve `count` distinct seed-derived ordinals in `within` into entries
    /// on `effect`'s global counter.
    fn push_random(
        mut self,
        effect: FaultEffect,
        count: u32,
        within: &std::ops::RangeInclusive<u32>,
    ) -> Self {
        for ordinal in sample_distinct(self.seed, effect, count, within) {
            if ordinal >= 1 {
                self.entries.insert(PlannedFault {
                    effect,
                    target: None,
                    ordinal,
                });
            }
        }
        self
    }
}

/// Draw `count` distinct values from `within`, uniformly, from the stream seeded
/// `seed ^ FAULT_STREAM_SALT ^ effect.stream_salt()`.
///
/// Folding the effect into the stream keeps the lanes independent: without it
/// `random_job_execution_faults(2, 1..=6)` and `random_db_checkout_faults(2,
/// 1..=6)` on one plan would return the same ordinals.
///
/// Floyd's sampling algorithm: exactly `min(count, |within|)` draws, no
/// rejection loop, and therefore no unbounded work for a nearly-exhausted range.
/// An inverted range yields nothing.
///
/// The `%` reduction below is very slightly biased towards the low end of the
/// range (at most one extra chance in `2^64 / (j + 1)`, i.e. under 2⁻³² for any
/// range that fits a `u32`). That is far below anything a fault schedule cares
/// about, and the draw is reproducible either way — which is the property this
/// module actually sells.
fn sample_distinct(
    seed: u64,
    effect: FaultEffect,
    count: u32,
    within: &std::ops::RangeInclusive<u32>,
) -> Vec<u32> {
    let (lo, hi) = (*within.start(), *within.end());
    if hi < lo || count == 0 {
        return Vec::new();
    }
    let len = u64::from(hi - lo) + 1;
    let take = u64::from(count).min(len);
    let stream = SeededEntropy::new(seed ^ FAULT_STREAM_SALT ^ effect.stream_salt());
    let mut chosen: BTreeSet<u32> = BTreeSet::new();
    for j in (len - take)..len {
        // `j + 1 <= len <= 2^32`, so the modulus and the offset both fit u32.
        let offset = u32::try_from(stream.next_u64() % (j + 1)).unwrap_or(u32::MAX);
        let candidate = lo.saturating_add(offset);
        if !chosen.insert(candidate) {
            let fallback = lo.saturating_add(u32::try_from(j).unwrap_or(u32::MAX));
            chosen.insert(fallback);
        }
    }
    chosen.into_iter().collect()
}

// ── Outcome ──────────────────────────────────────────────────────────────────

/// One fault that matched an authored ordinal, with the timing it matched at.
///
/// Present in [`FaultOutcome::fired`] when the fault was injected, or in
/// [`FaultOutcome::suppressed`] when it matched but fell outside
/// [`FaultPlan::only_between`]'s window.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiredFault {
    /// The effect class that was faulted.
    pub effect: FaultEffect,
    /// The actual target the effect ran against — the pool name for a checkout,
    /// the job name for an execution.
    pub target: String,
    /// The **global** (per-effect) 1-based ordinal this fault fired at.
    pub ordinal: u32,
    /// The **per-target** 1-based ordinal this fault fired at.
    pub target_ordinal: u32,
    /// Wall-clock reading from the app's injected
    /// [`ClockSource`] when the fault fired.
    pub at: DateTime<Utc>,
    /// Milliseconds since app start, on the injected monotonic clock.
    pub elapsed_ms: u64,
}

/// A 5xx response captured through autumn's error-reporting layer, projected to
/// the deterministic fields.
///
/// Deliberately **no request id**: those are minted from the app's entropy, so
/// carrying one would make an otherwise byte-stable outcome depend on the
/// identifier stream.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedError {
    /// The HTTP status of the failing response (always 5xx).
    pub status: u16,
    /// The HTTP method of the failing request, when the reporter knew it.
    pub method: Option<String>,
    /// The matched route template (e.g. `/users/{id}`), when known.
    pub route: Option<String>,
    /// The error message the reporter received.
    pub message: String,
    /// The Problem Details `type` URI, when the error carried one.
    pub problem_type: Option<String>,
}

/// Totals the fault interceptors observed over a whole scenario run.
///
/// These count **every** pass through the seam, faulted or not, so a test can
/// assert on the shape of the run and not only on the faults.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalState {
    /// Database checkouts that reached the fault interceptor.
    pub db_checkouts: u64,
    /// Job executions that reached the fault interceptor.
    pub job_executions: u64,
    /// Job executions that returned an error — injected **or** a genuine
    /// handler failure.
    ///
    /// A handler that **panics** counts here too: the panic unwinds through the
    /// fault interceptor (the job runtime catches it further out), and the
    /// interceptor records the pass as failed on the way out, so
    /// `job_executions == job_executions_failed + job_executions_succeeded`
    /// always holds.
    pub job_executions_failed: u64,
    /// Job executions that returned `Ok`.
    pub job_executions_succeeded: u64,
}

/// The structured, serializable record of one fault-injection scenario run.
///
/// This is the artifact a regression test asserts on. It is built only from
/// integers, strings, RFC 3339 timestamps, structs and `Vec`s — no maps, no
/// floats, no entropy-minted identifiers — so
/// [`to_json_string`](Self::to_json_string) is canonical and byte-comparable
/// across runs and machines.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultOutcome {
    /// The seed of the plan that produced this run.
    pub seed: u64,
    /// Faults that were injected, in the order they fired.
    ///
    /// One entry per faulted effect **pass**, not per planned entry: a single
    /// pass that matches both a global entry (`fail_job_execution(2)`) and a
    /// per-target entry (`fail_job("probe", 2)`) fails the operation once and
    /// is recorded here once, while marking **both** planned entries reached —
    /// so neither of them shows up in [`unfired`](Self::unfired).
    pub fired: Vec<FiredFault>,
    /// Faults that matched an authored ordinal but fell outside
    /// [`FaultPlan::only_between`]'s window, in the order they matched. Their
    /// ordinals were consumed; the operations proceeded normally.
    pub suppressed: Vec<FiredFault>,
    /// Planned faults whose ordinal was never reached, sorted.
    pub unfired: Vec<PlannedFault>,
    /// 5xx responses captured through autumn's error-reporting layer, in report
    /// order. Always present; empty when the `reporting` feature is off.
    pub server_errors: Vec<ReportedError>,
    /// Totals for the whole run.
    pub final_state: FinalState,
}

impl FaultOutcome {
    /// Serialize to canonical JSON.
    ///
    /// Field order is declaration order and every value is order-stable, so two
    /// identical runs produce byte-identical strings — which is what a "replay
    /// this scenario 100 times" regression test compares.
    ///
    /// # Panics
    ///
    /// Cannot panic in practice: every field is a plain integer, string,
    /// timestamp, struct or `Vec`, none of which can fail to serialize.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).expect("FaultOutcome contains only serializable plain data")
    }

    /// A stable 64-bit digest (FNV-1a) of [`to_json_string`](Self::to_json_string).
    ///
    /// Convenient for asserting "this run is the same run" without carrying the
    /// whole JSON in the assertion message.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut hash = FNV_OFFSET_BASIS;
        for byte in self.to_json_string().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Parse an outcome back from [`to_json_string`](Self::to_json_string).
    ///
    /// # Errors
    ///
    /// Returns the [`serde_json::Error`] when `json` is not valid JSON or does
    /// not match this shape.
    pub fn from_json_str(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

// ── Ledger ───────────────────────────────────────────────────────────────────

/// What the fault decision for one effect pass came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// No authored ordinal matched, or the match fell outside the window —
    /// run the real operation.
    Proceed,
    /// Inject the fault. Both ordinals travel with the decision so the injected
    /// error message can name the pass on either counter — the global one and
    /// the target's own — without re-reading (and re-racing) the ledger.
    Fail { ordinal: u32, target_ordinal: u32 },
}

/// The mutable half of a run's record, behind one lock so `fired`,
/// `suppressed` and `reached` can never disagree about a single decision.
#[derive(Debug, Default)]
struct Recorded {
    fired: Vec<FiredFault>,
    suppressed: Vec<FiredFault>,
    /// Planned entries whose ordinal has been reached (fired or suppressed).
    reached: BTreeSet<PlannedFault>,
    /// Per-`(effect, target)` counters; the 1-based ordinal is the value after
    /// the increment.
    target_seq: BTreeMap<(FaultEffect, String), u64>,
}

/// Shared runtime state backing one app's fault plan.
struct FaultState {
    seed: u64,
    /// The authored schedule, sorted — the basis for `unfired`.
    planned: Vec<PlannedFault>,
    /// Authored ordinals on each effect's global counter.
    global_ordinals: BTreeMap<FaultEffect, BTreeSet<u32>>,
    /// Authored ordinals on each `(effect, target)` counter.
    target_ordinals: BTreeMap<(FaultEffect, String), BTreeSet<u32>>,
    window: Option<(Duration, Duration)>,
    /// The clock `TestApp::build` resolved, so the timings this records are on
    /// the same (possibly virtual) timeline the app reads.
    clock: Arc<dyn ClockSource>,
    /// The app's start instant on that same clock.
    started_at: MonotonicInstant,
    db_checkout_seq: AtomicU64,
    job_execution_seq: AtomicU64,
    job_executions_failed: AtomicU64,
    job_executions_succeeded: AtomicU64,
    recorded: Mutex<Recorded>,
    server_errors: Mutex<Vec<ReportedError>>,
}

impl FaultState {
    fn new(plan: &FaultPlan, clock: Arc<dyn ClockSource>, started_at: MonotonicInstant) -> Self {
        let mut global_ordinals: BTreeMap<FaultEffect, BTreeSet<u32>> = BTreeMap::new();
        let mut target_ordinals: BTreeMap<(FaultEffect, String), BTreeSet<u32>> = BTreeMap::new();
        let planned = plan.planned();
        for entry in &planned {
            match entry.target.as_ref() {
                Some(target) => {
                    target_ordinals
                        .entry((entry.effect, target.clone()))
                        .or_default()
                        .insert(entry.ordinal);
                }
                None => {
                    global_ordinals
                        .entry(entry.effect)
                        .or_default()
                        .insert(entry.ordinal);
                }
            }
        }
        Self {
            seed: plan.seed(),
            planned,
            global_ordinals,
            target_ordinals,
            window: plan.window(),
            clock,
            started_at,
            db_checkout_seq: AtomicU64::new(0),
            job_execution_seq: AtomicU64::new(0),
            job_executions_failed: AtomicU64::new(0),
            job_executions_succeeded: AtomicU64::new(0),
            recorded: Mutex::new(Recorded::default()),
            server_errors: Mutex::new(Vec::new()),
        }
    }

    /// Count one pass of `effect` against `target` and decide what to do with
    /// it, recording the decision.
    ///
    /// At most **one** [`FiredFault`] is recorded per pass. A pass whose global
    /// ordinal and per-target ordinal are *both* authored fires once, and marks
    /// both planned entries reached — so neither lands in
    /// [`FaultOutcome::unfired`].
    ///
    /// Holds no `.await` across the lock, so the plain `std::sync::Mutex` is
    /// safe inside the async interceptors.
    fn observe(&self, effect: FaultEffect, target: &str) -> Decision {
        let counter = match effect {
            FaultEffect::DbCheckout => &self.db_checkout_seq,
            FaultEffect::JobExecution => &self.job_execution_seq,
        };

        // The global ordinal is allocated INSIDE the `recorded` lock, together
        // with the per-target one. Bumping the atomic before taking the lock
        // let two passes on a multi-threaded runtime interleave and pair one
        // pass's global ordinal with another's target ordinal in a single
        // record. The counters stay atomics; the lock is what orders them.
        let mut recorded = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ordinal = saturating_ordinal(counter.fetch_add(1, Ordering::SeqCst) + 1);
        let target_ordinal = {
            let seq = recorded
                .target_seq
                .entry((effect, target.to_owned()))
                .or_insert(0);
            *seq += 1;
            saturating_ordinal(*seq)
        };

        let matched_global = self
            .global_ordinals
            .get(&effect)
            .is_some_and(|ordinals| ordinals.contains(&ordinal));
        let matched_target = self
            .target_ordinals
            .get(&(effect, target.to_owned()))
            .is_some_and(|ordinals| ordinals.contains(&target_ordinal));
        if !matched_global && !matched_target {
            return Decision::Proceed;
        }
        if matched_global {
            recorded.reached.insert(PlannedFault {
                effect,
                target: None,
                ordinal,
            });
        }
        if matched_target {
            recorded.reached.insert(PlannedFault {
                effect,
                target: Some(target.to_owned()),
                ordinal: target_ordinal,
            });
        }

        let elapsed = self
            .clock
            .monotonic()
            .saturating_duration_since(self.started_at);
        let record = FiredFault {
            effect,
            target: target.to_owned(),
            ordinal,
            target_ordinal,
            at: self.clock.now(),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        };
        let in_window = self
            .window
            .is_none_or(|(from, to)| elapsed >= from && elapsed < to);
        if in_window {
            recorded.fired.push(record);
            Decision::Fail {
                ordinal,
                target_ordinal,
            }
        } else {
            recorded.suppressed.push(record);
            Decision::Proceed
        }
    }

    /// Fold one job execution's result into the run totals.
    fn record_execution_result(&self, succeeded: bool) {
        let counter = if succeeded {
            &self.job_executions_succeeded
        } else {
            &self.job_executions_failed
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Only the `reporting`-gated [`FaultReporter`] pushes here; without that
    /// feature [`FaultOutcome::server_errors`] is always empty (by contract).
    #[cfg(feature = "reporting")]
    fn record_server_error(&self, error: ReportedError) {
        self.server_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(error);
    }

    fn server_errors_len(&self) -> usize {
        self.server_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn outcome(&self) -> FaultOutcome {
        let recorded = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unfired = self
            .planned
            .iter()
            .filter(|entry| !recorded.reached.contains(*entry))
            .cloned()
            .collect();
        let outcome = FaultOutcome {
            seed: self.seed,
            fired: recorded.fired.clone(),
            suppressed: recorded.suppressed.clone(),
            unfired,
            server_errors: self
                .server_errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            final_state: FinalState {
                db_checkouts: self.db_checkout_seq.load(Ordering::SeqCst),
                job_executions: self.job_execution_seq.load(Ordering::SeqCst),
                job_executions_failed: self.job_executions_failed.load(Ordering::SeqCst),
                job_executions_succeeded: self.job_executions_succeeded.load(Ordering::SeqCst),
            },
        };
        drop(recorded);
        outcome
    }
}

/// Clamp a `u64` counter to a `u32` ordinal; a run with more than 4 billion
/// effects reports the ceiling rather than wrapping into a wrong ordinal.
fn saturating_ordinal(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The runtime handle for a [`FaultPlan`] attached to one built app.
///
/// Created by [`TestApp::build`](crate::test::TestApp::build) — never by the
/// plan — so counting restarts with the app: a
/// [`Sim::kill`](crate::sim::Sim::kill) + [`restart`](crate::sim::Sim::restart)
/// starts a fresh ledger, exactly as a real process restart would.
///
/// Cloning a `FaultLedger` shares the same ledger. Reach one through
/// [`TestClient::fault_ledger`](crate::test::TestClient::fault_ledger).
#[non_exhaustive]
#[derive(Clone)]
pub struct FaultLedger(Arc<FaultState>);

impl std::fmt::Debug for FaultLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultLedger")
            .field("seed", &self.0.seed)
            .field("planned", &self.0.planned)
            .field("fired", &self.0.recorded.lock().map(|r| r.fired.len()).ok())
            .finish()
    }
}

impl FaultLedger {
    /// Snapshot the run **now**, without settling the detached reporter tasks.
    ///
    /// Prefer
    /// [`TestClient::fault_outcome`](crate::test::TestClient::fault_outcome),
    /// which waits for the 5xx the client itself observed to reach
    /// [`FaultOutcome::server_errors`] first. Use this when there are no HTTP
    /// requests in play (a jobs-only scenario) or when reading mid-run.
    #[must_use]
    pub fn outcome(&self) -> FaultOutcome {
        self.0.outcome()
    }

    /// Build the ledger for `plan` against the clock and start instant
    /// `TestApp::build` resolved.
    pub(crate) fn new(
        plan: &FaultPlan,
        clock: Arc<dyn ClockSource>,
        started_at: MonotonicInstant,
    ) -> Self {
        Self(Arc::new(FaultState::new(plan, clock, started_at)))
    }

    /// The job interceptor to chain **innermost** of the job chain.
    pub(crate) fn job_interceptor(&self) -> Arc<dyn crate::interceptor::JobInterceptor> {
        Arc::new(FaultJobInterceptor {
            state: Arc::clone(&self.0),
        })
    }

    /// The database interceptor, wrapping `inner` (the recorder/transactional/
    /// user chain) so the fault decision runs **innermost**.
    #[cfg(feature = "db")]
    pub(crate) fn db_interceptor(
        &self,
        inner: Option<Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    ) -> Arc<dyn crate::interceptor::DbConnectionInterceptor> {
        Arc::new(FaultDbInterceptor {
            state: Arc::clone(&self.0),
            inner,
        })
    }

    /// The error reporter that projects 5xx events into
    /// [`FaultOutcome::server_errors`].
    #[cfg(feature = "reporting")]
    pub(crate) fn reporter(&self) -> Arc<dyn crate::reporting::ErrorReporter> {
        Arc::new(FaultReporter {
            state: Arc::clone(&self.0),
        })
    }

    /// How many 5xx have been recorded so far — the settle signal
    /// `TestClient::fault_outcome` waits on.
    pub(crate) fn server_errors_len(&self) -> usize {
        self.0.server_errors_len()
    }
}

// ── Interceptors ─────────────────────────────────────────────────────────────

/// Records a job execution that never returned — a panicking handler unwinding
/// through `next.await`, or a cancelled execution future — as a failed pass.
///
/// Without it [`FinalState::job_executions`] would not equal
/// `job_executions_failed + job_executions_succeeded`, because the counting
/// statement after the await is simply never reached.
struct RecordOnUnwind<'a> {
    state: &'a FaultState,
    /// Cleared once `next.await` has returned normally, so the real result is
    /// recorded exactly once instead of twice.
    armed: bool,
}

impl Drop for RecordOnUnwind<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.state.record_execution_result(false);
        }
    }
}

/// Injects the plan's job-execution faults.
///
/// `intercept_enqueue` is a pass-through: a plan targets *executions*, so
/// enqueuing is never refused. On the execute seam it counts the pass, and when
/// an authored ordinal matches inside the window it returns a
/// `service_unavailable` error **without awaiting `next`** — the handler never
/// runs, the attempt fails, and the runtime's own retry policy takes over
/// exactly as it would for a real transient failure.
struct FaultJobInterceptor {
    state: Arc<FaultState>,
}

impl crate::interceptor::JobInterceptor for FaultJobInterceptor {
    fn intercept_enqueue<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        next
    }

    fn intercept_execute<'a>(
        &'a self,
        name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.state.observe(FaultEffect::JobExecution, name) {
                Decision::Fail {
                    ordinal,
                    target_ordinal,
                } => {
                    self.state.record_execution_result(false);
                    Err(crate::AutumnError::service_unavailable_msg(format!(
                        "fault plan: injected job execution failure \
                         (execution #{ordinal} overall, #{target_ordinal} of `{name}`)"
                    )))
                }
                Decision::Proceed => {
                    // A panicking handler unwinds straight through `next.await`
                    // (the job runtime catches it outside this seam), so a bare
                    // post-await count would drop the pass and leave
                    // `job_executions != failed + succeeded`. The guard books
                    // the unwind as a failure; the happy path disarms it and
                    // records the real result.
                    let mut guard = RecordOnUnwind {
                        state: self.state.as_ref(),
                        armed: true,
                    };
                    let result = next.await;
                    guard.armed = false;
                    self.state.record_execution_result(result.is_ok());
                    result
                }
            }
        })
    }
}

/// Injects the plan's database-checkout faults, innermost of the checkout chain.
///
/// `inner` is whatever chain `TestApp::build` had already composed (the user's
/// interceptor, the transactional-isolation interceptor, or both). The fault
/// decision is built as the innermost `next` and handed to `inner`, so a user
/// interceptor observes an injected failure exactly as it would a real pool
/// timeout, and transactional test isolation keeps working — including under the
/// `sqlite` feature, where the harness's own `ComposedDbInterceptor` does not
/// exist.
#[cfg(feature = "db")]
struct FaultDbInterceptor {
    state: Arc<FaultState>,
    inner: Option<Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
}

#[cfg(feature = "db")]
impl crate::interceptor::DbConnectionInterceptor for FaultDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        ctx: crate::interceptor::DbCheckoutContext,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                    > + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                > + Send
                + 'a,
        >,
    > {
        let pool_name = ctx.pool_name.clone();
        let faulted: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                    > + Send
                    + 'a,
            >,
        > = Box::pin(async move {
            match self.state.observe(FaultEffect::DbCheckout, &pool_name) {
                Decision::Fail {
                    ordinal,
                    target_ordinal,
                } => Err(crate::AutumnError::service_unavailable_msg(format!(
                    "fault plan: injected database checkout failure \
                     (checkout #{ordinal} overall, #{target_ordinal} from `{pool_name}`)"
                ))),
                Decision::Proceed => next.await,
            }
        });
        match self.inner.as_ref() {
            Some(inner) => inner.intercept_checkout(ctx, faulted),
            None => faulted,
        }
    }

    /// Forwarded to the wrapped chain: the fault lane adds no isolation of its
    /// own, and swallowing this marker would silently disable transactional
    /// test isolation for every app that attaches a plan.
    fn is_transactional_test(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.is_transactional_test())
    }
}

/// Projects every reported 5xx into [`FaultOutcome::server_errors`].
///
/// Registered alongside (never instead of) the app's own reporters. The
/// projection drops the request id on purpose — see [`ReportedError`].
#[cfg(feature = "reporting")]
struct FaultReporter {
    state: Arc<FaultState>,
}

#[cfg(feature = "reporting")]
impl crate::reporting::ErrorReporter for FaultReporter {
    fn report<'a>(
        &'a self,
        event: &'a crate::reporting::ErrorEvent,
    ) -> crate::reporting::ReportFuture<'a> {
        // Recorded as the future is constructed rather than when it is polled:
        // the reporter chain does both on the same detached task, and doing it
        // here shortens the window `TestClient::fault_outcome` has to settle.
        self.state.record_server_error(ReportedError {
            status: event.status.as_u16(),
            method: event.method.clone(),
            route: event.route.clone(),
            message: event.message.clone(),
            problem_type: event.problem_type.clone(),
        });
        Box::pin(std::future::ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Decision, FaultEffect, FaultOutcome, FaultPlan, FaultState, FinalState, PlannedFault,
        sample_distinct,
    };
    use crate::time::{ClockSource as _, TickingClock};
    use std::sync::Arc;
    use std::time::Duration;

    const SEED: u64 = 0x5EED;

    /// The sim epoch (`2020-01-01T00:00:00Z`), so the timings a windowed test
    /// asserts on read like the ones a `#[sim_test]` produces.
    fn epoch() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_577_836_800, 0).expect("valid timestamp")
    }

    /// A ledger state driven by a virtual clock the test still holds a handle
    /// to — `TickingClock` clones share one instant, exactly as
    /// `TestApp::with_clock` relies on.
    fn ticking_state(plan: &FaultPlan) -> (TickingClock, FaultState) {
        let clock = TickingClock::starting_at(epoch());
        let started_at = clock.monotonic();
        let state = FaultState::new(plan, Arc::new(clock.clone()), started_at);
        (clock, state)
    }

    #[test]
    fn a_fresh_plan_is_inactive_and_describes_itself() {
        let plan = FaultPlan::from_seed(SEED);
        assert_eq!(plan.seed(), SEED);
        assert!(!plan.is_active());
        assert!(plan.planned().is_empty());
        assert!(plan.describe().contains("no faults planned"));
    }

    #[test]
    fn ordinal_zero_is_ignored() {
        let plan = FaultPlan::from_seed(SEED)
            .fail_job_execution(0)
            .fail_job("probe", 0);
        assert!(
            !plan.is_active(),
            "a 0 ordinal must never silently fault the first effect"
        );
        // …but a real ordinal on the same plan still lands.
        assert!(plan.fail_job_execution(1).is_active());
    }

    #[test]
    fn duplicate_entries_collapse_and_planned_is_sorted() {
        let plan = FaultPlan::from_seed(SEED)
            .fail_job("b", 2)
            .fail_job_execution(3)
            .fail_job("a", 1)
            .fail_job_execution(3) // duplicate of the global entry
            .fail_job("b", 2); // duplicate of the named entry
        let planned = plan.planned();
        assert_eq!(planned.len(), 3, "duplicates collapse: {planned:?}");
        let mut sorted = planned.clone();
        sorted.sort();
        assert_eq!(planned, sorted, "planned() is sorted");
        // `None` sorts before `Some`, so the global entry leads.
        assert_eq!(
            planned[0],
            PlannedFault {
                effect: FaultEffect::JobExecution,
                target: None,
                ordinal: 3,
            }
        );
        assert_eq!(planned[1].target.as_deref(), Some("a"));
        assert_eq!(planned[2].target.as_deref(), Some("b"));
    }

    #[test]
    fn authoring_order_does_not_change_the_plan() {
        let a = FaultPlan::from_seed(SEED)
            .fail_job_execution(1)
            .fail_job("probe", 4);
        let b = FaultPlan::from_seed(SEED)
            .fail_job("probe", 4)
            .fail_job_execution(1);
        assert_eq!(a, b);
    }

    #[test]
    fn describe_mentions_every_ordinal_and_the_window() {
        let plan = FaultPlan::from_seed(SEED)
            .fail_job_execution(2)
            .fail_job("probe", 7)
            .only_between(Duration::from_secs(5), Duration::from_secs(10));
        let described = plan.describe();
        assert!(!described.is_empty());
        assert!(described.contains("ordinal 2"), "{described}");
        assert!(described.contains("ordinal 7 of `probe`"), "{described}");
        assert!(described.contains("job_execution"), "{described}");
        assert!(described.contains("window"), "{described}");
        assert!(described.contains("5000ms"), "{described}");
    }

    #[test]
    fn random_ordinals_replay_from_the_seed() {
        let build = |seed: u64| {
            FaultPlan::from_seed(seed)
                .random_job_execution_faults(3, 1..=20)
                .planned()
        };
        assert_eq!(build(SEED), build(SEED), "same seed ⇒ same schedule");
        assert_ne!(
            build(SEED),
            build(SEED + 1),
            "different seeds should (overwhelmingly likely) diverge"
        );
        // Every pick lands inside the requested range, and there are `count` of
        // them (distinct, so the set does not collapse).
        let planned = build(SEED);
        assert_eq!(planned.len(), 3);
        assert!(planned.iter().all(|p| (1..=20).contains(&p.ordinal)));
        assert!(
            planned
                .iter()
                .all(|p| p.effect == FaultEffect::JobExecution && p.target.is_none())
        );
    }

    #[test]
    fn random_ordinals_do_not_depend_on_the_call_position() {
        let a = FaultPlan::from_seed(SEED)
            .fail_job("probe", 9)
            .random_job_execution_faults(2, 1..=8);
        let b = FaultPlan::from_seed(SEED)
            .random_job_execution_faults(2, 1..=8)
            .fail_job("probe", 9);
        assert_eq!(a, b);
    }

    #[test]
    fn a_count_beyond_the_range_clamps_to_the_range() {
        let planned = FaultPlan::from_seed(SEED)
            .random_job_execution_faults(50, 3..=6)
            .planned();
        assert_eq!(planned.len(), 4, "range 3..=6 holds only 4 ordinals");
        assert!(planned.iter().all(|p| (3..=6).contains(&p.ordinal)));
    }

    #[test]
    fn degenerate_sampling_ranges_pick_nothing() {
        let effect = FaultEffect::JobExecution;
        assert!(
            sample_distinct(SEED, effect, 0, &(1..=10)).is_empty(),
            "count 0"
        );
        assert!(
            sample_distinct(SEED, effect, 3, &std::ops::RangeInclusive::new(10, 1)).is_empty(),
            "inverted range"
        );
        assert_eq!(
            sample_distinct(SEED, effect, 3, &(7..=7)),
            vec![7],
            "single value"
        );
        // A range starting at 0 never yields a 0 ordinal in the plan.
        let planned = FaultPlan::from_seed(SEED)
            .random_job_execution_faults(1, 0..=0)
            .planned();
        assert!(planned.is_empty(), "ordinal 0 is filtered out of the plan");
    }

    /// Drives the real `FaultState::observe` through a virtual clock rather
    /// than re-implementing the predicate in the test (which would assert
    /// nothing about production code).
    #[test]
    fn the_window_is_half_open_on_the_injected_clock() {
        let window = (Duration::from_secs(5), Duration::from_secs(10));
        let plan = FaultPlan::from_seed(SEED)
            .fail_job_execution(1)
            .fail_job_execution(2)
            .fail_job_execution(3)
            .only_between(window.0, window.1);
        assert_eq!(plan.window(), Some(window));
        let (clock, state) = ticking_state(&plan);

        // t = 0 — before the window: the ordinal is consumed, nothing fires.
        assert_eq!(
            state.observe(FaultEffect::JobExecution, "probe"),
            Decision::Proceed
        );
        let outcome = state.outcome();
        assert!(outcome.fired.is_empty(), "before the window: {outcome:?}");
        assert_eq!(outcome.suppressed.len(), 1);

        // t = 5s — the start is inclusive.
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            state.observe(FaultEffect::JobExecution, "probe"),
            Decision::Fail {
                ordinal: 2,
                target_ordinal: 2,
            },
            "the window start is inclusive"
        );
        let outcome = state.outcome();
        assert_eq!(outcome.fired.len(), 1, "{outcome:?}");
        assert_eq!(outcome.fired[0].elapsed_ms, 5_000);
        assert_eq!(outcome.fired[0].at, epoch() + chrono::Duration::seconds(5));

        // t = 10s — the end is exclusive.
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            state.observe(FaultEffect::JobExecution, "probe"),
            Decision::Proceed
        );
        let outcome = state.outcome();
        assert_eq!(
            outcome.fired.len(),
            1,
            "the window end is exclusive: {outcome:?}"
        );
        assert_eq!(outcome.suppressed.len(), 2);
        assert!(
            outcome.unfired.is_empty(),
            "every authored ordinal was reached: {outcome:?}"
        );
    }

    #[test]
    fn an_inverted_window_suppresses_everything() {
        let plan = FaultPlan::from_seed(SEED)
            .fail_job_execution(1)
            .fail_job_execution(2)
            .fail_job_execution(3)
            .only_between(Duration::from_secs(10), Duration::from_secs(5));
        let (clock, state) = ticking_state(&plan);
        for _ in 0..3 {
            assert_eq!(
                state.observe(FaultEffect::JobExecution, "probe"),
                Decision::Proceed
            );
            clock.advance(Duration::from_secs(7));
        }
        let outcome = state.outcome();
        assert!(
            outcome.fired.is_empty(),
            "`from >= to` contains no instant at all: {outcome:?}"
        );
        assert_eq!(outcome.suppressed.len(), 3);
        assert!(outcome.unfired.is_empty());
    }

    /// One pass can satisfy two authored entries at once; it must fault the
    /// operation once and retire both entries.
    #[test]
    fn a_pass_matching_a_global_and_a_target_entry_fires_once() {
        let plan = FaultPlan::from_seed(SEED)
            .fail_job_execution(2)
            .fail_job("probe", 2);
        assert_eq!(
            plan.planned().len(),
            2,
            "two distinct entries were authored"
        );
        let (_clock, state) = ticking_state(&plan);

        assert_eq!(
            state.observe(FaultEffect::JobExecution, "probe"),
            Decision::Proceed
        );
        assert_eq!(
            state.observe(FaultEffect::JobExecution, "probe"),
            Decision::Fail {
                ordinal: 2,
                target_ordinal: 2,
            }
        );

        let outcome = state.outcome();
        assert_eq!(
            outcome.fired.len(),
            1,
            "one FiredFault per faulted pass, not per matched entry: {outcome:?}"
        );
        assert!(
            outcome.unfired.is_empty(),
            "both planned entries count as reached: {outcome:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn db_builders_author_db_entries() {
        let planned = FaultPlan::from_seed(SEED)
            .fail_db_checkout(3)
            .fail_db_checkout_on("replica", 2)
            .random_db_checkout_faults(2, 1..=9)
            .planned();
        assert!(
            planned
                .iter()
                .all(|p| p.effect == FaultEffect::DbCheckout && p.ordinal >= 1)
        );
        assert!(
            planned
                .iter()
                .any(|p| p.target.as_deref() == Some("replica"))
        );
        assert!(planned.len() >= 3);
    }

    /// The two `random_*` lanes must not shadow each other: before the
    /// per-effect salt they drew from one stream and picked the same ordinals.
    #[cfg(feature = "db")]
    #[test]
    fn the_random_lanes_draw_independent_ordinals() {
        let planned = FaultPlan::from_seed(SEED)
            .random_job_execution_faults(2, 1..=6)
            .random_db_checkout_faults(2, 1..=6)
            .planned();
        let picks = |effect: FaultEffect| -> Vec<u32> {
            planned
                .iter()
                .filter(|p| p.effect == effect)
                .map(|p| p.ordinal)
                .collect()
        };
        let jobs = picks(FaultEffect::JobExecution);
        let dbs = picks(FaultEffect::DbCheckout);
        assert_eq!(jobs.len(), 2, "{planned:?}");
        assert_eq!(dbs.len(), 2, "{planned:?}");
        assert_ne!(jobs, dbs, "each effect draws from its own salted stream");
    }

    fn sample_outcome() -> FaultOutcome {
        FaultOutcome {
            seed: SEED,
            fired: vec![super::FiredFault {
                effect: FaultEffect::JobExecution,
                target: "probe".to_owned(),
                ordinal: 2,
                target_ordinal: 2,
                at: chrono::DateTime::from_timestamp(1_577_836_806, 0).expect("valid timestamp"),
                elapsed_ms: 6_000,
            }],
            suppressed: Vec::new(),
            unfired: vec![PlannedFault {
                effect: FaultEffect::DbCheckout,
                target: Some("primary".to_owned()),
                ordinal: 4,
            }],
            server_errors: vec![super::ReportedError {
                status: 503,
                method: Some("GET".to_owned()),
                route: Some("/touch".to_owned()),
                message: "fault plan: injected database checkout failure (checkout #3)".to_owned(),
                problem_type: None,
            }],
            final_state: FinalState {
                db_checkouts: 5,
                job_executions: 4,
                job_executions_failed: 1,
                job_executions_succeeded: 3,
            },
        }
    }

    #[test]
    fn outcome_json_round_trips_and_is_canonical() {
        let outcome = sample_outcome();
        let json = outcome.to_json_string();
        assert_eq!(
            json,
            outcome.to_json_string(),
            "serialization is byte-stable"
        );
        assert!(json.contains("\"fired\""), "{json}");
        assert!(json.contains("\"server_errors\""), "{json}");
        assert!(json.contains("\"final_state\""), "{json}");
        assert!(json.contains("\"job_execution\""), "snake_case effects");
        let parsed = FaultOutcome::from_json_str(&json).expect("round-trips");
        assert_eq!(parsed, outcome);
        assert_eq!(parsed.fingerprint(), outcome.fingerprint());
    }

    #[test]
    fn fingerprint_is_stable_and_discriminating() {
        let outcome = sample_outcome();
        assert_eq!(outcome.fingerprint(), outcome.fingerprint());
        let mut other = outcome.clone();
        other.final_state.job_executions += 1;
        assert_ne!(
            outcome.fingerprint(),
            other.fingerprint(),
            "a different run must fingerprint differently"
        );
    }

    #[test]
    fn from_json_str_rejects_garbage() {
        assert!(FaultOutcome::from_json_str("not json").is_err());
        assert!(FaultOutcome::from_json_str("{}").is_err());
    }

    // ── End-to-end wiring smoke test ────────────────────────────────────────
    //
    // Proves the whole seam in one cheap, DB-free run: the plan reaches
    // `TestApp::build`, the fault interceptor is chained innermost of the job
    // chain (the always-on `JobRecorder` still records the enqueue), the
    // targeted execution fails, the runtime retries it, and the ledger's
    // outcome reports exactly that.

    /// Handler attempts the smoke-test job actually reached. Process-global
    /// because a `JobInfo` handler is a plain `fn` pointer with no captures.
    static SMOKE_ATTEMPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Registers the probe job the way an application would — jobs reach a
    /// `TestApp` through a plugin, not a direct setter.
    struct SmokeProbeJobs;

    impl crate::plugin::Plugin for SmokeProbeJobs {
        fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
            app.jobs(vec![crate::job::JobInfo::new(
                "smoke_probe",
                3,
                10,
                |_state, _payload| {
                    Box::pin(async move {
                        SMOKE_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                },
            )])
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_planned_job_execution_fault_fires_and_is_retried() {
        use std::sync::atomic::Ordering;

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();
        SMOKE_ATTEMPTS.store(0, Ordering::SeqCst);

        let mut sim = crate::sim::Sim::from_seed(SEED);
        let plan = FaultPlan::from_seed(SEED).fail_job("smoke_probe", 1);
        sim.build(
            crate::test::TestApp::new()
                .plugin(SmokeProbeJobs)
                .with_fault_plan(plan),
        );

        crate::job::enqueue("smoke_probe", serde_json::json!({}))
            .await
            .expect("enqueue succeeds");
        sim.run_to_idle().await;
        // Cross the retry backoff, then let the second attempt run.
        sim.advance(Duration::from_secs(1)).await;
        sim.run_to_idle().await;

        let outcome = sim
            .client()
            .fault_ledger()
            .expect("a plan was attached")
            .outcome();
        assert_eq!(outcome.fired.len(), 1, "{outcome:?}");
        assert_eq!(outcome.fired[0].effect, FaultEffect::JobExecution);
        assert_eq!(outcome.fired[0].target, "smoke_probe");
        assert_eq!(outcome.fired[0].ordinal, 1);
        assert_eq!(outcome.fired[0].target_ordinal, 1);
        assert!(outcome.unfired.is_empty());
        assert_eq!(outcome.final_state.job_executions, 2);
        assert_eq!(outcome.final_state.job_executions_failed, 1);
        assert_eq!(outcome.final_state.job_executions_succeeded, 1);
        assert_eq!(
            SMOKE_ATTEMPTS.load(Ordering::SeqCst),
            1,
            "the faulted attempt never reached the handler; the retry did"
        );
        // The always-on enqueue recorder still sees the job: the fault lane
        // composes with it rather than replacing it.
        assert_eq!(sim.client().enqueued_jobs().len(), 1);

        crate::job::clear_global_job_client();
    }
}
