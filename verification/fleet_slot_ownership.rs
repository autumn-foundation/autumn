use vstd::prelude::*;

verus! {

// Model of the slot-ownership table of a SQLite fleet (issue #3233). Each spec
// fn names the decision it models in `autumn/src/db/fleet_ownership.rs`
// (`OwnershipTable`).
//
// The model shows one slot. A claim of a range changes every slot in the range
// in the same way, so one slot is enough. `last` is the global epoch counter:
// a claim of another range also bumps it.

/// The ownership of one slot. `owner == 0` means "no row covers the slot".
pub struct Slot {
    pub owner: nat,
    pub epoch: nat,
    pub expires_at: int,
}

/// The slot and the global counter (the highest epoch issued).
pub struct State {
    pub slot: Slot,
    pub last: nat,
}

pub enum Op {
    /// A claim of a range that holds the slot. `keep` is the re-claim of a
    /// row with the same owner and the same bounds: the epoch stays.
    Claim { owner: nat, now: int, ttl: nat, keep: bool },
    /// A claim of a range that does not hold the slot.
    ClaimElsewhere,
    /// A claim that removes the row of the slot (a re-shaped range).
    Clear { owner: nat, now: int },
    Renew { owner: nat, epoch: nat, now: int, ttl: nat },
    Release { owner: nat, epoch: nat, now: int },
}

/// The table invariant: no slot has an epoch above the counter.
pub open spec fn inv(s: State) -> bool {
    s.slot.epoch <= s.last
}

/// `OwnershipTable::claim`: free, the claimer's own row, or a lapsed lease.
pub open spec fn can_claim(slot: Slot, owner: nat, now: int) -> bool {
    owner > 0 && (slot.owner == 0 || slot.owner == owner || slot.expires_at <= now)
}

/// `OwnershipTable::renew` and `release`: the row and the epoch must match.
pub open spec fn holds(slot: Slot, owner: nat, epoch: nat) -> bool {
    slot.owner != 0 && slot.owner == owner && slot.epoch == epoch
}

/// `OwnershipTable::admits`: a write under `epoch` by `owner` is current.
pub open spec fn admits(slot: Slot, owner: nat, epoch: nat) -> bool {
    holds(slot, owner, epoch)
}

/// True when `op` grants a new epoch for the slot.
pub open spec fn grants(s: State, op: Op) -> bool {
    match op {
        Op::Claim { owner, now, ttl, keep } =>
            can_claim(s.slot, owner, now) && !(keep && s.slot.owner == owner),
        _ => false,
    }
}

pub open spec fn apply(s: State, op: Op) -> State {
    match op {
        Op::Claim { owner, now, ttl, keep } => if !can_claim(s.slot, owner, now) {
            s
        } else if keep && s.slot.owner == owner {
            State { slot: Slot { expires_at: now + ttl, ..s.slot }, ..s }
        } else {
            State {
                slot: Slot { owner, epoch: s.last + 1, expires_at: now + ttl },
                last: s.last + 1,
            }
        },
        Op::ClaimElsewhere => State { last: s.last + 1, ..s },
        Op::Clear { owner, now } => if can_claim(s.slot, owner, now) {
            State { slot: Slot { owner: 0, ..s.slot }, ..s }
        } else {
            s
        },
        Op::Renew { owner, epoch, now, ttl } => if holds(s.slot, owner, epoch) {
            State { slot: Slot { expires_at: now + ttl, ..s.slot }, ..s }
        } else {
            s
        },
        Op::Release { owner, epoch, now } => if holds(s.slot, owner, epoch) {
            State { slot: Slot { expires_at: now, ..s.slot }, ..s }
        } else {
            s
        },
    }
}

pub open spec fn run(s: State, ops: Seq<Op>) -> State
    decreases ops.len()
{
    if ops.len() == 0 {
        s
    } else {
        apply(run(s, ops.drop_last()), ops.last())
    }
}

/// The epoch the `k`-th op grants, if it grants one.
pub open spec fn epoch_at(s: State, ops: Seq<Op>, k: int) -> nat {
    run(s, ops.take(k + 1)).slot.epoch
}

// ---- Proofs on one op ----

/// Every op keeps the invariant.
pub proof fn apply_keeps_inv(s: State, op: Op)
    requires inv(s)
    ensures inv(apply(s, op))
{}

/// The counter never goes down.
pub proof fn apply_never_decreases_last(s: State, op: Op)
    ensures apply(s, op).last >= s.last
{}

/// The epoch of a slot never goes down.
pub proof fn apply_never_decreases_epoch(s: State, op: Op)
    requires inv(s)
    ensures apply(s, op).slot.epoch >= s.slot.epoch
{}

/// A new epoch is above every epoch issued before it.
pub proof fn grant_is_above_the_counter(s: State, op: Op)
    requires grants(s, op)
    ensures
        apply(s, op).slot.epoch == s.last + 1,
        apply(s, op).last == s.last + 1,
{}

/// Two hosts race: a live lease of another owner blocks the claim.
pub proof fn live_lease_blocks_claim(s: State, owner: nat, now: int, ttl: nat, keep: bool)
    requires
        s.slot.owner != 0,
        s.slot.owner != owner,
        s.slot.expires_at > now,
    ensures
        apply(s, Op::Claim { owner, now, ttl, keep }) == s,
        apply(s, Op::Clear { owner, now }) == s,
{}

/// A lapsed lease can be claimed by another owner, at a new epoch.
pub proof fn lapsed_lease_can_be_claimed(s: State, owner: nat, now: int, ttl: nat, keep: bool)
    requires
        inv(s),
        owner > 0,
        s.slot.owner != owner,
        s.slot.expires_at <= now,
    ensures
        apply(s, Op::Claim { owner, now, ttl, keep }).slot.owner == owner,
        apply(s, Op::Claim { owner, now, ttl, keep }).slot.epoch > s.slot.epoch,
{}

/// The old owner after a new claim: it cannot renew, release or write.
pub proof fn old_owner_is_fenced(
    s: State, old: nat, old_epoch: nat, now: int, ttl: nat)
    requires
        inv(s),
        old_epoch <= s.last,
        s.slot.epoch == s.last,
        s.slot.owner != old || s.slot.epoch != old_epoch,
    ensures
        !admits(s.slot, old, old_epoch),
        apply(s, Op::Renew { owner: old, epoch: old_epoch, now, ttl }) == s,
        apply(s, Op::Release { owner: old, epoch: old_epoch, now }) == s,
{}

/// A stale epoch is never current.
pub proof fn stale_epoch_is_not_admitted(slot: Slot, owner: nat, stale: nat)
    requires stale < slot.epoch
    ensures !admits(slot, owner, stale)
{}

// ---- Proofs on a sequence of ops ----

proof fn take_step(ops: Seq<Op>, k: int)
    requires 0 <= k < ops.len()
    ensures ops.take(k + 1).drop_last() == ops.take(k), ops.take(k + 1).last() == ops[k]
{
    assert(ops.take(k + 1).drop_last() =~= ops.take(k));
}

/// The invariant holds along every run.
pub proof fn run_keeps_inv(s: State, ops: Seq<Op>)
    requires inv(s)
    ensures inv(run(s, ops))
    decreases ops.len()
{
    if ops.len() > 0 {
        run_keeps_inv(s, ops.drop_last());
        apply_keeps_inv(run(s, ops.drop_last()), ops.last());
    }
}

/// The counter never goes down along a run.
pub proof fn run_last_is_monotonic(s: State, ops: Seq<Op>, i: int, j: int)
    requires 0 <= i <= j <= ops.len()
    ensures run(s, ops.take(i)).last <= run(s, ops.take(j)).last
    decreases j - i
{
    if i < j {
        run_last_is_monotonic(s, ops, i, j - 1);
        take_step(ops, j - 1);
        apply_never_decreases_last(run(s, ops.take(j - 1)), ops[j - 1]);
    }
}

/// Epochs strictly increase: a later grant gets a larger epoch.
pub proof fn epochs_strictly_increase(s: State, ops: Seq<Op>, i: int, j: int)
    requires
        inv(s),
        0 <= i < j < ops.len(),
        grants(run(s, ops.take(i)), ops[i]),
        grants(run(s, ops.take(j)), ops[j]),
    ensures epoch_at(s, ops, i) < epoch_at(s, ops, j)
{
    take_step(ops, i);
    take_step(ops, j);
    grant_is_above_the_counter(run(s, ops.take(i)), ops[i]);
    grant_is_above_the_counter(run(s, ops.take(j)), ops[j]);
    run_last_is_monotonic(s, ops, i + 1, j);
}

/// Two grants never share an epoch.
pub proof fn grants_have_unique_epochs(s: State, ops: Seq<Op>, i: int, j: int)
    requires
        inv(s),
        0 <= i < ops.len(),
        0 <= j < ops.len(),
        i != j,
        grants(run(s, ops.take(i)), ops[i]),
        grants(run(s, ops.take(j)), ops[j]),
    ensures epoch_at(s, ops, i) != epoch_at(s, ops, j)
{
    if i < j {
        epochs_strictly_increase(s, ops, i, j);
    } else {
        epochs_strictly_increase(s, ops, j, i);
    }
}

// ---- Restore and replicator fencing ----

/// A replica generation: its epoch (0 when it has none) and its time.
pub struct Generation {
    pub epoch: nat,
    pub created_ms: int,
}

/// `restore::plan` picks the greatest (epoch, created_ms).
pub open spec fn newer(a: Generation, b: Generation) -> bool {
    a.epoch > b.epoch || (a.epoch == b.epoch && a.created_ms > b.created_ms)
}

/// A generation of an older epoch never wins, whatever its time.
pub proof fn older_epoch_never_wins(stale: Generation, current: Generation)
    requires stale.epoch < current.epoch
    ensures newer(current, stale), !newer(stale, current)
{}

/// The replicator ships only while no newer epoch is at the destination.
pub open spec fn may_ship(mine: nat, newest_at_destination: nat) -> bool {
    newest_at_destination <= mine
}

/// After a new owner writes its epoch marker, the old replicator stops.
pub proof fn fenced_replicator_stops(old_epoch: nat, new_epoch: nat)
    requires old_epoch < new_epoch
    ensures !may_ship(old_epoch, new_epoch)
{}

fn main() {}

} // verus!
