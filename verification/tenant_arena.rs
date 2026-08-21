use vstd::prelude::*;

verus! {

/// A mathematical registry is a function-like map: a tenant key can resolve
/// to at most one accounting-domain value.
pub struct RegistryModel {
    pub domains: Map<nat, nat>,
}

pub proof fn one_accounting_domain_per_tenant(
    registry: RegistryModel, tenant: nat, first: nat, second: nat)
    requires
        registry.domains.contains_key(tenant),
        first == registry.domains[tenant],
        second == registry.domains[tenant],
    ensures first == second
{}

pub struct ArenaState {
    pub tenant: nat,
    pub domain: nat,
    pub quota: nat,
    pub finite: bool,
    pub usage: nat,
    pub reachable: nat,
}

pub open spec fn arena_invariant(s: ArenaState) -> bool {
    &&& (!s.finite || s.usage <= s.quota)
    &&& s.reachable <= s.usage
}

pub open spec fn same_domain(pre: ArenaState, post: ArenaState) -> bool {
    pre.tenant == post.tenant && pre.domain == post.domain
}

/// The final-drop transition itself constructs the reclaimed state. Keeping
/// this definition separate from the proof prevents callers from assuming the
/// zeroed post-state that reclamation is meant to establish.
pub open spec fn teardown_transition(pre: ArenaState) -> ArenaState {
    ArenaState {
        tenant: pre.tenant,
        domain: pre.domain,
        quota: pre.quota,
        finite: pre.finite,
        usage: 0,
        reachable: 0,
    }
}

/// Registry eviction drops residency only. While an allocation/reference is
/// reachable, rebinding the tenant must preserve its domain and usage rather
/// than manufacture a zeroed generation.
pub open spec fn eviction_transition(pre: ArenaState) -> ArenaState {
    ArenaState {
        tenant: pre.tenant,
        domain: pre.domain,
        quota: pre.quota,
        finite: pre.finite,
        usage: pre.usage,
        reachable: pre.reachable,
    }
}

pub proof fn successful_allocation(pre: ArenaState, bytes: nat, post: ArenaState)
    requires
        arena_invariant(pre),
        !pre.finite || pre.usage + bytes <= pre.quota,
        same_domain(pre, post),
        post.finite == pre.finite,
        post.quota == pre.quota,
        post.usage == pre.usage + bytes,
        post.reachable == pre.reachable + bytes,
    ensures arena_invariant(post), same_domain(pre, post)
{}

pub proof fn failed_reservation_does_not_mutate(pre: ArenaState, post: ArenaState)
    requires
        arena_invariant(pre),
        post == pre,
    ensures
        arena_invariant(post),
        post.usage == pre.usage,
        post.reachable == pre.reachable,
        same_domain(pre, post),
{}

pub proof fn rebind_live_domain_preserves_accounting(pre: ArenaState)
    requires arena_invariant(pre), pre.reachable > 0
    ensures
        arena_invariant(eviction_transition(pre)),
        same_domain(pre, eviction_transition(pre)),
        eviction_transition(pre).usage == pre.usage,
        eviction_transition(pre).reachable == pre.reachable,
{}

pub proof fn final_teardown_reclaims_all(pre: ArenaState)
    requires arena_invariant(pre)
    ensures
        arena_invariant(teardown_transition(pre)),
        same_domain(pre, teardown_transition(pre)),
        teardown_transition(pre).usage == 0,
        teardown_transition(pre).reachable == 0,
{}

fn main() {}

} // verus!
