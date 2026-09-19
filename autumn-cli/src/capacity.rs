//! `autumn calibrate` — the pure half of the capacity contract tooling
//! (issue #1733).
//!
//! Everything in this module is deterministic and unit-tested: the ladder
//! statistics, the saturation-knee rule, the seeded request profile, the
//! `--check` comparison, and its rendered diff. The subprocess / TCP / timing
//! half — building the target binary, booting it, driving load, sampling —
//! lives in [`crate::capacity_driver`], mirroring the
//! `dev_loop_bench` / `overload_driver` split.
//!
//! # The rule the contract encodes
//!
//! Calibration walks a ladder of offered concurrency levels. The **saturation
//! knee** is the last rung where more concurrency still bought materially more
//! throughput; past it, concurrency buys queueing latency instead. That rung's
//! throughput and P99 become the proven envelope, and its concurrency becomes
//! the admission limit the runtime enforces — so the binary sheds exactly
//! where it stops getting faster, rather than degrading past it.
//!
//! # Why `--check` is narrow on purpose
//!
//! The gate compares two numbers, and only on a matching host class. It never
//! compares timestamps, git provenance, or the contract digest, because a
//! no-op rebuild changes all three — and a capacity gate that cries wolf gets
//! turned off, which is strictly worse than not having one.

use autumn_web::capacity::{CapacityContract, ResourceShape, RouteShape};
use serde::Deserialize;

/// Default band a rebuild's sustained throughput may fall inside before
/// `--check` calls it a regression.
///
/// Sized between observed run-to-run noise and the 30% regression the issue's
/// Success Metric requires be caught. That window is narrower than it looks:
/// repeated no-op calibrations of an identical build on a shared 4-vCPU runner
/// spread by up to 20% before median-of-repeats was introduced, so this sits
/// deliberately close to the regression it must catch. A capacity gate needs a
/// runner class quiet enough for the two to stay separated — see the guide.
pub const DEFAULT_RPS_TOLERANCE: f64 = 0.20;

/// Default band a rebuild's P99 latency may rise inside before `--check`
/// calls it a regression. Wider than the throughput band: tail latency is the
/// noisier of the two measurements.
pub const DEFAULT_P99_TOLERANCE: f64 = 0.25;

/// Absolute slack, in milliseconds, added to the P99 ceiling on top of the
/// proportional tolerance.
///
/// A purely relative band collapses on a fast app: an app whose handlers
/// return a `&'static str` has a loopback P99 of a few hundred microseconds,
/// where 25% is well under the jitter one context switch on a shared CI runner
/// contributes. Without this floor the gate would fail no-op rebuilds of
/// exactly the reference apps it is most likely to be pointed at — and on the
/// metric the issue's success criterion does not even ask about.
pub const P99_ABSOLUTE_SLACK_MS: f64 = 1.0;

/// Raw samples collected at one rung of the concurrency ladder.
#[derive(Debug, Clone)]
pub struct RungSamples {
    /// Offered concurrency at this rung.
    pub concurrency: usize,
    /// Requests that did not return a success status (or failed to connect).
    pub errors: u64,
    /// Wall-clock seconds the rung ran for.
    pub wall_secs: f64,
    /// One latency sample, in milliseconds, per successful response.
    pub latencies_ms: Vec<f64>,
}

/// Derived statistics for one rung.
#[derive(Debug, Clone, PartialEq)]
pub struct RungStats {
    /// Offered concurrency at this rung.
    pub concurrency: usize,
    /// Successful responses observed. Carried so a rung that completed
    /// *nothing* stays distinguishable from one that genuinely measured zero
    /// throughput — without it, a ladder that never ran would look like a
    /// valid saturation point at the first rung.
    pub completed: u64,
    /// Successful responses per second.
    pub rps: f64,
    /// P99 latency of successful responses, in milliseconds.
    pub p99_ms: f64,
    /// Failed responses as a fraction of attempted ones.
    pub error_rate: f64,
}

impl RungStats {
    /// Reduce raw samples to the three numbers the knee rule reads.
    ///
    /// P99 is nearest-rank over the successful responses; a rung that
    /// completed nothing is reported as zero throughput rather than as a
    /// division by zero.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn from_samples(samples: &RungSamples) -> Self {
        let completed = samples.latencies_ms.len() as u64;
        let attempted = completed.saturating_add(samples.errors);

        let rps = if samples.wall_secs > 0.0 {
            completed as f64 / samples.wall_secs
        } else {
            0.0
        };

        let mut sorted = samples.latencies_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let p99_ms = percentile(&sorted, 0.99);

