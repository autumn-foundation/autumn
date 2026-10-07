//! The post-cutover bake (issue #3069).
//!
//! After a host cuts over, the bake samples the new release's
//! `/actuator/metrics` over SSH for a set time. It returns a breach when:
//!
//! - the 5xx ratio of the bake window is above the limit,
//! - the gated latency quantile is above the limit,
//! - a counter goes down (the process restarted), or
//! - a sample cannot be read (the app does not answer).
//!
//! Thin traffic (fewer than `min_requests` new responses) never causes a
//! breach. The caller rolls the host back on a breach.
//!
//! [`judge`] is the decision. `verification/bake_verdict.rs` is its Verus
//! model, and a property test here checks that the two agree.
//!
//! The bake reads no clock. Each sample is one remote command that sleeps
//! and then runs `curl`, so the fake executor drives the tests.

// The items are crate-internal, as in `fleet.rs`. `deploy` is a private module
// of this bin crate, so clippy calls each `pub(crate)` redundant.
#![allow(clippy::redundant_pub_crate)]

use std::fmt;

use autumn_web::config::DeployBakeConfig;
use autumn_web::slo::{PPM, ROLLBACK_BURN_TENTHS, Sli, Slo, format_decimal, max_error_ppm};
use serde::Deserialize;

use super::exec::{DeployExecutor, RemoteCommand, shell_quote};

/// The step label of a bake failure, in fleet reports.
pub(crate) const BAKE_LABEL: &str = "bake";

/// The label of one metrics sample.
pub(crate) const SAMPLE_LABEL: &str = "bake-sample";

/// The error-ratio limit when no SLO and no `max_error_rate` set one: 5 %,
/// the same as the `[alerts]` default.
pub(crate) const DEFAULT_MAX_ERROR_PPM: u32 = 50_000;

/// A latency quantile from `/actuator/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quantile {
    P50,
    P95,
    P99,
}

impl fmt::Display for Quantile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::P50 => "p50",
            Self::P95 => "p95",
            Self::P99 => "p99",
        })
    }
}

/// A latency limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LatencyGate {
    pub quantile: Quantile,
    pub max_ms: u64,
}

/// The resolved bake settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BakePolicy {
    pub duration_secs: u64,
    pub interval_secs: u64,
    pub min_requests: u64,
    pub max_error_ppm: u32,
    pub latency: Option<LatencyGate>,
    /// Where the limits come from, for the operator.
    pub source: String,
}

/// One metrics sample: cumulative counters since the process started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Sample {
    /// All responses (2xx to 5xx).
    pub responses: u64,
    /// 5xx responses.
    pub errors: u64,
    /// The gated latency quantile, in milliseconds.
    pub latency_ms: u64,
}

/// Why the bake failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Breach {
    /// A counter went down: the process restarted.
    CounterReset,
    /// The 5xx ratio is above the limit.
    ErrorRate {
        errors: u64,
        responses: u64,
        max_ppm: u32,
    },
    /// The latency quantile is above the limit.
    Latency {
        quantile: Quantile,
        ms: u64,
        max_ms: u64,
    },
    /// A sample could not be read.
    Unreachable(String),
}

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CounterReset => f.write_str(
                "the metrics counters went down, so the new release restarted during the bake",
            ),
            Self::ErrorRate {
                errors,
                responses,
                max_ppm,
            } => write!(
                f,
                "{errors} of {responses} responses were 5xx ({}%), above the limit of {}%",
                percent(u128::from(*errors) * 1_000_000 / u128::from((*responses).max(1))),
                percent(u128::from(*max_ppm)),
            ),
            Self::Latency {
                quantile,
                ms,
                max_ms,
            } => write!(
                f,
                "{quantile} latency is {ms} ms, above the limit of {max_ms} ms"
            ),
            Self::Unreachable(why) => write!(f, "the metrics sample failed: {why}"),
        }
    }
}

/// A ppm value as a percentage with up to four decimals.
fn percent(ppm: u128) -> String {
    format_decimal(u64::try_from(ppm).unwrap_or(u64::MAX), 4)
}

/// The verdict on one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    TooFewRequests,
    Breach(Breach),
}

