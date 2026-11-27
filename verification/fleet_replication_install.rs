use vstd::prelude::*;

verus! {

// Model of the boot-time replication installer of a fleet.
//
// The runtime is `install_replication` in `autumn/src/fleet/replication.rs`
// and `DatabaseFleet::warn_if_unreplicated` in `autumn/src/db/fleet.rs`.
// The `[replication]` runtime core builds at boot is the source. A fleet
// root is a number. The model leaves out the I/O: the replication thread
// and the health indicator. The unit tests check those parts. The model has
// one ownership flag. The runtime reads it from the config at the install,
// and from the installed ownership at an open.
//
// The proofs show:
//
// - A fleet logs the unreplicated warning at most once, over any sequence
//   of installs and opens.
// - The fleet logs the warning only for ownership, no replica and an engine
//   that is not in `Rebuild` mode.
// - With a source, an install connects the fleet or refuses. It never
//   leaves a fleet with ownership and no replica in silence.
// - A source holds at most one fleet. The source refuses a second fleet,
//   and the state does not change.

/// One fleet.
pub struct Fleet {
    pub root: nat,
    pub ownership: bool,
    pub rebuild: bool,
    pub replicated: bool,
    pub warned: bool,
    /// Warnings logged so far.
    pub warnings: nat,
}

/// The `[replication]` source. `None`: `[replication]` is off.
pub struct Source {
    pub claimed: Option<nat>,
}

pub enum Outcome {
    Installed,
    NoReplication,
    AlreadyReplicates,
    RootInUse,
}

/// The warning rule.
pub open spec fn should_warn(f: Fleet) -> bool {
    f.ownership && !f.replicated && !f.rebuild && !f.warned
}

/// `warn_if_unreplicated`.
pub open spec fn warn(f: Fleet) -> Fleet {
    if should_warn(f) {
        Fleet {
            root: f.root,
            ownership: f.ownership,
            rebuild: f.rebuild,
            replicated: f.replicated,
            warned: true,
            warnings: f.warnings + 1,
        }
    } else {
        f
    }
}

/// `install_replication`: the outcome, the source after, the fleet after.
pub open spec fn install(source: Option<Source>, f: Fleet) -> (Outcome, Option<Source>, Fleet) {
    match source {
        None => (Outcome::NoReplication, None, warn(f)),
        Some(s) => {
            if f.replicated {
                (Outcome::AlreadyReplicates, Some(s), f)
            } else if s.claimed.is_some() {
                (Outcome::RootInUse, Some(s), f)
            } else {
                (
                    Outcome::Installed,
                    Some(Source { claimed: Some(f.root) }),
                    Fleet {
                        root: f.root,
                        ownership: f.ownership,
                        rebuild: f.rebuild,
                        replicated: true,
                        warned: f.warned,
                        warnings: f.warnings,
                    },
                )
            }
        },
    }
}

/// The first open of a database under ownership.
pub open spec fn open(f: Fleet) -> Fleet {
    if f.ownership {
        warn(f)
    } else {
        f
    }
}

/// The invariant: the count matches the flag.
pub open spec fn inv(f: Fleet) -> bool {
    f.warnings == (if f.warned { 1nat } else { 0nat })
}

/// One step: an install with `source`, or an open.
pub enum Step {
    Install { source: Option<Source> },
    Open,
}

pub open spec fn step(f: Fleet, s: Step) -> Fleet {
    match s {
        Step::Install { source } => install(source, f).2,
        Step::Open => open(f),
    }
}

pub open spec fn run(f: Fleet, steps: Seq<Step>) -> Fleet
    decreases steps.len(),
{
    if steps.len() == 0 {
        f
    } else {
        run(step(f, steps[0]), steps.subrange(1, steps.len() as int))
    }
}

proof fn warn_keeps_inv(f: Fleet)
    requires
        inv(f),
    ensures
        inv(warn(f)),
        warn(f).replicated == f.replicated,
        warn(f).root == f.root,
        warn(f).ownership == f.ownership,
        warn(f).rebuild == f.rebuild,
{
}

proof fn step_keeps_inv(f: Fleet, s: Step)
    requires
        inv(f),
    ensures
        inv(step(f, s)),
{
    match s {
        Step::Install { .. } => {
            warn_keeps_inv(f);
        },
        Step::Open => {
            warn_keeps_inv(f);
        },
    }
}

/// Over any sequence of steps, a fleet logs at most one warning.
proof fn at_most_one_warning(f: Fleet, steps: Seq<Step>)
    requires
        inv(f),
    ensures
        inv(run(f, steps)),
        run(f, steps).warnings <= 1,
    decreases steps.len(),
{
    if steps.len() > 0 {
        step_keeps_inv(f, steps[0]);
        at_most_one_warning(step(f, steps[0]), steps.subrange(1, steps.len() as int));
    }
}

/// A warning is logged only for ownership, no replica and not `Rebuild`.
proof fn a_warning_has_a_cause(f: Fleet, s: Step)
    requires
        step(f, s).warnings > f.warnings,
    ensures
        f.ownership,
        !f.replicated,
        !f.rebuild,
{
}

/// With a source, an install connects the fleet or refuses with a reason.
proof fn a_source_never_installs_in_silence(s: Source, f: Fleet)
    ensures
        install(Some(s), f).0 != Outcome::NoReplication,
        install(Some(s), f).0 == Outcome::Installed ==> install(Some(s), f).2.replicated,
{
}

/// A source holds one fleet. The source refuses a second fleet, also one
/// with the same root, and nothing changes.
proof fn one_fleet_per_source(s: Source, a: Fleet, b: Fleet)
    requires
        !b.replicated,
        install(Some(s), a).0 == Outcome::Installed,
    ensures
        ({
            let after = install(Some(s), a).1;
            after.is_some() && install(after, b).0 == Outcome::RootInUse
                && install(after, b).1 == after && install(after, b).2 == b
        }),
{
}

/// A replicated fleet never warns.
proof fn a_replicated_fleet_never_warns(f: Fleet, s: Step)
    requires
        f.replicated,
    ensures
        step(f, s).warnings == f.warnings,
{
}

fn main() {
}

} // verus!
