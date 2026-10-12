use vstd::prelude::*;

verus! {

// Half-open accounting of the circuit breaker in
// `autumn/src/circuit_breaker.rs`.
//
// `state` is 0 for `Closed`, 1 for `Open` and 2 for `HalfOpen`. Time is not
// modelled: `half_open_if_due` is the step that the clock allows, and `trip`
// says that the closed window crossed a threshold. The runtime takes each step
// under one lock, so one step here is one critical section there.
// `tests/shuttle_models.rs` runs the real code under concurrent callers.

pub struct Breaker {
    pub state: int,
    pub successes: nat,
    pub in_flight: nat,
    pub trials: nat,
    pub generation: nat,
}

pub open spec fn closed() -> int { 0 }
pub open spec fn open() -> int { 1 }
pub open spec fn half_open() -> int { 2 }

/// The invariant. In the half-open state, the successful trials plus the
/// running trials never exceed `trials`, and the breaker has not yet seen
/// enough successes to close.
pub open spec fn inv(b: Breaker) -> bool {
    &&& 0 <= b.state <= 2
    &&& b.trials >= 1
    &&& (b.state == half_open() ==> b.successes + b.in_flight <= b.trials)
    &&& (b.state == half_open() ==> b.successes < b.trials)
}

/// `CircuitBreakerInner::half_open_if_due`, once `open_until` has passed.
pub open spec fn half_open_if_due(b: Breaker) -> Breaker {
    if b.state == open() {
        Breaker {
            state: half_open(),
            successes: 0,
            in_flight: 0,
            trials: b.trials,
            generation: b.generation + 1,
        }
    } else {
        b
    }
}

/// `CircuitBreaker::before_call`: the new state, and the generation of an
/// admitted call (`None` when the call is refused).
pub open spec fn before_call(b: Breaker) -> (Breaker, Option<nat>) {
    if b.state == open() {
        (b, None)
    } else if b.state == half_open() {
        if b.successes + b.in_flight >= b.trials {
            (b, None)
        } else {
            (
                Breaker {
                    state: b.state,
                    successes: b.successes,
                    in_flight: b.in_flight + 1,
                    trials: b.trials,
                    generation: b.generation,
                },
                Some(b.generation),
            )
        }
    } else {
        (b, Some(b.generation))
    }
}

/// `CircuitBreakerInner::record` for a call admitted in `generation`. A
/// failure here includes a slow trial, which the runtime counts as failed.
pub open spec fn record(b: Breaker, generation: nat, failed: bool, trip: bool) -> Breaker {
    if generation != b.generation {
        b
    } else if b.state == closed() {
        if trip {
            Breaker {
                state: open(),
                successes: b.successes,
                in_flight: b.in_flight,
                trials: b.trials,
                generation: b.generation + 1,
            }
        } else {
            b
        }
    } else if b.state == half_open() {
        let in_flight: nat = if b.in_flight > 0 { (b.in_flight - 1) as nat } else { 0 };
        if failed {
            Breaker {
                state: open(),
                successes: b.successes,
                in_flight,
                trials: b.trials,
                generation: b.generation + 1,
            }
        } else if b.successes + 1 >= b.trials {
            Breaker {
                state: closed(),
                successes: b.successes + 1,
                in_flight,
                trials: b.trials,
                generation: b.generation + 1,
            }
        } else {
            Breaker {
                state: half_open(),
                successes: b.successes + 1,
                in_flight,
                trials: b.trials,
                generation: b.generation,
            }
        }
    } else {
        b
    }
}

/// `CircuitBreaker::cancel_call` for a call that was not slow: it frees the
/// half-open slot and counts nothing.
pub open spec fn cancel(b: Breaker, generation: nat) -> Breaker {
    if b.state == half_open() && generation == b.generation {
        Breaker {
            state: b.state,
            successes: b.successes,
            in_flight: if b.in_flight > 0 { (b.in_flight - 1) as nat } else { 0 },
            trials: b.trials,
            generation: b.generation,
        }
    } else {
        b
    }
}

/// Every step keeps the invariant.
pub proof fn steps_keep_the_invariant(b: Breaker, generation: nat, failed: bool, trip: bool)
    requires
        inv(b),
    ensures
        inv(half_open_if_due(b)),
        inv(before_call(b).0),
        inv(record(b, generation, failed, trip)),
        inv(cancel(b, generation)),
{}

/// A half-open breaker admits a call only while a trial slot is free. Thus,
/// with the invariant, no more than `trials` trials ever run at once.
pub proof fn half_open_admits_at_most_the_trials(b: Breaker)
    requires
        inv(b),
        b.state == half_open(),
    ensures
        before_call(b).1.is_some() ==> before_call(b).0.in_flight <= b.trials,
        before_call(b).1.is_some() ==> b.successes + b.in_flight < b.trials,
{}

/// Liveness of one step: right after the breaker goes half-open, the next
/// call is admitted as a trial, so a recovered dependency can close it.
/// This needs `trials >= 1`: `CircuitBreakerPolicy::clamped` enforces it.
pub proof fn a_fresh_half_open_breaker_admits_a_trial(b: Breaker)
    requires
        inv(b),
        b.state == open(),
    ensures
        before_call(half_open_if_due(b)).1.is_some(),
{}

/// Why the clamp exists. With `trials == 0`, a half-open breaker refuses
/// every call, so no trial can ever close it.
pub proof fn zero_trials_strand_the_breaker(b: Breaker)
    requires
        b.state == half_open(),
        b.trials == 0,
    ensures
        before_call(b).1.is_none(),
        before_call(b).0 == b,
{}

/// A successful trial in the current generation closes a one-trial breaker.
pub proof fn one_successful_trial_closes(b: Breaker)
    requires
        inv(b),
        b.state == half_open(),
        b.trials == 1,
    ensures
        ({
            let (admitted, generation) = before_call(b);
            generation.is_some() ==> record(admitted, generation.unwrap(), false, false).state
                == closed()
        }),
{}

/// A call admitted in an older generation changes nothing: a call that
/// started before the breaker opened is not a half-open trial, and a late
/// failure cannot reopen a breaker that has closed.
pub proof fn a_stale_call_changes_nothing(b: Breaker, generation: nat, failed: bool, trip: bool)
    requires
        generation != b.generation,
    ensures
        record(b, generation, failed, trip) == b,
        cancel(b, generation) == b,
{}

/// The generation never goes back, so a stale call stays stale.
pub proof fn generation_never_decreases(b: Breaker, generation: nat, failed: bool, trip: bool)
    ensures
        half_open_if_due(b).generation >= b.generation,
        before_call(b).0.generation == b.generation,
        record(b, generation, failed, trip).generation >= b.generation,
        cancel(b, generation).generation == b.generation,
{}

fn main() {}

} // verus!
