//! Isolated integration test: allocation gate for recording through the
//! app-metrics facade (`autumn_web::metrics`).
//!
//! Its own binary for the same reason `config_alloc_gate` is:
//! `allocation-counter` installs a process-wide counting `#[global_allocator]`,
//! a process-wide side effect per CLAUDE.md's isolated-test rules.
//!
//! # What is measured
//!
//! The registry is an intern table twice over: metric names map to an
//! instrument, and each instrument's canonical label sets map to a series.
//! Once a name or a label set has been seen, recording through it again must
//! cost a *lookup*, not a fresh owned copy of every string just to probe a map
//! that already holds an equal key. That is what this gate pins:
//!
//! - **A held handle** (`let c = counter(..).with_label(..); c.increment(1)`)
//!   allocates nothing per sample once its series exists: its labels are
//!   already canonical, so the series map is probed with them directly. The
//!   first sample for a label set still registers the series (one `Arc` and
//!   one owned key); the warm-up in [`per_sample`] pays that before the
//!   measured window, because the steady state is what this gate pins.
//! - **The per-call idiom** (`counter(..).with_label(..).increment(1)`) pays
//!   only for building the handle — the label `Vec` plus one owned key and one
//!   owned value per label, which `with_label`'s `&str` / `impl Into<String>`
//!   signature makes inherent — and nothing more at record time.
//!
//! Before the series map was probed with borrowed labels, every record copied
//! every label a second time into a throwaway key. Measured, allocations per
//! sample (deterministic across runs):
//!
//! | workload                       | before | after |
//! |--------------------------------|--------|-------|
//! | held counter, 2 labels         |      5 |     0 |
//! | held gauge, 1 label            |      3 |     0 |
//! | held histogram, 2 labels       |      5 |     0 |
//! | per-call counter, unlabeled    |      0 |     0 |
//! | per-call counter, 1 label      |      6 |     3 |
//! | per-call counter, 3 labels     |     14 |     7 |
//! | per-call timer, 2 labels       |     10 |     5 |
//!
//! A failure here means a refactor put an allocation back on the hit path;
//! re-measure and re-derive rather than raising a ceiling.

use std::hint::black_box;
use std::time::Duration;

use autumn_web::metrics;

/// Enough samples that a per-sample allocation cannot hide in noise.
const SAMPLES: u64 = 1_000;

/// A name nothing else in this binary registers, so each test owns its
/// instrument outright.
fn name(stem: &str) -> String {
    format!("alloc_gate_{stem}_total")
}

/// Allocations made by `f` over [`SAMPLES`] calls, after a warm-up that pays
/// every first-sight cost (instrument registration, series insertion, lazy
/// statics, first lock).
fn per_sample(label: &str, mut f: impl FnMut()) -> u64 {
    for _ in 0..10 {
        f();
    }
    let info = allocation_counter::measure(|| {
        for _ in 0..SAMPLES {
            f();
        }
    });
    let per = info.count_total / SAMPLES;
    println!(
        "{label}: {} allocations over {SAMPLES} samples ({per}/sample)",
        info.count_total
    );
    per
}

#[test]
fn held_labeled_counter_records_without_allocating() {
    let counter = metrics::counter(&name("held"))
        .with_label("route", "/users/{id}")
        .with_label("outcome", "ok");
    let allocs = per_sample("held counter, 2 labels", || counter.increment(1));
    assert_eq!(
        allocs, 0,
        "a held, labeled counter must find its series without allocating"
    );
}

#[test]
fn held_labeled_gauge_records_without_allocating() {
    let gauge = metrics::gauge("alloc_gate_held_gauge").with_label("pool", "primary");
    let allocs = per_sample("held gauge, 1 label", || gauge.set(3_u32));
    assert_eq!(allocs, 0, "a held, labeled gauge must not allocate");
}

#[test]
fn held_labeled_histogram_records_without_allocating() {
    let histogram = metrics::histogram("alloc_gate_held_bytes")
        .with_label("route", "/upload")
        .with_label("method", "POST");
    let allocs = per_sample("held histogram, 2 labels", || histogram.record(512_u32));
    assert_eq!(allocs, 0, "a held, labeled histogram must not allocate");
}

#[test]
fn unlabeled_per_call_records_without_allocating() {
    let metric = name("unlabeled");
    let allocs = per_sample("per-call counter, unlabeled", || {
        metrics::counter(&metric).increment(1);
    });
    assert_eq!(allocs, 0, "an unlabeled counter must not allocate");
}

#[test]
fn per_call_one_label_pays_only_for_the_handle() {
    let metric = name("one_label");
    let allocs = per_sample("per-call counter, 1 label", || {
        metrics::counter(&metric)
            .with_label("outcome", "ok")
            .increment(1);
    });
    // Vec + key + value.
    assert_eq!(allocs, 3, "one label, per-call");
}

#[test]
fn per_call_three_labels_pays_only_for_the_handle() {
    let metric = name("three_labels");
    let allocs = per_sample("per-call counter, 3 labels", || {
        metrics::counter(&metric)
            .with_label("route", "/users/{id}")
            .with_label("method", "GET")
            .with_label("outcome", "ok")
            .increment(1);
    });
    // Vec + 3 keys + 3 values.
    assert_eq!(allocs, 7, "three labels, per-call");
}

#[test]
fn per_call_labeled_timer_pays_only_for_the_handle() {
    let allocs = per_sample("per-call timer, 2 labels", || {
        metrics::timer("alloc_gate_render_seconds")
            .with_label("template", "index")
            .with_label("outcome", "ok")
            .record(black_box(Duration::from_millis(3)));
    });
    // Vec + 2 keys + 2 values.
    assert_eq!(allocs, 5, "two labels, per-call timer");
}
