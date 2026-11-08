use vstd::prelude::*;

verus! {

// Model of generation shipping of a fleet (`ShipMode::Generations` in
// `autumn/src/db/fleet_engine.rs`, the shipper in
// `autumn/src/db/fleet/generations.rs`, and the fence in
// `autumn/src/replication/fence.rs`).
//
// A file is its content hash, a `nat`. A generation is a `nat` id. Times are
// epoch milliseconds.

// ---- The fence before each put ----

/// How the databases of an engine get to object storage.
pub enum Mode {
    Continuous,
    Generations,
    Rebuild,
}

/// `ShipFence::check`. `retired`: the database closed for a delete, a
/// restore or a fence. `mine`: the epoch the database opened under, `None`
/// without ownership. `known`: the newest epoch this host saw for the slot.
/// `newest`: the newest epoch marker at the destination.
pub open spec fn may_put(retired: bool, mine: Option<nat>, known: nat, newest: nat) -> bool {
    !retired && match mine {
        Some(epoch) => known <= epoch && newest <= epoch,
        None => true,
    }
}

/// Whether core writes a replica in `mode` at all. `Rebuild` has none.
pub open spec fn ships(mode: Mode) -> bool {
    match mode {
        Mode::Rebuild => false,
        _ => true,
    }
}

/// A put of core in `mode`: the mode ships, and the fence admits the put.
/// The fence does not depend on the mode.
pub open spec fn put_allowed(
    mode: Mode,
    retired: bool,
    mine: Option<nat>,
    known: nat,
    newest: nat,
) -> bool {
    ships(mode) && may_put(retired, mine, known, newest)
}

/// A newer epoch at the destination refuses every put, in every mode.
pub proof fn fenced_put_is_refused(mode: Mode, retired: bool, epoch: nat, known: nat, newest: nat)
    requires epoch < newest
    ensures !put_allowed(mode, retired, Some(epoch), known, newest)
{}

/// A newer epoch this host saw refuses every put too, with no list.
pub proof fn known_fence_is_refused(mode: Mode, retired: bool, epoch: nat, known: nat, newest: nat)
    requires epoch < known
    ensures !put_allowed(mode, retired, Some(epoch), known, newest)
{}

/// A retired database writes nothing more.
pub proof fn retired_put_is_refused(mode: Mode, mine: Option<nat>, known: nat, newest: nat)
    ensures !put_allowed(mode, true, mine, known, newest)
{}

/// The current owner can put: the fence stops only a stale owner.
pub proof fn current_owner_may_put(epoch: nat, known: nat, newest: nat)
    requires known <= epoch, newest <= epoch
    ensures
        put_allowed(Mode::Continuous, false, Some(epoch), known, newest),
        put_allowed(Mode::Generations, false, Some(epoch), known, newest),
{}

// ---- Which files a generation ships ----

/// The files a generation puts: the hashes the previous generation does not
/// hold.
pub open spec fn to_ship(prev: Set<nat>, cur: Set<nat>) -> Set<nat> {
    cur.difference(prev)
}

/// A file that did not change is not shipped again.
pub proof fn unchanged_file_is_not_reshipped(prev: Set<nat>, cur: Set<nat>, h: nat)
    requires prev.contains(h), cur.contains(h)
    ensures !to_ship(prev, cur).contains(h)
{}

/// Every file of the new generation is shipped now or held already.
pub proof fn every_file_is_shipped_or_held(prev: Set<nat>, cur: Set<nat>, h: nat)
    requires cur.contains(h)
    ensures to_ship(prev, cur).contains(h) || prev.contains(h)
{}

/// The generation that holds file `h` of the new manifest `g`: the holder
/// of the previous manifest for an unchanged file, else `g`.
pub open spec fn holder(prev: Map<nat, nat>, g: nat, h: nat) -> nat {
    if prev.contains_key(h) { prev[h] } else { g }
}

/// A holder is `g` or a holder of the previous manifest.
pub proof fn holder_is_new_or_previous(prev: Map<nat, nat>, g: nat, h: nat)
    ensures holder(prev, g, h) == g || (prev.contains_key(h) && holder(prev, g, h) == prev[h])
{}

/// A holder is never newer than the generation that names it, when the
/// previous holders are not.
pub proof fn holder_is_not_newer(prev: Map<nat, nat>, g: nat, h: nat, time: Map<nat, int>)
    requires
        time.contains_key(g),
        forall|k: nat| #[trigger] prev.contains_key(k) ==> time.contains_key(prev[k]) && time[prev[k]] <= time[g],
    ensures
        time.contains_key(holder(prev, g, h)),
        time[holder(prev, g, h)] <= time[g],
{}

// ---- Prune ----

/// Whether a prune keeps generation `x`: it is one of the newest `kept`, or
/// a manifest of a kept generation names it as a holder.
pub open spec fn keeps(kept: Set<nat>, manifests: Map<nat, Map<nat, nat>>, x: nat) -> bool {
    kept.contains(x) || exists|g: nat, h: nat|
        kept.contains(g) && manifests.contains_key(g) && #[trigger] manifests[g].contains_key(h)
            && manifests[g][h] == x
}

/// A prune never removes a file that a kept manifest names. So a restore of
/// a kept generation finds every file.
pub proof fn prune_keeps_every_named_file(
    kept: Set<nat>,
    manifests: Map<nat, Map<nat, nat>>,
    g: nat,
    h: nat,
)
    requires
        kept.contains(g),
        manifests.contains_key(g),
        manifests[g].contains_key(h),
    ensures keeps(kept, manifests, manifests[g][h])
{
    assert(kept.contains(g) && manifests.contains_key(g) && manifests[g].contains_key(h)
        && manifests[g][h] == manifests[g][h]);
}

/// A prune keeps every kept generation.
pub proof fn prune_keeps_the_newest(kept: Set<nat>, manifests: Map<nat, Map<nat, nat>>, g: nat)
    requires kept.contains(g)
    ensures keeps(kept, manifests, g)
{}

// ---- Restore and tombstones ----

/// A replica generation: its epoch (0 when it has none) and its time.
pub struct Generation {
    pub epoch: nat,
    pub created_ms: int,
}

/// The restore takes the greatest (epoch, created_ms), as `restore::plan`.
pub open spec fn newer(a: Generation, b: Generation) -> bool {
    a.epoch > b.epoch || (a.epoch == b.epoch && a.created_ms > b.created_ms)
}

/// A generation of an older epoch never wins, whatever its time.
pub proof fn older_epoch_never_wins(stale: Generation, current: Generation)
    requires stale.epoch < current.epoch
    ensures newer(current, stale), !newer(stale, current)
{}

/// `restore_allowed` of `fleet_tombstones.rs`, unchanged. `row`: a tombstone
/// row exists. `marker`: the `deleted_at` of the marker object.
pub open spec fn restore_allowed(row: bool, marker: Option<int>, newest: int) -> bool {
    !row && match marker {
        Some(deleted_at) => newest > deleted_at,
        None => true,
    }
}

/// A restore of a generation after a delete is refused: the row blocks it,
/// and the marker blocks every generation that is not newer than the delete.
pub proof fn restore_after_delete_is_refused(marker: Option<int>, created_ms: int, deleted_at: int)
    requires created_ms <= deleted_at
    ensures
        !restore_allowed(true, marker, created_ms),
        !restore_allowed(false, Some(deleted_at), created_ms),
{}

/// `purge_deletes` of `fleet_tombstones.rs`, unchanged.
pub open spec fn purge_deletes(generation: Option<int>, cutoff: int, newer_exists: bool) -> bool {
    match generation {
        Some(created) => created <= cutoff,
        None => !newer_exists,
    }
}

/// A purge at or after the newest generation removes every holder it names:
/// a holder is not newer than its manifest.
pub proof fn purge_removes_every_holder(
    holder_ms: int,
    manifest_ms: int,
    cutoff: int,
    newer_exists: bool,
)
    requires holder_ms <= manifest_ms, manifest_ms <= cutoff
    ensures
        purge_deletes(Some(holder_ms), cutoff, newer_exists),
        purge_deletes(Some(manifest_ms), cutoff, newer_exists),
{}

/// A purge keeps a newer database of the same key, and the holders of its
/// first generation: that generation ships every file, so each is newer.
pub proof fn purge_keeps_a_newer_database(created_ms: int, cutoff: int, newer_exists: bool)
    requires created_ms > cutoff
    ensures !purge_deletes(Some(created_ms), cutoff, newer_exists)
{}

fn main() {}

} // verus!