        let error_rate = if attempted > 0 {
            samples.errors as f64 / attempted as f64
        } else {
            0.0
        };

        Self {
            concurrency: samples.concurrency,
            completed,
            rps,
            p99_ms,
            error_rate,
        }
    }
}

/// Nearest-rank percentile over an already-sorted slice. Zero when empty.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // Nearest rank: the smallest value at or above the q-th position. The
    // casts are bounded by construction — `rank` is clamped into `[1, len]`
    // before it is used as an index.
    #[allow(clippy::cast_precision_loss)]
    let scaled = q * sorted.len() as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = scaled.ceil().max(1.0) as usize;
    let idx = rank.min(sorted.len()).saturating_sub(1);
    sorted.get(idx).copied().unwrap_or(0.0)
}

/// The median of repeated measurements of one rung, by throughput.
///
/// A single sample per rung makes the whole gate hostage to whatever else the
/// machine was doing during that one window. Measured on a shared runner, the
/// spread between repeats of an *identical* build reached 20% — wider than the
/// throughput tolerance itself, which would fail no-op rebuilds. Taking the
/// median of an odd number of repeats is what the sibling dev-loop benchmarks
/// do, and it collapses that spread without pretending the noise is not there.
///
/// The whole `RungStats` of the median-throughput repeat is returned, rather
/// than a per-field median, so the reported P99 and error rate belong to the
/// same measurement as the reported throughput.
#[must_use]
pub fn median_rung(repeats: &[RungStats]) -> Option<RungStats> {
    if repeats.is_empty() {
        return None;
    }
    let mut sorted: Vec<&RungStats> = repeats.iter().collect();
    sorted.sort_by(|a, b| a.rps.total_cmp(&b.rps));
    sorted.get(sorted.len() / 2).map(|stats| (*stats).clone())
}

/// Knobs for the saturation rule.
#[derive(Debug, Clone, Copy)]
pub struct SaturationOptions {
    /// Fractional throughput gain a rung must show over the previous one to
    /// count as "still buying capacity".
    pub min_throughput_gain: f64,
    /// Error rate above which a rung is not sustained capacity at all,
    /// however fast it looked.
    pub max_error_rate: f64,
}

impl Default for SaturationOptions {
    fn default() -> Self {
        Self {
            min_throughput_gain: 0.05,
            max_error_rate: 0.01,
        }
    }
}

/// The saturation knee: the last rung that still bought throughput.
///
/// Walks the ladder in ascending concurrency and stops at the first rung that
/// either failed too much or gained too little. Stopping early is deliberate:
/// a contract that *under*-claims costs some headroom, while one that
/// over-claims is a promise the binary cannot keep under load.
///
/// `None` when the ladder is empty, when its first rung completed nothing at
/// all (a zero-length rung, or an app that answered no request), or when that
/// rung failed beyond [`SaturationOptions::max_error_rate`] — in each case
/// there is no sustainable envelope to record, and recording one anyway would
/// hand the runtime a ceiling nobody measured.
#[must_use]
pub fn find_saturation(rungs: &[RungStats], opts: &SaturationOptions) -> Option<RungStats> {
    let mut iter = rungs.iter();
    let first = iter.next()?;
    if first.error_rate > opts.max_error_rate || first.completed == 0 {
        return None;
    }

    let mut best = first;
    for rung in iter {
        if rung.error_rate > opts.max_error_rate || rung.completed == 0 {
            break;
        }
        if rung.rps > best.rps * (1.0 + opts.min_throughput_gain) {
            best = rung;
        } else {
            break;
        }
    }
    Some(best.clone())
}

/// How far a rebuild may drift from the committed contract before `--check`
/// fails.
#[derive(Debug, Clone, Copy)]
pub struct Tolerances {
    /// Fraction the sustained throughput may fall by.
    pub rps: f64,
    /// Fraction the P99 latency may rise by.
    pub p99: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            rps: DEFAULT_RPS_TOLERANCE,
            p99: DEFAULT_P99_TOLERANCE,
        }
    }
}

/// Verdict of a `--check` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Within tolerance (or better). Exit 0.
    Pass,
    /// Throughput or tail latency moved beyond tolerance. Exit non-zero.
    Regressed,
    /// The rebuild was measured on a different host class than the committed
    /// contract, so the two envelopes are not comparable and no verdict is
    /// reached. Exit non-zero, but with a different diagnosis than a
    /// regression — the fix is to re-calibrate, not to optimise.
    HostMismatch,
    /// The committed contract does not describe a usable envelope (a
    /// non-positive or `NaN` throughput or latency), so there is nothing to
    /// gate against. Distinct from [`Self::Regressed`] because the build under
    /// test is not what is wrong.
    Unusable,
}

