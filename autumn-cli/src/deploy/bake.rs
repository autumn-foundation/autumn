//! The post-cutover bake (issue #3069).
//!
//! After a host cuts over, the bake samples the new release's
//! `/actuator/metrics` over SSH for a set time. It returns a breach when:
//!
//! - the 5xx ratio of the bake window is above the limit,
//! - a gated latency quantile is above the limit,
//! - the process restarted (systemd `NRestarts` went up, or a counter went
//!   down), or
//! - a sample cannot be read two times in a row.
//!
//! Thin traffic never causes a breach: the bake needs `min_requests` new
//! responses, and at least [`MIN_BREACH_ERRORS`] new 5xx for an error breach.
//! The bake does not count its own metric requests as traffic. The caller
//! rolls the host back on a breach.
//!
//! [`judge`] is the decision. `verification/bake_verdict.rs` is its Verus
//! model. The property test `judge_matches_the_verus_model` checks the
//! runtime against a Rust copy of `spec_judge`.
//!
//! The bake reads no clock. Each sample is one remote command that sleeps
//! and then runs `curl`, so the fake executor drives the tests.

// The items are crate-internal, as in `fleet.rs`. `deploy` is a private module
// of this bin crate, so clippy calls each `pub(crate)` redundant.
#![allow(clippy::redundant_pub_crate)]

use std::fmt::{self, Write as _};

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

/// The fewest new 5xx responses for an error breach. One bad request alone
/// never rolls a release back.
pub(crate) const MIN_BREACH_ERRORS: u64 = 2;

/// Separates the metrics JSON from the systemd restart count in a sample.
pub(crate) const RESTARTS_MARKER: &str = "---autumn-bake-nrestarts---";

/// A latency quantile from `/actuator/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// At most one gate for each quantile.
    pub latency: Vec<LatencyGate>,
    /// Where the limits come from, for the operator.
    pub source: String,
}

/// The p50, p95 and p99 latency of the app, in milliseconds.
///
/// The app computes them over its last 10,000 requests, not over the bake
/// window only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Latencies {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
}

impl Latencies {
    pub(crate) const fn get(self, quantile: Quantile) -> u64 {
        match quantile {
            Quantile::P50 => self.p50,
            Quantile::P95 => self.p95,
            Quantile::P99 => self.p99,
        }
    }
}

/// One metrics sample. The counters are cumulative since the process started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Sample {
    /// All responses (2xx to 5xx).
    pub responses: u64,
    /// 5xx responses.
    pub errors: u64,
    pub latency: Latencies,
    /// systemd `NRestarts` of the slot unit. `None` when systemd did not
    /// report it.
    pub restarts: Option<u64>,
}

