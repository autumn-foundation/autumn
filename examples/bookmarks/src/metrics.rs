// ── 0.7.0 Feature: the app-metrics call-site facade ─────────────
//
// `autumn_web::metrics` lets application code record its own counters,
// gauges and timers at the point where the interesting thing happens —
// no trait to implement, no type to register with `AppBuilder`. Whatever
// is recorded here shows up on the same `/actuator/prometheus` and
// `/actuator/metrics` endpoints as the framework's built-in
// `autumn_http_*` families, which this example already exposes.
//
// See `docs/guide/metrics.md`. This module is the example's one place
// that names an instrument, so the call sites in `routes::bookmarks`
// stay one line each and the names cannot drift apart.

use autumn_web::metrics::{self, TimerGuard};

/// How many bookmarks the create form has accepted or rejected.
///
/// Counter, so PromQL `rate()` works on it. Named `*_total` per the
/// Prometheus convention.
pub const CREATED_TOTAL: &str = "bookmarks_created_total";

/// How long the `/bookmarks/stats` roll-up spends in the database.
///
/// A timer is a histogram of seconds, which is what `histogram_quantile()`
/// needs to give you a p99 — hence the `*_seconds` suffix.
pub const STATS_QUERY_SECONDS: &str = "bookmark_stats_query_seconds";

/// Label attached to [`CREATED_TOTAL`]. A small, closed set the code owns —
/// never user input, which would make every distinct value its own series
/// for the life of the process (`docs/guide/metrics.md`, "Labels and
/// cardinality").
pub mod outcome {
    /// The changeset validated and the row was written.
    pub const CREATED: &str = "created";
    /// The changeset failed validation and the form was re-rendered as 422.
    pub const REJECTED: &str = "rejected";
}

/// Attach `# HELP` text to both instruments, and give the stats timer
/// buckets that match what a two-`GROUP BY` page actually costs.
///
/// Describing a metric does not register it, so this may run before any
/// call site — which is exactly why bucket bounds can still be set here.
/// Called once from `main`, before the server starts.
pub fn describe() {
    metrics::describe_counter(
        CREATED_TOTAL,
        "Bookmarks submitted through the create form, by outcome",
    );
    metrics::describe_histogram(
        STATS_QUERY_SECONDS,
        "Seconds spent in the grouped-aggregate queries behind /bookmarks/stats",
    );
    // The default bounds top out at 10s, which wastes most of the histogram
    // on a page whose two aggregates are single round trips. Bounds are
    // frozen at registration, so this has to happen before the first
    // `timer(...)` call — i.e. here, at startup.
    metrics::set_histogram_buckets(
        STATS_QUERY_SECONDS,
        &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0],
    );
}

/// Record one create-form submission under `outcome`.
pub fn record_created(outcome: &'static str) {
    metrics::counter(CREATED_TOTAL)
        .with_label("outcome", outcome)
        .increment(1);
}

/// Start timing the `/bookmarks/stats` aggregates.
///
/// The returned guard records when it **drops**, so every exit path is
/// covered — including the `?` on a failing query. Bind it to a named
/// variable: `let _ = ...` drops it immediately and records ~0s.
#[must_use]
pub fn time_stats_query() -> TimerGuard {
    metrics::timer(STATS_QUERY_SECONDS).start()
}

#[cfg(test)]
mod tests {
    use autumn_web::test::TestApp;

    use super::{CREATED_TOTAL, STATS_QUERY_SECONDS, describe, outcome, record_created};

    /// The actuator smoke for this example's own metrics: record through the
    /// same helpers the handlers use, then prove both instruments show up on
    /// the stock actuator endpoints — the Prometheus scrape *and* the JSON
    /// view — with the labels and the histogram families intact.
    ///
    /// One test function rather than several: the facade registry is
    /// process-global, so two concurrent tests recording the same fixed
    /// instrument names would race on the values they assert.
    #[tokio::test]
    async fn domain_counter_and_timer_reach_the_actuator() {
        describe();
        record_created(outcome::CREATED);
        record_created(outcome::CREATED);
        record_created(outcome::REJECTED);
        // Resolve the guard explicitly so the observation is recorded before
        // the scrape below rather than at the end of the test body.
        super::time_stats_query().stop();

        let client = TestApp::new().build();

        let scrape = client.get("/actuator/prometheus").send().await;
        scrape.assert_ok();
        let text = scrape.text();

        assert!(
            text.contains(&format!(
                "# HELP {CREATED_TOTAL} Bookmarks submitted through the create form, by outcome"
            )),
            "describe_counter should have attached HELP text:\n{text}"
        );
        assert!(
            text.contains(&format!("# TYPE {CREATED_TOTAL} counter")),
            "missing TYPE line for {CREATED_TOTAL}:\n{text}"
        );
        assert!(
            text.contains(&format!("{CREATED_TOTAL}{{outcome=\"created\"}} 2")),
            "the two accepted submissions should land on the created series:\n{text}"
        );
        assert!(
            text.contains(&format!("{CREATED_TOTAL}{{outcome=\"rejected\"}} 1")),
            "a rejected submission is its own series, not a lost sample:\n{text}"
        );

        assert!(
            text.contains(&format!("# TYPE {STATS_QUERY_SECONDS} histogram")),
            "a timer renders as a Prometheus histogram:\n{text}"
        );
        assert!(
            text.contains(&format!("{STATS_QUERY_SECONDS}_bucket{{le=\"0.025\"}}")),
            "the overridden bucket bounds should be the ones exposed:\n{text}"
        );
        assert!(
            text.contains(&format!("{STATS_QUERY_SECONDS}_count 1")),
            "the guard records exactly one observation:\n{text}"
        );

        // The same data under `/actuator/metrics`' `app` key.
        let json_resp = client.get("/actuator/metrics").send().await;
        json_resp.assert_ok();
        let json: serde_json::Value = json_resp.json();
        let app = json
            .get("app")
            .unwrap_or_else(|| panic!("missing top-level `app` key in {json}"));
        let entries = app
            .as_array()
            .unwrap_or_else(|| panic!("`app` must be an array, got {app}"));

        let counter = entries
            .iter()
            .find(|entry| entry["name"] == serde_json::json!(CREATED_TOTAL))
            .unwrap_or_else(|| panic!("missing {CREATED_TOTAL} under `app` in {app}"));
        assert_eq!(counter["kind"], serde_json::json!("counter"));

        let timer = entries
            .iter()
            .find(|entry| entry["name"] == serde_json::json!(STATS_QUERY_SECONDS))
            .unwrap_or_else(|| panic!("missing {STATS_QUERY_SECONDS} under `app` in {app}"));
        assert_eq!(timer["kind"], serde_json::json!("histogram"));
        assert_eq!(
            timer["series"][0]["value"]["count"].as_u64(),
            Some(1),
            "the JSON view must agree with the scrape's _count, got {timer}"
        );
    }
}
