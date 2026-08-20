//! Unit/behaviour tests for per-tenant memory accounting cells (issue #1766).
//!
//! These exercise the public [`TenantCell`]/[`TenantCellRegistry`] API directly
//! and need no HTTP or database scaffolding, so they carry no feature gate.

use autumn_web::tenant_cell::{QuotaExceeded, TenantCell, TenantCellRegistry};
use std::sync::Arc;
use std::time::Duration;

/// A charge raises both the per-tenant and process-wide gauges by exactly its
/// size, and dropping it returns them to zero.
#[test]
fn charge_release_monotonicity() {
    let registry = TenantCellRegistry::new();
    let cell = registry.get_or_create("t", 0); // unlimited

    let charge = cell
        .try_charge(1000)
        .expect("unlimited charge should succeed");
    assert_eq!(cell.tracked_bytes(), 1000);
    assert_eq!(registry.total_tracked_bytes(), 1000);
    assert_eq!(charge.bytes(), 1000);

    drop(charge);
    assert_eq!(cell.tracked_bytes(), 0);
    assert_eq!(registry.total_tracked_bytes(), 0);
}

/// The quota gate admits a charge that fits, rejects the one that would overflow
/// with a fully-populated [`QuotaExceeded`], and admits again once space frees.
#[test]
fn quota_gate_one_over_one_under() {
    let registry = TenantCellRegistry::new();
    let cell = registry.get_or_create("t", 1000);

    let first = cell.try_charge(600).expect("first 600 fits under 1000");
    assert_eq!(cell.tracked_bytes(), 600);

    // Second 600 would bring us to 1200 > 1000 while the first is still alive.
    let err = cell
        .try_charge(600)
        .expect_err("second 600 must exceed the quota");
    assert_eq!(
        err,
        QuotaExceeded {
            tenant_id: "t".to_string(),
            requested: 600,
            in_use: 600,
            quota: 1000,
        }
    );
    // The failed charge must not have mutated the counter.
    assert_eq!(cell.tracked_bytes(), 600);

    drop(first);
    assert_eq!(cell.tracked_bytes(), 0);
    let _second = cell
        .try_charge(600)
        .expect("after releasing the first, 600 fits again");
    assert_eq!(cell.tracked_bytes(), 600);
}

/// One tenant exhausting its quota must not affect another tenant's ability to
/// charge — cells are independent accounting boundaries.
#[test]
fn per_tenant_isolation() {
    let registry = TenantCellRegistry::new();
    let a = registry.get_or_create("a", 1000);
    let b = registry.get_or_create("b", 1000);

    // Saturate "a" so its next charge errors.
    let _saturate = a.try_charge(1000).expect("a can fill to its quota");
    a.try_charge(1)
        .expect_err("a is saturated and must reject further charges");

    // "b" is untouched and can still charge freely.
    let _b_charge = b
        .try_charge(1000)
        .expect("b is independent and can charge to its own quota");
    assert_eq!(b.tracked_bytes(), 1000);
}

/// Scratch inserts/removes track their byte size, and replacing an existing key
/// nets the counter to the new value's size.
#[test]
fn scratch_accounting() {
    let registry = TenantCellRegistry::new();
    let cell = registry.get_or_create("t", 0);

    // A new key is charged its own allocation capacity in addition to the value.
    // The key is short, so its contribution is small and stable across ops on
    // the same key (which don't re-charge the key).
    let kc = String::from("k").capacity();
    // Each resident scratch entry also carries a fixed per-entry overhead,
    // charged once on the new-key insert and released on removal.
    let overhead = TenantCell::scratch_entry_overhead();

    cell.scratch_insert("k", vec![0u8; 500])
        .expect("insert under unlimited quota");
    assert_eq!(cell.tracked_bytes(), kc + 500 + overhead);
    assert_eq!(cell.scratch_get("k"), Some(vec![0u8; 500]));

    // Replacing "k" with a larger value nets to the new value size (500 -> 800);
    // the stored key is unchanged, so only the value delta is charged.
    cell.scratch_insert("k", vec![1u8; 800])
        .expect("replace value under unlimited quota");
    assert_eq!(cell.tracked_bytes(), kc + 800 + overhead);
    assert_eq!(cell.scratch_get("k"), Some(vec![1u8; 800]));

    let removed = cell.scratch_remove("k");
    assert_eq!(removed, Some(vec![1u8; 800]));
    // The key and value heap are released, but the fixed per-entry overhead is
    // retained against the entry high-water mark until the cell is evicted
    // (`HashMap` keeps its enlarged bucket array), so tracked settles at exactly
    // one overhead, not zero.
    assert_eq!(cell.tracked_bytes(), overhead);
    assert_eq!(cell.scratch_get("k"), None);
}

