use vstd::prelude::*;

verus! {

// Route groups and per-role mounting (issue #3544, ADR 0036).
//
// The runtime is `RolePreset::mounts_group`, `Role::mounts_group`,
// `retain_mounted_groups` in `autumn/src/app.rs` and
// `groups_mounted_by_no_preset` in `autumn/src/config.rs`. A group is an
// integer here. The proofs show:
//
// - A role without `mount` mounts every group.
// - A role that serves no HTTP mounts no group.
// - A valid role mounts each group in its `mount` list, and no other group.
// - A framework route is served on every role that serves HTTP.
// - A group that some fleet role mounts is not drift.

pub enum Base {
    Combined,
    Web,
    Worker,
}

/// The shadow of the `mount` part of `RolePreset`.
pub struct Preset {
    pub base: Base,
    pub mount: Option<Seq<int>>,
}

/// The shadow of a mounted route: a framework route, or a route in a group.
pub struct Route {
    pub framework: bool,
    pub group: int,
}

/// `ProcessRole::serves_http`.
pub open spec fn serves_http(b: Base) -> bool {
    !(b is Worker)
}

/// `group` is in `groups`.
pub open spec fn listed(groups: Seq<int>, group: int) -> bool {
    exists|i: int| 0 <= i < groups.len() && groups[i] == group
}

/// The mount check of `RolePreset::validate`: `mount` needs an HTTP base and
/// at least one group.
pub open spec fn valid(p: Preset) -> bool {
    match p.mount {
        Some(groups) => serves_http(p.base) && groups.len() > 0,
        None => true,
    }
}

/// `RolePreset::mounts_group`.
pub open spec fn mounts(p: Preset, group: int) -> bool {
    serves_http(p.base) && match p.mount {
        Some(groups) => listed(groups, group),
        None => true,
    }
}

/// The route filter: a framework route stays, a group route stays when the
/// role mounts its group.
pub open spec fn served(p: Preset, r: Route) -> bool {
    serves_http(p.base) && (r.framework || mounts(p, r.group))
}

/// Drift: no fleet role and not the current role mounts the group.
pub open spec fn drift(fleet: Seq<Preset>, current: Preset, group: int) -> bool {
    !mounts(current, group) && !(exists|i: int| 0 <= i < fleet.len() && mounts(fleet[i], group))
}

/// A role without `mount` mounts every group.
pub proof fn unset_mount_mounts_every_group(p: Preset, group: int)
    requires
        p.mount is None,
        serves_http(p.base),
    ensures
        mounts(p, group),
{
}

/// A role that serves no HTTP mounts no group.
pub proof fn no_http_mounts_no_group(p: Preset, group: int)
    requires
        !serves_http(p.base),
    ensures
        !mounts(p, group),
{
}

/// A valid role with `mount` mounts exactly the listed groups.
pub proof fn mount_is_exact(p: Preset, groups: Seq<int>, group: int)
    requires
        valid(p),
        p.mount == Some(groups),
    ensures
        mounts(p, group) == listed(groups, group),
{
}

/// A framework route is served on every role that serves HTTP.
pub proof fn framework_route_is_shared(p: Preset, r: Route)
    requires
        r.framework,
        serves_http(p.base),
    ensures
        served(p, r),
{
}

/// A group route is served only where its group is mounted.
pub proof fn group_route_follows_the_mount(p: Preset, r: Route)
    requires
        !r.framework,
    ensures
        served(p, r) == mounts(p, r.group),
{
}

/// A group that some fleet role mounts is not drift.
pub proof fn fleet_mount_is_not_drift(fleet: Seq<Preset>, current: Preset, group: int, i: int)
    requires
        0 <= i < fleet.len(),
        mounts(fleet[i], group),
    ensures
        !drift(fleet, current, group),
{
}

fn main() {
}

} // verus!
