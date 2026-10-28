use vstd::arithmetic::div_mod::lemma_mod_bound;
use vstd::arithmetic::power2::{
    lemma2_to64,
    lemma_pow2_pos,
    lemma_pow2_strictly_increases,
    lemma_pow2_unfold,
    pow2,
};
use vstd::bits::{lemma_u64_pow2_no_overflow, lemma_u64_shl_is_mul};
use vstd::prelude::*;

verus! {

// Capped exponential backoff with jitter (issue #3054).
//
// `ceiling_ms` and `full_jitter_ms` are in `autumn/src/backoff.rs`; the outbox
// relay's equal jitter is `retry_delay_ms` in `autumn/src/outbox.rs`. The
// executable functions here use the runtime's `u64` arithmetic. Kani checks
// the runtime functions themselves (`cargo kani -p autumn-web --lib`); this
// file proves the closed forms that those harnesses compare against.

/// `min(cap, base * 2^retry)`, with no bound on the product.
pub open spec fn ceiling(base: nat, cap: nat, retry: nat) -> nat {
    if base * pow2(retry) < cap {
        base * pow2(retry)
    } else {
        cap
    }
}

/// `ceiling_ms`: a shift for `2^retry`, a saturating multiply, then the cap.
/// The runtime takes `retry: u32`; a `u64` here covers every `u32`.
pub fn ceiling_ms(base: u64, cap: u64, retry: u64) -> (r: u64)
    ensures
        r as nat == ceiling(base as nat, cap as nat, retry as nat),
        r <= cap,
{
    let factor: u64 = if retry >= 64 {
        u64::MAX
    } else {
        1u64 << retry
    };
    proof {
        if retry < 64 {
            lemma_u64_pow2_no_overflow(retry as nat);
            lemma_u64_shl_is_mul(1, retry);
            assert(factor as nat == pow2(retry as nat));
        } else {
            lemma2_to64();
            if retry > 64 {
                lemma_pow2_strictly_increases(64, retry as nat);
            }
            assert(pow2(retry as nat) > u64::MAX);
        }
        lemma_pow2_pos(retry as nat);
    }
    // `base.saturating_mul(factor)`.
    let p = Ghost(pow2(retry as nat));
    let raw: u64 = if base > u64::MAX / factor {
        proof {
            assert(base as nat * factor as nat > u64::MAX) by (nonlinear_arith)
                requires
                    base > u64::MAX / factor,
                    factor > 0,
            ;
            if retry >= 64 {
                assert(base as nat * p@ > u64::MAX) by (nonlinear_arith)
                    requires
                        base >= 1,
                        p@ > u64::MAX,
                ;
            }
        }
        u64::MAX
    } else {
        proof {
            assert(base as nat * factor as nat <= u64::MAX) by (nonlinear_arith)
                requires
                    base <= u64::MAX / factor,
                    factor > 0,
            ;
            if retry >= 64 {
                // `base <= 1` here, as `u64::MAX / u64::MAX == 1`.
                assert(base <= 1);
                if base == 0 {
                    assert(base as nat * p@ == 0);
                } else {
                    assert(base as nat * p@ == p@);
                }
            }
        }
        base * factor
    };
    if raw < cap {
        raw
    } else {
        cap
    }
}

/// The ceiling never decreases from one retry to the next.
pub proof fn ceiling_is_monotonic(base: nat, cap: nat, retry: nat)
    ensures
        ceiling(base, cap, retry) <= ceiling(base, cap, retry + 1),
{
    lemma_pow2_unfold(retry + 1);
    assert(base * pow2(retry) <= base * pow2(retry + 1)) by (nonlinear_arith)
        requires
            pow2(retry + 1) == 2 * pow2(retry),
    ;
}

/// Past the cap, the ceiling stays at the cap: retries beyond that point all
/// draw from the same window.
pub proof fn ceiling_stays_at_the_cap(base: nat, cap: nat, retry: nat, later: nat)
    requires
        retry <= later,
        ceiling(base, cap, retry) == cap,
    ensures
        ceiling(base, cap, later) == cap,
    decreases later - retry,
{
    if retry < later {
        ceiling_is_monotonic(base, cap, retry);
        ceiling_stays_at_the_cap(base, cap, retry + 1, later);
    }
}

/// `full_jitter_ms` after the ceiling: a draw in `[0, ceiling]`, and no
/// overflow at `ceiling == u64::MAX`.
pub fn full_jitter_ms(ceiling: u64, draw: u64) -> (r: u64)
    ensures
        r <= ceiling,
{
    if ceiling == u64::MAX {
        draw
    } else {
        proof {
            lemma_mod_bound(draw as int, ceiling as int + 1);
        }
        draw % (ceiling + 1)
    }
}

/// The outbox relay's equal jitter: a draw in `[delay / 2, delay]`. Neither
/// `delay - half + 1` nor `half + draw % span` can overflow.
pub fn equal_jitter_ms(delay: u64, draw: u64) -> (r: u64)
    ensures
        delay / 2 <= r <= delay,
{
    let half = delay / 2;
    let span = delay - half + 1;
    proof {
        lemma_mod_bound(draw as int, span as int);
    }
    half + draw % span
}

fn main() {}

} // verus!