/// Evicting a cell with residual tracked bytes and dropping the last strong
/// reference must return the process-wide gauge to zero. Zero can only hold if
/// `TenantCellInner::drop` ran, proving the memory was deterministically
/// reclaimed.
#[test]
fn eviction_reclaims_to_zero() {
    let registry = TenantCellRegistry::new();

    {
        let cell = registry.get_or_create("t", 0);
        cell.scratch_insert("k", vec![0u8; 256])
            .expect("insert under unlimited quota");
        // Drop our local strong reference; only the registry holds one now.
    }
    assert!(registry.total_tracked_bytes() > 0);
    assert_eq!(registry.len(), 1);

    let evicted = registry.evict("t").expect("cell was resident");
    // Registry no longer references the cell; `evicted` is the sole strong ref.
    drop(evicted);

    assert!(registry.get("t").is_none());
    assert_eq!(registry.len(), 0);
    assert_eq!(
        registry.total_tracked_bytes(),
        0,
        "residual bytes must be reclaimed once the cell is dropped"
    );
}

/// Eviction ends cache residency, not the tenant's accounting lifetime. An
/// in-flight owner forces a subsequent lookup to rejoin the same quota counter
/// and scratch domain until every owner has gone away.
#[test]
fn eviction_does_not_split_a_live_tenant_accounting_domain() {
    let registry = TenantCellRegistry::new();
    let old = registry.get_or_create("t", 1_000);
    old.scratch_insert("shared", vec![7; 100])
        .expect("initial scratch allocation fits");
    let initial = old.tracked_bytes();
    let old_charge = old.try_charge(600).expect("initial charge fits");
    let in_flight = Arc::clone(&old);

    let evicted = registry.evict("t").expect("tenant was resident");
    assert_eq!(registry.len(), 0, "eviction removes cache residency");

    let replacement = registry.get_or_create("t", 1_000);
    assert_eq!(
        replacement.scratch_get("shared"),
        Some(vec![7; 100]),
        "the replacement must use the still-live scratch domain"
    );
    let remaining = 1_000 - initial - old_charge.bytes();
    replacement
        .try_charge(remaining + 1)
        .expect_err("old and new requests share one aggregate tenant quota");
    assert_eq!(replacement.tracked_bytes(), initial + 600);

    drop(old_charge);
    drop(old);
    drop(in_flight);
    drop(evicted);
    let final_resident = registry
        .evict("t")
        .expect("resurrected cell remains resident until explicitly evicted");
    drop(replacement);
    drop(final_resident);
    assert_eq!(registry.total_tracked_bytes(), 0);
}

/// Density smoke test: 1000 concurrent cells each holding a small buffer track
/// exactly, and evicting all of them reclaims everything.
#[test]
fn density_smoke_thousand_cells() {
    const CELLS: usize = 1000;
    const BUF: usize = 16;

    // Each cell is charged its value buffer plus the "buf" key's capacity and a
    // fixed per-entry overhead for the one scratch entry it stores.
    let kc = String::from("buf").capacity();
    let overhead = TenantCell::scratch_entry_overhead();

    let registry = TenantCellRegistry::new();
    for i in 0..CELLS {
        let cell = registry.get_or_create(&format!("tenant-{i}"), 0);
        cell.scratch_insert("buf", vec![0u8; BUF])
            .expect("insert under unlimited quota");
    }

    assert_eq!(registry.len(), CELLS);
    assert_eq!(
        registry.total_tracked_bytes(),
        CELLS * (BUF + kc + overhead)
    );
    println!(
        "size_of::<TenantCell>() = {} bytes (fixed per-cell handle overhead)",
        std::mem::size_of::<autumn_web::tenant_cell::TenantCell>()
    );

    for i in 0..CELLS {
        let evicted = registry
            .evict(&format!("tenant-{i}"))
            .expect("each cell was resident");
        drop(evicted);
    }

    assert_eq!(registry.len(), 0);
    assert_eq!(registry.total_tracked_bytes(), 0);
}