/// Judge `now` against the bake `baseline`. The Verus model is `spec_judge`.
pub(crate) fn judge(baseline: &Sample, now: &Sample, policy: &BakePolicy) -> Verdict {
    if now.responses < baseline.responses || now.errors < baseline.errors {
        return Verdict::Breach(Breach::CounterReset);
    }
    let responses = now.responses - baseline.responses;
    let errors = now.errors - baseline.errors;
    if responses < policy.min_requests {
        return Verdict::TooFewRequests;
    }
    // u128: a product of two u64 values cannot overflow.
    if u128::from(errors) * u128::from(PPM)
        > u128::from(policy.max_error_ppm) * u128::from(responses)
    {
        return Verdict::Breach(Breach::ErrorRate {
            errors,
            responses,
            max_ppm: policy.max_error_ppm,
        });
    }
    if let Some(gate) = policy.latency
        && now.latency_ms > gate.max_ms
    {
        return Verdict::Breach(Breach::Latency {
            quantile: gate.quantile,
            ms: now.latency_ms,
            max_ms: gate.max_ms,
        });
    }
    Verdict::Pass
}

/// Parse a `/actuator/metrics` JSON body.
pub(crate) fn parse_sample(json: &str, quantile: Quantile) -> Result<Sample, String> {
    #[derive(Deserialize)]
    struct Metrics {
        http: Http,
    }
    #[derive(Deserialize)]
    struct Http {
        by_status: Status,
        latency_ms: Latency,
    }
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "2xx", default)]
        s2xx: u64,
        #[serde(rename = "3xx", default)]
        s3xx: u64,
        #[serde(rename = "4xx", default)]
        s4xx: u64,
        #[serde(rename = "5xx", default)]
        s5xx: u64,
    }
    #[derive(Deserialize)]
    struct Latency {
        #[serde(default)]
        p50: u64,
        #[serde(default)]
        p95: u64,
        #[serde(default)]
        p99: u64,
    }
    let metrics: Metrics = serde_json::from_str(json)
        .map_err(|e| format!("the response is not /actuator/metrics JSON: {e}"))?;
    let status = metrics.http.by_status;
    let latency = metrics.http.latency_ms;
    Ok(Sample {
        responses: status
            .s2xx
            .saturating_add(status.s3xx)
            .saturating_add(status.s4xx)
            .saturating_add(status.s5xx),
        errors: status.s5xx,
        latency_ms: match quantile {
            Quantile::P50 => latency.p50,
            Quantile::P95 => latency.p95,
            Quantile::P99 => latency.p99,
        },
    })
}

/// The remote command for one sample: sleep, then read the metrics.
pub(crate) fn sample_command(port: u16, metrics_path: &str, sleep_secs: u64) -> RemoteCommand {
    let curl = format!(
        "curl -fsS -m 5 {}",
        shell_quote(&format!("http://127.0.0.1:{port}{metrics_path}"))
    );
    let shell = if sleep_secs == 0 {
        curl
    } else {
        format!("sleep {sleep_secs} && {curl}")
    };
    RemoteCommand::new(SAMPLE_LABEL, shell)
}

/// Run one sample command and parse it.
fn take_sample<E: DeployExecutor>(
    executor: &E,
    port: u16,
    metrics_path: &str,
    sleep_secs: u64,
    quantile: Quantile,
) -> Result<Sample, Breach> {
    let output = executor
        .run(&sample_command(port, metrics_path, sleep_secs))
        .map_err(|e| Breach::Unreachable(e.to_string()))?;
    parse_sample(&output.stdout, quantile).map_err(Breach::Unreachable)
}

/// The result of a bake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BakeOutcome {
    /// No breach. `responses` is the traffic the bake saw.
    Passed { responses: u64, judged: bool },
    /// A breach. The caller rolls back.
    Breached(Breach),
}