/// One metric's committed → candidate movement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricDelta {
    /// Value recorded in the committed contract.
    pub committed: f64,
    /// Value measured by this rebuild.
    pub candidate: f64,
    /// Signed percentage change from committed to candidate.
    pub pct_change: f64,
}

impl MetricDelta {
    #[must_use]
    fn new(committed: f64, candidate: f64) -> Self {
        let pct_change = if committed > 0.0 {
            (candidate - committed) / committed * 100.0
        } else {
            0.0
        };
        Self {
            committed,
            candidate,
            pct_change,
        }
    }
}

/// The result of comparing a rebuild's contract against the committed one.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// Overall verdict.
    pub outcome: CheckOutcome,
    /// Sustained throughput movement.
    pub rps: MetricDelta,
    /// P99 latency movement.
    pub p99: MetricDelta,
    /// Tolerances the comparison used.
    pub tolerances: Tolerances,
    /// Human-readable reasons the gate failed. Empty on a pass.
    pub regressions: Vec<String>,
    /// Informational observations — improvements, and route-shape drift.
    /// Never fail the gate: a route graph change is a reason to *read* the
    /// diff, not to block a build that still meets its envelope.
    pub notes: Vec<String>,
    /// Host class the committed contract was measured on.
    pub host_committed: String,
    /// Host class this rebuild was measured on.
    pub host_candidate: String,
}

/// Compare a freshly measured contract against the committed one.
#[must_use]
pub fn check_contract(
    committed: &CapacityContract,
    candidate: &CapacityContract,
    tolerances: &Tolerances,
) -> CheckReport {
    let rps = MetricDelta::new(
        committed.envelope.sustained_rps,
        candidate.envelope.sustained_rps,
    );
    let p99 = MetricDelta::new(
        committed.envelope.p99_latency_ms,
        candidate.envelope.p99_latency_ms,
    );

    let mut report = CheckReport {
        outcome: CheckOutcome::Pass,
        rps,
        p99,
        tolerances: *tolerances,
        regressions: Vec::new(),
        notes: Vec::new(),
        host_committed: committed.host.summary(),
        host_candidate: candidate.host.summary(),
    };

    if !committed.host.matches(&candidate.host) {
        // Judging the numbers here would be judging the hardware.
        report.outcome = CheckOutcome::HostMismatch;
        return report;
    }

    // A committed envelope of zero (a degenerate calibration, or a hand-edit)
    // would make `candidate < 0.0 * 0.85` unsatisfiable and silently neuter
    // exactly the check this gate exists for. Refuse to compare rather than
    // report a green pass nobody earned. `is_finite` also rejects NaN and
    // infinities, which every ordinary comparison would wave through.
    let unusable = |value: f64| !value.is_finite() || value <= 0.0;
    if unusable(committed.envelope.sustained_rps) || unusable(committed.envelope.p99_latency_ms) {
        report.outcome = CheckOutcome::Unusable;
        return report;
    }

    report.notes.extend(shape_drift_notes(committed, candidate));

    let rps_floor = committed.envelope.sustained_rps * (1.0 - tolerances.rps);
    if candidate.envelope.sustained_rps < rps_floor {
        report.regressions.push(format!(
            "sustained throughput regressed {:.1}% ({:.1} → {:.1} req/s), \
             beyond the {:.1}% tolerance",
            -rps.pct_change,
            rps.committed,
            rps.candidate,
            tolerances.rps * 100.0
        ));
    }

    let p99_ceiling = (committed.envelope.p99_latency_ms * (1.0 + tolerances.p99))
        .max(committed.envelope.p99_latency_ms + P99_ABSOLUTE_SLACK_MS);
    if candidate.envelope.p99_latency_ms > p99_ceiling {
        report.regressions.push(format!(
            "P99 latency regressed {:.1}% ({:.2} → {:.2} ms), \
             beyond the {:.1}% tolerance",
            p99.pct_change,
            p99.committed,
            p99.candidate,
            tolerances.p99 * 100.0
        ));
    }

    if !report.regressions.is_empty() {
        report.outcome = CheckOutcome::Regressed;
    }
    report
}