/// Replacing an existing scratch key must account only for the net byte delta,
/// not the full new size, so a same-size or shrinking replace can never
/// transiently overshoot the quota and spuriously return `QuotaExceeded`.
#[test]
fn scratch_insert_replace_accounts_net_delta() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 1000);

    // The single short key "k" is charged its capacity once (on the first,
    // new-key insert). Every later op below reuses that same key, so it is never
    // re-charged; the value deltas are what this test exercises. `kc` accounts
    // for that fixed key contribution so the value math stays exact.
    let kc = String::from("k").capacity();
    // The per-entry overhead is charged once, on the first new-key insert; later
    // same-key replaces re-use the entry and never re-charge it.
    let overhead = TenantCell::scratch_entry_overhead();

    cell.scratch_insert("k", vec![0u8; 800]).unwrap();
    assert_eq!(cell.tracked_bytes(), kc + 800 + overhead);

    // Replace with same size: net 0. Must NOT error even though 800 + 800 > 1000.
    cell.scratch_insert("k", vec![0u8; 800]).unwrap();
    assert_eq!(cell.tracked_bytes(), kc + 800 + overhead);

    // Grow to exactly the quota (key + value + overhead == 1000).
    cell.scratch_insert("k", vec![0u8; 1000 - kc - overhead])
        .unwrap();
    assert_eq!(cell.tracked_bytes(), 1000);

    // Shrink releases the delta.
    cell.scratch_insert("k", vec![0u8; 100]).unwrap();
    assert_eq!(cell.tracked_bytes(), kc + 100 + overhead);

    // A genuine over-quota insert on a NEW key still fails and leaves state intact.
    cell.scratch_insert("k", vec![0u8; 900]).unwrap();
    assert_eq!(cell.tracked_bytes(), kc + 900 + overhead);
    let err = cell.scratch_insert("j", vec![0u8; 200]).unwrap_err();
    assert_eq!(err.quota, 1000);
    assert_eq!(err.in_use, kc + 900 + overhead);
    assert_eq!(cell.tracked_bytes(), kc + 900 + overhead);
}

/// Scratch accounting must charge the stored `String` key's allocation
/// capacity in addition to the value, so a large unique/user-derived key counts
/// against the quota and is released on removal. A genuinely new key is charged
/// for `key.capacity() + value.capacity()`; a huge key on an empty value is now
/// rejected by the quota even though only the value was charged before.
#[test]
fn scratch_insert_accounts_key_capacity() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 1000);

    // Empty value but a large key: the key allocation must be charged, along
    // with the fixed per-entry overhead.
    let overhead = TenantCell::scratch_entry_overhead();
    let key = "k".repeat(500); // String capacity >= 500
    let key_cap = key.capacity();
    cell.scratch_insert(key.clone(), Vec::new()).unwrap();
    assert_eq!(cell.tracked_bytes(), key_cap + overhead);
    assert!(cell.tracked_bytes() >= 500);

    // Removing releases the key allocation, but the per-entry overhead is
    // retained against the high-water mark until eviction, so tracked settles
    // at exactly one overhead rather than zero.
    let _ = cell.scratch_remove(&key);
    assert_eq!(cell.tracked_bytes(), overhead);

    // A huge unique key on an empty value is now rejected by the quota
    // (it was accepted when only value capacity was charged). The rejected
    // insert leaves the counter at the retained overhead, unchanged.
    let big_key = "x".repeat(5000);
    let err = cell.scratch_insert(big_key, Vec::new()).unwrap_err();
    assert_eq!(err.quota, 1000);
    assert_eq!(cell.tracked_bytes(), overhead);
}

