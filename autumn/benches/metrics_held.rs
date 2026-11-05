//! Fixed-iteration workload for profilers: recording through **held** metric
//! handles (`autumn_web::metrics`), the idiom a hot request path uses after
//! building its handles once.
//!
//! Each round records into a labeled counter, gauge and histogram (2 labels
//! each), the shape of a per-route request counter and latency histogram.
//! `metrics_record` is the criterion counterpart; this one has a fixed
//! iteration count so callgrind totals are comparable across commits.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench metrics_held
//! BIN=$(find target/release/deps -maxdepth 1 -name "metrics_held-*" -type f ! -name "*.d")
//! valgrind --tool=callgrind --callgrind-out-file=cg.out "$BIN" --iterations 10000
//! valgrind --tool=dhat --dhat-out-file=dhat.json "$BIN" --iterations 10000
//! ```
//!
//! `--iterations N` runs N rounds after a fixed warm-up that registers every
//! series.

use std::hint::black_box;

use autumn_web::metrics;

fn main() {
    let iterations: u32 = std::env::args()
        .skip_while(|a| a != "--iterations")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let requests = metrics::counter("bolt_held_requests_total")
        .with_label("route", "/users/{id}")
        .with_label("outcome", "ok");
    let inflight = metrics::gauge("bolt_held_inflight")
        .with_label("route", "/users/{id}")
        .with_label("pool", "primary");
    let bytes = metrics::histogram("bolt_held_response_bytes")
        .with_label("route", "/users/{id}")
        .with_label("method", "GET");

    for _ in 0..50 {
        requests.increment(1);
        inflight.set(1_u32);
        bytes.record(512_u32);
    }
    for i in 0..iterations {
        black_box(&requests).increment(1);
        black_box(&inflight).set(black_box(i % 8));
        black_box(&bytes).record(black_box(i % 4096));
    }
    black_box(metrics::snapshot());
}