/// Route-graph observations worth surfacing next to the numbers.
///
/// Routes are keyed by `(method, path)`; handler renames are deliberately
/// invisible, matching `route_graph_digest`.
fn shape_drift_notes(committed: &CapacityContract, candidate: &CapacityContract) -> Vec<String> {
    use std::collections::BTreeMap;

    let key = |r: &RouteShape| (r.method.clone(), r.path.clone());
    let before: BTreeMap<_, _> = committed
        .routes
        .iter()
        .map(|r| (key(r), r.clone()))
        .collect();
    let after: BTreeMap<_, _> = candidate
        .routes
        .iter()
        .map(|r| (key(r), r.clone()))
        .collect();

    let mut notes = Vec::new();
    for ((method, path), route) in &after {
        match before.get(&(method.clone(), path.clone())) {
            None => notes.push(format!(
                "route added: {method} {path} ({}{})",
                route.shape,
                pool_suffix(&route.pools)
            )),
            Some(old) if old.shape != route.shape || old.pools != route.pools => {
                notes.push(format!(
                    "route shape changed: {method} {path} {}{} → {}{}",
                    old.shape,
                    pool_suffix(&old.pools),
                    route.shape,
                    pool_suffix(&route.pools)
                ));
            }
            Some(_) => {}
        }
    }
    for (method, path) in before.keys() {
        if !after.contains_key(&(method.clone(), path.clone())) {
            notes.push(format!("route removed: {method} {path}"));
        }
    }
    notes
}

/// `" [db, cache]"`, or empty when a route proves no pool.
fn pool_suffix(pools: &[String]) -> String {
    if pools.is_empty() {
        String::new()
    } else {
        format!(" [{}]", pools.join(", "))
    }
}

/// Render a `--check` report for a human reading CI output.
#[must_use]
pub fn render_check_report(report: &CheckReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();

    out.push_str("\u{1F342} autumn calibrate --check\n\n");
    // Writing into a `String` is infallible, so the `Result` carries nothing
    // to handle.
    let _ = writeln!(
        out,
        "  host              {} (committed: {})",
        report.host_candidate, report.host_committed
    );
    let _ = writeln!(
        out,
        "  sustained req/s   {:.1} \u{2192} {:.1}  ({}{:.1}%, tolerance -{:.1}%)",
        report.rps.committed,
        report.rps.candidate,
        sign(report.rps.pct_change),
        report.rps.pct_change,
        report.tolerances.rps * 100.0
    );
    let _ = writeln!(
        out,
        "  P99 latency (ms)  {:.2} \u{2192} {:.2}  ({}{:.1}%, tolerance +{:.1}%)",
        report.p99.committed,
        report.p99.candidate,
        sign(report.p99.pct_change),
        report.p99.pct_change,
        report.tolerances.p99 * 100.0
    );

    if !report.notes.is_empty() {
        out.push_str("\n  route graph:\n");
        for note in &report.notes {
            let _ = writeln!(out, "    \u{2022} {note}");
        }
    }

    out.push('\n');
    match report.outcome {
        CheckOutcome::Pass => {
            out.push_str("\u{2713} within the committed capacity.lock envelope\n");
        }
        CheckOutcome::Regressed => {
            out.push_str("\u{2717} this build no longer meets the committed capacity.lock:\n");
            for regression in &report.regressions {
                let _ = writeln!(out, "    - {regression}");
            }
            out.push_str(
                "\n  Fix the regression, or re-run `autumn calibrate` to record a new \
                 capacity.lock\n  if the envelope changed on purpose.\n",
            );
        }
        CheckOutcome::Unusable => {
            out.push_str(
                "\u{2717} the committed capacity.lock does not record a usable envelope \
                 (its sustained\n  throughput or P99 latency is zero, negative, or not a \
                 number), so there is nothing\n  to gate against. Re-run `autumn calibrate` \
                 to record a real one.\n",
            );
        }
        CheckOutcome::HostMismatch => {
            out.push_str(
                "\u{2717} host class differs from the committed capacity.lock, so the two \
                 envelopes\n  are not comparable. Run this gate on the host class the contract \
                 was\n  calibrated on, or re-run `autumn calibrate` there to record a new \
                 capacity.lock.\n",
            );
        }
    }
    out
}

/// `"+"` for a rise, `""` for a fall (the `{:.1}` already carries the minus).
const fn sign(pct: f64) -> &'static str {
    if pct > 0.0 { "+" } else { "" }
}

// ── Reading the route graph back from the calibrated binary ──────────────

