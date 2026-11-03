use vstd::prelude::*;

verus! {

// Named role presets (issue #3543, ADR 0035).
//
// The runtime is `RolePreset::capabilities` and `RolePreset::validate` in
// `autumn/src/config.rs`. A preset is a built-in base plus optional
// overrides. The proofs show:
//
// - A preset never runs a capability that its base does not run.
// - `serves_http` is always the base value.
// - A preset with no overrides runs exactly its base set.
// - A valid preset gets the value of each override it sets.
// - A preset over `web` runs no background work.

pub enum Base {
    Combined,
    Web,
    Worker,
}

/// The shadow of `RoleCapabilities`.
pub struct Caps {
    pub serves_http: bool,
    pub job_workers: bool,
    pub scheduler: bool,
    pub outbox_relay: bool,
    pub commit_hooks: bool,
    pub fleet_relay: bool,
    pub restore_check: bool,
    pub retention: bool,
}

/// The shadow of `RolePreset`. `pin_empty` is `pin = []`.
pub struct Preset {
    pub base: Base,
    pub pin_empty: bool,
    pub job_workers: Option<bool>,
    pub scheduler: Option<bool>,
    pub outbox_relay: Option<bool>,
    pub commit_hooks: Option<bool>,
    pub fleet_relay: Option<bool>,
    pub restore_check: Option<bool>,
    pub retention: Option<bool>,
}

/// `ProcessRole::capabilities`.
pub open spec fn base_caps(b: Base) -> Caps {
    let http = !(b is Worker);
    let background = !(b is Web);
    Caps {
        serves_http: http,
        job_workers: background,
        scheduler: background,
        outbox_relay: background,
        commit_hooks: background,
        fleet_relay: background,
        restore_check: background,
        retention: background,
    }
}

/// An unset override keeps the base value.
pub open spec fn or_true(o: Option<bool>) -> bool {
    match o {
        Some(v) => v,
        None => true,
    }
}

/// An override turns a capability on that the base does not run.
pub open spec fn above(base: bool, o: Option<bool>) -> bool {
    o == Some(true) && !base
}

/// `RolePreset::capabilities`.
pub open spec fn resolve(p: Preset) -> Caps {
    let b = base_caps(p.base);
    Caps {
        serves_http: b.serves_http,
        job_workers: b.job_workers && or_true(p.job_workers) && !p.pin_empty,
        scheduler: b.scheduler && or_true(p.scheduler),
        outbox_relay: b.outbox_relay && or_true(p.outbox_relay),
        commit_hooks: b.commit_hooks && or_true(p.commit_hooks),
        fleet_relay: b.fleet_relay && or_true(p.fleet_relay),
        restore_check: b.restore_check && or_true(p.restore_check),
        retention: b.retention && or_true(p.retention),
    }
}

/// The capability check of `RolePreset::validate`.
pub open spec fn valid(p: Preset) -> bool {
    let b = base_caps(p.base);
    !above(b.job_workers, p.job_workers) && !above(b.scheduler, p.scheduler) && !above(
        b.outbox_relay,
        p.outbox_relay,
    ) && !above(b.commit_hooks, p.commit_hooks) && !above(b.fleet_relay, p.fleet_relay)
        && !above(b.restore_check, p.restore_check) && !above(b.retention, p.retention)
        && !(p.pin_empty && p.job_workers == Some(true))
}

/// Each field of `a` implies the same field of `b`.
pub open spec fn subset(a: Caps, b: Caps) -> bool {
    (a.serves_http ==> b.serves_http) && (a.job_workers ==> b.job_workers) && (a.scheduler
        ==> b.scheduler) && (a.outbox_relay ==> b.outbox_relay) && (a.commit_hooks
        ==> b.commit_hooks) && (a.fleet_relay ==> b.fleet_relay) && (a.restore_check
        ==> b.restore_check) && (a.retention ==> b.retention)
}

/// The executable shadow of one field: the base value and the override.
fn field(base: bool, o: Option<bool>) -> (r: bool)
    ensures
        r == (base && or_true(o)),
{
    match o {
        Some(v) => base && v,
        None => base,
    }
}

/// The executable shadow of `RolePreset::capabilities`.
pub fn resolve_exec(p: &Preset) -> (c: Caps)
    ensures
        c == resolve(*p),
{
    let (http, background) = match p.base {
        Base::Combined => (true, true),
        Base::Web => (true, false),
        Base::Worker => (false, true),
    };
    Caps {
        serves_http: http,
        job_workers: field(background, p.job_workers) && !p.pin_empty,
        scheduler: field(background, p.scheduler),
        outbox_relay: field(background, p.outbox_relay),
        commit_hooks: field(background, p.commit_hooks),
        fleet_relay: field(background, p.fleet_relay),
        restore_check: field(background, p.restore_check),
        retention: field(background, p.retention),
    }
}

/// A preset never runs a capability that its base does not run. This holds
/// for every preset, valid or not.
pub proof fn preset_is_within_its_base(p: Preset)
    ensures
        subset(resolve(p), base_caps(p.base)),
{
}

/// The base decides HTTP.
pub proof fn base_decides_http(p: Preset)
    ensures
        resolve(p).serves_http == base_caps(p.base).serves_http,
{
}

/// A preset with no overrides runs exactly its base set.
pub proof fn empty_preset_is_its_base(p: Preset)
    requires
        !p.pin_empty,
        p.job_workers is None,
        p.scheduler is None,
        p.outbox_relay is None,
        p.commit_hooks is None,
        p.fleet_relay is None,
        p.restore_check is None,
        p.retention is None,
    ensures
        resolve(p) == base_caps(p.base),
{
}

/// A valid preset gets the value of each override it sets.
pub proof fn valid_override_is_respected(p: Preset)
    requires
        valid(p),
    ensures
        p.scheduler == Some(true) ==> resolve(p).scheduler,
        p.scheduler == Some(false) ==> !resolve(p).scheduler,
        p.outbox_relay == Some(true) ==> resolve(p).outbox_relay,
        p.outbox_relay == Some(false) ==> !resolve(p).outbox_relay,
        p.commit_hooks == Some(true) ==> resolve(p).commit_hooks,
        p.commit_hooks == Some(false) ==> !resolve(p).commit_hooks,
        p.fleet_relay == Some(true) ==> resolve(p).fleet_relay,
        p.fleet_relay == Some(false) ==> !resolve(p).fleet_relay,
        p.restore_check == Some(true) ==> resolve(p).restore_check,
        p.restore_check == Some(false) ==> !resolve(p).restore_check,
        p.retention == Some(true) ==> resolve(p).retention,
        p.retention == Some(false) ==> !resolve(p).retention,
        p.job_workers == Some(true) ==> resolve(p).job_workers,
        p.job_workers == Some(false) ==> !resolve(p).job_workers,
{
}

/// `pin = []` runs no job workers.
pub proof fn empty_pin_runs_no_job_workers(p: Preset)
    requires
        p.pin_empty,
    ensures
        !resolve(p).job_workers,
{
}

/// A preset over `web` runs no background work.
pub proof fn web_base_runs_no_background_work(p: Preset)
    requires
        p.base is Web,
    ensures
        resolve(p).serves_http,
        !resolve(p).job_workers,
        !resolve(p).scheduler,
        !resolve(p).outbox_relay,
        !resolve(p).commit_hooks,
        !resolve(p).fleet_relay,
        !resolve(p).restore_check,
        !resolve(p).retention,
{
}

/// A preset over `worker` serves no user routes.
pub proof fn worker_base_serves_no_routes(p: Preset)
    requires
        p.base is Worker,
    ensures
        !resolve(p).serves_http,
{
}

fn main() {
}

} // verus!
