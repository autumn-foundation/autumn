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

// ---- A close-time generation that did not ship ----

/// The result of the last generation of a close, or of a retry.
pub enum CloseShip {
    /// The generation shipped, or there was nothing to ship.
    Shipped,
    /// The ship failed, or the shutdown cutoff skipped it.
    Failed,
    /// A newer epoch refused it.
    Fenced,
    /// The key is open or draining. The retry waits.
    Busy,
}

/// Why a database stopped shipping.
pub enum Retire {
    /// It did not.
    No,
    /// A delete or a restore replaced its files.
    Replaced,
    /// A newer epoch owns its slot.
    NewerEpoch,
    /// The lease of this host lapsed. No newer epoch is known.
    Lapse,
}

/// Whether the record of a closed database stays. `had`: a record was
/// there before.
pub open spec fn keeps_record(retire: Retire, had: bool, ship: CloseShip) -> bool {
    match retire {
        Retire::Replaced => false,
        Retire::NewerEpoch => false,
        // The writes are still this host's: they ship later.
        Retire::Lapse => true,
        Retire::No => match ship {
            CloseShip::Shipped => false,
            CloseShip::Fenced => false,
            CloseShip::Failed => true,
            CloseShip::Busy => had,
        },
    }
}

/// A delete or a restore that ends. `replaced`: the files are gone or
/// replaced. A failed one puts the record back.
pub open spec fn keeps_after_release(had: bool, replaced: bool) -> bool {
    had && !replaced
}

/// `release_ownership` releases only with no parked WAL replicator and no
/// kept generation. Otherwise the ranges lapse.
pub open spec fn may_release(parked: nat, kept: nat) -> bool {
    parked == 0 && kept == 0
}

/// A failed close-time ship keeps the record.
pub proof fn failed_close_keeps_the_record(had: bool)
    ensures keeps_record(Retire::No, had, CloseShip::Failed)
{}

/// A kept record makes the ranges lapse.
pub proof fn a_record_blocks_release(parked: nat, kept: nat)
    requires kept > 0
    ensures !may_release(parked, kept)
{}

/// A retry that ships drops the record.
pub proof fn a_shipped_retry_drops_the_record(had: bool)
    ensures !keeps_record(Retire::No, had, CloseShip::Shipped)
{}

/// A lapse keeps the record, whatever the ship did.
pub proof fn a_lapse_keeps_the_record(had: bool, ship: CloseShip)
    ensures keeps_record(Retire::Lapse, had, ship)
{}

/// A delete, a restore or a newer epoch drops the record. A retired
/// database puts nothing, so nothing ships for it.
pub proof fn a_retire_drops_the_record_and_ships_nothing(
    had: bool,
    ship: CloseShip,
    mine: Option<nat>,
    known: nat,
    newest: nat,
)
    ensures
        !keeps_record(Retire::Replaced, had, ship),
        !keeps_record(Retire::NewerEpoch, had, ship),
        !put_allowed(Mode::Generations, true, mine, known, newest),
{}

/// A delete or a restore that fails keeps the record.
pub proof fn a_failed_release_keeps_the_record()
    ensures
        keeps_after_release(true, false),
        !keeps_after_release(true, true),
{}

/// Only a ship, a fence, a delete or a restore clears a record.
pub proof fn only_a_ship_or_a_retire_clears_the_record(retire: Retire, ship: CloseShip)
    requires !keeps_record(retire, true, ship)
    ensures
        retire == Retire::Replaced || retire == Retire::NewerEpoch
            || ship == CloseShip::Shipped || ship == CloseShip::Fenced,
{}

fn main() {}

} // verus!
