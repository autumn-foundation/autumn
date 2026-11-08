use vstd::prelude::*;

verus! {

// Release manifests and fleet groups per role (ADR 0042).
//
// The runtime is `manifest_roles` in `autumn-cli/src/release_roles.rs`,
// `DeployConfig::group_hosts` in `autumn/src/config.rs` and
// `grade_group_roles` in `autumn-cli/src/deploy.rs`. A role name is an
// integer here. The built-in names are 0 (`combined`), 1 (`web`) and
// 2 (`worker`). A preset name is greater than 2, because a preset cannot use
// a built-in name. The proofs show:
//
// - Each role in the default set is declared.
// - The default set has no duplicate names.
// - Every role gets a workload. Only a role that serves HTTP gets a Service.
// - A fleet host never gets two roles.
// - A fleet that passes preflight runs no split role on a local jobs backend.

pub enum Base {
    Combined,
    Web,
    Worker,
}

pub open spec fn combined() -> int {
    0
}

pub open spec fn web() -> int {
    1
}

pub open spec fn worker() -> int {
    2
}

/// `ProcessRole::serves_http`.
pub open spec fn serves_http(b: Base) -> bool {
    !(b is Worker)
}

/// A built-in name, or a name in `[roles.*]`.
pub open spec fn declared(presets: Seq<int>, name: int) -> bool {
    (0 <= name <= 2) || presets.contains(name)
}

/// The preset names are valid: no built-in name, no duplicate.
pub open spec fn valid_presets(presets: Seq<int>) -> bool {
    &&& forall|k: int| 0 <= k < presets.len() ==> presets[k] > 2
    &&& forall|a: int, b: int| 0 <= a < b < presets.len() ==> presets[a] != presets[b]
}

/// The role set with no `--roles`: `combined` alone, or `web`, `worker`
/// and each preset.
pub open spec fn default_set(presets: Seq<int>, split: bool) -> Seq<int> {
    if split || presets.len() > 0 {
        seq![web(), worker()] + presets
    } else {
        seq![combined()]
    }
}

/// The objects that one manifest role gets.
pub struct Objects {
    pub workload: bool,
    pub service: bool,
}

/// The template rule: a workload always, a Service only for HTTP.
pub open spec fn objects(base: Base) -> Objects {
    Objects { workload: true, service: serves_http(base) }
}

/// Each role in the default set is declared.
pub proof fn default_set_is_declared(presets: Seq<int>, split: bool, i: int)
    requires
        0 <= i < default_set(presets, split).len(),
    ensures
        declared(presets, default_set(presets, split)[i]),
{
    if split || presets.len() > 0 {
        let s = seq![web(), worker()] + presets;
        if i >= 2 {
            assert(s[i] == presets[i - 2]);
            assert(presets.contains(presets[i - 2]));
        }
    }
}

/// The default set has no duplicate names.
pub proof fn default_set_is_distinct(presets: Seq<int>, split: bool, i: int, j: int)
    requires
        valid_presets(presets),
        0 <= i < j < default_set(presets, split).len(),
    ensures
        default_set(presets, split)[i] != default_set(presets, split)[j],
{
    if split || presets.len() > 0 {
        let s = seq![web(), worker()] + presets;
        if j >= 2 {
            assert(s[j] == presets[j - 2]);
            if i >= 2 {
                assert(s[i] == presets[i - 2]);
            }
        }
    }
}

/// Every role gets a workload.
pub proof fn every_role_gets_a_workload(base: Base)
    ensures
        objects(base).workload,
{
}

/// A worker gets no Service, so no traffic reaches it.
pub proof fn worker_gets_no_service(base: Base)
    requires
        base is Worker,
    ensures
        !objects(base).service,
{
}

/// A role that serves HTTP gets a Service.
pub proof fn http_role_gets_a_service(base: Base)
    requires
        serves_http(base),
    ensures
        objects(base).service,
{
}

/// One fleet host and the role of its group. `base` is `Role::guard_base`.
pub struct HostRole {
    pub host: int,
    pub role: int,
    pub base: Base,
}

/// The checks of `group_hosts` and `grade_group_roles`: each role is
/// declared, and no host is in two groups.
pub open spec fn valid_fleet(presets: Seq<int>, plan: Seq<HostRole>) -> bool {
    &&& forall|k: int| 0 <= k < plan.len() ==> declared(presets, plan[k].role)
    &&& forall|a: int, b: int| 0 <= a < b < plan.len() ==> plan[a].host != plan[b].host
}

/// The boot rule `split_role_requires_durable_backend`.
pub open spec fn needs_durable_backend(base: Base) -> bool {
    !(base is Combined)
}

/// The preflight check: no split role on a local jobs backend.
pub open spec fn preflight_passes(plan: Seq<HostRole>, local_backend: bool) -> bool {
    forall|k: int| 0 <= k < plan.len() ==> !(local_backend && needs_durable_backend(plan[k].base))
}

/// A fleet host never gets two roles.
pub proof fn one_role_per_host(presets: Seq<int>, plan: Seq<HostRole>, a: int, b: int)
    requires
        valid_fleet(presets, plan),
        0 <= a < plan.len(),
        0 <= b < plan.len(),
        plan[a].host == plan[b].host,
    ensures
        plan[a].role == plan[b].role,
{
    if a < b {
        assert(plan[a].host != plan[b].host);
    } else if b < a {
        assert(plan[b].host != plan[a].host);
    }
}

/// Each host in a valid fleet runs a declared role.
pub proof fn fleet_roles_are_declared(presets: Seq<int>, plan: Seq<HostRole>, k: int)
    requires
        valid_fleet(presets, plan),
        0 <= k < plan.len(),
    ensures
        declared(presets, plan[k].role),
{
}

/// A fleet that passes preflight on a local backend runs only `combined`.
pub proof fn local_backend_runs_only_combined(plan: Seq<HostRole>, k: int)
    requires
        preflight_passes(plan, true),
        0 <= k < plan.len(),
    ensures
        plan[k].base is Combined,
{
    assert(!(true && needs_durable_backend(plan[k].base)));
}

fn main() {
}

} // verus!
