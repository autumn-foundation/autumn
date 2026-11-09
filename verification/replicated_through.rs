use vstd::prelude::*;

verus! {

// Model of the replica bound `replicated_through` of one replicator
// (`Replicator::tick_inner` and `settled_tail` in
// `autumn/src/replication/engine.rs`, and `ReplicationStatus` in
// `autumn/src/replication/status.rs`).
//
// Times are clock readings, an `int`. A commit has the time the app reads
// after the commit returns. The clock does not step back: each tick reads at
// or after the last tick, and a commit after a tick reads at or after it.
// Commits need not read their times in commit order.

/// One replicator and the commits to its database, in commit order.
pub struct State {
    /// The time of each commit.
    pub commits: Seq<int>,
    /// The commits the replica holds: a prefix of `commits`.
    pub shipped: nat,
    /// The last clock reading of a tick.
    pub clock: int,
    /// The bound. `None`: no claim.
    pub through: Option<int>,
    /// A newer owner fenced the replica.
    pub fenced: bool,
}

/// The claim the bound makes: each commit at or before it is shipped.
pub open spec fn covered_are_shipped(s: State) -> bool {
    match s.through {
        Some(b) => forall|i: int|
            0 <= i < s.commits.len() && #[trigger] s.commits[i] <= b ==> i < s.shipped,
        None => true,
    }
}

pub open spec fn inv(s: State) -> bool {
    &&& s.shipped <= s.commits.len()
    &&& (match s.through {
        Some(b) => b < s.clock,
        None => true,
    })
    &&& covered_are_shipped(s)
    &&& (s.fenced ==> s.through is None)
}

/// No commit yet, no claim.
pub open spec fn initial(c: int) -> State {
    State { commits: Seq::empty(), shipped: 0, clock: c, through: None, fenced: false }
}

/// A commit after the last tick. Its time is at or after that tick's reading.
pub open spec fn commit(s: State, t: int) -> State {
    State { commits: s.commits.push(t), ..s }
}

/// An upload of `n` more commits.
pub open spec fn ship(s: State, n: nat) -> State {
    State {
        shipped: if s.shipped + n <= s.commits.len() { s.shipped + n } else { s.commits.len() },
        ..s
    }
}

/// The caught-up check. The clock is read first (`r`), then the tail is
/// measured. A bound at or above `r` is dropped: the clock stepped back. A
/// caught-up tail stamps one step below `r`: a commit with the same reading
/// may come after the measurement. A commit between the reading and the
/// measurement only makes the tail not caught up, so the model measures at
/// the reading.
pub open spec fn tick(s: State, r: int) -> State {
    let caught_up = s.shipped == s.commits.len();
    let kept = match s.through {
        Some(b) => if b >= r { None } else { s.through },
        None => None,
    };
    State {
        clock: r,
        through: if !s.fenced && caught_up { Some(r - 1) } else { kept },
        ..s
    }
}

/// A new generation: its base snapshot holds every commit. The bound starts
/// again.
pub open spec fn new_generation(s: State) -> State {
    State { shipped: s.commits.len(), through: None, ..s }
}

/// A new epoch (`set_epoch`): the next generation proves its own bound, and
/// the fence of the old epoch no longer applies.
pub open spec fn new_epoch(s: State) -> State {
    State { shipped: s.commits.len(), through: None, fenced: false, ..s }
}

/// A newer owner fenced the replica.
pub open spec fn fence(s: State) -> State {
    State { through: None, fenced: true, ..s }
}

pub proof fn initial_has_inv(c: int)
    ensures inv(initial(c))
{}

pub proof fn commit_keeps_inv(s: State, t: int)
    requires inv(s), t >= s.clock
    ensures inv(commit(s, t))
{
    let n = commit(s, t);
    match s.through {
        Some(b) => {
            assert forall|i: int|
                0 <= i < n.commits.len() && #[trigger] n.commits[i] <= b implies i < n.shipped by {
                if i < s.commits.len() {
                    assert(n.commits[i] == s.commits[i]);
                } else {
                    assert(n.commits[i] == t);
                }
            }
        },
        None => {},
    }
}

pub proof fn ship_keeps_inv(s: State, n: nat)
    requires inv(s)
    ensures inv(ship(s, n))
{}

pub proof fn tick_keeps_inv(s: State, r: int)
    requires inv(s), r >= s.clock
    ensures inv(tick(s, r))
{}

pub proof fn new_generation_keeps_inv(s: State)
    requires inv(s)
    ensures inv(new_generation(s))
{}

pub proof fn new_epoch_keeps_inv(s: State)
    requires inv(s)
    ensures inv(new_epoch(s))
{}

pub proof fn fence_keeps_inv(s: State)
    requires inv(s)
    ensures inv(fence(s))
{}

/// In one generation, a tick never moves the bound back.
pub proof fn tick_never_moves_back(s: State, r: int, a: int)
    requires inv(s), s.through == Some(a), r >= s.clock
    ensures
        match tick(s, r).through {
            Some(b) => b >= a,
            None => false,
        },
{}

/// A tick with an unshipped commit does not advance the bound.
pub proof fn behind_does_not_advance(s: State, r: int)
    requires inv(s), s.shipped < s.commits.len(), r >= s.clock
    ensures tick(s, r).through == s.through
{}

/// A fenced replica claims nothing until a new epoch.
pub proof fn a_fenced_replica_claims_nothing(s: State, r: int)
    requires inv(s), s.fenced
    ensures tick(s, r).through is None
{}

/// Only a new epoch ends a fence.
pub proof fn fenced_is_absorbing(s: State, r: int, t: int, n: nat)
    requires s.fenced
    ensures
        tick(s, r).fenced,
        commit(s, t).fenced,
        ship(s, n).fenced,
        new_generation(s).fenced,
        fence(s).fenced,
{}

/// The stamp the old order took: the tail is measured, a commit lands, and
/// only then is the clock read. Even one step below that reading, it covers
/// a commit that is not shipped.
pub open spec fn late_stamp(s: State, t: int, r: int) -> State {
    let caught_up = s.shipped == s.commits.len();
    let after = commit(s, t);
    State {
        clock: r,
        through: if caught_up { Some(r - 1) } else { after.through },
        ..after
    }
}

pub proof fn the_late_stamp_is_unsound()
    ensures !covered_are_shipped(late_stamp(initial(0), 1, 2))
{
    let s = late_stamp(initial(0), 1, 2);
    assert(s.commits.len() == 1);
    assert(s.commits[0] == 1);
    assert(s.through == Some(1int));
}

fn main() {}

} // verus!
