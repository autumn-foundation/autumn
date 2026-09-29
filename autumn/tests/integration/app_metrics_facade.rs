//! End-to-end proof for the app-metrics call-site facade (issue #1378).
//!
//! This is the issue's success-metric artifact: a handler records a metric in
//! **one line**, defines **zero** app-side types, wires **nothing** into the
//! app builder, and the value shows up on `/actuator/prometheus` and
//! `/actuator/metrics` of a stock [`TestApp`].
//!
//! The facade registry is process-global and the consolidated test binary runs
//! tests concurrently, so every instrument name here is made unique with the
//! local [`unique_name`] helper and assertions only ever look for those names.
//! `metrics::testing::unique_name` is not used because it sits behind the
//! non-default `test-support` feature, and this module must compile under a
//! plain `cargo test --workspace` too.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::metrics;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};

/// Sequence backing [`unique_name`].
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Build a metric name no other test in this process will collide with.
fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

// ── AC: one line in a handler, zero app-defined types ──────────────────

static CHECKOUT_COUNTER: LazyLock<String> =
    LazyLock::new(|| format!("{}_total", unique_name("checkout_paid")));

#[get("/checkout")]
async fn checkout() -> &'static str {
    metrics::counter(&CHECKOUT_COUNTER)
        .with_label("status", "paid")
        .increment(1);
    "ok"
}

#[tokio::test]
async fn handler_counter_is_scrapeable_and_in_the_metrics_json() {
    let client = TestApp::new().routes(routes![checkout]).build();

    client.get("/checkout").send().await.assert_ok();

    // Prometheus text format.
    let scrape = client.get("/actuator/prometheus").send().await;
    scrape.assert_ok();
    let text = scrape.text();
    let name: &str = &CHECKOUT_COUNTER;

    assert!(
        text.contains(&format!("# TYPE {name} counter")),
        "missing TYPE line for {name} in:\n{text}"
    );
    assert!(
        text.contains(&format!("{name}{{status=\"paid\"}} 1")),
        "missing sample line for {name} in:\n{text}"
    );

    // JSON view of the same data.
    let metrics_resp = client.get("/actuator/metrics").send().await;
    metrics_resp.assert_ok();
    let json: serde_json::Value = metrics_resp.json();
    let app = json
        .get("app")
        .unwrap_or_else(|| panic!("missing top-level `app` key in {json}"));
    let entry = app
        .as_array()
        .unwrap_or_else(|| panic!("`app` must be an array, got {app}"))
        .iter()
        .find(|i| i["name"] == serde_json::json!(name))
        .unwrap_or_else(|| panic!("missing {name} under `app` in {app}"));

    assert_eq!(entry["kind"], serde_json::json!("counter"));
    assert_eq!(
        entry["series"][0]["labels"]["status"],
        serde_json::json!("paid")
    );
    assert_eq!(
        entry["series"][0]["value"]["value"].as_u64(),
        Some(1),
        "JSON value must match the scrape value, got {entry}"
    );
}

// ── AC: timing a handler with the guard ────────────────────────────────

static WORK_TIMER: LazyLock<String> =
    LazyLock::new(|| format!("{}_seconds", unique_name("work_duration")));

#[get("/work")]
async fn work() -> &'static str {
    let _timing = metrics::timer(&WORK_TIMER).start();
    "done"
}

#[tokio::test]
async fn handler_timer_is_scrapeable_as_a_histogram() {
    let client = TestApp::new().routes(routes![work]).build();

    client.get("/work").send().await.assert_ok();

    let scrape = client.get("/actuator/prometheus").send().await;
    scrape.assert_ok();
    let text = scrape.text();
    let name: &str = &WORK_TIMER;

    assert!(
        text.contains(&format!("# TYPE {name} histogram")),
        "missing histogram TYPE for {name} in:\n{text}"
    );
    assert!(
        text.contains(&format!("{name}_count 1")),
        "missing `{name}_count 1` in:\n{text}"
    );
    assert!(
        text.contains(&format!("{name}_bucket{{le=\"+Inf\"}} 1")),
        "the +Inf bucket must equal the count in:\n{text}"
    );
    assert!(
        text.contains(&format!("{name}_sum ")),
        "missing `{name}_sum` line in:\n{text}"
    );
}
