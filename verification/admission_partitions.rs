use vstd::arithmetic::div_mod::{lemma_div_by_multiple, lemma_div_is_ordered};
use vstd::arithmetic::mul::lemma_mul_inequality;
use vstd::prelude::*;

verus! {

/// Parts per thousand in a share of 1.0 (`PERMILLE` in
/// `autumn/src/admission.rs`).
pub open spec fn permille_one() -> nat {
    1000
}

/// `scale_permille`: `floor(limit * permille / 1000)`.
pub open spec fn scaled(limit: nat, permille: nat) -> nat {
    (limit * permille) / 1000
}

/// `PartitionShares::threshold` for a share: `floor(limit * share)`, but at
/// least 1 when the share and the limit are above 0.
pub open spec fn share_threshold(limit: nat, permille: nat) -> nat {
    if permille == 0 || limit == 0 {
        0
    } else if scaled(limit, permille) == 0 {
        1
    } else {
        scaled(limit, permille)
    }
}

/// The admission classes. `class` 0 is critical, 1 is default, 2 is
/// sheddable, as `Criticality` in `autumn/src/admission.rs`.
pub open spec fn threshold(class: int, limit: nat, default_pm: nat, sheddable_pm: nat) -> nat {
    if class == 0 {
        limit
    } else if class == 1 {
        share_threshold(limit, default_pm)
    } else {
        share_threshold(limit, sheddable_pm)
    }
}

/// `LoadShedService::call` admits a request only below its threshold.
pub open spec fn admitted(
    class: int,
    in_flight: nat,
    limit: nat,
    default_pm: nat,
    sheddable_pm: nat,
) -> bool {
    in_flight < threshold(class, limit, default_pm, sheddable_pm)
}

/// The shares that `PartitionShares::new` accepts.
pub open spec fn valid_shares(default_pm: nat, sheddable_pm: nat) -> bool {
    sheddable_pm <= default_pm && default_pm <= permille_one()
}

/// A larger share never gives a smaller threshold.
pub proof fn scaled_is_monotone(limit: nat, p: nat, q: nat)
    requires
        p <= q,
    ensures
        scaled(limit, p) <= scaled(limit, q),
{
    lemma_mul_inequality(p as int, q as int, limit as int);
    assert(limit * p == p * limit) by (nonlinear_arith);
    assert(limit * q == q * limit) by (nonlinear_arith);
    lemma_div_is_ordered((limit * p) as int, (limit * q) as int, 1000);
}

/// A share of at most 1.0 never gives more than the limit.
pub proof fn scaled_at_most_limit(limit: nat, p: nat)
    requires
        p <= permille_one(),
    ensures
        scaled(limit, p) <= limit,
        scaled(limit, permille_one()) == limit,
{
    scaled_is_monotone(limit, p, permille_one());
    lemma_div_by_multiple(limit as int, 1000);
}

/// The class threshold keeps the order of the shares.
pub proof fn share_threshold_is_monotone(limit: nat, p: nat, q: nat)
    requires
        p <= q,
    ensures
        share_threshold(limit, p) <= share_threshold(limit, q),
{
    scaled_is_monotone(limit, p, q);
}

/// The class threshold is never above the limit.
pub proof fn share_threshold_at_most_limit(limit: nat, p: nat)
    requires
        p <= permille_one(),
    ensures
        share_threshold(limit, p) <= limit,
{
    scaled_at_most_limit(limit, p);
}

/// The thresholds are nested: sheddable <= default <= critical == limit.
pub proof fn thresholds_are_nested(limit: nat, default_pm: nat, sheddable_pm: nat)
    requires
        valid_shares(default_pm, sheddable_pm),
    ensures
        threshold(2, limit, default_pm, sheddable_pm) <= threshold(1, limit, default_pm, sheddable_pm),
        threshold(1, limit, default_pm, sheddable_pm) <= threshold(0, limit, default_pm, sheddable_pm),
        threshold(0, limit, default_pm, sheddable_pm) == limit,
{
    share_threshold_is_monotone(limit, sheddable_pm, default_pm);
    share_threshold_at_most_limit(limit, default_pm);
}

/// Issue #3068, "sheddable requests are rejected before critical ones": at
/// any in-flight count, if a higher class is rejected, every lower class is
/// rejected too. Equivalently, a lower class is admitted only when every
/// higher class would be.
pub proof fn sheddable_is_rejected_before_critical(
    in_flight: nat,
    limit: nat,
    default_pm: nat,
    sheddable_pm: nat,
)
    requires
        valid_shares(default_pm, sheddable_pm),
    ensures
        admitted(2, in_flight, limit, default_pm, sheddable_pm) ==> admitted(
            1,
            in_flight,
            limit,
            default_pm,
            sheddable_pm,
        ),
        admitted(1, in_flight, limit, default_pm, sheddable_pm) ==> admitted(
            0,
            in_flight,
            limit,
            default_pm,
            sheddable_pm,
        ),
        // No class is ever admitted at or above the limit.
        admitted(0, in_flight, limit, default_pm, sheddable_pm) ==> in_flight < limit,
{
    thresholds_are_nested(limit, default_pm, sheddable_pm);
}

/// Models `scale_permille`: the `u128` product cannot overflow, and the
/// result equals the spec.
pub fn scale_permille(limit: u64, permille: u16) -> (r: u64)
    requires
        permille <= 1000,
    ensures
        r as nat == scaled(limit as nat, permille as nat),
        r <= limit,
{
    proof {
        scaled_at_most_limit(limit as nat, permille as nat);
        assert((limit as nat) * (permille as nat) <= 18446744073709551615nat * 1000) by (nonlinear_arith)
            requires
                limit <= 18446744073709551615u64,
                permille <= 1000,
        ;
    }
    let product: u128 = (limit as u128) * (permille as u128);
    let r: u128 = product / 1000;
    assert(r as nat == scaled(limit as nat, permille as nat));
    r as u64
}

/// Models `PartitionShares::threshold` for the `default` and `sheddable`
/// classes.
pub fn class_threshold(limit: u64, permille: u16) -> (r: u64)
    requires
        permille <= 1000,
    ensures
        r as nat == share_threshold(limit as nat, permille as nat),
        r <= limit,
{
    if permille == 0 || limit == 0 {
        return 0;
    }
    let floor = scale_permille(limit, permille);
    if floor == 0 {
        1
    } else {
        floor
    }
}

/// Models the AIMD update in `Aimd::update`: the limit stays in
/// `min..=max` after every sample. The back-off here rounds up
/// (`limit - limit / 10`) where the runtime rounds down; the clamp makes the
/// property hold for either.
pub fn aimd_update(limit: u64, min: u64, max: u64, backoff: bool, grow: bool) -> (r: u64)
    requires
        1 <= min <= limit <= max,
    ensures
        min <= r <= max,
{
    let next = if backoff {
        limit - limit / 10
    } else if grow && limit < u64::MAX {
        limit + 1
    } else {
        limit
    };
    if next < min {
        min
    } else if next > max {
        max
    } else {
        next
    }
}

} // verus!

fn main() {}
