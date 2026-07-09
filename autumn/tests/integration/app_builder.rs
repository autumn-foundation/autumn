//!
//! Integration tests for the `AppBuilder`.
//!
use autumn_web::{get, routes};

#[get("/test")]
async fn test_handler() -> &'static str {
    "test"
}

#[get("/other")]
async fn other_handler() -> &'static str {
    "other"
}

#[test]
fn app_builder_accepts_routes() {
    let builder = autumn_web::app().routes(routes![test_handler]);
    let _ = builder;
}

#[test]
fn app_builder_multiple_route_calls() {
    let builder = autumn_web::app()
        .routes(routes![test_handler])
        .routes(routes![other_handler]);
    let _ = builder;
}

#[tokio::test]
#[should_panic(expected = "No routes registered")]
async fn empty_routes_panics() {
    autumn_web::app().run().await;
}