/// Run the bake against the release on `port`.
///
/// `progress` gets one line per sample.
pub(crate) fn run<E: DeployExecutor>(
    policy: &BakePolicy,
    port: u16,
    metrics_path: &str,
    executor: &E,
    progress: &mut dyn FnMut(&str),
) -> BakeOutcome {
    let quantile = policy.latency.map_or(Quantile::P99, |g| g.quantile);
    let baseline = match take_sample(executor, port, metrics_path, 0, quantile) {
        Ok(sample) => sample,
        Err(breach) => return BakeOutcome::Breached(breach),
    };
    let interval = policy.interval_secs.max(1);
    let mut elapsed = 0;
    let mut last = baseline;
    let mut judged = false;
    while elapsed < policy.duration_secs {
        let sleep = interval.min(policy.duration_secs - elapsed);
        elapsed += sleep;
        let now = match take_sample(executor, port, metrics_path, sleep, quantile) {
            Ok(sample) => sample,
            Err(breach) => return BakeOutcome::Breached(breach),
        };
        progress(&format!(
            "bake {elapsed}/{} s: {} responses, {} 5xx, {quantile} {} ms",
            policy.duration_secs,
            now.responses.saturating_sub(baseline.responses),
            now.errors.saturating_sub(baseline.errors),
            now.latency_ms,
        ));
        match judge(&baseline, &now, policy) {
            Verdict::Breach(breach) => return BakeOutcome::Breached(breach),
            Verdict::Pass => judged = true,
            Verdict::TooFewRequests => {}
        }
        last = now;
    }
    BakeOutcome::Passed {
        responses: last.responses.saturating_sub(baseline.responses),
        judged,
    }
}

/// Resolve the bake settings from `[deploy.bake]`, the `[[slo]]` tables and
/// the `--bake-secs` flag. `None` means the bake is off.
///
/// # Errors
///
/// Returns an error for a `max_error_rate` outside `0.0..=1.0`.
pub(crate) fn resolve_policy(
    config: &DeployBakeConfig,
    slos: &[Slo],
    duration_override: Option<u64>,
) -> Result<Option<BakePolicy>, String> {
    let duration_secs = duration_override.unwrap_or(config.duration_secs);
    if duration_secs == 0 {
        return Ok(None);
    }
    let mut sources = Vec::new();

    let max_error_ppm = if let Some(rate) = config.max_error_rate {
        if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
            return Err(format!(
                "[deploy.bake] max_error_rate = {rate} must be from 0.0 to 1.0"
            ));
        }
        sources.push("[deploy.bake] max_error_rate".to_owned());
        // The range check keeps the product in 0..=1e6.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ppm = (rate * f64::from(PPM)).round() as u32;
        ppm
    } else if let Some(strict) = slos
        .iter()
        .filter(|s| matches!(s.sli, Sli::Availability { route: None }))
        .min_by_key(|s| s.budget_ppm())
    {
        sources.push(format!(
            "SLO {} at {}x burn",
            strict.name,
            format_decimal(u64::from(ROLLBACK_BURN_TENTHS), 1)
        ));
        max_error_ppm(strict.budget_ppm(), ROLLBACK_BURN_TENTHS)
    } else {
        sources.push("the default 5% error limit".to_owned());
        DEFAULT_MAX_ERROR_PPM
    };

    let latency = if let Some(max_ms) = config.max_p99_ms {
        sources.push("[deploy.bake] max_p99_ms".to_owned());
        Some(LatencyGate {
            quantile: Quantile::P99,
            max_ms,
        })
    } else {
        slos.iter()
            .filter_map(|s| match s.sli {
                Sli::Latency {
                    route: None,
                    threshold_ms,
                } => Some((s, threshold_ms)),
                _ => None,
            })
            .min_by_key(|(_, ms)| *ms)
            .map(|(s, max_ms)| {
                sources.push(format!("SLO {}", s.name));
                LatencyGate {
                    quantile: quantile_for(s.objective_ppm),
                    max_ms,
                }
            })
    };

    Ok(Some(BakePolicy {
        duration_secs,
        interval_secs: config.interval_secs.clamp(1, duration_secs),
        min_requests: config.min_requests,
        max_error_ppm,
        latency,
        source: sources.join(", "),
    }))
}

/// The quantile that a latency objective maps to. `/actuator/metrics` has
/// only p50, p95 and p99, so this takes the highest one at or below the
/// objective.
const fn quantile_for(objective_ppm: u32) -> Quantile {
    if objective_ppm >= 990_000 {
        Quantile::P99
    } else if objective_ppm >= 950_000 {
        Quantile::P95
    } else {
        Quantile::P50
    }
}

