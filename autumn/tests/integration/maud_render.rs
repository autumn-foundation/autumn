//!
//! Integration tests for maud rendering.
//!
#![cfg(feature = "maud")]

use autumn_web::{Markup, html};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn maud_handler_returns_html() {
    async fn index() -> Markup {
        html! { h1 { "hello" } }
    }

    let app = Router::new().route("/", axum::routing::get(index));

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/html"));

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html_str = String::from_utf8(body.to_vec()).unwrap();
    assert!(html_str.contains("<h1>hello</h1>"));
}
