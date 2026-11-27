use vstd::prelude::*;

verus! {

// Model of the move of one row between two fleet databases, as
// "Move rows between fleet databases" in `docs/guide/sqlite-fleet.md`
// gives it, on top of the replica bound (`replicated_through.rs`).
//
// Each database has a local file and a replica. A rewind (a takeover, a
// fail-back, a restore, a new volume, or a restart after a power loss)
// replaces the local file with the replica. A rewind of the destination
// starts a new file, at the time `since`. It can keep the epoch. The source
// can rewind too.
//
// A delivery writes the row into the destination. It has a time from the
// clock and the epoch of the destination. The row is in the destination
// when one delivery is in it: a flag for each of the file and the replica
// says so. The source keeps a mark: the time and the
// epoch of its last delivery.
//
// The model is for loss, for one move of one row. Versions and tombstones,
// which stop duplicates, are not in it.

/// The time and the epoch of a delivery.
pub struct Mark {
    pub at: int,
    pub epoch: nat,
}

/// A bound of the destination: its time, its epoch, and the start of its
/// file.
pub struct Bound {
    pub at: int,
    pub epoch: nat,
    pub since: int,
}

pub struct State {
    /// The source copy is in the local file.
    pub src_local: bool,
    /// The source copy is in the source replica.
    pub src_replica: bool,
    /// The mark in the local file and in the replica of the source.
    pub mark_local: Option<Mark>,
    pub mark_replica: Option<Mark>,
    /// The times of the deliveries in the local file and in the replica of
    /// the destination.
    pub dst_local: Set<int>,
    pub dst_replica: Set<int>,
    /// The row is in the local file and in the replica of the destination.
    pub dst_local_holds: bool,
    pub dst_replica_holds: bool,
    /// The epoch of the destination now.
    pub epoch: nat,
    /// When the destination file started.
    pub since: int,
    /// The bound of the destination. `None`: no claim.
    pub bound: Option<Bound>,
    /// The clock.
    pub now: int,
}

/// The row survives every choice of rewinds: for each database, the local
/// file or the replica, one of the two holds the row.
pub open spec fn safe(s: State) -> bool {
    &&& (s.src_local || s.dst_local_holds)
    &&& (s.src_local || s.dst_replica_holds)
    &&& (s.src_replica || s.dst_local_holds)
    &&& (s.src_replica || s.dst_replica_holds)
}

/// A mark of the current epoch and file names a delivery in the local file:
/// no rewind came after it.
pub open spec fn mark_ok(s: State, m: Option<Mark>) -> bool {
    match m {
        Some(m) => m.epoch <= s.epoch && m.at < s.now && (m.epoch == s.epoch && s.since < m.at
            ==> s.dst_local.contains(m.at)),
        None => true,
    }
}

/// The claim of the bound (proved in `replicated_through.rs`): each delivery
/// in the local file at or before it is in the replica.
pub open spec fn bound_ok(s: State) -> bool {
    match s.bound {
        Some(b) => b.epoch == s.epoch && b.since == s.since && b.at < s.now && forall|t: int|
            #[trigger] s.dst_local.contains(t) && t <= b.at ==> s.dst_replica.contains(t),
        None => true,
    }
}

pub open spec fn inv(s: State) -> bool {
    &&& safe(s)
    &&& s.since < s.now
    &&& (s.dst_replica_holds ==> s.dst_local_holds)
    &&& forall|t: int| #[trigger] s.dst_local.contains(t) ==> s.dst_local_holds
    &&& forall|t: int| #[trigger] s.dst_replica.contains(t) ==> s.dst_replica_holds
    &&& mark_ok(s, s.mark_local)
    &&& mark_ok(s, s.mark_replica)
    &&& bound_ok(s)
}

/// The source holds the row. Nothing is delivered yet.
pub open spec fn initial() -> State {
    State {
        src_local: true,
        src_replica: true,
        mark_local: None,
        mark_replica: None,
        dst_local: Set::empty(),
        dst_replica: Set::empty(),
        dst_local_holds: false,
        dst_replica_holds: false,
        epoch: 1,
        since: -1,
        bound: None,
        now: 0,
    }
}

/// Rules 1, 2 and 4: deliver the row (again), then mark the source copy
/// moved. Safe to repeat (rule 5): a second delivery changes nothing but
/// the mark.
pub open spec fn deliver(s: State) -> State {
    State {
        dst_local: s.dst_local.insert(s.now),
        dst_local_holds: true,
        mark_local: Some(Mark { at: s.now, epoch: s.epoch }),
        now: s.now + 1,
        ..s
    }
}

pub open spec fn ship_src(s: State) -> State {
    State { src_replica: s.src_local, mark_replica: s.mark_local, ..s }
}

pub open spec fn ship_dst(s: State) -> State {
    State { dst_replica: s.dst_local, dst_replica_holds: s.dst_local_holds, ..s }
}

/// The destination proves a bound `b`. See `tick_dst_keeps_inv`.
pub open spec fn tick_dst(s: State, b: int) -> State {
    State { bound: Some(Bound { at: b, epoch: s.epoch, since: s.since }), ..s }
}

/// A rewind of the destination: a new file, restored from the replica, with
/// a new epoch or the same one. The file starts at `since`, and the clock
/// moves on. The bound starts again.
pub open spec fn rewind_dst(s: State, new_epoch: bool) -> State {
    State {
        dst_local: s.dst_replica,
        dst_local_holds: s.dst_replica_holds,
        epoch: if new_epoch { s.epoch + 1 } else { s.epoch },
        since: s.now,
        now: s.now + 1,
        bound: None,
        ..s
    }
}

/// A takeover or a fail-back of the source. The copy and the mark come back
/// as the replica holds them.
pub open spec fn rewind_src(s: State) -> State {
    State { src_local: s.src_replica, mark_local: s.mark_replica, ..s }
}

/// Rule 3: delete the source copy only when the bound covers the mark, the
/// bound and the mark have one epoch, and the mark is newer than the file
/// (`ReplicaBound::coverage` is `Covered`).
pub open spec fn may_delete(s: State) -> bool {
    match s.mark_local {
        Some(m) => match s.bound {
            Some(b) => m.epoch == b.epoch && b.since < m.at && m.at <= b.at,
            None => false,
        },
        None => false,
    }
}

/// The check of the time and the epoch, with no start of the file.
pub open spec fn epoch_covers(s: State) -> bool {
    match s.mark_local {
        Some(m) => match s.bound {
            Some(b) => m.epoch == b.epoch && m.at <= b.at,
            None => false,
        },
        None => false,
    }
}

/// The check of the time alone, with no epoch.
pub open spec fn time_covers(s: State) -> bool {
    match s.mark_local {
        Some(m) => match s.bound {
            Some(b) => m.at <= b.at,
            None => false,
        },
        None => false,
    }
}

pub open spec fn delete_src(s: State) -> State {
    State { src_local: false, ..s }
}

pub proof fn initial_has_inv()
    ensures inv(initial())
{}

pub proof fn deliver_keeps_inv(s: State)
    requires inv(s)
    ensures inv(deliver(s))
{
    let n = deliver(s);
    assert(n.dst_local.contains(s.now));
    match s.bound {
        Some(b) => {
            assert forall|t: int|
                #[trigger] n.dst_local.contains(t) && t <= b.at implies n.dst_replica.contains(t) by {
                if t != s.now {
                    assert(s.dst_local.contains(t));
                }
            }
        },
        None => {},
    }
}

