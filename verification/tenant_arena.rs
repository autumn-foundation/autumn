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

pub open spec fn invariant(s: ArenaState) -> bool {
    &&& (!s.finite || s.usage <= s.quota)
    &&& s.reachable <= s.usage
}

pub open spec fn same_domain(pre: ArenaState, post: ArenaState) -> bool {
    pre.tenant == post.tenant && pre.domain == post.domain
}

pub proof fn successful_allocation(pre: ArenaState, bytes: nat, post: ArenaState)
    requires
        invariant(pre),
        !pre.finite || pre.usage + bytes <= pre.quota,
        same_domain(pre, post),
        post.finite == pre.finite,
        post.quota == pre.quota,
        post.usage == pre.usage + bytes,
        post.reachable == pre.reachable + bytes,
    ensures invariant(post), same_domain(pre, post)
{}

pub proof fn failed_reservation_does_not_mutate(pre: ArenaState, post: ArenaState)
    requires
        invariant(pre),
        post == pre,
    ensures
        invariant(post),
        post.usage == pre.usage,
        post.reachable == pre.reachable,
        same_domain(pre, post),
{}

pub proof fn final_teardown(pre: ArenaState, post: ArenaState)
    requires
        invariant(pre),
        same_domain(pre, post),
        post.finite == pre.finite,
        post.quota == pre.quota,
        post.usage == 0,
        post.reachable == 0,
    ensures invariant(post), post.reachable == 0
{}

} // verus!
