//! Criterion benchmarks for recording through the app-metrics facade
//! (`autumn_web::metrics`).
//!
//! The registry is an intern table twice over: names map to an instrument,
//! and each instrument's canonical label sets map to a series. What these
//! benches measure is the steady state, where both lookups hit:
//!
//! - `held/*`: a handle built once and recorded through repeatedly. Its labels
//!   already are the series key, so a sample is one borrowed map probe and an
//!   atomic update.
//! - `per_call/*`: the `counter(NAME).with_label(..).increment(1)` idiom, which
//!   also resolves the name and builds the label strings on every call.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p autumn-web --bench metrics_record
//! ```
//!
//! `autumn/tests/metrics_alloc_gate.rs` pins the allocation half in CI.

use std::hint::black_box;
use std::time::Duration;

use autumn_web::metrics;
use criterion::{Criterion, criterion_group, criterion_main};

fn held(c: &mut Criterion) {
    let mut group = c.benchmark_group("held");

    let unlabeled = metrics::counter("bench_held_unlabeled_total");
    group.bench_function("counter_unlabeled", |b| {
        b.iter(|| black_box(&unlabeled).increment(1));
    });

    let two = metrics::counter("bench_held_two_labels_total")
        .with_label("route", "/users/{id}")
        .with_label("outcome", "ok");
    group.bench_function("counter_2_labels", |b| {
        b.iter(|| black_box(&two).increment(1));
    });

    let histogram = metrics::histogram("bench_held_bytes")
        .with_label("route", "/upload")
        .with_label("method", "POST");
    group.bench_function("histogram_2_labels", |b| {
        b.iter(|| black_box(&histogram).record(black_box(512_u32)));
    });

    group.finish();
}

fn per_call(c: &mut Criterion) {
    let mut group = c.benchmark_group("per_call");

    group.bench_function("counter_unlabeled", |b| {
        b.iter(|| metrics::counter(black_box("bench_call_unlabeled_total")).increment(1));
    });

    group.bench_function("counter_1_label", |b| {
        b.iter(|| {
            metrics::counter(black_box("bench_call_one_label_total"))
                .with_label("outcome", black_box("ok"))
                .increment(1);
        });
    });

    group.bench_function("counter_3_labels", |b| {
        b.iter(|| {
            metrics::counter(black_box("bench_call_three_labels_total"))
                .with_label("route", black_box("/users/{id}"))
                .with_label("method", black_box("GET"))
                .with_label("outcome", black_box("ok"))
                .increment(1);
        });
    });

    group.bench_function("timer_2_labels", |b| {
        b.iter(|| {
            metrics::timer(black_box("bench_call_render_seconds"))
                .with_label("template", black_box("index"))
                .with_label("outcome", black_box("ok"))
                .record(black_box(Duration::from_millis(3)));
        });
    });

    group.finish();
}

criterion_group!(benches, held, per_call);
criterion_main!(benches);