/// One route as read from the target binary's `AUTUMN_DUMP_ROUTES` listing.
///
/// Only the capacity-relevant fields are deserialized; unknown ones are
/// ignored and missing ones fall back to defaults, so calibration keeps
/// working against a binary built from a slightly different autumn — the same
/// forward/backward-compatibility posture `routes audit` takes.
#[derive(Debug, Clone, Deserialize)]
pub struct DumpedRoute {
    /// HTTP method.
    pub method: String,
    /// Mounted path template.
    pub path: String,
    /// Handler function name.
    #[serde(default)]
    pub handler: String,
    /// Registration origin (`user`, `framework`, `plugin:<name>`).
    #[serde(default)]
    pub source: String,
    /// Build-time security classification.
    #[serde(default)]
    pub classification: String,
    /// Statically derived resource shape tag.
    #[serde(default)]
    pub resource_shape: String,
    /// Pools the handler's declared extractors prove it touches.
    #[serde(default)]
    pub pools: Vec<String>,
}

/// The contract's per-route section, in canonical order.
///
/// Framework-owned routes (probes, actuator, asset serving) are excluded: they
/// are the same in every autumn app and would bury the application's own shape
/// in noise.
#[must_use]
pub fn route_shapes(dump: &[DumpedRoute]) -> Vec<RouteShape> {
    let mut shapes: Vec<RouteShape> = dump
        .iter()
        .filter(|r| r.source != "framework")
        .map(|r| {
            let mut pools = r.pools.clone();
            pools.sort();
            pools.dedup();
            // Trust the binary's own tag when it carries one, and fall back to
            // deriving it from the pools for a dump written before #1733.
            let shape = r
                .resource_shape
                .parse::<ResourceShape>()
                .unwrap_or_else(|()| ResourceShape::from_pools(&pools));
            RouteShape {
                method: r.method.clone(),
                path: r.path.clone(),
                handler: r.handler.clone(),
                shape,
                pools,
            }
        })
        .collect();
    shapes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    shapes
}

/// Paths the calibration ladder may drive load against.
///
/// Deliberately conservative — a calibration run must measure the app, not a
/// side effect of measuring it:
///
/// - **`GET` only**, so the run never mutates application state.
/// - **No path parameters**, because the driver has no legitimate id to
///   substitute and a fabricated one measures the 404 path.
/// - **Not `gated`**, because an unauthenticated run would measure the auth
///   rejection, not the handler.
/// - **Not framework-owned**, since probes are exempt from load shedding
///   anyway and would flatter the envelope. Plugin routes *are* included: they
///   run in the same process, are governed by the same admission limit, and an
///   app whose public traffic is served by a plugin would otherwise be
///   calibrated against whatever cheap route it happened to write by hand.
///
/// A parameterless `GET` is not automatically callable — one that needs query
/// values or headers answers 4xx, and enough of those make every rung exceed
/// the error threshold so no envelope is recorded (loudly, rather than
/// wrongly). `--target` names the paths explicitly when that happens.
#[must_use]
pub fn calibratable_targets(dump: &[DumpedRoute]) -> Vec<String> {
    let mut targets: Vec<String> = dump
        .iter()
        .filter(|r| {
            r.method.eq_ignore_ascii_case("GET")
                && r.source != "framework"
                && r.classification != "framework"
                && r.classification != "gated"
                && !r.path.contains('{')
        })
        .map(|r| r.path.clone())
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

// ── Seeded, reproducible request profile ─────────────────────────────────

/// The request sequence a calibration run replays.
///
/// Seeded so a re-calibration of an unchanged build drives the *same* mix of
/// routes in the same order: without that, two runs of a mixed-shape app
/// differ by which routes happened to be sampled, and `--check` would be
/// comparing two different workloads.
#[must_use]
pub fn plan_requests(seed: u64, targets: &[String], count: usize) -> Vec<String> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut rng = SplitMix64::new(seed);
    let len = targets.len();
    (0..count)
        .map(|_| {
            // `% len` keeps the index in range, so the `usize` narrowing is
            // bounded by the slice length regardless of pointer width.
            #[allow(clippy::cast_possible_truncation)]
            let idx = (rng.next_u64() % len as u64) as usize;
            targets.get(idx).cloned().unwrap_or_default()
        })
        .collect()
}

