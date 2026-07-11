//! Unit/behaviour tests for per-tenant memory accounting cells (issue #1766).
//!
//! These exercise the public [`TenantCell`]/[`TenantCellRegistry`] API directly
//! and need no HTTP or database scaffolding, so they carry no feature gate.

use autumn_web::tenant_cell::{QuotaExceeded, TenantCellRegistry};

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

    cell.scratch_insert("k", vec![0u8; 500])
        .expect("insert under unlimited quota");
    assert_eq!(cell.tracked_bytes(), 500);
    assert_eq!(cell.scratch_get("k"), Some(vec![0u8; 500]));

    // Replacing "k" with a larger value nets to the new size (500 -> 800).
    cell.scratch_insert("k", vec![1u8; 800])
        .expect("replace value under unlimited quota");
    assert_eq!(cell.tracked_bytes(), 800);
    assert_eq!(cell.scratch_get("k"), Some(vec![1u8; 800]));

    let removed = cell.scratch_remove("k");
    assert_eq!(removed, Some(vec![1u8; 800]));
    assert_eq!(cell.tracked_bytes(), 0);
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

/// Density smoke test: 1000 concurrent cells each holding a small buffer track
/// exactly, and evicting all of them reclaims everything.
#[test]
fn density_smoke_thousand_cells() {
    const CELLS: usize = 1000;
    const BUF: usize = 16;

    let registry = TenantCellRegistry::new();
    for i in 0..CELLS {
        let cell = registry.get_or_create(&format!("tenant-{i}"), 0);
        cell.scratch_insert("buf", vec![0u8; BUF])
            .expect("insert under unlimited quota");
    }

    assert_eq!(registry.len(), CELLS);
    assert_eq!(registry.total_tracked_bytes(), CELLS * BUF);
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

    cell.scratch_insert("k", vec![0u8; 800]).unwrap();
    assert_eq!(cell.tracked_bytes(), 800);

    // Replace with same size: net 0. Must NOT error even though 800 + 800 > 1000.
    cell.scratch_insert("k", vec![0u8; 800]).unwrap();
    assert_eq!(cell.tracked_bytes(), 800);

    // Grow to exactly the quota (net delta +200).
    cell.scratch_insert("k", vec![0u8; 1000]).unwrap();
    assert_eq!(cell.tracked_bytes(), 1000);

    // Shrink releases the delta.
    cell.scratch_insert("k", vec![0u8; 100]).unwrap();
    assert_eq!(cell.tracked_bytes(), 100);

    // A genuine over-quota insert on a NEW key still fails and leaves state intact.
    cell.scratch_insert("k", vec![0u8; 900]).unwrap();
    assert_eq!(cell.tracked_bytes(), 900);
    let err = cell.scratch_insert("j", vec![0u8; 200]).unwrap_err();
    assert_eq!(err.quota, 1000);
    assert_eq!(err.in_use, 900);
    assert_eq!(cell.tracked_bytes(), 900);
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
    // capacity — the cell owns the whole allocation.
    let mut v = Vec::with_capacity(800);
    v.extend_from_slice(&[0u8; 8]); // len 8, capacity >= 800
    let cap = v.capacity();
    cell.scratch_insert("k", v).unwrap();
    assert_eq!(cell.tracked_bytes(), cap);
    assert!(cell.tracked_bytes() >= 800);

    // Removing releases the full capacity back to zero.
    let _ = cell.scratch_remove("k");
    assert_eq!(cell.tracked_bytes(), 0);

    // An over-capacity empty Vec is now rejected by the quota (it was accepted
    // when accounting by len()).
    let big = Vec::<u8>::with_capacity(5000); // len 0, capacity >= 5000
    let err = cell.scratch_insert("k", big).unwrap_err();
    assert_eq!(err.quota, 1000);
    assert!(err.requested >= 5000);
    assert_eq!(cell.tracked_bytes(), 0);
}