/// Scratch accounting must charge a value's allocation *capacity*, not its
/// length — the cell owns the whole allocation, so a `Vec` with large spare
/// capacity (or a buffer truncated after decoding) must count its capacity
/// against the quota.
#[test]
fn scratch_insert_accounts_capacity_not_length() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 1000);

    // A Vec with large spare capacity but small length must be charged its
    // capacity — the cell owns the whole allocation. The short key "k" adds its
    // own (small) capacity on this new-key insert.
    let kc = String::from("k").capacity();
    let overhead = TenantCell::scratch_entry_overhead();
    let mut v = Vec::with_capacity(800);
    v.extend_from_slice(&[0u8; 8]); // len 8, capacity >= 800
    let cap = v.capacity();
    cell.scratch_insert("k", v).unwrap();
    assert_eq!(cell.tracked_bytes(), kc + cap + overhead);
    assert!(cell.tracked_bytes() >= 800);

    // Removing releases the full value capacity and key, but the per-entry
    // overhead is retained against the high-water mark until eviction, so
    // tracked settles at exactly one overhead rather than zero.
    let _ = cell.scratch_remove("k");
    assert_eq!(cell.tracked_bytes(), overhead);

    // An over-capacity empty Vec is now rejected by the quota (it was accepted
    // when accounting by len()). The rejected insert leaves the counter at the
    // retained overhead, unchanged.
    let big = Vec::<u8>::with_capacity(5000); // len 0, capacity >= 5000
    let err = cell.scratch_insert("k", big).unwrap_err();
    assert_eq!(err.quota, 1000);
    assert!(err.requested >= 5000);
    assert_eq!(cell.tracked_bytes(), overhead);
}

#[test]
fn scratch_entries_charge_fixed_overhead() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let overhead = autumn_web::tenant_cell::TenantCell::scratch_entry_overhead();
    assert!(overhead > 0);

    let cell = reg.get_or_create("t", 1_000_000);
    // An empty key AND empty value still costs the fixed per-entry overhead.
    cell.scratch_insert(String::new(), Vec::new()).unwrap();
    assert_eq!(cell.tracked_bytes(), overhead);

    // A second distinct entry costs another overhead.
    cell.scratch_insert("a", Vec::new()).unwrap();
    let a_key_cap = String::from("a").capacity();
    assert_eq!(cell.tracked_bytes(), overhead + overhead + a_key_cap);

    // Removing the empty entry releases only its key/value heap (both empty
    // here, so nothing), NOT the per-entry overhead: it is retained against the
    // high-water mark until eviction, so both entries' overheads remain charged.
    let _ = cell.scratch_remove("");
    assert_eq!(cell.tracked_bytes(), overhead + overhead + a_key_cap);
}

#[test]
fn scratch_overhead_retained_after_removal_and_churn_safe() {
    let overhead = autumn_web::tenant_cell::TenantCell::scratch_entry_overhead();
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 1_000_000);

    for i in 0..100 {
        cell.scratch_insert(format!("k{i}"), Vec::new()).unwrap();
    }
    let peak_tracked = cell.tracked_bytes();
    assert!(peak_tracked >= 100 * overhead);

    for i in 0..100 {
        let _ = cell.scratch_remove(&format!("k{i}"));
    }
    // Key/value heap released, but the per-entry (bucket) overhead is retained
    // until eviction — tracked does NOT fall to zero.
    assert!(cell.tracked_bytes() >= 100 * overhead);

    // Re-inserting within the prior peak charges NO new overhead (churn-safe):
    let before = cell.tracked_bytes();
    for i in 0..100 {
        cell.scratch_insert(format!("k{i}"), Vec::new()).unwrap();
    }
    // Only tiny key heaps added back; overhead unchanged (len never exceeds peak).
    assert!(cell.tracked_bytes() < before + 100 * overhead);

    // Eviction still reclaims everything to zero.
    let _ = reg.evict("t");
    drop(cell);
    assert_eq!(reg.total_tracked_bytes(), 0);
}