/// Why the bake failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Breach {
    /// The process restarted during the bake.
    Restarted,
    /// The 5xx ratio is above the limit.
    ErrorRate {
        errors: u64,
        responses: u64,
        max_ppm: u32,
    },
    /// A latency quantile is above the limit.
    Latency {
        quantile: Quantile,
        ms: u64,
        max_ms: u64,
    },
    /// A sample could not be read two times in a row.
    Unreachable(String),
}

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restarted => f.write_str("the new release restarted during the bake"),
            Self::ErrorRate {
                errors,
                responses,
                max_ppm,
            } => write!(
                f,
                "{errors} of {responses} responses were 5xx ({}%), above the limit of {}%",
                percent(u128::from(*errors) * u128::from(PPM) / u128::from((*responses).max(1))),
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

/// A ppm value as a percentage with up to four decimals, rounded down.
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

/// The new responses since `baseline`, less the bake's own `own_requests`.
pub(crate) const fn window_responses(baseline: &Sample, now: &Sample, own_requests: u64) -> u64 {
    now.responses
        .saturating_sub(baseline.responses)
        .saturating_sub(own_requests)
}

/// Judge `now` against the bake `baseline`. The Verus model is `spec_judge`.
///
/// `own_requests` is the number of the bake's own metric requests that `now`
/// can count. They are not traffic.
pub(crate) fn judge(
    baseline: &Sample,
    now: &Sample,
    own_requests: u64,
    policy: &BakePolicy,
) -> Verdict {
    let restarted = match (baseline.restarts, now.restarts) {
        (Some(before), Some(after)) => after != before,
        _ => false,
    };
    if restarted || now.responses < baseline.responses || now.errors < baseline.errors {
        return Verdict::Breach(Breach::Restarted);
    }
    let responses = window_responses(baseline, now, own_requests);
    let errors = now.errors - baseline.errors;
    if responses < policy.min_requests {
        return Verdict::TooFewRequests;
    }
    // u128: a product of two u64 values cannot overflow.
    if errors >= MIN_BREACH_ERRORS
        && u128::from(errors) * u128::from(PPM)
            > u128::from(policy.max_error_ppm) * u128::from(responses)
    {
        return Verdict::Breach(Breach::ErrorRate {
            errors,
            responses,
            max_ppm: policy.max_error_ppm,
        });
    }
    for gate in &policy.latency {
        let ms = now.latency.get(gate.quantile);
        if ms > gate.max_ms {
            return Verdict::Breach(Breach::Latency {
                quantile: gate.quantile,
                ms,
                max_ms: gate.max_ms,
            });
        }
    }
    Verdict::Pass
}

/// Parse one sample: the `/actuator/metrics` JSON, then optionally the
/// restart marker and the systemd `NRestarts` value.
pub(crate) fn parse_sample(stdout: &str) -> Result<Sample, String> {
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
    let (json, restarts) = stdout
        .split_once(RESTARTS_MARKER)
        .map_or((stdout, None), |(json, rest)| (json, Some(rest)));
    let metrics: Metrics = serde_json::from_str(json.trim())
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
        latency: Latencies {
            p50: latency.p50,
            p95: latency.p95,
            p99: latency.p99,
        },
        restarts: restarts.and_then(|r| r.trim().parse().ok()),
    })
}

/// Where a bake reads its samples.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BakeTarget<'a> {
    /// The loopback port of the new release.
    pub port: u16,
    /// The `/actuator/metrics` path, already normalized.
    pub metrics_path: &'a str,
    /// A `Host` header the app trusts. The `prod` profile rejects
    /// `127.0.0.1` unless `[security.trusted_hosts]` lists it.
    pub host_header: Option<&'a str>,
    /// The systemd unit of the new release.
    pub unit: &'a str,
}

/// The remote command for one sample: sleep, read the metrics, then read the
/// systemd restart count.
pub(crate) fn sample_command(target: &BakeTarget<'_>, sleep_secs: u64) -> RemoteCommand {
    let host = target.host_header.map_or_else(String::new, |host| {
        format!(" -H {}", shell_quote(&format!("Host: {host}")))
    });
    let curl = format!(
        "curl -fsSg -m 5{host} {}",
        shell_quote(&format!(
            "http://127.0.0.1:{}{}",
            target.port, target.metrics_path
        ))
    );
    let restarts = format!(
        "printf '\\n%s\\n' {} && (systemctl show -p NRestarts --value {} || true)",
        shell_quote(RESTARTS_MARKER),
        shell_quote(&format!("{}.service", target.unit)),
    );
    let shell = if sleep_secs == 0 {
        format!("{curl} && {restarts}")
    } else {
        format!("sleep {sleep_secs} && {curl} && {restarts}")
    };
    RemoteCommand::new(SAMPLE_LABEL, shell)
}

/// Run one sample. A failed sample is tried one more time at once, so one
/// lost SSH connection does not roll a release back.
///
/// Returns the sample and the number of attempts.
fn take_sample<E: DeployExecutor>(
    executor: &E,
    target: &BakeTarget<'_>,
    sleep_secs: u64,
) -> Result<(Sample, u64), Breach> {
    let attempt = |sleep| {
        executor
            .run(&sample_command(target, sleep))
            .map_err(|e| e.to_string())
            .and_then(|output| parse_sample(&output.stdout))
    };
    attempt(sleep_secs).map_or_else(
        |_| {
            attempt(0)
                .map(|sample| (sample, 2))
                .map_err(Breach::Unreachable)
        },
        |sample| Ok((sample, 1)),
    )
}