pub proof fn ship_src_keeps_inv(s: State)
    requires inv(s)
    ensures inv(ship_src(s))
{}

pub proof fn ship_dst_keeps_inv(s: State)
    requires inv(s)
    ensures inv(ship_dst(s))
{
    let n = ship_dst(s);
    match s.bound {
        Some(b) => {
            assert forall|t: int|
                #[trigger] n.dst_local.contains(t) && t <= b.at implies n.dst_replica.contains(t) by {}
        },
        None => {},
    }
}

/// A tick proves a bound only when each delivery at or before it is in the
/// replica.
pub proof fn tick_dst_keeps_inv(s: State, b: int)
    requires
        inv(s),
        b < s.now,
        forall|t: int| #[trigger] s.dst_local.contains(t) && t <= b ==> s.dst_replica.contains(t),
    ensures inv(tick_dst(s, b))
{}

pub proof fn rewind_dst_keeps_inv(s: State, new_epoch: bool)
    requires inv(s)
    ensures inv(rewind_dst(s, new_epoch))
{}

pub proof fn rewind_src_keeps_inv(s: State)
    requires inv(s)
    ensures inv(rewind_src(s))
{}

/// The rule loses nothing: a delete it allows keeps the row safe.
pub proof fn delete_src_keeps_inv(s: State)
    requires inv(s), may_delete(s)
    ensures inv(delete_src(s))
{
    match s.mark_local {
        Some(m) => match s.bound {
            Some(b) => {
                assert(b.epoch == s.epoch);
                assert(b.since == s.since);
                assert(m.epoch == s.epoch);
                assert(s.since < m.at);
                assert(s.dst_local.contains(m.at));
                assert(s.dst_replica.contains(m.at));
                assert(s.dst_replica_holds);
                assert(s.dst_local_holds);
            },
            None => {},
        },
        None => {},
    }
}

