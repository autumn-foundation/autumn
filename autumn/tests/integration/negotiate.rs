//!
//! Integration tests for the content-negotiated success responder.
//!
#![cfg(feature = "maud")]

use autumn_web::prelude::*;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde::Serialize;
use tower::ServiceExt;

#[derive(Serialize, Clone)]
struct Widget {
    id: u32,
    name: &'static str,
}

const fn widget() -> Widget {
    Widget {
        id: 7,
        name: "spanner",
    }
}

async fn show(negotiate: Negotiate) -> impl IntoResponse {
    let w = widget();
    negotiate.respond(move || html! { h1 { (w.name) } }, w)
}

async fn show_json_default(negotiate: Negotiate) -> impl IntoResponse {
    let w = widget();
    negotiate
        .default_format(Format::Json)
        .respond(move || html! { h1 { (w.name) } }, w)
}

fn app() -> Router {
    Router::new().route("/widget", axum::routing::get(show))
}

async fn send(app: Router, accept: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().uri("/widget");
    if let Some(value) = accept {
        builder = builder.header("accept", value);
    }
    app.oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn header(response: &axum::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn serves_html_to_browser_clients() {
    let response = send(app(), Some("text/html")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "content-type").starts_with("text/html"));
    assert_eq!(header(&response, "vary"), "Accept");
    assert!(body_string(response).await.contains("<h1>spanner</h1>"));
}

#[tokio::test]
async fn serves_json_to_api_clients() {
    let response = send(app(), Some("application/json")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), "application/json");
    assert_eq!(header(&response, "vary"), "Accept");
    assert_eq!(body_string(response).await, r#"{"id":7,"name":"spanner"}"#);
}

#[tokio::test]
async fn q_value_negotiation_prefers_html() {
    let response = send(app(), Some("application/json;q=0.9, text/html;q=1.0")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "content-type").starts_with("text/html"));
    assert!(body_string(response).await.contains("<h1>spanner</h1>"));
}

#[tokio::test]
async fn missing_accept_defaults_to_html() {
    let response = send(app(), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "content-type").starts_with("text/html"));
    assert!(body_string(response).await.contains("<h1>spanner</h1>"));
}

#[tokio::test]
async fn configurable_default_yields_json_when_accept_missing() {
    let app = Router::new().route("/widget", axum::routing::get(show_json_default));
    let response = send(app, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), "application/json");
    assert_eq!(header(&response, "vary"), "Accept");
    assert_eq!(body_string(response).await, r#"{"id":7,"name":"spanner"}"#);
}
