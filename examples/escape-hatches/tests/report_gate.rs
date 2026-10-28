//! H4: `#[intercept(ReportGate)]` lets one report run at a time.
//!
//! The gate is process-global (one semaphore), so this test has its own
//! binary. No other test here may call a gated route at the same time.

use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use escape_hatches::hatches::report_gate::{ReportGate, hold_report_slot};

/// A stand-in report. It needs no database, so the test sees only the gate.
#[get("/gated")]
#[public]
#[intercept(ReportGate)]
async fn gated() -> &'static str {
    "report"
}

#[tokio::test]
async fn report_gate_sheds_a_second_report_and_then_recovers() {
    let client = TestApp::new()
        .routes(routes![gated])
        .routes(escape_hatches::routes())
        .build();

    // Free slot: the report runs.
    client
        .get("/gated")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("report");

    // Another report holds the only slot: a second caller gets 503 at once.
    let slot = hold_report_slot().expect("the slot is free");
    for path in ["/gated", "/reports/stock-value"] {
        client
            .get(path)
            .send()
            .await
            .assert_status(503)
            .assert_header("retry-after", "1")
            .assert_body_contains("report is busy");
    }

    // The first report ends: the slot is free again.
    drop(slot);
    client.get("/gated").send().await.assert_ok();
}
