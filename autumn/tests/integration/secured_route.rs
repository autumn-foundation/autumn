//! Integration tests for the `#[secured]` attribute's extraction ordering
//! relative to Axum's body extractors (issue #1668).
//!
//! `#[secured]`'s role/session check historically ran as a statement *inside*
//! the handler body, which only executes after every extractor — including a
//! body extractor like `Json`/`Form`/`Multipart` — has already succeeded. That
//! meant an unauthenticated request with a malformed body was rejected with a
//! body-extraction error (`400`/`422`) instead of `#[secured]`'s own `401`,
//! masking the guard's outcome behind whatever the body extractor did first.

use autumn_web::reexports::axum::Json;
use autumn_web::secured;
use autumn_web::test::TestApp;
use autumn_web::{post, routes};

#[post("/secured-body")]
#[secured]
async fn secured_body(Json(_): Json<serde_json::Value>) -> &'static str {
    "secured-body-ok"
}

/// An unauthenticated client sending a malformed body must see `#[secured]`'s
/// `401`, not a body-extraction error. If the session/role check still ran
/// inside the handler body (after the `Json` extractor), the malformed body
/// would fail extraction first and the client would never see the guard's
/// intended response.
#[tokio::test]
async fn secured_route_401s_before_parsing_malformed_body() {
    let client = TestApp::new().routes(routes![secured_body]).build();

    let response = client
        .post("/secured-body")
        .header("content-type", "application/json")
        .body("this is not valid json")
        .send()
        .await;
    response.assert_status(401);
}