impl BakePolicy {
    /// One line that describes the policy, for the operator.
    pub(crate) fn describe(&self) -> String {
        let latency = self.latency.map_or_else(String::new, |g| {
            format!(" or {} latency is above {} ms", g.quantile, g.max_ms)
        });
        format!(
            "bake for {} s, sample every {} s; roll back when the 5xx ratio is above {}%{latency} \
             (after {} responses; limits from {})",
            self.duration_secs,
            self.interval_secs,
            percent(u128::from(self.max_error_ppm)),
            self.min_requests,
            self.source,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::exec::test_support::RecordingExecutor;
    use autumn_web::slo::{SliKind, SloConfig};

    fn policy() -> BakePolicy {
        BakePolicy {
            duration_secs: 30,
            interval_secs: 10,
            min_requests: 20,
            max_error_ppm: 14_400,
            latency: Some(LatencyGate {
                quantile: Quantile::P99,
                max_ms: 250,
            }),
            source: "test".to_owned(),
        }
    }

    fn sample(responses: u64, errors: u64, latency_ms: u64) -> Sample {
        Sample {
            responses,
            errors,
            latency_ms,
        }
    }

    /// A `/actuator/metrics` body with the given counters.
    pub(crate) fn metrics_json(ok: u64, server_errors: u64, p99: u64) -> String {
        format!(
            r#"{{"http":{{"requests_total":{total},"requests_active":0,
            "latency_ms":{{"p50":1,"p95":2,"p99":{p99}}},
            "by_status":{{"2xx":{ok},"3xx":0,"4xx":0,"5xx":{server_errors}}},
            "by_route":{{}}}},"uptime_seconds":5}}"#,
            total = ok + server_errors
        )
    }

    // ── judge ────────────────────────────────────────────────────────────────

    #[test]
    fn judge_passes_healthy_traffic() {
        let v = judge(&sample(100, 0, 0), &sample(200, 1, 100), &policy());
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn judge_waits_for_enough_traffic() {
        // 19 new responses, all 5xx: still no verdict.
        let v = judge(&sample(100, 0, 0), &sample(119, 19, 900), &policy());
        assert_eq!(v, Verdict::TooFewRequests);
    }

    #[test]
    fn judge_flags_an_error_spike() {
        // 10 % 5xx against a 1.44 % limit.
        let v = judge(&sample(100, 0, 0), &sample(200, 10, 10), &policy());
        assert_eq!(
            v,
            Verdict::Breach(Breach::ErrorRate {
                errors: 10,
                responses: 100,
                max_ppm: 14_400,
            })
        );
    }

    #[test]
    fn judge_is_exact_at_the_limit() {
        let mut p = policy();
        p.max_error_ppm = 10_000; // 1 %
        assert_eq!(
            judge(&sample(0, 0, 0), &sample(100, 1, 0), &p),
            Verdict::Pass
        );
        assert!(matches!(
            judge(&sample(0, 0, 0), &sample(99, 1, 0), &p),
            Verdict::Breach(Breach::ErrorRate { .. })
        ));
    }

    #[test]
    fn judge_flags_slow_latency() {
        let v = judge(&sample(0, 0, 0), &sample(100, 0, 251), &policy());
        assert_eq!(
            v,
            Verdict::Breach(Breach::Latency {
                quantile: Quantile::P99,
                ms: 251,
                max_ms: 250,
            })
        );
        let mut no_gate = policy();
        no_gate.latency = None;
        assert_eq!(
            judge(&sample(0, 0, 0), &sample(100, 0, 9_999), &no_gate),
            Verdict::Pass
        );
    }

    #[test]
    fn judge_flags_a_restart() {
        let v = judge(&sample(500, 3, 0), &sample(10, 0, 0), &policy());
        assert_eq!(v, Verdict::Breach(Breach::CounterReset));
        let v = judge(&sample(500, 3, 0), &sample(600, 2, 0), &policy());
        assert_eq!(v, Verdict::Breach(Breach::CounterReset));
    }

    #[test]
    fn judge_checks_errors_before_latency() {
        let v = judge(&sample(0, 0, 0), &sample(100, 50, 9_999), &policy());
        assert!(
            matches!(v, Verdict::Breach(Breach::ErrorRate { .. })),
            "{v:?}"
        );
    }

    #[test]
    fn judge_does_not_overflow() {
        let mut p = policy();
        p.max_error_ppm = PPM;
        let v = judge(&sample(0, 0, 0), &sample(u64::MAX, u64::MAX, 0), &p);
        assert_eq!(v, Verdict::Pass);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        /// The Verus `spec_judge`, written over `i128`.
        fn model(b: Sample, n: Sample, p: &BakePolicy) -> &'static str {
            if n.responses < b.responses || n.errors < b.errors {
                return "reset";
            }
            let dr = i128::from(n.responses) - i128::from(b.responses);
            let de = i128::from(n.errors) - i128::from(b.errors);
            if dr < i128::from(p.min_requests) {
                return "few";
            }
            if de * 1_000_000 > i128::from(p.max_error_ppm) * dr {
                return "errors";
            }
            if p.latency.is_some_and(|g| n.latency_ms > g.max_ms) {
                return "latency";
            }
            "pass"
        }

        fn name(v: &Verdict) -> &'static str {
            match v {
                Verdict::Pass => "pass",
                Verdict::TooFewRequests => "few",
                Verdict::Breach(Breach::CounterReset) => "reset",
                Verdict::Breach(Breach::ErrorRate { .. }) => "errors",
                Verdict::Breach(Breach::Latency { .. }) => "latency",
                Verdict::Breach(Breach::Unreachable(_)) => "unreachable",
            }
        }

        proptest! {
            #[test]
            fn judge_matches_the_verus_model(
                b_r in 0u64..10_000, b_e in 0u64..100,
                n_r in 0u64..20_000, n_e in 0u64..2_000,
                lat in 0u64..1_000, max_ppm in 0u32..=PPM, min in 0u64..200,
                gate in proptest::option::of(0u64..1_000),
            ) {
                let p = BakePolicy {
                    min_requests: min,
                    max_error_ppm: max_ppm,
                    latency: gate.map(|max_ms| LatencyGate { quantile: Quantile::P99, max_ms }),
                    ..policy()
                };
                let (b, n) = (sample(b_r, b_e, 0), sample(n_r, n_e, lat));
                prop_assert_eq!(name(&judge(&b, &n, &p)), model(b, n, &p));
            }
        }
    }

    // ── parse_sample ─────────────────────────────────────────────────────────

    #[test]
    fn parse_sample_sums_status_classes() {
        let s = parse_sample(&metrics_json(90, 10, 42), Quantile::P99).expect("parse");
        assert_eq!(s, sample(100, 10, 42));
        let s = parse_sample(&metrics_json(90, 10, 42), Quantile::P95).expect("parse");
        assert_eq!(s.latency_ms, 2);
    }

    #[test]
    fn parse_sample_rejects_bad_json() {
        let error = parse_sample("<html>", Quantile::P99).expect_err("bad");
        assert!(error.contains("JSON"), "{error}");
    }

    // ── sample_command ───────────────────────────────────────────────────────

    #[test]
    fn sample_command_sleeps_then_reads_loopback_metrics() {
        let cmd = sample_command(3001, "/actuator/metrics", 10);
        assert_eq!(cmd.label, SAMPLE_LABEL);
        assert_eq!(
            cmd.shell,
            "sleep 10 && curl -fsS -m 5 'http://127.0.0.1:3001/actuator/metrics'"
        );
        let first = sample_command(3002, "/ops/metrics", 0);
        assert_eq!(
            first.shell,
            "curl -fsS -m 5 'http://127.0.0.1:3002/ops/metrics'"
        );
    }

    // ── run ──────────────────────────────────────────────────────────────────

    fn exec_with(samples: &[String]) -> RecordingExecutor {
        let mut exec = RecordingExecutor::new().strict();
        for (index, body) in samples.iter().enumerate() {
            exec = exec.with_stdout_on_occurrence(SAMPLE_LABEL, index + 1, body.clone());
        }
        exec
    }

    fn run_quiet(exec: &RecordingExecutor) -> BakeOutcome {
        run(&policy(), 3001, "/actuator/metrics", exec, &mut |_| {})
    }

    #[test]
    fn a_healthy_bake_passes_after_every_sample() {
        let exec = exec_with(&[
            metrics_json(10, 0, 5),
            metrics_json(110, 0, 5),
            metrics_json(210, 1, 5),
            metrics_json(310, 1, 5),
        ]);
        assert_eq!(
            run_quiet(&exec),
            BakeOutcome::Passed {
                responses: 301,
                judged: true,
            }
        );
        // 30 s at 10 s: a baseline and three samples.
        assert_eq!(exec.run_labels(), vec![SAMPLE_LABEL; 4]);
        let shells: Vec<_> = exec
            .calls()
            .iter()
            .map(|c| match c {
                crate::deploy::exec::test_support::RecordedCall::Run { shell, .. } => shell.clone(),
                crate::deploy::exec::test_support::RecordedCall::Upload { .. } => String::new(),
            })
            .collect();
        assert!(shells[0].starts_with("curl "), "{shells:?}");
        assert!(shells[1].starts_with("sleep 10 && "), "{shells:?}");
    }

    #[test]
    fn an_injected_error_spike_breaches_and_stops_sampling() {
        let exec = exec_with(&[
            metrics_json(10, 0, 5),
            metrics_json(110, 0, 5),
            // The spike: 30 new 5xx in 130 new responses.
            metrics_json(210, 30, 5),
            metrics_json(310, 30, 5),
        ]);
        let outcome = run_quiet(&exec);
        assert_eq!(
            outcome,
            BakeOutcome::Breached(Breach::ErrorRate {
                errors: 30,
                responses: 230,
                max_ppm: 14_400,
            })
        );
        assert_eq!(exec.run_labels().len(), 3, "no sample after the breach");
    }

    #[test]
    fn thin_traffic_passes_without_a_verdict() {
        let exec = exec_with(&[
            metrics_json(0, 0, 5),
            metrics_json(1, 1, 5),
            metrics_json(2, 2, 5),
            metrics_json(3, 3, 5),
        ]);
        assert_eq!(
            run_quiet(&exec),
            BakeOutcome::Passed {
                responses: 6,
                judged: false,
            }
        );
    }

    #[test]
    fn a_failed_sample_is_a_breach() {
        let exec = RecordingExecutor::new()
            .with_stdout_on_occurrence(SAMPLE_LABEL, 1, metrics_json(0, 0, 1))
            .failing_on_occurrence(SAMPLE_LABEL, 2);
        let BakeOutcome::Breached(Breach::Unreachable(why)) = run_quiet(&exec) else {
            panic!("expected an unreachable breach");
        };
        assert!(why.contains("bake-sample"), "{why}");
    }

    #[test]
    fn an_unreadable_baseline_is_a_breach() {
        let exec = RecordingExecutor::new().with_stdout(SAMPLE_LABEL, "not json");
        assert!(matches!(
            run_quiet(&exec),
            BakeOutcome::Breached(Breach::Unreachable(_))
        ));
    }

    #[test]
    fn the_last_sample_sleeps_only_the_remainder() {
        let mut p = policy();
        p.duration_secs = 25;
        let exec = exec_with(&[
            metrics_json(0, 0, 1),
            metrics_json(100, 0, 1),
            metrics_json(200, 0, 1),
            metrics_json(300, 0, 1),
        ]);
        run(&p, 3001, "/actuator/metrics", &exec, &mut |_| {});
        let last = exec
            .calls()
            .last()
            .and_then(|c| match c {
                crate::deploy::exec::test_support::RecordedCall::Run { shell, .. } => {
                    Some(shell.clone())
                }
                crate::deploy::exec::test_support::RecordedCall::Upload { .. } => None,
            })
            .expect("a sample");
        assert!(last.starts_with("sleep 5 && "), "{last}");
    }

    #[test]
    fn progress_reports_each_sample() {
        let exec = exec_with(&[
            metrics_json(0, 0, 1),
            metrics_json(100, 0, 1),
            metrics_json(200, 0, 1),
            metrics_json(300, 0, 1),
        ]);
        let mut lines = Vec::new();
        run(&policy(), 3001, "/actuator/metrics", &exec, &mut |l| {
            lines.push(l.to_owned());
        });
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[2].contains("30/30 s"), "{lines:?}");
    }

    // ── resolve_policy ───────────────────────────────────────────────────────

    fn slo(name: &str, objective: f64, sli: SliKind, route: Option<&str>, ms: Option<u64>) -> Slo {
        autumn_web::slo::validate(&[SloConfig {
            name: name.to_owned(),
            objective,
            sli,
            route: route.map(str::to_owned),
            threshold_ms: ms,
            description: None,
        }])
        .expect("valid")
        .remove(0)
    }

    fn on(duration_secs: u64) -> DeployBakeConfig {
        DeployBakeConfig {
            duration_secs,
            ..DeployBakeConfig::default()
        }
    }

    #[test]
    fn the_bake_is_off_by_default() {
        assert_eq!(
            resolve_policy(&DeployBakeConfig::default(), &[], None),
            Ok(None)
        );
        assert_eq!(resolve_policy(&on(300), &[], Some(0)), Ok(None));
    }

    #[test]
    fn the_flag_turns_the_bake_on() {
        let p = resolve_policy(&DeployBakeConfig::default(), &[], Some(60))
            .expect("ok")
            .expect("on");
        assert_eq!(p.duration_secs, 60);
        assert_eq!(p.interval_secs, 10);
        assert_eq!(p.min_requests, 20);
        assert_eq!(p.max_error_ppm, DEFAULT_MAX_ERROR_PPM);
        assert_eq!(p.latency, None);
    }

    #[test]
    fn availability_slos_set_the_error_limit_at_the_fast_burn_rate() {
        let slos = [
            slo("loose", 99.0, SliKind::Availability, None, None),
            slo("strict", 99.9, SliKind::Availability, None, None),
            // A route SLO is not measurable from the whole-app counters.
            slo("route", 99.99, SliKind::Availability, Some("/x"), None),
        ];
        let p = resolve_policy(&on(60), &slos, None)
            .expect("ok")
            .expect("on");
        assert_eq!(p.max_error_ppm, max_error_ppm(1_000, ROLLBACK_BURN_TENTHS));
        assert!(p.source.contains("strict"), "{}", p.source);
    }

    #[test]
    fn latency_slos_set_the_latency_gate() {
        let slos = [
            slo("slow", 99.0, SliKind::Latency, None, Some(500)),
            slo("fast", 95.0, SliKind::Latency, None, Some(100)),
            slo("route", 99.9, SliKind::Latency, Some("/x"), Some(5)),
        ];
        let p = resolve_policy(&on(60), &slos, None)
            .expect("ok")
            .expect("on");
        assert_eq!(
            p.latency,
            Some(LatencyGate {
                quantile: Quantile::P95,
                max_ms: 100,
            })
        );
    }

    #[test]
    fn the_quantile_follows_the_objective() {
        for (objective, quantile) in [
            (99.9, Quantile::P99),
            (99.0, Quantile::P99),
            (98.0, Quantile::P95),
            (95.0, Quantile::P95),
            (90.0, Quantile::P50),
        ] {
            let p = resolve_policy(
                &on(60),
                &[slo("l", objective, SliKind::Latency, None, Some(250))],
                None,
            )
            .expect("ok")
            .expect("on");
            assert_eq!(p.latency.map(|g| g.quantile), Some(quantile), "{objective}");
        }
    }

    #[test]
    fn explicit_limits_win_over_slos() {
        let config = DeployBakeConfig {
            duration_secs: 60,
            max_error_rate: Some(0.02),
            max_p99_ms: Some(800),
            ..DeployBakeConfig::default()
        };
        let slos = [
            slo("a", 99.9, SliKind::Availability, None, None),
            slo("l", 95.0, SliKind::Latency, None, Some(100)),
        ];
        let p = resolve_policy(&config, &slos, None)
            .expect("ok")
            .expect("on");
        assert_eq!(p.max_error_ppm, 20_000);
        assert_eq!(
            p.latency,
            Some(LatencyGate {
                quantile: Quantile::P99,
                max_ms: 800,
            })
        );
    }

    #[test]
    fn a_bad_error_rate_is_rejected() {
        for bad in [-0.1, 1.5, f64::NAN] {
            let config = DeployBakeConfig {
                duration_secs: 60,
                max_error_rate: Some(bad),
                ..DeployBakeConfig::default()
            };
            assert!(resolve_policy(&config, &[], None).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_interval_is_at_least_one_and_at_most_the_duration() {
        let config = DeployBakeConfig {
            duration_secs: 5,
            interval_secs: 0,
            ..DeployBakeConfig::default()
        };
        let p = resolve_policy(&config, &[], None).expect("ok").expect("on");
        assert_eq!(p.interval_secs, 1);
        let config = DeployBakeConfig {
            duration_secs: 5,
            interval_secs: 60,
            ..DeployBakeConfig::default()
        };
        let p = resolve_policy(&config, &[], None).expect("ok").expect("on");
        assert_eq!(p.interval_secs, 5);
    }

    #[test]
    fn breach_messages_name_the_numbers() {
        let text = Breach::ErrorRate {
            errors: 30,
            responses: 230,
            max_ppm: 14_400,
        }
        .to_string();
        assert!(text.contains("30 of 230"), "{text}");
        assert!(text.contains("1.44%"), "{text}");
        let text = Breach::Latency {
            quantile: Quantile::P99,
            ms: 900,
            max_ms: 250,
        }
        .to_string();
        assert!(text.contains("p99 latency is 900 ms"), "{text}");
    }
}
