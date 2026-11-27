use vstd::prelude::*;

verus! {

// Model of the fleet lake (issue #3236, ADR 0032). Each spec fn names the
// rule it models in `autumn/src/db/fleet_lake.rs`.
//
// The model has two parts:
// - Which databases the lake loads, and the row count it then holds.
// - Where the lake writes: never under the fleet root.

// ---- Part 1: inclusion and the aggregate ----

/// One tenant database the destination lists.
pub struct Candidate {
    /// The listed name parses as a tenant id.
    pub valid_name: bool,
    /// A complete generation exists in either format: a `snapshot.json` or
    /// a `manifest.json`. See `unreplicated` in part 4.
    pub has_replica: bool,
    /// A tombstone row exists on the control database.
    pub row: bool,
    /// The `deleted_at` of the tombstone marker, if any.
    pub marker: Option<int>,
    /// When the generation the plan takes was created.
    pub newest: int,
    /// Rows of one table in that generation.
    pub rows: nat,
    /// The database is live in the fleet: not deleted.
    pub live: bool,
}

/// `fleet_tombstones::restore_allowed`.
pub open spec fn restore_allowed(row: bool, marker: Option<int>, newest: int) -> bool {
    !row && match marker {
        Some(deleted_at) => newest > deleted_at,
        None => true,
    }
}

/// `decide` with the `restore_allowed` check after the plan in
/// `FleetLake::plan_from`: the lake loads a database only when its name is valid, it
/// has a replica, and the tombstone rule lets a restore take it.
pub open spec fn included(c: Candidate) -> bool {
    c.valid_name && c.has_replica && restore_allowed(c.row, c.marker, c.newest)
}

/// The rows the lake table holds after it loads `s`, in any order.
pub open spec fn lake_rows(s: Seq<Candidate>) -> nat
    decreases s.len(),
{
    if s.len() == 0 {
        0
    } else {
        lake_rows(s.drop_last()) + if included(s.last()) {
            s.last().rows
        } else {
            0
        }
    }
}

/// The rows a cross-tenant read on the volume sees: live databases only.
pub open spec fn live_rows(s: Seq<Candidate>) -> nat
    decreases s.len(),
{
    if s.len() == 0 {
        0
    } else {
        live_rows(s.drop_last()) + if s.last().live {
            s.last().rows
        } else {
            0
        }
    }
}

/// The replica is caught up and agrees with the volume (the test setup):
/// a live database has a valid name, a replica, no row, and no marker
/// newer than its generation. A deleted one has a row, or a marker that
/// covers its generation, or no replica.
pub open spec fn synced(c: Candidate) -> bool {
    &&& c.live ==> (c.valid_name && c.has_replica && restore_allowed(c.row, c.marker, c.newest))
    &&& !c.live ==> (c.row || !c.has_replica || match c.marker {
        Some(deleted_at) => c.newest <= deleted_at,
        None => false,
    })
}

pub open spec fn all_synced(s: Seq<Candidate>) -> bool {
    forall|i: int| 0 <= i < s.len() ==> synced(#[trigger] s[i])
}

/// A tombstoned database adds no rows to the lake.
pub proof fn a_row_excludes(c: Candidate)
    requires
        c.row,
    ensures
        !included(c),
{
}

/// A marker excludes each generation that is not newer than the delete.
pub proof fn a_marker_excludes(c: Candidate, deleted_at: int)
    requires
        c.marker == Some(deleted_at),
        c.newest <= deleted_at,
    ensures
        !included(c),
{
}

/// A database with no replica adds no rows.
pub proof fn no_replica_excludes(c: Candidate)
    requires
        !c.has_replica,
    ensures
        !included(c),
{
}

/// For a synced database, the lake takes it exactly when it is live.
pub proof fn synced_included_iff_live(c: Candidate)
    requires
        synced(c),
    ensures
        included(c) == c.live,
{
}

/// The lake count matches the cross-tenant count on the volume (the AC
/// test).
pub proof fn lake_matches_live(s: Seq<Candidate>)
    requires
        all_synced(s),
    ensures
        lake_rows(s) == live_rows(s),
    decreases s.len(),
{
    if s.len() > 0 {
        let rest = s.drop_last();
        assert forall|i: int| 0 <= i < rest.len() implies synced(#[trigger] rest[i]) by {
            assert(rest[i] == s[i]);
        }
        lake_matches_live(rest);
        assert(synced(s[s.len() - 1]));
        synced_included_iff_live(s.last());
    }
}

/// Loading in parts gives the same total: the workers may hand databases
/// to the loader in any batch order.
pub proof fn lake_rows_append(s: Seq<Candidate>, t: Seq<Candidate>)
    ensures
        lake_rows(s + t) == lake_rows(s) + lake_rows(t),
    decreases t.len(),
{
    if t.len() == 0 {
        assert(s + t =~= s);
    } else {
        lake_rows_append(s, t.drop_last());
        assert((s + t).drop_last() =~= s + t.drop_last());
        assert((s + t).last() == t.last());
    }
}

// ---- Part 2: the lake never writes under the fleet root ----

/// A path as a sequence of components (after canonicalisation).
pub open spec fn under(path: Seq<int>, root: Seq<int>) -> bool {
    root.len() <= path.len() && path.subrange(0, root.len() as int) =~= root
}

/// The scratch directory is a new child `name` of `parent`. `create_dir`
/// fails when the path exists, so `dir != root`, and no existing path,
/// the root included, is under `dir`.
pub open spec fn fresh_dir(parent: Seq<int>, name: int, root: Seq<int>) -> bool {
    let dir = parent.push(name);
    dir != root && !under(root, dir)
}

/// The new directory is not under the root.
pub proof fn fresh_dir_not_under_root(parent: Seq<int>, name: int, root: Seq<int>)
    requires
        !under(parent, root),
        fresh_dir(parent, name, root),
    ensures
        !under(parent.push(name), root),
{
    let dir = parent.push(name);
    if under(dir, root) {
        if root.len() == dir.len() {
            assert(dir.subrange(0, dir.len() as int) =~= dir);
            assert(dir =~= root);
        } else {
            let k = root.len() as int;
            assert(k <= parent.len());
            assert(dir.subrange(0, k) =~= parent.subrange(0, k));
            assert(under(parent, root));
        }
    }
}

/// Every file the lake makes is under its fresh directory, so none is
/// under the root.
pub proof fn lake_files_not_under_root(
    parent: Seq<int>,
    name: int,
    rest: Seq<int>,
    root: Seq<int>,
)
    requires
        !under(parent, root),
        fresh_dir(parent, name, root),
    ensures
        !under(parent.push(name) + rest, root),
{
    let dir = parent.push(name);
    let file = dir + rest;
    fresh_dir_not_under_root(parent, name, root);
    if under(file, root) {
        let k = root.len() as int;
        if k <= dir.len() {
            assert(file.subrange(0, k) =~= dir.subrange(0, k));
            assert(under(dir, root));
        } else {
            let d = dir.len() as int;
            assert(root.subrange(0, d) =~= file.subrange(0, d));
            assert(file.subrange(0, d) =~= dir);
            assert(under(root, dir));
        }
    }
}

// ---- Part 3: a sink that keeps its lake between loads ----
//
// Core runs `included` first. Then it asks the sink `has(key, generation)`.
// If yes, core calls `keep` and restores nothing. Else core restores the
// member and calls `add`. At `finish`, the sink drops each member that it
// did not get from `add` or `keep`. The model has no member that fails.

/// What a sink holds for one member from an earlier load.
pub struct Held {
    /// The replica state it holds: the generation and the WAL position.
    pub generation: int,
    /// The rows it holds for that state.
    pub rows: nat,
}

/// The rows one member gives to a kept lake after a load. An excluded
/// member gives none: the sink drops it at `finish`.
pub open spec fn kept_rows(c: Candidate, generation: int, held: Option<Held>) -> nat {
    if !included(c) {
        0
    } else {
        match held {
            Some(h) => if h.generation == generation {
                h.rows
            } else {
                c.rows
            },
            None => c.rows,
        }
    }
}

/// The sink invariant: the rows it holds for a state are the rows of that
/// state.
pub open spec fn held_valid(c: Candidate, generation: int, held: Option<Held>) -> bool {
    match held {
        Some(h) => (h.generation == generation ==> h.rows == c.rows),
        None => true,
    }
}

/// The rows of a kept lake after it loads `s`. `generations[i]` is the
/// replica state of `s[i]` now. `held[i]` is what the sink holds for it.
pub open spec fn kept_lake_rows(
    s: Seq<Candidate>,
    generations: Seq<int>,
    held: Seq<Option<Held>>,
) -> nat
    decreases s.len(),
{
    if s.len() == 0 || generations.len() == 0 || held.len() == 0 {
        0
    } else {
        kept_lake_rows(s.drop_last(), generations.drop_last(), held.drop_last()) + kept_rows(
            s.last(),
            generations.last(),
            held.last(),
        )
    }
}

/// A held member that is now excluded (deleted) gives no rows.
pub proof fn an_excluded_held_member_is_dropped(c: Candidate, generation: int, held: Option<Held>)
    requires
        !included(c),
    ensures
        kept_rows(c, generation, held) == 0,
{
}

/// One member gives the same rows to a kept lake as to a new lake.
pub proof fn kept_member_matches_new(c: Candidate, generation: int, held: Option<Held>)
    requires
        held_valid(c, generation, held),
    ensures
        kept_rows(c, generation, held) == if included(c) {
            c.rows
        } else {
            0
        },
{
}

/// A load that skips held members gives the same rows as a load that
/// restores every member.
pub proof fn kept_lake_matches_new(
    s: Seq<Candidate>,
    generations: Seq<int>,
    held: Seq<Option<Held>>,
)
    requires
        s.len() == generations.len(),
        s.len() == held.len(),
        forall|i: int| 0 <= i < s.len() ==> held_valid(#[trigger] s[i], generations[i], held[i]),
    ensures
        kept_lake_rows(s, generations, held) == lake_rows(s),
    decreases s.len(),
{
    if s.len() > 0 {
        let rest = s.drop_last();
        let rest_generations = generations.drop_last();
        let rest_held = held.drop_last();
        assert forall|i: int| 0 <= i < rest.len() implies held_valid(
            #[trigger] rest[i],
            rest_generations[i],
            rest_held[i],
        ) by {
            assert(rest[i] == s[i]);
            assert(rest_generations[i] == generations[i]);
            assert(rest_held[i] == held[i]);
        }
        kept_lake_matches_new(rest, rest_generations, rest_held);
        let last = s.len() - 1;
        assert(held_valid(s[last], generations[last], held[last]));
        kept_member_matches_new(s[last], generations[last], held[last]);
    }
}

// ---- Part 4: the two replica formats ----
//
// A member's replica holds complete generations in two formats: WAL
// shipping commits with `snapshot.json`, generation shipping with
// `manifest.json`. `decide` reads every generation. Then
// `FleetLake::plan_from` takes the newest generation of each format at or
// before the target, and takes the newer of the two.

/// One complete generation of a member.
pub struct Gen {
    /// The epoch of the writer. 0: no epoch.
    pub epoch: int,
    /// When the generation was made.
    pub created: int,
    /// A `manifest.json` generation. `false`: a `snapshot.json` one.
    pub manifest: bool,
}

/// `a` is not older than `b`: by epoch, then by time.
pub open spec fn not_older(a: Gen, b: Gen) -> bool {
    a.epoch > b.epoch || (a.epoch == b.epoch && a.created >= b.created)
}

/// The newest generation of `s`. A tie gives either one.
pub open spec fn newest(s: Seq<Gen>) -> Gen
    decreases s.len(),
{
    if s.len() <= 1 {
        s[0]
    } else {
        let rest = newest(s.drop_last());
        if not_older(s.last(), rest) {
            s.last()
        } else {
            rest
        }
    }
}

/// `newest(s)` is in `s` and is not older than any generation of `s`.
pub proof fn newest_is_newest(s: Seq<Gen>)
    requires
        s.len() > 0,
    ensures
        s.contains(newest(s)),
        forall|i: int| 0 <= i < s.len() ==> not_older(newest(s), #[trigger] s[i]),
    decreases s.len(),
{
    if s.len() == 1 {
        assert(s[0] == newest(s));
    } else {
        let rest = s.drop_last();
        newest_is_newest(rest);
        assert(newest(s) == if not_older(s.last(), newest(rest)) {
            s.last()
        } else {
            newest(rest)
        });
        assert forall|i: int| 0 <= i < s.len() implies not_older(newest(s), #[trigger] s[i]) by {
            if i < s.len() - 1 {
                assert(s[i] == rest[i]);
                assert(not_older(newest(rest), rest[i]));
            }
        }
        if not_older(s.last(), newest(rest)) {
            assert(s[s.len() - 1] == newest(s));
        } else {
            assert(newest(s) == newest(rest));
            let j = choose|j: int| 0 <= j < rest.len() && rest[j] == newest(rest);
            assert(s[j] == rest[j]);
            assert(s[j] == newest(s));
        }
    }
}

/// The generations of `s` made at or before `t`, in order.
pub open spec fn at(s: Seq<Gen>, t: int) -> Seq<Gen>
    decreases s.len(),
{
    if s.len() == 0 {
        s
    } else if s.last().created <= t {
        at(s.drop_last(), t).push(s.last())
    } else {
        at(s.drop_last(), t)
    }
}

/// The target filter keeps the format of each generation.
pub proof fn at_keeps_the_format(s: Seq<Gen>, t: int, manifest: bool)
    requires
        forall|i: int| 0 <= i < s.len() ==> (#[trigger] s[i]).manifest == manifest,
    ensures
        forall|j: int| 0 <= j < at(s, t).len() ==> (#[trigger] at(s, t)[j]).manifest == manifest,
    decreases s.len(),
{
    if s.len() > 0 {
        let rest = s.drop_last();
        assert forall|i: int| 0 <= i < rest.len() implies (#[trigger] rest[i]).manifest
            == manifest by {
            assert(rest[i] == s[i]);
        }
        at_keeps_the_format(rest, t, manifest);
        let kept = at(rest, t);
        assert(s.last() == s[s.len() - 1]);
        assert forall|j: int| 0 <= j < at(s, t).len() implies (#[trigger] at(s, t)[j]).manifest
            == manifest by {
            if s.last().created <= t {
                assert(at(s, t) == kept.push(s.last()));
                if j < kept.len() {
                    assert(at(s, t)[j] == kept[j]);
                } else {
                    assert(at(s, t)[j] == s.last());
                }
            } else {
                assert(at(s, t) == kept);
            }
        }
    }
}

/// The generation the lake plans: the newer of the newest of each format.
/// The manifest wins a tie.
pub open spec fn pick(wal: Seq<Gen>, manifests: Seq<Gen>) -> Gen {
    if wal.len() == 0 {
        newest(manifests)
    } else if manifests.len() == 0 {
        newest(wal)
    } else if not_older(newest(manifests), newest(wal)) {
        newest(manifests)
    } else {
        newest(wal)
    }
}

/// The pick is not older than any generation of either format. So a
/// split by format gives the same order as one list.
pub proof fn pick_is_newest_of_both(wal: Seq<Gen>, manifests: Seq<Gen>)
    requires
        wal.len() + manifests.len() > 0,
    ensures
        forall|i: int| 0 <= i < wal.len() ==> not_older(pick(wal, manifests), #[trigger] wal[i]),
        forall|i: int|
            0 <= i < manifests.len() ==> not_older(
                pick(wal, manifests),
                #[trigger] manifests[i],
            ),
        wal.contains(pick(wal, manifests)) || manifests.contains(pick(wal, manifests)),
{
    if wal.len() > 0 {
        newest_is_newest(wal);
    }
    if manifests.len() > 0 {
        newest_is_newest(manifests);
    }
}

/// `decide` reports `Unreplicated` only when neither format has a
/// complete generation. It reads every generation, not only those at or
/// before the target.
pub open spec fn unreplicated(wal: Seq<Gen>, manifests: Seq<Gen>) -> bool {
    wal.len() == 0 && manifests.len() == 0
}

/// What the lake does with a member that the tombstone rule takes.
pub enum Outcome {
    /// No complete generation: `SkipReason::Unreplicated`.
    Unreplicated,
    /// Each generation is after the target: `SkipReason::AbsentAtTarget`.
    AbsentAtTarget,
    /// The sink cannot read the format: an error, or `Failed` when
    /// skipped.
    Failed,
    /// Restored, checked and given to the sink.
    Added,
}

/// `wal`, `manifests`: every complete generation of each format. `t`: the
/// target. `sink_reads_manifests`: the sink checks the identity of a
/// generations member (`LakeSink::check_identity`).
pub open spec fn outcome(
    wal: Seq<Gen>,
    manifests: Seq<Gen>,
    t: int,
    sink_reads_manifests: bool,
) -> Outcome {
    if unreplicated(wal, manifests) {
        Outcome::Unreplicated
    } else if at(wal, t).len() == 0 && at(manifests, t).len() == 0 {
        Outcome::AbsentAtTarget
    } else if pick(at(wal, t), at(manifests, t)).manifest && !sink_reads_manifests {
        Outcome::Failed
    } else {
        Outcome::Added
    }
}

/// A manifest is a replica: the lake never reports such a member as
/// `Unreplicated`, whatever the target and the sink.
pub proof fn a_manifest_is_not_unreplicated(
    wal: Seq<Gen>,
    manifests: Seq<Gen>,
    t: int,
    sink_reads_manifests: bool,
)
    requires
        manifests.len() > 0,
    ensures
        outcome(wal, manifests, t, sink_reads_manifests) != Outcome::Unreplicated,
{
}

/// A target before every generation gives `AbsentAtTarget`, not
/// `Unreplicated`.
pub proof fn a_target_before_every_generation_is_absent(
    wal: Seq<Gen>,
    manifests: Seq<Gen>,
    t: int,
    sink_reads_manifests: bool,
)
    requires
        wal.len() + manifests.len() > 0,
        at(wal, t).len() == 0,
        at(manifests, t).len() == 0,
    ensures
        outcome(wal, manifests, t, sink_reads_manifests) == Outcome::AbsentAtTarget,
{
}

/// A generations fleet (manifests only) gives each member to a sink that
/// reads manifests.
pub proof fn a_generations_member_is_added(manifests: Seq<Gen>, t: int)
    requires
        at(manifests, t).len() > 0,
    ensures
        outcome(Seq::empty(), manifests, t, true) == Outcome::Added,
{
    let none: Seq<Gen> = Seq::empty();
    assert(at(none, t) == none);
}

/// A sink that cannot read manifests fails a generations member. It does
/// not report it as `Unreplicated`.
pub proof fn an_unreadable_member_fails(manifests: Seq<Gen>, t: int)
    requires
        at(manifests, t).len() > 0,
        forall|i: int| 0 <= i < manifests.len() ==> (#[trigger] manifests[i]).manifest,
    ensures
        outcome(Seq::empty(), manifests, t, false) == Outcome::Failed,
{
    let none: Seq<Gen> = Seq::empty();
    assert(at(none, t) == none);
    let kept = at(manifests, t);
    at_keeps_the_format(manifests, t, true);
    newest_is_newest(kept);
    let j = choose|j: int| 0 <= j < kept.len() && kept[j] == newest(kept);
    assert(kept[j].manifest);
}

/// A WAL replica is read as before: the sink need not read manifests.
pub proof fn a_wal_member_is_added(wal: Seq<Gen>, t: int, sink_reads_manifests: bool)
    requires
        at(wal, t).len() > 0,
        forall|i: int| 0 <= i < wal.len() ==> !(#[trigger] wal[i]).manifest,
    ensures
        outcome(wal, Seq::empty(), t, sink_reads_manifests) == Outcome::Added,
{
    let none: Seq<Gen> = Seq::empty();
    assert(at(none, t) == none);
    let kept = at(wal, t);
    at_keeps_the_format(wal, t, false);
    newest_is_newest(kept);
    let j = choose|j: int| 0 <= j < kept.len() && kept[j] == newest(kept);
    assert(!kept[j].manifest);
}

fn main() {}

} // verus!