/// After a rewind of the destination, the old mark allows no delete: it is
/// of an older epoch or not newer than the file (`Stale`). Only a new delivery
/// (rule 4) lets the move end.
pub proof fn an_old_mark_allows_no_delete(s: State, b: int)
    requires
        inv(s),
        match s.mark_local {
            Some(m) => m.epoch < s.epoch || m.at <= s.since,
            None => false,
        },
    ensures !may_delete(tick_dst(s, b))
{}

/// A new delivery that ships ends the move.
pub proof fn a_shipped_delivery_allows_the_delete(s: State)
    requires inv(s)
    ensures
        may_delete(tick_dst(ship_dst(deliver(s)), s.now)),
        inv(tick_dst(ship_dst(deliver(s)), s.now)),
{
    deliver_keeps_inv(s);
    ship_dst_keeps_inv(deliver(s));
    tick_dst_keeps_inv(ship_dst(deliver(s)), s.now);
}

/// Delete on acknowledgement: the destination acknowledged the delivery,
/// and the source deleted its copy. A rewind of the destination now loses
/// the row.
pub proof fn a_delete_on_acknowledgement_is_unsafe()
    ensures !safe(delete_src(deliver(initial())))
{
    let s = delete_src(deliver(initial()));
    assert(!s.src_local);
    assert(!s.dst_replica_holds);
}

/// A bound of the new epoch covers the time of a delivery that the rewind
/// lost. A check of the time alone deletes the source copy.
pub open spec fn after_a_rewind() -> State {
    tick_dst(rewind_dst(deliver(initial()), true), 0)
}

pub proof fn a_time_only_check_is_unsafe()
    ensures
        inv(after_a_rewind()),
        time_covers(after_a_rewind()),
        !may_delete(after_a_rewind()),
        !safe(delete_src(after_a_rewind())),
{
    initial_has_inv();
    deliver_keeps_inv(initial());
    rewind_dst_keeps_inv(deliver(initial()), true);
    let r = rewind_dst(deliver(initial()), true);
    assert(r.dst_local == Set::<int>::empty());
    tick_dst_keeps_inv(r, 0);
    let s = delete_src(after_a_rewind());
    assert(!s.src_local);
    assert(!s.dst_local_holds);
}

/// A rewind that keeps the epoch: a new volume under the same owner, or a
/// restart after a power loss. A check of the time and the epoch deletes the
/// source copy.
pub open spec fn after_a_same_epoch_rewind() -> State {
    tick_dst(rewind_dst(deliver(initial()), false), 0)
}

pub proof fn an_epoch_only_check_is_unsafe()
    ensures
        inv(after_a_same_epoch_rewind()),
        epoch_covers(after_a_same_epoch_rewind()),
        !may_delete(after_a_same_epoch_rewind()),
        !safe(delete_src(after_a_same_epoch_rewind())),
{
    initial_has_inv();
    deliver_keeps_inv(initial());
    rewind_dst_keeps_inv(deliver(initial()), false);
    let r = rewind_dst(deliver(initial()), false);
    assert(r.dst_local == Set::<int>::empty());
    tick_dst_keeps_inv(r, 0);
    let s = delete_src(after_a_same_epoch_rewind());
    assert(!s.src_local);
    assert(!s.dst_local_holds);
}

fn main() {}

} // verus!
