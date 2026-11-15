//! Fixed-iteration workload for profilers: recording through the **per-call**
//! idiom of `autumn_web::metrics`, `counter(NAME).with_label(..).increment(1)`.
//!
//! This is what a call site writes when it does not keep a handle. Each call
//! resolves the name in the registry and the label set in the instrument, so
//! the two map probes are the workload. `metrics_held` is the counterpart for
//! a kept handle, which probes nothing after its first record.
//!
//! Each round records an unlabeled counter, a counter with one label, a
//! counter with three labels and a timer with two: the cases of the
//! `per_call` group in the criterion bench `metrics_record`. This one has a
//! fixed iteration count, so callgrind totals compare across commits.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench metrics_per_call
//! BIN=$(find target/release/deps -maxdepth 1 -name "metrics_per_call-*" -type f ! -name "*.d")
//! valgrind --tool=callgrind --callgrind-out-file=cg.out "$BIN" --iterations 10000
//! valgrind --tool=dhat --dhat-out-file=dhat.json "$BIN" --iterations 10000
//! ```
//!
//! `--iterations N` runs N rounds after a fixed warm-up that registers every
//! instrument and series.

use std::hint::black_box;
use std::time::Duration;

use autumn_web::metrics;

fn round(i: u32) {
    metrics::counter(black_box("bolt_call_unlabeled_total")).increment(1);
    metrics::counter(black_box("bolt_call_one_label_total"))
        .with_label("outcome", black_box("ok"))
        .increment(1);
    metrics::counter(black_box("bolt_call_three_labels_total"))
        .with_label("route", black_box("/users/{id}"))
        .with_label("method", black_box("GET"))
        .with_label("outcome", black_box("ok"))
        .increment(1);
    metrics::timer(black_box("bolt_call_render_seconds"))
        .with_label("template", black_box("index"))
        .with_label("outcome", black_box("ok"))
        .record(black_box(Duration::from_micros(u64::from(i % 4096))));
}

fn main() {
    let iterations: u32 = std::env::args()
        .skip_while(|a| a != "--iterations")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    for i in 0..50 {
        round(i);
    }
    for i in 0..iterations {
        round(i);
    }
    black_box(metrics::snapshot());
}
