use vstd::prelude::*;

verus! {

// Model of the replica bound `replicated_through` of one replicator
// (`Replicator::tick_inner` and `settled_tail` in
// `autumn/src/replication/engine.rs`, and `ReplicationStatus` in
// `autumn/src/replication/status.rs`).
//
// Times are clock readings, an `int`. A commit has the time the app reads
// after the commit returns. The clock does not step back.

/// One replicator and the commits to its database, in commit order.
pub struct State {
    /// The time of each commit.
    pub commits: Seq<int>,
    /// The commits the replica holds: a prefix of `commits`.
    pub shipped: nat,
    /// The last clock reading.
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
    &&& forall|i: int| 0 <= i < s.commits.len() ==> #[trigger] s.commits[i] <= s.clock
    &&& match s.through {
        Some(b) => b < s.clock,
        None => true,
    }
    &&& covered_are_shipped(s)
    &&& (s.fenced ==> s.through is None)
}

/// A commit. The app reads the clock after it returns.
pub open spec fn commit(s: State, t: int) -> State {
    State { commits: s.commits.push(t), clock: t, ..s }
}

/// An upload of `n` more commits.
pub open spec fn ship(s: State, n: nat) -> State {
    State {
        shipped: if s.shipped + n <= s.commits.len() { s.shipped + n } else { s.commits.len() },
        ..s
    }
}

pub open spec fn max(a: int, b: int) -> int {
    if a >= b { a } else { b }
}

/// The caught-up check. The clock is read first (`r`), then the tail is
/// measured. The stamp is one step below `r`: a commit with the same reading
/// may come after the measurement. A commit between the reading and the
/// measurement only makes the tail not caught up, so the model measures at
/// the reading.
pub open spec fn tick(s: State, r: int) -> State {
    let caught_up = s.shipped == s.commits.len();
    State {
        clock: r,
        through: if !s.fenced && caught_up {
            match s.through {
                Some(b) => Some(max(b, r - 1)),
                None => Some(r - 1),
            }
        } else {
            s.through
        },
        ..s
    }
}

/// A new generation: its base snapshot holds every commit. The bound starts
/// again.
pub open spec fn new_generation(s: State) -> State {
    State { shipped: s.commits.len(), through: None, ..s }
}

/// A newer owner fenced the replica.
pub open spec fn fence(s: State) -> State {
    State { through: None, fenced: true, ..s }
}

pub proof fn commit_keeps_inv(s: State, t: int)
    requires inv(s), t >= s.clock
    ensures inv(commit(s, t))
{
    let n = commit(s, t);
    assert forall|i: int| 0 <= i < n.commits.len() implies #[trigger] n.commits[i] <= n.clock by {
        if i < s.commits.len() {
            assert(n.commits[i] == s.commits[i]);
        }
    }
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

pub proof fn fence_keeps_inv(s: State)
    requires inv(s)
    ensures inv(fence(s))
{}

/// In one generation, a tick never moves the bound back.
pub proof fn tick_never_moves_back(s: State, r: int, a: int)
    requires s.through == Some(a), r >= s.clock
    ensures
        match tick(s, r).through {
            Some(b) => b >= a,
            None => false,
        },
{}

/// A commit or an upload leaves the bound as it is.
pub proof fn commit_and_ship_keep_the_bound(s: State, t: int, n: nat)
    ensures commit(s, t).through == s.through, ship(s, n).through == s.through
{}

/// A fenced replica claims nothing, whatever happens next.
pub proof fn a_fenced_replica_claims_nothing(s: State, r: int, t: int, n: nat)
    requires inv(s), s.fenced
    ensures
        s.through is None,
        tick(s, r).through is None,
        commit(s, t).through is None,
        ship(s, n).through is None,
        new_generation(s).through is None,
{}

/// A tick with an unshipped commit does not advance the bound.
pub proof fn behind_does_not_advance(s: State, r: int)
    requires s.shipped < s.commits.len()
    ensures tick(s, r).through == s.through
{}

/// The stamp the old order took: the tail is measured, a commit lands, and
/// only then is the clock read. It covers a commit that is not shipped.
pub open spec fn late_stamp(s: State, t: int, r: int) -> State {
    let caught_up = s.shipped == s.commits.len();
    let after = commit(s, t);
    State {
        clock: r,
        through: if caught_up { Some(r) } else { after.through },
        ..after
    }
}

pub proof fn the_late_stamp_is_unsound()
    ensures
        !covered_are_shipped(
            late_stamp(State { commits: Seq::empty(), shipped: 0, clock: 0, through: None, fenced: false }, 1, 1),
        ),
{
    let s = late_stamp(
        State { commits: Seq::empty(), shipped: 0, clock: 0, through: None, fenced: false },
        1,
        1,
    );
    assert(s.commits.len() == 1);
    assert(s.commits[0] == 1);
    assert(s.through == Some(1int));
}

fn main() {}

} // verus!
