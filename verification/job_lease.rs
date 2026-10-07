use vstd::prelude::*;

verus! {

// Durable job claim lease (issue #3051, ADR 0016).
//
// Time is one clock: the database clock (Postgres), the app clock (SQLite),
// or Redis `TIME` (Redis). Owner `0` means "no claim".

pub struct Claim {
    pub owner: nat,
    pub deadline: int,
}

/// Claim a free row for `worker` at `now`.
pub open spec fn claim(worker: nat, now: int, visibility: int) -> Claim {
    Claim { owner: worker, deadline: now + visibility }
}

/// A heartbeat renewal. It moves the deadline only when `worker` still owns
/// the claim (the `claimed_by = $me` guard).
pub open spec fn renew(c: Claim, worker: nat, now: int, visibility: int) -> Claim {
    if worker != 0 && c.owner == worker {
        Claim { owner: c.owner, deadline: now + visibility }
    } else {
        c
    }
}

/// The sweep can recover a claim only after its deadline.
pub open spec fn recoverable(c: Claim, now: int) -> bool {
    c.owner != 0 && c.deadline <= now
}

/// A settle (ack, retry, dead letter) applies only for the owner.
pub open spec fn settle_applies(c: Claim, worker: nat) -> bool {
    worker != 0 && c.owner == worker
}

/// A renewal never changes the owner, and never touches another worker's
/// claim.
pub proof fn renewal_is_guarded_by_owner(c: Claim, worker: nat, now: int, visibility: int)
    ensures
        renew(c, worker, now, visibility).owner == c.owner,
        c.owner != worker ==> renew(c, worker, now, visibility) == c,
{}

/// The heartbeat renews every `interval`, with `3 * interval <= visibility`.
/// If the last renewal was at `renewed_at` and the next one is due by
/// `renewed_at + interval`, the claim is not recoverable before then.
pub proof fn live_heartbeat_claim_is_not_recoverable(
    c: Claim, renewed_at: int, now: int, visibility: int, interval: int)
    requires
        c.owner != 0,
        c.deadline == renewed_at + visibility,
        interval > 0,
        3 * interval <= visibility,
        now <= renewed_at + interval,
    ensures
        !recoverable(c, now),
        c.deadline - now >= 2 * interval,
{}

/// A claim that a renewal moved forward is not recoverable right after it.
pub proof fn renewal_defers_recovery(c: Claim, worker: nat, now: int, visibility: int)
    requires
        settle_applies(c, worker),
        visibility > 0,
    ensures
        !recoverable(renew(c, worker, now, visibility), now),
{}

/// Give-up rule. A renewal that started at `started` wrote the claim at
/// `written >= started`, so the deadline is `written + visibility`. The worker
/// stops at `started + give_up`, with `3 * give_up <= 2 * visibility`. Thus it
/// stops strictly before the claim can be recovered.
pub proof fn worker_gives_up_before_the_claim_expires(
    started: int, written: int, visibility: int, give_up: int)
    requires
        visibility > 0,
        written >= started,
        give_up >= 0,
        3 * give_up <= 2 * visibility,
    ensures
        started + give_up < written + visibility,
{}

/// After a recovery gives the row to another worker, the first worker's
/// settle does not apply. This is the ack guard.
pub proof fn old_owner_cannot_settle_after_recovery(
    old_owner: nat, new_owner: nat, now: int, visibility: int)
    requires
        old_owner != new_owner,
        new_owner != 0,
    ensures
        !settle_applies(claim(new_owner, now, visibility), old_owner),
{}

fn main() {}

} // verus!
