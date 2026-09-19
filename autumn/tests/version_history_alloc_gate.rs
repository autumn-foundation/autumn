//! Isolated integration test: allocation gate for the version-history diff
//! entry points on the workload the `#[repository(versioned = true)]` codegen
//! actually runs (#2429).
//!
//! Its own binary for the same reason `config_alloc_gate` and
//! `password_policy_alloc_gate` are: `allocation-counter` installs a
//! process-wide counting `#[global_allocator]`, a process-wide side effect per
//! CLAUDE.md's isolated-test rules, and taxing every allocation in the
//! consolidated suite to measure a handful here is not a trade worth making.
//!
//! # What is measured
//!
//! One *mutation* as the generated code performs it: materialize a throwaway
//! `serde_json::Value` (standing in for `VersionedRecord::version_column_values`,
//! which serializes the record fresh on every write) and hand it to the diff.
//! Both the borrowed and the owned entry point are charged for that
//! materialization, so the delta between them is exactly what each does with
//! the value it is given — `&Value` clones every retained column name and value
//! out of it, the owned variant moves them.
//!
//! The pointer-identity tests in `autumn/src/version_history.rs` prove *that*
//! the owned path moves rather than clones, for one column. This gate is the
//! aggregate: it pins the whole-record cost so a future refactor back toward
//! cloning fails a test instead of only showing up in a profiler.

use std::hint::black_box;

use autumn_web::version_history::{
    compute_delete_changes, compute_delete_changes_owned, compute_diff, compute_diff_owned,
    compute_insert_changes, compute_insert_changes_owned,
};

/// Enough repetitions that a per-mutation allocation cannot hide in noise,
/// and enough that the ceilings below are legible per-mutation figures.
const MUTATIONS: u64 = 100;

// Measured on this 7-column record, deterministic across runs, blocks per
// `MUTATIONS`-run window (each figure includes the throwaway `Value` both the
// borrowed and the owned path are charged for):
//
// | entry point              | borrowed | owned | delta   |
// |--------------------------|----------|-------|---------|
// | `compute_diff`           |    3,400 | 2,700 | -20.6%  |
// | `compute_insert_changes` |    2,600 | 1,400 | -46.2%  |
// | `compute_delete_changes` |    2,600 | 1,400 | -46.2%  |
//
// `compute_diff` gains least because it only ever retained the *changed*
// columns (3 of 7 here), so it had the fewest clones to drop.
//
// Ceilings sit a little above the measurement, same convention as
// `config_alloc_gate`'s. A failure a hair over the line means re-measure and
// re-derive, not nudge upwards.
const DIFF_OWNED_CEILING: u64 = 2_800;
const INSERT_OWNED_CEILING: u64 = 1_500;
const DELETE_OWNED_CEILING: u64 = 1_500;

/// A realistic 7-column record (title/body/published/author/timestamps),
/// matching `autumn/benches/version_history.rs`.
fn before_record() -> serde_json::Value {
    serde_json::json!({
        "id": 1,
        "title": "Old title",
        "body": "Some body text that is reasonably long to be realistic",
        "published": false,
        "author": "alice",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z"
    })
}

fn after_record() -> serde_json::Value {
    serde_json::json!({
        "id": 1,
        "title": "New title",
        "body": "Some body text that is reasonably long to be realistic",
        "published": true,
        "author": "alice",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-05-26T12:00:00Z"
    })
}

/// Blocks and bytes charged to `MUTATIONS` runs of `body`.
fn measure(label: &str, body: impl Fn()) -> (u64, u64) {
    // Warm-up outside the measured window, so nothing one-time is charged.
    body();
    let info = allocation_counter::measure(|| {
        for _ in 0..MUTATIONS {
            body();
        }
    });
    let per_mutation_blocks = info.count_total / MUTATIONS;
    let per_mutation_bytes = info.bytes_total / MUTATIONS;
    println!(
        "{label}: {per_mutation_blocks} blocks / {per_mutation_bytes} bytes per mutation \
         ({MUTATIONS} mutations, {} blocks / {} bytes total)",
        info.count_total, info.bytes_total
    );
    (info.count_total, info.bytes_total)
}

