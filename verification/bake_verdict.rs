use vstd::prelude::*;

verus! {

/// The shadow of one bake sample (`autumn-cli/src/deploy/bake.rs`, issue #3069).
/// `responses` and `errors` are cumulative counters since the process started.
/// `errors` counts 5xx responses. `restarts` is systemd `NRestarts`; the
/// runtime maps an unknown count to "no change".
pub struct SampleView {
    pub responses: u64,
    pub errors: u64,
    pub restarts: u64,
}

/// The shadow of `BakePolicy`. The latency gates are abstract: the runtime
/// passes `latency_over` when any gate is above its limit.
pub struct PolicyView {
    pub min_requests: u64,
    pub max_error_ppm: u64,
    pub min_errors: u64,
}

pub enum Verdict {
    Pass,
    TooFewRequests,
    Restarted,
    ErrorRate,
    Latency,
}

/// The process restarted during the bake.
pub open spec fn restarted(b: SampleView, n: SampleView) -> bool {
    n.restarts != b.restarts || n.responses < b.responses || n.errors < b.errors
}

/// New responses, less the bake's `own` metric requests, never below 0.
pub open spec fn window_responses(b: SampleView, n: SampleView, own: u64) -> int {
    if n.responses - b.responses >= own {
        n.responses - b.responses - own
    } else {
        0
    }
}

/// New 5xx, less the bake's `failed` own requests (a failed own request can
/// be a 5xx), never below 0.
pub open spec fn delta_errors(b: SampleView, n: SampleView, failed: u64) -> int {
    if n.errors - b.errors >= failed {
        n.errors - b.errors - failed
    } else {
        0
    }
}

/// Enough errors, and the error ratio is above `max_error_ppm / 1e6`.
pub open spec fn error_breach(b: SampleView, n: SampleView, own: u64, failed: u64, p: PolicyView) -> bool {
    delta_errors(b, n, failed) >= p.min_errors && delta_errors(b, n, failed) * 1_000_000 > p.max_error_ppm
        * window_responses(b, n, own)
}

/// The verdict contract. The order of the checks is part of the contract.
pub open spec fn spec_judge(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
) -> Verdict {
    if restarted(b, n) {
        Verdict::Restarted
    } else if window_responses(b, n, own) < p.min_requests {
        Verdict::TooFewRequests
    } else if error_breach(b, n, own, failed, p) {
        Verdict::ErrorRate
    } else if latency_over {
        Verdict::Latency
    } else {
        Verdict::Pass
    }
}

/// A verdict that rolls the release back.
pub open spec fn rolls_back(v: Verdict) -> bool {
    match v {
        Verdict::Restarted | Verdict::ErrorRate | Verdict::Latency => true,
        Verdict::Pass | Verdict::TooFewRequests => false,
    }
}

/// The runtime decision, with the same overflow-free arithmetic as
/// `bake::judge` (u128 products of u64 values, saturating subtraction).
pub fn judge(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
) -> (v: Verdict)
    ensures
        v == spec_judge(b, n, own, failed, latency_over, p),
{
    if n.restarts != b.restarts || n.responses < b.responses || n.errors < b.errors {
        return Verdict::Restarted;
    }
    let delta = n.responses - b.responses;
    let requests = if delta >= own {
        delta - own
    } else {
        0
    };
    let raw_errors = n.errors - b.errors;
    let errors = if raw_errors >= failed {
        raw_errors - failed
    } else {
        0
    };
    if requests < p.min_requests {
        return Verdict::TooFewRequests;
    }
    let lhs = (errors as u128) * 1_000_000u128;
    // A product of two u64 values fits in a u128.
    assert((p.max_error_ppm as u128) * (requests as u128) <= 0xffff_ffff_ffff_ffff_ffff_ffff_ffff_ffffu128)
        by (nonlinear_arith)
        requires
            p.max_error_ppm <= 0xffff_ffff_ffff_ffffu64,
            requests <= 0xffff_ffff_ffff_ffffu64,
    ;
    let rhs = (p.max_error_ppm as u128) * (requests as u128);
    assert(lhs == delta_errors(b, n, failed) * 1_000_000) by (nonlinear_arith)
        requires
            errors == delta_errors(b, n, failed),
            lhs == errors * 1_000_000,
    ;
    assert(rhs == p.max_error_ppm * window_responses(b, n, own)) by (nonlinear_arith)
        requires
            requests == window_responses(b, n, own),
            rhs == p.max_error_ppm * requests,
    ;
    if errors >= p.min_errors && lhs > rhs {
        return Verdict::ErrorRate;
    }
    if latency_over {
        return Verdict::Latency;
    }
    Verdict::Pass
}

/// Thin traffic never rolls back. A quiet bake cannot fail on its traffic.
proof fn lemma_thin_traffic_never_rolls_back(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
)
    requires
        !restarted(b, n),
        window_responses(b, n, own) < p.min_requests,
    ensures
        !rolls_back(spec_judge(b, n, own, failed, latency_over, p)),
{
}

/// A restart always rolls back, whatever the traffic.
proof fn lemma_restart_always_rolls_back(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
)
    requires
        restarted(b, n),
    ensures
        rolls_back(spec_judge(b, n, own, failed, latency_over, p)),
{
}

/// Fewer than `min_errors` new 5xx never give an error breach. With
/// `min_errors = 2`, one bad request alone never rolls back.
proof fn lemma_too_few_errors_no_error_breach(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
)
    requires
        !restarted(b, n),
        delta_errors(b, n, failed) < p.min_errors,
    ensures
        spec_judge(b, n, own, failed, latency_over, p) != Verdict::ErrorRate,
{
}

/// More errors in the same traffic cannot turn an error breach into a pass.
proof fn lemma_error_breach_is_monotonic(
    b: SampleView,
    n1: SampleView,
    n2: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
)
    requires
        !restarted(b, n1),
        n2.responses == n1.responses,
        n2.restarts == n1.restarts,
        n2.errors >= n1.errors,
        spec_judge(b, n1, own, failed, latency_over, p) == Verdict::ErrorRate,
    ensures
        spec_judge(b, n2, own, failed, latency_over, p) == Verdict::ErrorRate,
{
    assert(delta_errors(b, n2, failed) * 1_000_000 >= delta_errors(b, n1, failed) * 1_000_000)
        by (nonlinear_arith)
        requires
            delta_errors(b, n2, failed) >= delta_errors(b, n1, failed),
    ;
}

/// A pass means that no limit is exceeded and the traffic is large enough.
proof fn lemma_pass_is_sound(
    b: SampleView,
    n: SampleView,
    own: u64,
    failed: u64,
    latency_over: bool,
    p: PolicyView,
)
    requires
        spec_judge(b, n, own, failed, latency_over, p) == Verdict::Pass,
    ensures
        !restarted(b, n),
        window_responses(b, n, own) >= p.min_requests,
        !error_breach(b, n, own, failed, p),
        !latency_over,
{
}

fn main() {
}

} // verus!
