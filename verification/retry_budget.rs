use vstd::arithmetic::div_mod::{lemma_fundamental_div_mod, lemma_mod_bound};
use vstd::prelude::*;

verus! {

// The retry budget token bucket in `autumn/src/retry_budget.rs` (issue #3058).
//
// The runtime keeps the tokens in one `AtomicU64` and changes them with a
// compare-exchange loop, so each step here is one successful exchange.
// `tests/shuttle_models.rs` and `tests/loom_models.rs` check that the loop
// itself loses no update; this file proves the arithmetic of each step.

/// `RetryBudget::add`: `min(tokens + amount, capacity)`.
pub open spec fn added(tokens: nat, amount: nat, capacity: nat) -> nat {
    if tokens + amount <= capacity {
        tokens + amount
    } else {
        capacity
    }
}

/// The closure in `RetryBudget::add`, with the runtime's `saturating_add`
/// and `min`.
pub fn add(tokens: u64, amount: u64, capacity: u64) -> (r: u64)
    requires
        tokens <= capacity,
    ensures
        r as nat == added(tokens as nat, amount as nat, capacity as nat),
        tokens <= r <= capacity,
{
    let sum = if amount > u64::MAX - tokens {
        u64::MAX
    } else {
        tokens + amount
    };
    if sum < capacity {
        sum
    } else {
        capacity
    }
}

/// The closure in `RetryBudget::try_acquire`: `tokens.checked_sub(cost)`.
pub fn try_acquire(tokens: u64, cost: u64) -> (r: Option<u64>)
    ensures
        r.is_some() <==> tokens >= cost,
        r.is_some() ==> r.unwrap() == tokens - cost,
{
    if tokens >= cost {
        Some(tokens - cost)
    } else {
        None
    }
}

/// Neither step can push the bucket past its capacity.
pub proof fn steps_stay_within_capacity(tokens: u64, amount: u64, cost: u64, capacity: u64)
    requires
        tokens <= capacity,
    ensures
        added(tokens as nat, amount as nat, capacity as nat) <= capacity,
        tokens >= cost ==> tokens - cost <= capacity,
{}

/// A retry that succeeds gives back exactly what it took.
pub proof fn release_undoes_acquire(tokens: nat, cost: nat, capacity: nat)
    requires
        tokens <= capacity,
        tokens >= cost,
    ensures
        added((tokens - cost) as nat, cost, capacity) == tokens,
{}

/// The tokens after `n` acquires in a row, or `None` once one is refused.
pub open spec fn acquire_n(tokens: nat, cost: nat, n: nat) -> Option<nat>
    decreases n,
{
    if n == 0 {
        Some(tokens)
    } else {
        match acquire_n(tokens, cost, (n - 1) as nat) {
            Some(left) => if left >= cost {
                Some((left - cost) as nat)
            } else {
                None
            },
            None => None,
        }
    }
}

/// `n` acquires in a row succeed exactly when they cost at most the tokens,
/// and they leave exactly the rest.
pub proof fn acquire_n_is_exact(tokens: nat, cost: nat, n: nat)
    ensures
        acquire_n(tokens, cost, n) == (if n * cost <= tokens {
            Some((tokens - n * cost) as nat)
        } else {
            None::<nat>
        }),
    decreases n,
{
    if n == 0 {
        assert(n * cost == 0) by (nonlinear_arith)
            requires
                n == 0,
        ;
    } else {
        acquire_n_is_exact(tokens, cost, (n - 1) as nat);
        assert(n * cost == (n - 1) * cost + cost) by (nonlinear_arith)
            requires
                n > 0,
        ;
    }
}

/// With no refill, a full bucket grants exactly `capacity / cost` retries:
/// the next one is refused. The integration test
/// `a_full_bucket_allows_capacity_over_cost_retries` checks the same count
/// for the default config.
pub proof fn a_full_bucket_grants_capacity_over_cost(capacity: nat, cost: nat)
    requires
        cost > 0,
    ensures
        acquire_n(capacity, cost, capacity / cost).is_some(),
        acquire_n(capacity, cost, capacity / cost + 1).is_none(),
{
    let q = capacity / cost;
    let c = capacity as int;
    let d = cost as int;
    lemma_fundamental_div_mod(c, d);
    lemma_mod_bound(c, d);
    let r = c % d;
    assert(c / d == q as int);
    assert(q * cost <= capacity) by (nonlinear_arith)
        requires
            c == d * (q as int) + r,
            r >= 0,
            c == capacity,
            d == cost,
    ;
    assert((q + 1) * cost > capacity) by (nonlinear_arith)
        requires
            c == d * (q as int) + r,
            r < d,
            c == capacity,
            d == cost,
    ;
    acquire_n_is_exact(capacity, cost, q);
    acquire_n_is_exact(capacity, cost, q + 1);
}

fn main() {}

} // verus!