/// The result of a bake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BakeOutcome {
    /// No breach. `responses` is the traffic the bake saw.
    Passed { responses: u64, judged: bool },
    /// A breach. The caller rolls back.
    Breached(Breach),
}

/// Run the bake against `target`. `progress` gets one line per sample.
pub(crate) fn run<E: DeployExecutor>(
    policy: &BakePolicy,
    target: &BakeTarget<'_>,
    executor: &E,
    progress: &mut dyn FnMut(&str),
) -> BakeOutcome {
    let (baseline, _) = match take_sample(executor, target, 0) {
        Ok(sample) => sample,
        Err(breach) => return BakeOutcome::Breached(breach),
    };
    // The app counts a metric request after it answers it. A failed baseline
    // attempt is already in the baseline counters, so only the successful
    // baseline request falls in the window.
    let mut own_requests = 1;
    let interval = policy.interval_secs.max(1);
    let mut elapsed = 0;
    let mut responses = 0;
    let mut judged = false;
    while elapsed < policy.duration_secs {
        let sleep = interval.min(policy.duration_secs - elapsed);
        elapsed += sleep;
        let (now, attempts) = match take_sample(executor, target, sleep) {
            Ok(sample) => sample,
            Err(breach) => return BakeOutcome::Breached(breach),
        };
        // A retried sample can count its own first attempt.
        let own = own_requests + attempts - 1;
        own_requests += attempts;
        responses = window_responses(&baseline, &now, own);
        progress(&format!(
            "bake {elapsed}/{} s: {responses} responses, {} 5xx, p99 {} ms",
            policy.duration_secs,
            now.errors.saturating_sub(baseline.errors),
            now.latency.p99,
        ));
        match judge(&baseline, &now, own, policy) {
            Verdict::Breach(breach) => return BakeOutcome::Breached(breach),
            Verdict::Pass => judged = true,
            Verdict::TooFewRequests => {}
        }
    }
    BakeOutcome::Passed { responses, judged }
}

/// The `/actuator/metrics` path for an `[actuator] prefix`. It matches the
/// app's own prefix rules: trim, one leading `/`, no trailing `/`.
pub(crate) fn metrics_path(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/metrics".to_owned()
    } else {
        format!("/{trimmed}/metrics")
    }
}