/// `SplitMix64` — a tiny, dependency-free, fully specified PRNG.
///
/// Written out rather than pulled from `rand` so the load profile is
/// reproducible across autumn versions: a contract is only re-checkable if the
/// workload that produced it can be replayed exactly.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::capacity::{
        CONTRACT_VERSION, Calibration, CapacityContract, Envelope, HostProfile, Provenance,
        ResourceShape, RouteShape, route_graph_digest,
    };

    fn stats(concurrency: usize, rps: f64, p99_ms: f64) -> RungStats {
        RungStats {
            concurrency,
            completed: 100,
            rps,
            p99_ms,
            error_rate: 0.0,
        }
    }

    fn contract(
        sustained_rps: f64,
        p99_latency_ms: f64,
        routes: Vec<RouteShape>,
    ) -> CapacityContract {
        CapacityContract {
            version: CONTRACT_VERSION,
            provenance: Provenance {
                autumn_version: "0.7.0".to_owned(),
                calibrated_at: "2026-09-01T00:00:00Z".to_owned(),
                git_commit: Some("deadbee".to_owned()),
                git_dirty: false,
                route_graph_digest: route_graph_digest(&routes),
            },
            host: HostProfile {
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                logical_cpus: 8,
                total_memory_mb: Some(16384),
            },
            envelope: Envelope {
                sustained_rps,
                p99_latency_ms,
                saturation_concurrency: 64,
                admission_limit: 128,
            },
            calibration: Calibration::default(),
            routes,
        }
    }

    fn one_route() -> Vec<RouteShape> {
        vec![RouteShape {
            method: "GET".to_owned(),
            path: "/posts".to_owned(),
            handler: "index".to_owned(),
            shape: ResourceShape::DbBound,
            pools: vec!["db".to_owned()],
        }]
    }

    // ── rung statistics ──────────────────────────────────────────────────

    #[test]
    fn rung_stats_derive_throughput_tail_latency_and_error_rate() {
        let samples = RungSamples {
            concurrency: 8,
            errors: 1,
            wall_secs: 2.0,
            // 99 successful latencies, 1..=99 ms: nearest-rank p99 is the
            // 99th value; 1 error out of 100 attempts is a 1% error rate.
            latencies_ms: (1..=99).map(f64::from).collect(),
        };
        let s = RungStats::from_samples(&samples);

        assert_eq!(s.concurrency, 8);
        assert!((s.rps - 49.5).abs() < 1e-9, "rps was {}", s.rps);
        assert!((s.p99_ms - 99.0).abs() < 1e-9, "p99 was {}", s.p99_ms);
        assert!(
            (s.error_rate - 0.01).abs() < 1e-9,
            "error rate {}",
            s.error_rate
        );
    }

    #[test]
    fn rung_stats_are_defined_for_an_empty_rung() {
        let s = RungStats::from_samples(&RungSamples {
            concurrency: 4,
            errors: 0,
            wall_secs: 0.0,
            latencies_ms: Vec::new(),
        });
        assert!((s.rps - 0.0).abs() < f64::EPSILON);
        assert!((s.p99_ms - 0.0).abs() < f64::EPSILON);
        assert!((s.error_rate - 0.0).abs() < f64::EPSILON);
    }

    // ── saturation knee ──────────────────────────────────────────────────

    #[test]
    fn saturation_is_the_last_rung_that_still_bought_throughput() {
        // 1→2 and 2→4 gain well past the 5% floor; 4→8 gains 1%, so the knee
        // is at 4: past it, concurrency buys latency, not throughput.
        let rungs = [
            stats(1, 500.0, 2.0),
            stats(2, 950.0, 2.2),
            stats(4, 1800.0, 4.4),
            stats(8, 1818.0, 9.0),
            stats(16, 1750.0, 20.0),
        ];
        let knee = find_saturation(&rungs, &SaturationOptions::default()).expect("a knee");
        assert_eq!(knee.concurrency, 4);
        assert!((knee.rps - 1800.0).abs() < 1e-9);
    }

    #[test]
    fn saturation_never_walks_past_an_error_spike() {
        let rungs = [
            stats(1, 500.0, 2.0),
            stats(2, 1000.0, 2.2),
            RungStats {
                concurrency: 4,
                completed: 8000,
                rps: 2000.0,
                p99_ms: 5.0,
                // Throughput doubled, but a twentieth of it was failures —
                // that is not sustained capacity.
                error_rate: 0.05,
            },
        ];
        let knee = find_saturation(&rungs, &SaturationOptions::default()).expect("a knee");
        assert_eq!(knee.concurrency, 2);
    }

    #[test]
    fn a_ladder_that_completed_nothing_yields_no_envelope() {
        // `--rung-ms 0`, or an app that answers nothing, must not be written
        // up as "sustains 0 req/s, admit 1" — that contract would cap a deploy
        // at a single in-flight request.
        let empty = [RungStats {
            concurrency: 1,
            completed: 0,
            rps: 0.0,
            p99_ms: 0.0,
            error_rate: 0.0,
        }];
        assert!(find_saturation(&empty, &SaturationOptions::default()).is_none());
    }

    #[test]
    fn the_median_repeat_is_taken_whole() {
        // The reported P99 must belong to the same measurement as the reported
        // throughput, so the median is a whole rung, not a per-field median.
        let repeats = [
            RungStats {
                rps: 900.0,
                p99_ms: 9.0,
                ..stats(8, 0.0, 0.0)
            },
            RungStats {
                rps: 100.0,
                p99_ms: 1.0,
                ..stats(8, 0.0, 0.0)
            },
            RungStats {
                rps: 500.0,
                p99_ms: 5.0,
                ..stats(8, 0.0, 0.0)
            },
        ];
        let median = median_rung(&repeats).expect("a median");
        assert!((median.rps - 500.0).abs() < 1e-9);
        assert!(
            (median.p99_ms - 5.0).abs() < 1e-9,
            "p99 must come from the same repeat"
        );

        assert!(median_rung(&[]).is_none());
        let single = [stats(4, 42.0, 1.0)];
        assert!((median_rung(&single).expect("a median").rps - 42.0).abs() < 1e-9);
    }

    #[test]
    fn saturation_of_a_degenerate_ladder() {
        assert!(find_saturation(&[], &SaturationOptions::default()).is_none());
        let single = [stats(1, 500.0, 2.0)];
        assert_eq!(
            find_saturation(&single, &SaturationOptions::default())
                .expect("a knee")
                .concurrency,
            1
        );
    }

    // ── --check ──────────────────────────────────────────────────────────

    #[test]
    fn an_identical_rebuild_passes() {
        let committed = contract(1000.0, 20.0, one_route());
        let candidate = contract(1000.0, 20.0, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(report.outcome, CheckOutcome::Pass);
        assert!(report.regressions.is_empty());
    }

    #[test]
    fn run_to_run_noise_inside_the_tolerance_passes() {
        let committed = contract(1000.0, 20.0, one_route());
        // 6% slower, 10% worse tail — inside the default 15% / 25% bands.
        let candidate = contract(940.0, 22.0, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(
            report.outcome,
            CheckOutcome::Pass,
            "{:?}",
            report.regressions
        );
    }

    #[test]
    fn a_thirty_percent_throughput_regression_fails() {
        // The issue's Success Metric: a seeded 30% throughput regression is
        // caught.
        let committed = contract(1000.0, 20.0, one_route());
        let candidate = contract(700.0, 20.0, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(report.outcome, CheckOutcome::Regressed);
        assert!(
            report.regressions.iter().any(|r| r.contains("throughput")),
            "{:?}",
            report.regressions
        );
        assert!((report.rps.pct_change + 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_tail_latency_blowout_fails_even_when_throughput_holds() {
        let committed = contract(1000.0, 20.0, one_route());
        let candidate = contract(1000.0, 40.0, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(report.outcome, CheckOutcome::Regressed);
        assert!(
            report.regressions.iter().any(|r| r.contains("P99")),
            "{:?}",
            report.regressions
        );
    }

    #[test]
    fn a_committed_envelope_of_zero_is_refused_rather_than_passed() {
        // `candidate < 0.0 * 0.85` is unsatisfiable, so comparing against a
        // zero envelope would report a green pass for any build at all —
        // silently neutering the one check this gate exists for.
        let committed = contract(0.0, 0.0, one_route());
        let candidate = contract(3912.4, 0.8, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(report.outcome, CheckOutcome::Unusable);
        assert!(render_check_report(&report).contains("usable envelope"));
    }

    #[test]
    fn a_small_absolute_tail_movement_is_not_a_regression() {
        // A reference app with sub-millisecond handlers has a 25% band of a
        // fraction of a millisecond — less than the jitter one context switch
        // on a shared runner contributes.
        let committed = contract(1000.0, 0.40, one_route());
        let candidate = contract(1000.0, 0.95, one_route());
        let report = check_contract(&committed, &candidate, &Tolerances::default());

        assert_eq!(
            report.outcome,
            CheckOutcome::Pass,
            "{:?}",
            report.regressions
        );

        // The absolute slack must not swallow a real blowout on a slow app.
        let slow = contract(1000.0, 200.0, one_route());
        let blown = contract(1000.0, 400.0, one_route());
        assert_eq!(
            check_contract(&slow, &blown, &Tolerances::default()).outcome,
            CheckOutcome::Regressed
        );
    }

    #[test]
    fn a_different_host_class_is_reported_rather_than_compared() {
        // Comparing a laptop's envelope with a CI runner's is the fastest way
        // to make a capacity gate a flake, so the numbers are never judged.
        let committed = contract(1000.0, 20.0, one_route());
        let mut candidate = contract(300.0, 90.0, one_route());
        candidate.host.logical_cpus = 2;

        let report = check_contract(&committed, &candidate, &Tolerances::default());
        assert_eq!(report.outcome, CheckOutcome::HostMismatch);
        assert!(report.regressions.is_empty());
    }

    #[test]
    fn route_shape_drift_is_noted_but_does_not_fail_the_gate() {
        let committed = contract(1000.0, 20.0, one_route());
        let mut drifted = one_route();
        drifted[0].shape = ResourceShape::ComputeBound;
        drifted[0].pools.clear();
        drifted.push(RouteShape {
            method: "POST".to_owned(),
            path: "/posts".to_owned(),
            handler: "create".to_owned(),
            shape: ResourceShape::DbBound,
            pools: vec!["db".to_owned()],
        });
        let candidate = contract(1000.0, 20.0, drifted);

        let report = check_contract(&committed, &candidate, &Tolerances::default());
        assert_eq!(report.outcome, CheckOutcome::Pass);
        assert!(
            report.notes.iter().any(|n| n.contains("GET /posts")),
            "changed shape must be named: {:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|n| n.contains("POST /posts")),
            "added route must be named: {:?}",
            report.notes
        );
    }

    #[test]
    fn the_rendered_diff_names_the_metric_and_both_numbers() {
        let committed = contract(1000.0, 20.0, one_route());
        let candidate = contract(700.0, 20.0, one_route());
        let rendered = render_check_report(&check_contract(
            &committed,
            &candidate,
            &Tolerances::default(),
        ));

        assert!(rendered.contains("1000"), "{rendered}");
        assert!(rendered.contains("700"), "{rendered}");
        assert!(rendered.contains("-30.0%"), "{rendered}");
        assert!(rendered.contains("capacity.lock"), "{rendered}");
    }

    // ── seeded, reproducible request mix ─────────────────────────────────

    #[test]
    fn the_request_plan_is_reproducible_for_a_seed() {
        let targets = ["/a".to_owned(), "/b".to_owned(), "/c".to_owned()];
        let a = plan_requests(42, &targets, 64);
        let b = plan_requests(42, &targets, 64);
        assert_eq!(a, b, "the same seed must replay the same load profile");

        let c = plan_requests(43, &targets, 64);
        assert_ne!(a, c, "a different seed must produce a different profile");
        assert_eq!(a.len(), 64);
        assert!(a.iter().all(|t| targets.contains(t)));
    }

    #[test]
    fn the_request_plan_is_empty_without_targets() {
        assert!(plan_requests(1, &[], 16).is_empty());
    }

    // ── route graph read back from the calibrated binary ─────────────────

    fn dumped(
        method: &str,
        path: &str,
        classification: &str,
        shape: &str,
        pools: &[&str],
    ) -> DumpedRoute {
        DumpedRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: "handler".to_owned(),
            source: "user".to_owned(),
            classification: classification.to_owned(),
            resource_shape: shape.to_owned(),
            pools: pools.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn route_shapes_carry_the_statically_derived_shape_in_stable_order() {
        let dump = vec![
            dumped("POST", "/posts", "gated", "db-bound", &["db"]),
            dumped("GET", "/about", "public", "compute-bound", &[]),
        ];
        let shapes = route_shapes(&dump);

        assert_eq!(shapes.len(), 2);
        assert_eq!(shapes[0].path, "/about");
        assert_eq!(shapes[1].shape, ResourceShape::DbBound);
        assert_eq!(shapes[1].pools, vec!["db".to_owned()]);
    }

    #[test]
    fn only_parameterless_unguarded_gets_are_calibratable_targets() {
        let dump = vec![
            dumped("GET", "/about", "public", "compute-bound", &[]),
            // Mutating: driving load against it would change app state.
            dumped("POST", "/posts", "public", "db-bound", &["db"]),
            // Guarded: every request would be a 401, measuring the auth
            // rejection path rather than the app.
            dumped("GET", "/admin", "gated", "db-bound", &["db"]),
            // Parameterised: the driver has no legitimate id to substitute.
            dumped("GET", "/posts/{id}", "public", "db-bound", &["db"]),
            DumpedRoute {
                source: "framework".to_owned(),
                ..dumped("GET", "/live", "framework", "compute-bound", &[])
            },
            // A plugin route runs in the same process and is governed by the
            // same admission limit, so it belongs in the measured workload.
            DumpedRoute {
                source: "plugin:media".to_owned(),
                ..dumped("GET", "/media/gallery", "public", "io-bound", &["storage"])
            },
        ];

        assert_eq!(
            calibratable_targets(&dump),
            vec!["/about".to_owned(), "/media/gallery".to_owned()]
        );
    }
}
