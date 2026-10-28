use vstd::prelude::*;

verus! {

// The per-tenant bulkhead in `autumn/src/bulkhead.rs`.
//
// The runtime keeps a `HashMap<String, usize>` of permits out per tenant,
// under one lock. Here the map is a `Map<int, nat>` from tenant to count.
// `cap == 0` means no cap. `tests/shuttle_models.rs` and
// `tests/loom_models.rs` run the real code under concurrent callers.

/// A tenant has an entry only while it holds a permit, and never holds more
/// than the cap. Thus the map is at most as large as the permits out, and
/// tenant ids from requests cannot grow it.
pub open spec fn inv(m: Map<int, nat>, cap: nat) -> bool {
    forall|t: int| #[trigger] m.contains_key(t) ==> 1 <= m[t] && (cap > 0 ==> m[t] <= cap)
}

/// The permits that `tenant` holds.
pub open spec fn held(m: Map<int, nat>, tenant: int) -> nat {
    if m.contains_key(tenant) {
        m[tenant]
    } else {
        0
    }
}

/// `TenantBulkhead::try_acquire`: the new map, and whether a permit was
/// granted.
pub open spec fn acquire(m: Map<int, nat>, cap: nat, tenant: int) -> (Map<int, nat>, bool) {
    let h = held(m, tenant);
    if cap != 0 && h >= cap {
        (m, false)
    } else {
        (m.insert(tenant, h + 1), true)
    }
}

/// `TenantBulkhead::release`, from `TenantPermit::drop`.
pub open spec fn release(m: Map<int, nat>, tenant: int) -> Map<int, nat> {
    if m.contains_key(tenant) {
        if m[tenant] <= 1 {
            m.remove(tenant)
        } else {
            m.insert(tenant, (m[tenant] - 1) as nat)
        }
    } else {
        m
    }
}

/// Both steps keep the invariant.
pub proof fn steps_keep_the_invariant(m: Map<int, nat>, cap: nat, tenant: int)
    requires
        inv(m, cap),
    ensures
        inv(acquire(m, cap, tenant).0, cap),
        inv(release(m, tenant), cap),
{
    let (after, _) = acquire(m, cap, tenant);
    assert forall|t: int| #[trigger] after.contains_key(t) implies 1 <= after[t] && (cap > 0
        ==> after[t] <= cap) by {
        if t != tenant {
            assert(m.contains_key(t));
        }
    }
    let freed = release(m, tenant);
    assert forall|t: int| #[trigger] freed.contains_key(t) implies 1 <= freed[t] && (cap > 0
        ==> freed[t] <= cap) by {
        assert(m.contains_key(t));
    }
}

/// A grant is refused only at the cap, and never when there is no cap.
pub proof fn refused_only_at_the_cap(m: Map<int, nat>, cap: nat, tenant: int)
    ensures
        !acquire(m, cap, tenant).1 ==> cap > 0 && held(m, tenant) >= cap,
        cap == 0 ==> acquire(m, cap, tenant).1,
{}

/// One tenant's grant never touches another tenant's count.
pub proof fn tenants_are_isolated(m: Map<int, nat>, cap: nat, tenant: int, other: int)
    requires
        tenant != other,
    ensures
        held(acquire(m, cap, tenant).0, other) == held(m, other),
        held(release(m, tenant), other) == held(m, other),
{}

/// A permit that comes back leaves no trace: the map is the same as before
/// the grant. This is why the map cannot leak entries.
pub proof fn a_permit_round_trip_leaves_no_trace(m: Map<int, nat>, cap: nat, tenant: int)
    requires
        inv(m, cap),
        acquire(m, cap, tenant).1,
    ensures
        release(acquire(m, cap, tenant).0, tenant) =~= m,
{
    if m.contains_key(tenant) {
        assert(m[tenant] >= 1);
        assert(m.insert(tenant, m[tenant]) =~= m);
    }
}

fn main() {}

} // verus!