/// A `Host` header that `[security.trusted_hosts]` accepts: the first
/// entry, without a leading `.`. `None` when the list is empty or allows any
/// host.
pub(crate) fn host_header(trusted_hosts: &[String]) -> Option<String> {
    let first = trusted_hosts
        .iter()
        .map(|h| h.trim().trim_end_matches('.'))
        .find(|h| !h.is_empty())?;
    if first == "*" {
        return None;
    }
    Some(first.trim_start_matches('.').to_owned())
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
        vec![LatencyGate {
            quantile: Quantile::P99,
            max_ms,
        }]
    } else {
        // One gate for each quantile: the smallest limit wins.
        let mut gates: Vec<(LatencyGate, &str)> = Vec::new();
        for slo in slos {
            let Sli::Latency {
                route: None,
                threshold_ms,
            } = slo.sli
            else {
                continue;
            };
            let gate = LatencyGate {
                quantile: quantile_for(slo.objective_ppm),
                max_ms: threshold_ms,
            };
            match gates.iter_mut().find(|(g, _)| g.quantile == gate.quantile) {
                Some(entry) if entry.0.max_ms <= gate.max_ms => {}
                Some(entry) => *entry = (gate, &slo.name),
                None => gates.push((gate, &slo.name)),
            }
        }
        gates.sort_by_key(|(g, _)| g.quantile);
        for (_, name) in &gates {
            sources.push(format!("SLO {name}"));
        }
        gates.into_iter().map(|(g, _)| g).collect()
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
        let mut latency = String::new();
        for gate in &self.latency {
            let _ = write!(
                latency,
                " or {} latency is above {} ms",
                gate.quantile, gate.max_ms
            );
        }
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
    use crate::deploy::exec::test_support::{RecordedCall, RecordingExecutor};
    use autumn_web::slo::{SliKind, SloConfig};

    fn policy() -> BakePolicy {
        BakePolicy {
            duration_secs: 30,
            interval_secs: 10,
            min_requests: 20,
            max_error_ppm: 14_400,
            latency: vec![LatencyGate {
                quantile: Quantile::P99,
                max_ms: 250,
            }],
            source: "test".to_owned(),
        }
    }

    fn sample(responses: u64, errors: u64, p99: u64) -> Sample {
        Sample {
            responses,
            errors,
            latency: Latencies {
                p50: 1,
                p95: 2,
                p99,
            },
            restarts: Some(0),
        }
    }

    /// A sample's stdout: the metrics JSON, the marker and `NRestarts`.
    pub(crate) fn sample_stdout(ok: u64, server_errors: u64, p99: u64, restarts: u64) -> String {
        format!(
            r#"{{"http":{{"requests_total":{total},"requests_active":0,
            "latency_ms":{{"p50":1,"p95":2,"p99":{p99}}},
            "by_status":{{"2xx":{ok},"3xx":0,"4xx":0,"5xx":{server_errors}}},
            "by_route":{{}}}}}}
{RESTARTS_MARKER}
{restarts}
"#,
            total = ok + server_errors
        )
    }

    const TARGET: BakeTarget<'static> = BakeTarget {
        port: 3001,
        metrics_path: "/actuator/metrics",
        host_header: Some("app.example.com"),
        unit: "myapp-green",
    };

    // ── judge ────────────────────────────────────────────────────────────────

    #[test]
    fn judge_passes_healthy_traffic() {
        let v = judge(&sample(100, 0, 0), &sample(200, 1, 100), 0, &policy());
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn judge_waits_for_enough_traffic() {
        // 19 new responses, all 5xx: still no verdict.
        let v = judge(&sample(100, 0, 0), &sample(119, 19, 900), 0, &policy());
        assert_eq!(v, Verdict::TooFewRequests);
    }

    #[test]
    fn judge_does_not_count_its_own_requests() {
        // 25 new responses, but 6 are the bake's own metric requests.
        let v = judge(&sample(0, 0, 0), &sample(25, 0, 0), 6, &policy());
        assert_eq!(v, Verdict::TooFewRequests);
        let v = judge(&sample(0, 0, 0), &sample(25, 0, 0), 5, &policy());
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn judge_flags_an_error_spike() {
        // 10 % 5xx against a 1.44 % limit.
        let v = judge(&sample(100, 0, 0), &sample(200, 10, 10), 0, &policy());
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
    fn one_error_alone_never_breaches() {
        // 1 of 20 is 5 %, above 1.44 %, but one bad request is not a trend.
        let v = judge(&sample(0, 0, 0), &sample(20, 1, 0), 0, &policy());
        assert_eq!(v, Verdict::Pass);
        let v = judge(&sample(0, 0, 0), &sample(20, 2, 0), 0, &policy());
        assert!(
            matches!(v, Verdict::Breach(Breach::ErrorRate { .. })),
            "{v:?}"
        );
    }

    #[test]
    fn judge_is_exact_at_the_limit() {
        let mut p = policy();
        p.max_error_ppm = 20_000; // 2 %
        assert_eq!(
            judge(&sample(0, 0, 0), &sample(100, 2, 0), 0, &p),
            Verdict::Pass
        );
        assert!(matches!(
            judge(&sample(0, 0, 0), &sample(99, 2, 0), 0, &p),
            Verdict::Breach(Breach::ErrorRate { .. })
        ));
    }

    #[test]
    fn judge_flags_slow_latency_on_each_gate() {
        let v = judge(&sample(0, 0, 0), &sample(100, 0, 251), 0, &policy());
        assert_eq!(
            v,
            Verdict::Breach(Breach::Latency {
                quantile: Quantile::P99,
                ms: 251,
                max_ms: 250,
            })
        );
        let mut p = policy();
        p.latency.insert(
            0,
            LatencyGate {
                quantile: Quantile::P50,
                max_ms: 0,
            },
        );
        assert_eq!(
            judge(&sample(0, 0, 0), &sample(100, 0, 1), 0, &p),
            Verdict::Breach(Breach::Latency {
                quantile: Quantile::P50,
                ms: 1,
                max_ms: 0,
            })
        );
        p.latency.clear();
        assert_eq!(
            judge(&sample(0, 0, 0), &sample(100, 0, 9_999), 0, &p),
            Verdict::Pass
        );
    }

    #[test]
    fn judge_flags_a_restart() {
        let v = judge(&sample(500, 3, 0), &sample(10, 0, 0), 0, &policy());
        assert_eq!(v, Verdict::Breach(Breach::Restarted));
        let v = judge(&sample(500, 3, 0), &sample(600, 2, 0), 0, &policy());
        assert_eq!(v, Verdict::Breach(Breach::Restarted));
        // systemd restarted the unit, and the counters grew again.
        let mut after = sample(900, 3, 0);
        after.restarts = Some(1);
        assert_eq!(
            judge(&sample(500, 3, 0), &after, 0, &policy()),
            Verdict::Breach(Breach::Restarted)
        );
        // An unknown restart count is not a restart.
        after.restarts = None;
        assert_eq!(
            judge(&sample(500, 3, 0), &after, 0, &policy()),
            Verdict::Pass
        );
    }

    #[test]
    fn judge_checks_errors_before_latency() {
        let v = judge(&sample(0, 0, 0), &sample(100, 50, 9_999), 0, &policy());
        assert!(
            matches!(v, Verdict::Breach(Breach::ErrorRate { .. })),
            "{v:?}"
        );
    }

    #[test]
    fn judge_does_not_overflow() {
        let mut p = policy();
        p.max_error_ppm = PPM;
        p.latency.clear();
        let v = judge(&sample(0, 0, 0), &sample(u64::MAX, u64::MAX, 0), 0, &p);
        assert_eq!(v, Verdict::Pass);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        /// `spec_judge` from `verification/bake_verdict.rs`, over `i128`.
        fn model(
            base: Sample,
            now: Sample,
            own: u64,
            latency_over: bool,
            policy: &BakePolicy,
        ) -> &'static str {
            let restarted = matches!((base.restarts, now.restarts), (Some(x), Some(y)) if x != y);
            if restarted || now.responses < base.responses || now.errors < base.errors {
                return "restart";
            }
            let dr =
                (i128::from(now.responses) - i128::from(base.responses) - i128::from(own)).max(0);
            let de = i128::from(now.errors) - i128::from(base.errors);
            if dr < i128::from(policy.min_requests) {
                return "few";
            }
            if de >= i128::from(MIN_BREACH_ERRORS)
                && de * 1_000_000 > i128::from(policy.max_error_ppm) * dr
            {
                return "errors";
            }
            if latency_over {
                return "latency";
            }
            "pass"
        }

        fn name(v: &Verdict) -> &'static str {
            match v {
                Verdict::Pass => "pass",
                Verdict::TooFewRequests => "few",
                Verdict::Breach(Breach::Restarted) => "restart",
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
                own in 0u64..50, lat in 0u64..1_000, max_ppm in 0u32..=PPM,
                min in 0u64..200, gate in proptest::option::of(0u64..1_000),
                b_restarts in proptest::option::of(0u64..3),
                n_restarts in proptest::option::of(0u64..3),
            ) {
                let p = BakePolicy {
                    min_requests: min,
                    max_error_ppm: max_ppm,
                    latency: gate
                        .map(|max_ms| LatencyGate { quantile: Quantile::P99, max_ms })
                        .into_iter()
                        .collect(),
                    ..policy()
                };
                let mut b = sample(b_r, b_e, 0);
                b.restarts = b_restarts;
                let mut n = sample(n_r, n_e, lat);
                n.restarts = n_restarts;
                let latency_over = gate.is_some_and(|max| lat > max);
                prop_assert_eq!(name(&judge(&b, &n, own, &p)), model(b, n, own, latency_over, &p));
            }
        }
    }

    // ── parse_sample ─────────────────────────────────────────────────────────

    #[test]
    fn parse_sample_reads_counters_latency_and_restarts() {
        let s = parse_sample(&sample_stdout(90, 10, 42, 3)).expect("parse");
        assert_eq!(s.responses, 100);
        assert_eq!(s.errors, 10);
        assert_eq!(
            s.latency,
            Latencies {
                p50: 1,
                p95: 2,
                p99: 42
            }
        );
        assert_eq!(s.restarts, Some(3));
    }

    #[test]
    fn parse_sample_without_a_restart_count() {
        let json = sample_stdout(1, 0, 1, 0);
        let (json, _) = json.split_once(RESTARTS_MARKER).expect("marker");
        assert_eq!(parse_sample(json).expect("parse").restarts, None);
        let blank = format!("{json}{RESTARTS_MARKER}\n\n");
        assert_eq!(parse_sample(&blank).expect("parse").restarts, None);
    }

    #[test]
    fn parse_sample_reads_the_real_actuator_json() {
        // The shape `/actuator/metrics` serializes (autumn_web::middleware::metrics).
        let collector = autumn_web::middleware::MetricsCollector::new();
        collector.record("GET", "/a", 200, 7);
        collector.record("GET", "/a", 404, 7);
        collector.record("POST", "/b", 503, 7);
        let json = serde_json::to_string(&collector.snapshot()).expect("serialize");
        let s = parse_sample(&json).expect("parse the real shape");
        assert_eq!((s.responses, s.errors), (3, 1));
        assert_eq!(s.latency.p99, 7);
    }

    #[test]
    fn parse_sample_rejects_bad_json() {
        let error = parse_sample("<html>").expect_err("bad");
        assert!(error.contains("JSON"), "{error}");
    }

    // ── sample_command ───────────────────────────────────────────────────────

    #[test]
    fn sample_command_sends_a_trusted_host_and_reads_restarts() {
        let cmd = sample_command(&TARGET, 10);
        assert_eq!(cmd.label, SAMPLE_LABEL);
        assert_eq!(
            cmd.shell,
            "sleep 10 && curl -fsSg -m 5 -H 'Host: app.example.com' \
             'http://127.0.0.1:3001/actuator/metrics' && printf '\\n%s\\n' \
             '---autumn-bake-nrestarts---' && (systemctl show -p NRestarts --value \
             'myapp-green.service' || true)"
        );
        let first = sample_command(
            &BakeTarget {
                host_header: None,
                ..TARGET
            },
            0,
        );
        assert!(
            first
                .shell
                .starts_with("curl -fsSg -m 5 'http://127.0.0.1:3001/actuator/metrics' && "),
            "{}",
            first.shell
        );
    }

    #[test]
    fn sample_command_quotes_hostile_values() {
        let cmd = sample_command(
            &BakeTarget {
                metrics_path: "/a'; rm -rf /; '/metrics",
                host_header: Some("x'$(id)"),
                ..TARGET
            },
            0,
        );
        assert!(
            cmd.shell
                .contains(r"'http://127.0.0.1:3001/a'\''; rm -rf /; '\''/metrics'")
        );
        assert!(cmd.shell.contains(r"-H 'Host: x'\''$(id)'"));
    }

    #[test]
    fn metrics_path_follows_the_app_prefix_rules() {
        assert_eq!(metrics_path("/actuator"), "/actuator/metrics");
        assert_eq!(metrics_path("ops"), "/ops/metrics");
        assert_eq!(metrics_path(" /ops/ "), "/ops/metrics");
        assert_eq!(metrics_path("/"), "/metrics");
        assert_eq!(metrics_path(""), "/metrics");
    }

    #[test]
    fn host_header_takes_the_first_trusted_host() {
        let hosts = |list: &[&str]| list.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            host_header(&hosts(&["app.example.com", "x"])).as_deref(),
            Some("app.example.com")
        );
        assert_eq!(
            host_header(&hosts(&[".example.com"])).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_header(&hosts(&[" ", "b.example.com."])).as_deref(),
            Some("b.example.com")
        );
        assert_eq!(host_header(&hosts(&["*"])), None);
        assert_eq!(host_header(&[]), None);
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
        run(&policy(), &TARGET, exec, &mut |_| {})
    }

    fn shells(exec: &RecordingExecutor) -> Vec<String> {
        exec.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::Run { shell, .. } => Some(shell),
                RecordedCall::Upload { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_healthy_bake_passes_after_every_sample() {
        let exec = exec_with(&[
            sample_stdout(10, 0, 5, 0),
            sample_stdout(110, 0, 5, 0),
            sample_stdout(210, 1, 5, 0),
            sample_stdout(310, 1, 5, 0),
        ]);
        // 301 new responses, less the 3 metric requests of the bake.
        assert_eq!(
            run_quiet(&exec),
            BakeOutcome::Passed {
                responses: 298,
                judged: true,
            }
        );
        // 30 s at 10 s: a baseline and three samples.
        assert_eq!(exec.run_labels(), vec![SAMPLE_LABEL; 4]);
        let shells = shells(&exec);
        assert!(shells[0].starts_with("curl "), "{shells:?}");
        assert!(shells[1].starts_with("sleep 10 && "), "{shells:?}");
    }

    #[test]
    fn an_injected_error_spike_breaches_and_stops_sampling() {
        let exec = exec_with(&[
            sample_stdout(10, 0, 5, 0),
            sample_stdout(110, 0, 5, 0),
            // The spike: 30 new 5xx.
            sample_stdout(210, 30, 5, 0),
            sample_stdout(310, 30, 5, 0),
        ]);
        assert_eq!(
            run_quiet(&exec),
            BakeOutcome::Breached(Breach::ErrorRate {
                errors: 30,
                responses: 228,
                max_ppm: 14_400,
            })
        );
        assert_eq!(exec.run_labels().len(), 3, "no sample after the breach");
    }

    #[test]
    fn a_systemd_restart_breaches() {
        let exec = exec_with(&[sample_stdout(10, 0, 5, 0), sample_stdout(500, 0, 5, 1)]);
        assert_eq!(run_quiet(&exec), BakeOutcome::Breached(Breach::Restarted));
    }

    #[test]
    fn an_idle_app_does_not_pass_on_its_own_metric_requests() {
        // Each sample sees only the previous metric request.
        let exec = exec_with(&[
            sample_stdout(0, 0, 1, 0),
            sample_stdout(1, 0, 1, 0),
            sample_stdout(2, 0, 1, 0),
            sample_stdout(3, 0, 1, 0),
        ]);
        assert_eq!(
            run_quiet(&exec),
            BakeOutcome::Passed {
                responses: 0,
                judged: false,
            }
        );
    }

    #[test]
    fn one_failed_sample_is_retried() {
        let exec = RecordingExecutor::new()
            .with_stdout_on_occurrence(SAMPLE_LABEL, 1, sample_stdout(0, 0, 1, 0))
            .failing_on_occurrence(SAMPLE_LABEL, 2)
            .with_stdout(SAMPLE_LABEL, sample_stdout(500, 0, 1, 0));
        let outcome = run_quiet(&exec);
        assert!(
            matches!(outcome, BakeOutcome::Passed { judged: true, .. }),
            "{outcome:?}"
        );
        let shells = shells(&exec);
        assert!(shells[1].starts_with("sleep 10 && "), "{shells:?}");
        assert!(
            shells[2].starts_with("curl "),
            "the retry does not sleep: {shells:?}"
        );
    }

    #[test]
    fn a_retried_baseline_subtracts_only_its_successful_request() {
        let mut p = policy();
        p.duration_secs = 10;
        let exec = RecordingExecutor::new()
            .failing_on_occurrence(SAMPLE_LABEL, 1)
            .with_stdout_on_occurrence(SAMPLE_LABEL, 2, sample_stdout(0, 0, 1, 0))
            .with_stdout_on_occurrence(SAMPLE_LABEL, 3, sample_stdout(21, 0, 1, 0));
        // 21 new responses, less 1 metric request: exactly min_requests.
        assert_eq!(
            run(&p, &TARGET, &exec, &mut |_| {}),
            BakeOutcome::Passed {
                responses: 20,
                judged: true,
            }
        );
    }

    #[test]
    fn two_failed_samples_in_a_row_breach() {
        let exec = RecordingExecutor::new()
            .with_stdout_on_occurrence(SAMPLE_LABEL, 1, sample_stdout(0, 0, 1, 0))
            .failing_on_occurrence(SAMPLE_LABEL, 2)
            .failing_on_occurrence(SAMPLE_LABEL, 3);
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
            sample_stdout(0, 0, 1, 0),
            sample_stdout(100, 0, 1, 0),
            sample_stdout(200, 0, 1, 0),
            sample_stdout(300, 0, 1, 0),
        ]);
        run(&p, &TARGET, &exec, &mut |_| {});
        let last = shells(&exec).pop().expect("a sample");
        assert!(last.starts_with("sleep 5 && "), "{last}");
    }

    #[test]
    fn progress_reports_each_sample() {
        let exec = exec_with(&[
            sample_stdout(0, 0, 1, 0),
            sample_stdout(100, 0, 1, 0),
            sample_stdout(200, 0, 1, 0),
            sample_stdout(300, 0, 1, 0),
        ]);
        let mut lines = Vec::new();
        run(&policy(), &TARGET, &exec, &mut |l| {
            lines.push(l.to_owned());
        });
        assert_eq!(
            lines,
            vec![
                "bake 10/30 s: 99 responses, 0 5xx, p99 1 ms",
                "bake 20/30 s: 198 responses, 0 5xx, p99 1 ms",
                "bake 30/30 s: 297 responses, 0 5xx, p99 1 ms",
            ]
        );
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
        assert!(p.latency.is_empty());
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
    fn latency_slos_set_one_gate_per_quantile() {
        let slos = [
            slo("slow", 99.0, SliKind::Latency, None, Some(500)),
            slo("slower", 99.5, SliKind::Latency, None, Some(1_000)),
            slo("fast", 95.0, SliKind::Latency, None, Some(100)),
            slo("route", 99.9, SliKind::Latency, Some("/x"), Some(5)),
        ];
        let p = resolve_policy(&on(60), &slos, None)
            .expect("ok")
            .expect("on");
        assert_eq!(
            p.latency,
            vec![
                LatencyGate {
                    quantile: Quantile::P95,
                    max_ms: 100,
                },
                LatencyGate {
                    quantile: Quantile::P99,
                    max_ms: 500,
                },
            ]
        );
        assert!(p.source.contains("SLO fast, SLO slow"), "{}", p.source);
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
            assert_eq!(p.latency[0].quantile, quantile, "{objective}");
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
            vec![LatencyGate {
                quantile: Quantile::P99,
                max_ms: 800,
            }]
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
    fn describe_states_every_limit() {
        let mut p = policy();
        p.duration_secs = 300;
        p.source = "SLO availability at 14.4x burn, SLO latency".to_owned();
        assert_eq!(
            p.describe(),
            "bake for 300 s, sample every 10 s; roll back when the 5xx ratio is above 1.44% \
             or p99 latency is above 250 ms (after 20 responses; limits from SLO \
             availability at 14.4x burn, SLO latency)"
        );
        p.latency.clear();
        p.source = "the default 5% error limit".to_owned();
        p.max_error_ppm = DEFAULT_MAX_ERROR_PPM;
        assert_eq!(
            p.describe(),
            "bake for 300 s, sample every 10 s; roll back when the 5xx ratio is above 5% \
             (after 20 responses; limits from the default 5% error limit)"
        );
    }

    #[test]
    fn breach_messages_name_the_numbers() {
        let text = Breach::ErrorRate {
            errors: 37,
            responses: 431,
            max_ppm: 14_400,
        }
        .to_string();
        assert_eq!(
            text,
            "37 of 431 responses were 5xx (8.5846%), above the limit of 1.44%"
        );
        let text = Breach::Latency {
            quantile: Quantile::P99,
            ms: 900,
            max_ms: 250,
        }
        .to_string();
        assert_eq!(text, "p99 latency is 900 ms, above the limit of 250 ms");
    }
}
