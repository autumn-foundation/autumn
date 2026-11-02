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
    /// A complete generation exists (`restore::plan` gave a plan).
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

fn main() {}

} // verus!