#[test]
fn many_tiny_scratch_entries_are_bounded_by_quota() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 2000);
    let mut n = 0usize;
    while cell.scratch_insert(format!("k{n}"), Vec::new()).is_ok() {
        n += 1;
        assert!(n <= 10_000, "quota failed to bound scratch entry count");
    }
    // With a fixed per-entry overhead, only a small number of tiny entries fit.
    assert!(n < 100, "expected quota to bound tiny-entry count, got {n}");
}

#[test]
fn handle_caches_cell_across_mid_request_eviction() {
    let reg = autumn_web::tenant_cell::TenantCellRegistry::new();
    let handle =
        autumn_web::tenant_cell::TenantCellHandle::new(reg.clone(), "t".to_string(), 1_000_000);

    let cell1 = handle.cell();
    cell1.scratch_insert("k", vec![0u8; 100]).unwrap();
    let tracked = cell1.tracked_bytes();

    // Admin evicts the tenant while the request is still in flight.
    let _ = reg.evict("t");

    // A later access in the SAME request must return the SAME cell — not a fresh
    // empty one — so scratch state and quota usage survive to request end.
    let cell2 = handle.cell();
    assert!(std::sync::Arc::ptr_eq(&cell1, &cell2));
    assert_eq!(cell2.tracked_bytes(), tracked);
    assert_eq!(cell2.scratch_get("k").map(|v| v.len()), Some(100));
}

/// A resident cell refreshes its soft quota from the latest `get_or_create`
/// value (#1783): the same cell is returned, its quota reflects the new value,
/// and `reserve` honors the atomically-updated quota — a charge that overflowed
/// the original quota succeeds once the quota is raised.
#[test]
fn resident_cell_quota_refreshes_on_get_or_create() {
    let reg = TenantCellRegistry::new();
    let cell_a = reg.get_or_create("t", 1000);

    // 1500 exceeds the initial quota of 1000.
    cell_a
        .try_charge(1500)
        .expect_err("1500 must exceed the initial quota of 1000");

    // Re-fetching with a larger quota returns the SAME cell with its quota
    // atomically refreshed — no eviction/rebuild.
    let cell_b = reg.get_or_create("t", 2000);
    assert!(Arc::ptr_eq(&cell_a, &cell_b));
    assert_eq!(cell_b.quota_bytes(), 2000);

    // The same charge now fits under the refreshed quota, proving `reserve`
    // reads the atomic quota rather than a cached value.
    let charge = cell_b
        .try_charge(1500)
        .expect("1500 fits under the refreshed quota of 2000");
    assert_eq!(cell_b.tracked_bytes(), 1500);
    drop(charge);
}

/// Dynamic quota (`set_quota_bytes`) with `0 == unlimited` must be honored on
/// every `reserve` retry, not just on entry: flipping a resident cell's finite
/// quota to `0` mid-life makes a subsequent charge that far exceeds the old
/// finite quota succeed (unlimited), while a finite quota still gates an
/// over-limit charge with `QuotaExceeded`.
#[test]
fn dynamic_quota_zero_is_unlimited_after_refresh() {
    let reg = TenantCellRegistry::new();
    let cell = reg.get_or_create("t", 100);

    // Charge some bytes under the finite quota of 100.
    let charge = cell.try_charge(50).expect("50 fits under the quota of 100");
    assert_eq!(cell.tracked_bytes(), 50);

    // A charge over the finite quota is still rejected while it is non-zero.
    cell.try_charge(100)
        .expect_err("50 + 100 = 150 must exceed the finite quota of 100");

    // Flip the quota to unlimited at runtime.
    cell.set_quota_bytes(0);
    assert_eq!(cell.quota_bytes(), 0);

    // A charge far exceeding the old finite quota (and current tracked) now
    // succeeds — `reserve` re-applies `0 == unlimited` on the atomic reload
    // rather than comparing against `0`.
    let big = cell
        .try_charge(10_000)
        .expect("under an unlimited quota, a large charge must succeed");
    assert_eq!(cell.tracked_bytes(), 10_050);
    drop(big);
    drop(charge);
    assert_eq!(cell.tracked_bytes(), 0);

    // Restoring a finite quota re-gates: an over-limit charge fails again.
    cell.set_quota_bytes(1000);
    let err = cell
        .try_charge(1001)
        .expect_err("1001 must exceed the restored finite quota of 1000");
    assert_eq!(err.quota, 1000);
    assert_eq!(cell.tracked_bytes(), 0);
}

