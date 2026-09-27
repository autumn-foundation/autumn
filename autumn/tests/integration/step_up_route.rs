//! Integration tests for the `#[step_up]` attribute's extraction ordering
//! relative to Axum's body extractors (issue #1668).
//!
//! Like `#[secured]`, `#[step_up]`'s freshness check historically ran as a
//! statement *inside* the handler body, so it only executed after every
//! extractor — including a body extractor like `Json` — had already
//! succeeded. A client with no fresh step-up session sending a malformed body
//! was rejected with a body-extraction error (`400`/`422`) instead of
//! `#[step_up]`'s own `401`, masking the guard's outcome.

use autumn_web::reexports::axum::Json;
use autumn_web::step_up;
use autumn_web::test::TestApp;
use autumn_web::{post, routes};

#[post("/step-up-body")]
#[step_up]
async fn step_up_body(Json(_): Json<serde_json::Value>) -> &'static str {
    "step-up-body-ok"
}

/// A client with no fresh step-up session sending a malformed body must see
/// `#[step_up]`'s own `401` (JSON clients get a `WWW-Authenticate: StepUp`
/// problem-details response), not a body-extraction error. If the freshness
/// check still ran inside the handler body (after the `Json` extractor), the
/// malformed body would fail extraction first.
#[tokio::test]
async fn step_up_route_401s_before_parsing_malformed_body() {
    let client = TestApp::new().routes(routes![step_up_body]).build();

    let response = client
        .post("/step-up-body")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body("this is not valid json")
        .send()
        .await;
    response.assert_status(401);
}
