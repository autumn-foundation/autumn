use vstd::prelude::*;

verus! {

/// The shadow of one `/actuator/metrics` sample in the deploy bake
/// (`autumn-cli/src/deploy/bake.rs`, issue #3069).
/// `responses` and `errors` are cumulative counters since the process started.
/// `errors` counts 5xx responses. `latency_ms` is the gated quantile.
pub struct SampleView {
    pub responses: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

/// The shadow of `BakePolicy`.
pub struct PolicyView {
    pub min_requests: u64,
    pub max_error_ppm: u64,
    pub latency_on: bool,
    pub max_latency_ms: u64,
}

pub enum Verdict {
    Pass,
    TooFewRequests,
    CounterReset,
    ErrorRate,
    Latency,
}

/// A counter went down: the process restarted during the bake.
pub open spec fn counter_reset(b: SampleView, n: SampleView) -> bool {
    n.responses < b.responses || n.errors < b.errors
}

pub open spec fn delta_responses(b: SampleView, n: SampleView) -> int {
    n.responses - b.responses
}

pub open spec fn delta_errors(b: SampleView, n: SampleView) -> int {
    n.errors - b.errors
}

/// The error ratio of the bake window is above `max_error_ppm / 1e6`.
pub open spec fn error_breach(b: SampleView, n: SampleView, p: PolicyView) -> bool {
    delta_errors(b, n) * 1_000_000 > p.max_error_ppm * delta_responses(b, n)
}

/// The verdict contract. The order of the checks is part of the contract.
pub open spec fn spec_judge(b: SampleView, n: SampleView, p: PolicyView) -> Verdict {
    if counter_reset(b, n) {
        Verdict::CounterReset
    } else if delta_responses(b, n) < p.min_requests {
        Verdict::TooFewRequests
    } else if error_breach(b, n, p) {
        Verdict::ErrorRate
    } else if p.latency_on && n.latency_ms > p.max_latency_ms {
        Verdict::Latency
    } else {
        Verdict::Pass
    }
}

/// A verdict that rolls the release back.
pub open spec fn rolls_back(v: Verdict) -> bool {
    match v {
        Verdict::CounterReset | Verdict::ErrorRate | Verdict::Latency => true,
        Verdict::Pass | Verdict::TooFewRequests => false,
    }
}

/// The runtime decision, with the same overflow-free arithmetic as
/// `bake::judge` (u128 products of u64 values).
pub fn judge(b: SampleView, n: SampleView, p: PolicyView) -> (v: Verdict)
    ensures
        v == spec_judge(b, n, p),
{
    if n.responses < b.responses || n.errors < b.errors {
        return Verdict::CounterReset;
    }
    let requests = n.responses - b.responses;
    let errors = n.errors - b.errors;
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
    assert(lhs == delta_errors(b, n) * 1_000_000) by (nonlinear_arith)
        requires
            errors == delta_errors(b, n),
            lhs == errors * 1_000_000,
    ;
    assert(rhs == p.max_error_ppm * delta_responses(b, n)) by (nonlinear_arith)
        requires
            requests == delta_responses(b, n),
            rhs == p.max_error_ppm * requests,
    ;
    if lhs > rhs {
        return Verdict::ErrorRate;
    }
    if p.latency_on && n.latency_ms > p.max_latency_ms {
        return Verdict::Latency;
    }
    Verdict::Pass
}

/// Thin traffic never rolls back. A quiet bake cannot fail on one bad request.
proof fn lemma_thin_traffic_never_rolls_back(b: SampleView, n: SampleView, p: PolicyView)
    requires
        !counter_reset(b, n),
        delta_responses(b, n) < p.min_requests,
    ensures
        !rolls_back(spec_judge(b, n, p)),
{
}

/// A process restart always rolls back, whatever the traffic.
proof fn lemma_restart_always_rolls_back(b: SampleView, n: SampleView, p: PolicyView)
    requires
        counter_reset(b, n),
    ensures
        rolls_back(spec_judge(b, n, p)),
{
}

/// With no new 5xx, the error gate never fires.
proof fn lemma_no_errors_no_error_breach(b: SampleView, n: SampleView, p: PolicyView)
    requires
        !counter_reset(b, n),
        n.errors == b.errors,
    ensures
        spec_judge(b, n, p) != Verdict::ErrorRate,
{
    assert(delta_errors(b, n) == 0);
    assert(p.max_error_ppm * delta_responses(b, n) >= 0) by (nonlinear_arith)
        requires
            delta_responses(b, n) >= 0,
    ;
}

/// More errors in the same traffic cannot turn an error breach into a pass.
proof fn lemma_error_breach_is_monotonic(
    b: SampleView,
    n1: SampleView,
    n2: SampleView,
    p: PolicyView,
)
    requires
        !counter_reset(b, n1),
        n2.responses == n1.responses,
        n2.errors >= n1.errors,
        spec_judge(b, n1, p) == Verdict::ErrorRate,
    ensures
        spec_judge(b, n2, p) == Verdict::ErrorRate,
{
    assert(delta_errors(b, n2) * 1_000_000 >= delta_errors(b, n1) * 1_000_000) by (nonlinear_arith)
        requires
            delta_errors(b, n2) >= delta_errors(b, n1),
    ;
}

/// A pass proves the error ratio is within the limit and the traffic is
/// large enough to judge.
proof fn lemma_pass_is_sound(b: SampleView, n: SampleView, p: PolicyView)
    requires
        spec_judge(b, n, p) == Verdict::Pass,
    ensures
        !counter_reset(b, n),
        delta_responses(b, n) >= p.min_requests,
        delta_errors(b, n) * 1_000_000 <= p.max_error_ppm * delta_responses(b, n),
        !p.latency_on || n.latency_ms <= p.max_latency_ms,
{
}

fn main() {
}

} // verus!