/// A bounded registry evicts the least-recently-used cell once resident cells
/// exceed the cap (#1792). Inserting t1, t2, t3 into a cap-2 registry drives the
/// access order t1 < t2 < t3 via the globally-monotonic access sequence (no
/// sleeps needed), so t1 is unambiguously the LRU victim and t2/t3 are kept.
#[test]
fn registry_bounded_capacity_evicts_lru() {
    let reg = TenantCellRegistry::with_limits(2, None);
    assert_eq!(reg.max_cells(), 2);

    // Each get_or_create draws a strictly-greater access sequence, so insertion
    // order alone establishes t1 < t2 < t3 — no wall-clock spacing required.
    let _t1 = reg.get_or_create("t1", 0);
    let _t2 = reg.get_or_create("t2", 0);
    let _t3 = reg.get_or_create("t3", 0);

    // t3's insert pushed the count to 3 > 2, evicting the LRU cell (t1).
    assert_eq!(reg.len(), 2);
    assert!(
        reg.get("t1").is_none(),
        "t1 was the LRU and must be evicted"
    );
    assert!(reg.get("t2").is_some());
    assert!(reg.get("t3").is_some());
}

/// An idle-TTL registry evicts a cell whose last access is older than the TTL
/// (#1792). t1 goes stale across a sleep; creating t2 triggers the idle sweep
/// (and `evict_idle_older_than` is the explicit equivalent), removing t1 while
/// the freshly-touched t2 survives.
#[test]
fn registry_idle_ttl_evicts_stale() {
    let reg = TenantCellRegistry::with_limits(0, Some(Duration::from_millis(40)));
    assert_eq!(reg.idle_ttl(), Some(Duration::from_millis(40)));

    let _t1 = reg.get_or_create("t1", 0);
    // Let t1 age well past the 40ms TTL before touching the registry again.
    std::thread::sleep(Duration::from_millis(60));

    // Creating t2 runs the auto idle-sweep: t1 (idle > 40ms) is evicted, and the
    // just-touched t2 is retained.
    let _t2 = reg.get_or_create("t2", 0);
    assert!(reg.get("t1").is_none(), "t1 aged past the idle TTL");
    assert!(
        reg.get("t2").is_some(),
        "t2 was just touched and must survive"
    );
    assert_eq!(reg.len(), 1);
}

