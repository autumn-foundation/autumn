//! Integration test for issue #1012: two user routes that resolve to the
//! same `(method, path)` fail app build with a structured
//! `RouterBuildError::DuplicateUserRoute` BEFORE any router is mounted —
//! no `axum::Router::merge`-time panic escapes into the caller.
//!
//! Acceptance criterion #6 (verbatim from the issue): "A test asserts that
//! two `GET /` handlers produce the structured error containing both
//! handler names and `GET /`, and that the process does not panic."
//!
//! `RouterBuildError` is `pub(crate)` inside the `autumn` crate, so this
//! integration test observes the error through the panic payload that
//! `TestApp::build` produces when it surfaces `try_build_router_inner`'s
//! `Err(...)`. That payload includes the `Debug`-formatted variant, which
//! carries both handler names, the method, and the path — exactly the
//! contract AC #6 checks. Wrapping the build in `catch_unwind` proves
//! (a) that no `axum` startup panic escapes and (b) that we produce the
//! preflight failure ourselves before axum ever sees the overlap.

use autumn_web::test::TestApp;
use autumn_web::{get, post, routes};

#[get("/")]
async fn root_a() -> &'static str {
    "a"
}

#[get("/")]
async fn root_b() -> &'static str {
    "b"
}

#[get("/users/{id}")]
async fn user_by_id() -> &'static str {
    "id"
}

#[post("/users/{slug}")]
async fn user_by_slug() -> &'static str {
    "slug"
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_owned();
    }
    "<non-string panic payload>".to_owned()
}

/// AC #6: two `GET /` handlers must produce the structured
/// `DuplicateUserRoute` error containing both handler names and `GET /`,
/// and the process must not panic on the axum-side merge — the preflight
/// converts the collision into a `Result::Err` first.
#[tokio::test]
async fn two_root_handlers_return_structured_error_without_axum_panic() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TestApp::new().routes(routes![root_a, root_b]).build();
    }));

    let payload = result
        .expect_err("duplicate `GET /` handlers must fail the build; instead the build succeeded");
    let message = panic_message(payload.as_ref());

    // The panic must originate from our preflight (`try_build_router_inner`
    // returned Err, unwrapped by TestApp's `.expect("failed to build test
    // router")`), NOT from `axum::Router::merge`'s overlapping-method-routes
    // panic. Distinguish by asserting on the variant name — an axum panic
    // would carry "Overlapping method route" / "Insertion failed" etc.
    assert!(
        message.contains("DuplicateUserRoute"),
        "expected structured DuplicateUserRoute preflight error, got: {message}"
    );

    // AC #2: names both handlers, the method, and the path.
    assert!(
        message.contains("root_a"),
        "error must name the first handler (root_a): {message}"
    );
    assert!(
        message.contains("root_b"),
        "error must name the second handler (root_b): {message}"
    );
    assert!(
        message.contains("GET"),
        "error must include the HTTP method: {message}"
    );
    // The Debug format includes `path: "/"`, so the path token is unambiguous.
    assert!(
        message.contains(r#"path: "/""#) || message.contains('/'),
        "error must include the offending path: {message}"
    );
}

/// Finding A (issue #1012 review, round 2): two handlers on DIFFERENT methods
/// whose paths differ only by capture name (`GET /users/{id}` +
/// `POST /users/{slug}`) key as distinct `(method, shape)` pairs, so the
/// original preflight passed them through — and axum's matchit rejected the
/// second template as a route conflict at mount, leaking a panic. The
/// method-independent shape check now converts this into a structured
/// `ConflictingRouteShape` build error BEFORE axum ever sees the overlap. This
/// asserts the panic payload is OUR preflight error (naming both handlers and
/// both templates), NOT a raw matchit "Conflict"/"Insertion failed" panic.
#[tokio::test]
async fn cross_method_shape_conflict_returns_structured_error_without_axum_panic() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TestApp::new()
            .routes(routes![user_by_id, user_by_slug])
            .build();
    }));

    let payload = result.expect_err(
        "cross-method capture-name-only conflict must fail the build; instead it succeeded",
    );
    let message = panic_message(payload.as_ref());

    assert!(
        message.contains("ConflictingRouteShape"),
        "expected structured ConflictingRouteShape preflight error, got: {message}"
    );
    assert!(
        message.contains("user_by_id") && message.contains("user_by_slug"),
        "error must name both handlers: {message}"
    );
    assert!(
        message.contains("/users/{id}") && message.contains("/users/{slug}"),
        "error must name both original path templates: {message}"
    );
    // A leaked axum/matchit panic would carry these tokens instead.
    assert!(
        !message.contains("Insertion failed") && !message.contains("Overlapping method route"),
        "no raw axum matchit panic should escape: {message}"
    );
}