#[test]
fn owned_entry_points_allocate_less_than_borrowed_on_a_versioned_write() {
    let before = before_record();
    let after = after_record();
    let sensitive: &[&str] = &[];

    let (diff_borrowed, diff_borrowed_bytes) = measure("compute_diff", || {
        let (b, a) = (before.clone(), after.clone());
        black_box(compute_diff(black_box(&b), black_box(&a), sensitive));
    });
    let (diff_owned, diff_owned_bytes) = measure("compute_diff_owned", || {
        let (b, a) = (before.clone(), after.clone());
        black_box(compute_diff_owned(black_box(b), black_box(a), sensitive));
    });

    let (insert_borrowed, insert_borrowed_bytes) = measure("compute_insert_changes", || {
        let r = after.clone();
        black_box(compute_insert_changes(black_box(&r), sensitive));
    });
    let (insert_owned, insert_owned_bytes) = measure("compute_insert_changes_owned", || {
        let r = after.clone();
        black_box(compute_insert_changes_owned(black_box(r), sensitive));
    });

    let (delete_borrowed, delete_borrowed_bytes) = measure("compute_delete_changes", || {
        let r = before.clone();
        black_box(compute_delete_changes(black_box(&r), sensitive));
    });
    let (delete_owned, delete_owned_bytes) = measure("compute_delete_changes_owned", || {
        let r = before.clone();
        black_box(compute_delete_changes_owned(black_box(r), sensitive));
    });

    // ── Relative: the whole point of the owned entry points ──────────
    //
    // Stated as strict inequalities rather than fixed ratios so the gate
    // survives an unrelated change to how `serde_json::json!` allocates, while
    // still failing outright if an `_owned` function is ever reimplemented as a
    // delegate to its borrowed twin (which allocates identically).
    assert!(
        diff_owned < diff_borrowed,
        "compute_diff_owned allocated {diff_owned} blocks vs compute_diff's {diff_borrowed} \
         over {MUTATIONS} mutations — the owned path must allocate strictly less"
    );
    assert!(
        insert_owned < insert_borrowed,
        "compute_insert_changes_owned allocated {insert_owned} blocks vs \
         compute_insert_changes's {insert_borrowed} over {MUTATIONS} mutations"
    );
    assert!(
        delete_owned < delete_borrowed,
        "compute_delete_changes_owned allocated {delete_owned} blocks vs \
         compute_delete_changes's {delete_borrowed} over {MUTATIONS} mutations"
    );
    assert!(
        diff_owned_bytes < diff_borrowed_bytes
            && insert_owned_bytes < insert_borrowed_bytes
            && delete_owned_bytes < delete_borrowed_bytes,
        "every owned entry point must also allocate fewer bytes: diff {diff_owned_bytes} vs \
         {diff_borrowed_bytes}, insert {insert_owned_bytes} vs {insert_borrowed_bytes}, \
         delete {delete_owned_bytes} vs {delete_borrowed_bytes}"
    );

    // ── Absolute: pin the measured cost ──────────────────────────────
    //
    // See the table beside the ceiling constants for the measured baselines.
    assert!(
        insert_owned <= INSERT_OWNED_CEILING,
        "compute_insert_changes_owned allocated {insert_owned} blocks over {MUTATIONS} \
         mutations, over the {INSERT_OWNED_CEILING}-block ceiling"
    );
    assert!(
        delete_owned <= DELETE_OWNED_CEILING,
        "compute_delete_changes_owned allocated {delete_owned} blocks over {MUTATIONS} \
         mutations, over the {DELETE_OWNED_CEILING}-block ceiling"
    );
    assert!(
        diff_owned <= DIFF_OWNED_CEILING,
        "compute_diff_owned allocated {diff_owned} blocks over {MUTATIONS} mutations, \
         over the {DIFF_OWNED_CEILING}-block ceiling"
    );
}