/// Auto-eviction removes only the registry's strong reference (#1792): a cell
/// evicted to honor the capacity bound while a request still holds its
/// `Arc<TenantCell>` stays fully usable, and its tracked bytes reclaim
/// deterministically once that held reference drops — never mid-request.
#[test]
fn auto_eviction_preserves_in_flight_cell() {
    let reg = TenantCellRegistry::with_limits(1, None);

    // t1 is the "in-flight" cell: create it, charge some persistent scratch
    // bytes, and keep its Arc as a request would.
    let t1 = reg.get_or_create("t1", 1_000_000);
    t1.scratch_insert("k", vec![0u8; 256])
        .expect("insert under a generous quota");
    let tracked = t1.tracked_bytes();
    assert!(tracked > 0);
    assert_eq!(reg.total_tracked_bytes(), tracked);

    // Space the second creation out so t1 is unambiguously the LRU victim.
    std::thread::sleep(Duration::from_millis(5));
    let _t2 = reg.get_or_create("t2", 1_000_000);

    // t1 was evicted from the registry (cap 1) but the held Arc keeps it alive.
    assert!(reg.get("t1").is_none(), "t1 evicted from the registry");
    assert_eq!(reg.len(), 1);

    // The in-flight cell is still fully functional: its scratch, quota, and
    // charging all work against the same accounting state.
    assert_eq!(t1.tracked_bytes(), tracked);
    assert_eq!(t1.quota_bytes(), 1_000_000);
    assert_eq!(t1.scratch_get("k").map(|v| v.len()), Some(256));
    let charge = t1
        .try_charge(100)
        .expect("the evicted-but-held cell still charges");
    drop(charge);
    assert_eq!(t1.tracked_bytes(), tracked);

    // t1's bytes are still counted process-wide while the request holds it.
    assert_eq!(reg.total_tracked_bytes(), tracked);

    // Dropping the last strong reference reclaims t1's footprint; only the empty
    // t2 remains resident, so the process-wide gauge returns to zero.
    drop(t1);
    assert_eq!(
        reg.total_tracked_bytes(),
        0,
        "held cell's bytes reclaim deterministically on drop"
    );
}

/// Millisecond-resolution access stamps tie when several tenants are created in
/// the same tick, which could let LRU eviction drop the cell `get_or_create`
/// just inserted. The globally-monotonic access sequence breaks those ties so
/// the newest cell is never the victim. A same-ms burst of 5 distinct tenants
/// into a cap-2 registry (no sleeps) must keep exactly 2 cells, with the
/// last-created tenant always resident and an early tenant evicted.
#[test]
fn registry_lru_tiebreak_keeps_newest_on_burst() {
    let registry = TenantCellRegistry::with_limits(2, None);

    // Rapid same-tick burst: no sleeps, so the cells' millisecond access stamps
    // may all tie — only the access sequence disambiguates their LRU order.
    for i in 0..5 {
        let _ = registry.get_or_create(&format!("t{i}"), 0);
    }

    // The cap is honored exactly.
    assert_eq!(registry.len(), 2);
    // The just-inserted cell (t4 holds the greatest access sequence) is never
    // chosen as the LRU victim, so the newest tenant stays resident.
    assert!(
        registry.get("t4").is_some(),
        "the last-created tenant must remain resident under the cap"
    );
    // An early tenant was evicted as the least-recently-used.
    assert!(
        registry.get("t0").is_none(),
        "an early tenant must be evicted under the cap"
    );
}

/// A `get` on a resident cell refreshes its access sequence under the read lock
/// (Fix 1's touch fires): the cell's `last_access_seq` strictly increases across
/// the lookup, proving the access time is refreshed before the guard drops.
#[test]
fn get_touch_raises_access_seq() {
    let registry = TenantCellRegistry::new();
    let cell = registry.get_or_create("t", 0);
    let before = cell.last_access_seq();

    let fetched = registry.get("t").expect("cell is resident");
    assert!(
        fetched.last_access_seq() > before,
        "get() must raise the resident cell's access sequence"
    );
}

/// `touch` stores its timestamp and sequence monotonically (`fetch_max`), so a
/// later `touch` carrying an *older* time/sequence — e.g. one captured before a
/// lock wait and applied out of order — can never regress the recorded access
/// time or sequence below a fresher value already stamped. This is what keeps a
/// just-accessed tenant from being seen as idle (and evicted) because of a
/// lagging timestamp.
#[test]
fn touch_does_not_regress_timestamp() {
    let registry = TenantCellRegistry::new();
    let cell = registry.get_or_create("t", 0);

    cell.touch(1000, 5);
    assert_eq!(cell.last_access_millis(), 1000);
    assert_eq!(cell.last_access_seq(), 5);

    // A stale/out-of-order touch with older values must not lower either field.
    cell.touch(500, 3);
    assert_eq!(
        cell.last_access_millis(),
        1000,
        "a stale timestamp must not regress the recorded access time"
    );
    assert_eq!(
        cell.last_access_seq(),
        5,
        "an out-of-order sequence must not regress the recorded access sequence"
    );
}
