//!
//! Integration tests for the raw router escape hatch.
//!
use autumn_web::test::TestApp;
use autumn_web::{AppState, get, routes};

#[tokio::test]
async fn merged_route_is_accessible() {
    let raw = axum::Router::<AppState>::new()
        .route("/raw", axum::routing::get(|| async { "from raw router" }));
    let app = TestApp::new().merge(raw).build();
    app.get("/raw")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("from raw router");
}

#[tokio::test]
async fn multiple_merged_routers_are_accessible() {
    let raw1 =
        axum::Router::<AppState>::new().route("/raw1", axum::routing::get(|| async { "one" }));
    let raw2 =
        axum::Router::<AppState>::new().route("/raw2", axum::routing::get(|| async { "two" }));
    let app = TestApp::new().merge(raw1).merge(raw2).build();
    app.get("/raw1")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("one");
    app.get("/raw2")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("two");
}

#[get("/managed")]
async fn managed_handler() -> &'static str {
    "managed"
}

#[tokio::test]
async fn merged_routes_coexist_with_managed_routes() {
    let raw = axum::Router::<AppState>::new()
        .route("/raw", axum::routing::get(|| async { "from raw router" }));
    let app = TestApp::new()
        .routes(routes![managed_handler])
        .merge(raw)
        .build();
    app.get("/managed")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("managed");
    app.get("/raw")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("from raw router");
}

#[tokio::test]
async fn nested_route_is_accessible_under_prefix() {
    let raw = axum::Router::<AppState>::new().route(
        "/child",
        axum::routing::get(|| async { "from nested router" }),
    );
    let app = TestApp::new().nest("/parent", raw).build();
    app.get("/parent/child")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("from nested router");
}

#[tokio::test]
async fn multiple_nested_routers_are_accessible() {
    let raw1 =
        axum::Router::<AppState>::new().route("/child1", axum::routing::get(|| async { "one" }));
    let raw2 =
        axum::Router::<AppState>::new().route("/child2", axum::routing::get(|| async { "two" }));
    let app = TestApp::new().nest("/v1", raw1).nest("/v2", raw2).build();
    app.get("/v1/child1")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("one");
    app.get("/v2/child2")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("two");
}

#[tokio::test]
async fn nested_routes_coexist_with_managed_routes() {
    let raw = axum::Router::<AppState>::new().route(
        "/child",
        axum::routing::get(|| async { "from nested router" }),
    );
    let app = TestApp::new()
        .routes(routes![managed_handler])
        .nest("/parent", raw)
        .build();
    app.get("/managed")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("managed");
    app.get("/parent/child")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("from nested router");
}

#[tokio::test]
async fn raw_routes_can_extract_app_state() {
    let raw = axum::Router::<AppState>::new().route(
        "/state",
        axum::routing::get(
            |axum::extract::State(state): axum::extract::State<AppState>| async move {
                state.profile().to_owned()
            },
        ),
    );
    let app = TestApp::new().merge(raw).build();
    app.get("/state")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("test");
}

#[tokio::test]
async fn merged_routes_get_request_id_header() {
    let raw = axum::Router::<AppState>::new().route("/raw", axum::routing::get(|| async { "raw" }));
    let app = TestApp::new().merge(raw).build();
    app.get("/raw")
        .send()
        .await
        .assert_status(200)
        .assert_header_contains("x-request-id", "-");
}

#[tokio::test]
async fn merged_router_favicon_route_wins_without_startup_conflict() {
    let raw = axum::Router::<AppState>::new().route(
        "/favicon.ico",
        axum::routing::get(|| async { "raw favicon" }),
    );
    let app = TestApp::new().merge(raw).build();
    app.get("/favicon.ico")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("raw favicon");
}

#[tokio::test]
async fn nested_routes_can_extract_app_state() {
    let raw = axum::Router::<AppState>::new().route(
        "/state",
        axum::routing::get(
            |axum::extract::State(state): axum::extract::State<AppState>| async move {
                state.profile().to_owned()
            },
        ),
    );
    let app = TestApp::new().nest("/api", raw).build();
    app.get("/api/state")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("test");
}

#[tokio::test]
async fn merge_rejects_overlapping_paths() {
    #[get("/conflict")]
    async fn conflict_handler() -> &'static str {
        "managed"
    }
    let raw =
        axum::Router::<AppState>::new().route("/conflict", axum::routing::get(|| async { "raw" }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TestApp::new()
            .routes(routes![conflict_handler])
            .merge(raw)
            .build();
    }));
    assert!(result.is_err());
}
