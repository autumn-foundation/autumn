use vstd::arithmetic::power2::{lemma_pow2_pos, pow2};
use vstd::prelude::*;

verus! {

// Model of the replica tombstones of a SQLite fleet (issue #3235). Each spec
// fn names the rule it models in `autumn/src/db/fleet_tombstones.rs`.
//
// The model shows one database key. A row is the tombstone on the control
// database. `None` means "no row". Times are epoch milliseconds.

/// One tombstone row.
pub struct Tombstone {
    /// When the database was deleted. The purge cutoff.
    pub deleted_at: int,
    /// `false` while the delete is still removing the file.
    pub confirmed: bool,
    /// The earliest time of a purge: `deleted_at + retention`.
    pub purge_after: int,
    pub attempts: nat,
    /// The earliest time of the next purge attempt.
    pub next_attempt: int,
}

pub enum Op {
    /// `delete` starts: the row is written before any file is removed. An
    /// existing row stays as it is.
    Record { now: int },
    /// The file is gone (`delete`, or the retry task after a crash).
    Confirm { now: int },
    /// The delete removed nothing: a pending row goes. A confirmed row is
    /// an older delete's, and it stays.
    Refused,
    /// A purge removed the replica.
    PurgeOk,
    /// A purge failed at `now`.
    PurgeFailed { now: int },
    /// An explicit restore brought the database back.
    Restore,
    /// A new database with the same key. `purged`: the old replica is gone.
    Recreate { purged: bool },
}

/// `retry_delay_ms`: `min(cap, base * 2^attempts)`.
pub open spec fn delay(base: nat, cap: nat, attempts: nat) -> nat {
    if base * pow2(attempts) < cap {
        base * pow2(attempts)
    } else {
        cap
    }
}

/// `Tombstone::new` and `Tombstone::confirmed_at`.
pub open spec fn fresh(now: int, retention: nat, confirmed: bool) -> Tombstone {
    Tombstone {
        deleted_at: now,
        confirmed,
        purge_after: now + retention,
        attempts: 0,
        next_attempt: 0,
    }
}

/// `Tombstone::after_failure`.
pub open spec fn after_failure(t: Tombstone, now: int, base: nat, cap: nat) -> Tombstone {
    Tombstone {
        attempts: t.attempts + 1,
        next_attempt: now + delay(base, cap, t.attempts),
        ..t
    }
}

/// `Tombstone::is_due`. A pending row is never due: the delete still runs.
pub open spec fn is_due(t: Tombstone, now: int) -> bool {
    t.confirmed && now >= t.purge_after && now >= t.next_attempt
}

/// `restore_allowed`: no row, and every marker is older than the newest
/// generation.
pub open spec fn restore_allowed(row: Option<Tombstone>, marker: Option<int>, newest: int) -> bool {
    match row {
        Some(_) => false,
        None => match marker {
            Some(deleted_at) => newest > deleted_at,
            None => true,
        },
    }
}

/// `purge_deletes`: a generation object goes when it is not newer than the
/// cutoff. Any other object goes only when no newer generation exists.
pub open spec fn purge_deletes(generation: Option<int>, cutoff: int, newer_exists: bool) -> bool {
    match generation {
        Some(created) => created <= cutoff,
        None => !newer_exists,
    }
}

pub open spec fn apply(
    row: Option<Tombstone>,
    op: Op,
    retention: nat,
    base: nat,
    cap: nat,
) -> Option<Tombstone> {
    match op {
        Op::Record { now } => match row {
            Some(_) => row,
            None => Some(fresh(now, retention, false)),
        },
        Op::Confirm { now } => match row {
            Some(t) => if t.confirmed {
                row
            } else {
                Some(fresh(now, retention, true))
            },
            None => Some(fresh(now, retention, true)),
        },
        Op::Refused => match row {
            Some(t) => if t.confirmed {
                row
            } else {
                None
            },
            None => None,
        },
        Op::PurgeOk => None,
        Op::PurgeFailed { now } => match row {
            Some(t) => Some(after_failure(t, now, base, cap)),
            None => None,
        },
        Op::Restore => None,
        Op::Recreate { purged } => if purged {
            None
        } else {
            row
        },
    }
}

/// The row invariant: the purge time is the delete time plus the retention.
pub open spec fn inv(row: Option<Tombstone>, retention: nat) -> bool {
    match row {
        Some(t) => t.purge_after == t.deleted_at + retention,
        None => true,
    }
}

// ---- Proofs ----

/// The backoff is positive and capped.
pub proof fn delay_bounds(base: nat, cap: nat, attempts: nat)
    requires
        base > 0,
        cap > 0,
    ensures
        0 < delay(base, cap, attempts) <= cap,
{
    lemma_pow2_pos(attempts);
    assert(base * pow2(attempts) > 0) by (nonlinear_arith)
        requires
            base > 0,
            pow2(attempts) > 0,
    ;
}

/// Every op keeps the invariant.
pub proof fn apply_keeps_inv(row: Option<Tombstone>, op: Op, retention: nat, base: nat, cap: nat)
    requires
        inv(row, retention),
    ensures
        inv(apply(row, op, retention, base, cap), retention),
{
}

/// No resurrection: while a row exists, `restore_missing` restores nothing.
pub proof fn a_row_blocks_restore(t: Tombstone, marker: Option<int>, newest: int)
    ensures
        !restore_allowed(Some(t), marker, newest),
{
}

/// No resurrection across hosts: a marker blocks every generation that is
/// not newer than the delete.
pub proof fn a_marker_blocks_old_generations(deleted_at: int, newest: int)
    requires
        newest <= deleted_at,
    ensures
        !restore_allowed(None, Some(deleted_at), newest),
{
}

/// A database made again after the delete restores.
pub proof fn a_newer_generation_restores(deleted_at: int, newest: int)
    requires
        newest > deleted_at,
    ensures
        restore_allowed(None, Some(deleted_at), newest),
{
}

/// A failed purge never loses the row, and it waits before the next try.
pub proof fn a_failure_keeps_the_row(t: Tombstone, now: int, retention: nat, base: nat, cap: nat)
    requires
        base > 0,
        cap > 0,
    ensures
        apply(Some(t), Op::PurgeFailed { now }, retention, base, cap) is Some,
        after_failure(t, now, base, cap).next_attempt > now,
        after_failure(t, now, base, cap).next_attempt <= now + cap,
        !is_due(after_failure(t, now, base, cap), now),
{
    delay_bounds(base, cap, t.attempts);
}

/// The row goes only on a purge, a refused delete, a restore or a
/// re-create that purged.
pub proof fn only_these_ops_drop_the_row(t: Tombstone, op: Op, retention: nat, base: nat, cap: nat)
    requires
        apply(Some(t), op, retention, base, cap) is None,
    ensures
        match op {
            Op::PurgeOk | Op::Restore => true,
            Op::Refused => !t.confirmed,
            Op::Recreate { purged } => purged,
            _ => false,
        },
{
}

/// A delete that finds no file keeps the tombstone of an older delete.
pub proof fn a_second_delete_keeps_a_confirmed_row(t: Tombstone, now: int, retention: nat, base: nat, cap: nat)
    requires
        t.confirmed,
    ensures
        apply(apply(Some(t), Op::Record { now }, retention, base, cap), Op::Refused, retention, base, cap)
            == Some(t),
{
}

/// The retention is kept: a row is never due before `deleted_at + retention`.
pub proof fn retention_is_kept(t: Tombstone, now: int, retention: nat)
    requires
        inv(Some(t), retention),
        is_due(t, now),
    ensures
        now >= t.deleted_at + retention,
{
}

/// A purge never runs while the delete removes the file.
pub proof fn a_pending_row_is_never_due(now: int, retention: nat)
    ensures
        !is_due(fresh(now, retention, false), now + retention),
{
}

/// A confirm keeps a confirmed row: a second confirm does not move the
/// cutoff past a re-created database.
pub proof fn confirm_is_idempotent(t: Tombstone, now: int, retention: nat, base: nat, cap: nat)
    requires
        t.confirmed,
    ensures
        apply(Some(t), Op::Confirm { now }, retention, base, cap) == Some(t),
{
}

/// Live data is safe: a purge never deletes a generation newer than the
/// cutoff.
pub proof fn a_purge_keeps_newer_generations(created: int, cutoff: int, newer_exists: bool)
    requires
        created > cutoff,
    ensures
        !purge_deletes(Some(created), cutoff, newer_exists),
{
}

/// A purge with no newer generation deletes every object.
pub proof fn a_purge_with_no_newer_generation_is_complete(generation: Option<int>, cutoff: int)
    requires
        match generation {
            Some(created) => created <= cutoff,
            None => true,
        },
    ensures
        purge_deletes(generation, cutoff, false),
{
}

/// A purge keeps the epoch markers of a newer generation.
pub proof fn a_purge_keeps_shared_objects_for_a_newer_generation(cutoff: int)
    ensures
        !purge_deletes(None, cutoff, true),
{
}

fn main() {}

} // verus!
