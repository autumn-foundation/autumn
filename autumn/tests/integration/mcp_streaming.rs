//! Integration tests for progressive (streaming) MCP tool results over the
//! Streamable-HTTP SSE channel (issue #1118).
//!
//! Covers the acceptance criteria:
//! * An opted-in tool (`#[api_doc(mcp, stream)]`) returns an Autumn `Sse`
//!   stream rather than a single buffered value.
//! * When the client supplies `_meta.progressToken`, the server emits
//!   `notifications/progress` messages referencing that token over SSE.
//! * Streamed content is delivered over SSE, terminated by the final
//!   `tools/call` result.
//! * Buffered (non-streaming) tools continue to work unchanged.
//! * A client that does not accept `text/event-stream` gets a buffered
//!   JSON result (graceful fallback), so streaming is opt-in end to end.

#![cfg(feature = "mcp")]

use std::convert::Infallible;
use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::{TestApp, TestClient};
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
struct Todo {
    id: u32,
    title: String,
}

// A streaming MCP tool: emits three incremental "matches" then completes. The
// handler writes a *normal* Autumn `Sse` stream — no MCP/JSON-RPC framing.
#[get("/api/search")]
#[api_doc(mcp, stream, summary = "Streaming code search")]
async fn streaming_search() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::iter(vec![
        Ok(Event::default().data("match 1")),
        Ok(Event::default().data("match 2")),
        Ok(Event::default().data("match 3")),
    ]);
    Sse::new(stream)
}

// A buffered (non-streaming) tool, to prove the base path is unchanged.
#[get("/api/todos")]
#[api_doc(mcp, summary = "List todos")]
async fn list_todos() -> AutumnResult<Json<Vec<Todo>>> {
    Ok(Json(vec![Todo {
        id: 1,
        title: "first".into(),
    }]))
}

/// Parse the `data:` payloads out of an SSE response body into JSON values,
/// skipping comments/keep-alives.
fn sse_messages(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .collect()
}

async fn post_sse(client: &TestClient, body: serde_json::Value) -> autumn_web::test::TestResponse {
    client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
}

#[tokio::test]
async fn streaming_tool_is_listed_with_input_schema() {
    let client = TestApp::new()
        .routes(routes![streaming_search])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    let tools = out["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // A streaming tool has no JSON response schema, but `stream` exempts it
    // from the JSON-out eligibility gate, so it is still advertised.
    assert!(
        names.contains(&"streaming_search"),
        "streaming tool must be listed: {names:?}"
    );
}

#[tokio::test]
async fn streaming_tool_emits_progress_then_final_result() {
    let client = TestApp::new()
        .routes(routes![streaming_search])
        .mount_mcp("/mcp")
        .build();

    let resp = post_sse(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":42,"method":"tools/call",
            "params": {
                "name":"streaming_search",
                "arguments":{},
                "_meta": { "progressToken": "tok-1" }
            }
        }),
    )
    .await;
    resp.assert_ok();
    // The response rides the Streamable-HTTP SSE channel.
    assert_eq!(
        resp.header("content-type")
            .map(|c| c.starts_with("text/event-stream")),
        Some(true),
        "streaming tool must answer with text/event-stream, got {:?}",
        resp.header("content-type")
    );

    let messages = sse_messages(&resp.text());

    // Progress notifications reference the client's token and carry the
    // incremental matches as their `message`.
    let progress: Vec<&serde_json::Value> = messages
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert_eq!(
        progress.len(),
        3,
        "one progress per streamed event: {messages:?}"
    );
    for p in &progress {
        assert_eq!(p["params"]["progressToken"], "tok-1");
    }
    assert_eq!(progress[0]["params"]["message"], "match 1");
    assert_eq!(progress[2]["params"]["message"], "match 3");

    // The stream terminates with the final tools/call result (id-correlated).
    let final_msg = messages
        .iter()
        .find(|m| m["id"] == 42)
        .expect("a final id-correlated result terminates the stream");
    assert_ne!(final_msg["result"]["isError"], true);
    let text = final_msg["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("match 1") && text.contains("match 3"),
        "got: {text}"
    );
}

#[tokio::test]
async fn streaming_without_progress_token_still_terminates_with_result() {
    // No `_meta.progressToken`: the spec forbids progress notifications without
    // a token, but the final result must still be delivered over the channel.
    let client = TestApp::new()
        .routes(routes![streaming_search])
        .mount_mcp("/mcp")
        .build();

    let resp = post_sse(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params": {"name":"streaming_search","arguments":{}}
        }),
    )
    .await;
    resp.assert_ok();
    let messages = sse_messages(&resp.text());
    assert!(
        messages
            .iter()
            .all(|m| m["method"] != "notifications/progress"),
        "no progressToken => no progress notifications: {messages:?}"
    );
    let final_msg = messages
        .iter()
        .find(|m| m["id"] == 7)
        .expect("final result");
    assert_ne!(final_msg["result"]["isError"], true);
}

#[tokio::test]
async fn client_without_sse_accept_gets_buffered_result() {
    // A client that only accepts application/json must still get a usable
    // (buffered) tool result rather than an SSE body it can't read.
    let client = TestApp::new()
        .routes(routes![streaming_search])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("accept", "application/json")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":9,"method":"tools/call",
            "params": {"name":"streaming_search","arguments":{}}
        }))
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(
        resp.header("content-type")
            .map(|c| c.starts_with("application/json")),
        Some(true),
        "non-SSE client gets buffered JSON, got {:?}",
        resp.header("content-type")
    );
    let out = resp.json::<serde_json::Value>();
    assert_eq!(out["id"], 9);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("match 1"),
        "buffered content joins the stream: {text}"
    );
}

#[tokio::test]
async fn buffered_tool_is_unchanged_by_streaming_support() {
    // The base #1117 buffered path must regress in no way.
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params": {"name":"list_todos","arguments":{}}
        }))
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(
        resp.header("content-type")
            .map(|c| c.starts_with("application/json")),
        Some(true),
        "buffered tool still answers application/json"
    );
    let out = resp.json::<serde_json::Value>();
    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("first"));
}

#[tokio::test]
async fn route_mcp_stream_toggle_exposes_untagged_sse_handler() {
    // An SSE handler with NO `#[api_doc(...)]` attributes, opted in at
    // registration time via the chainable `Route::mcp_stream()` toggle —
    // the plugin-author escape hatch when the handler source can't carry
    // attributes. `mcp_stream()` implies the opt-in and bypasses the
    // JSON-out eligibility gate, mirroring `#[api_doc(mcp, stream)]`.
    #[get("/api/untagged-stream")]
    async fn untagged_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
        let stream = stream::iter(vec![
            Ok(Event::default().data("chunk 1")),
            Ok(Event::default().data("chunk 2")),
        ]);
        Sse::new(stream)
    }

    let client = TestApp::new()
        .routes(
            routes![untagged_stream]
                .into_iter()
                .map(autumn_web::Route::mcp_stream)
                .collect(),
        )
        .mount_mcp("/mcp")
        .build();

    // Listed despite having no JSON response schema.
    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    let tools = out["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"untagged_stream"),
        "Route::mcp_stream() must expose the tool: {names:?}"
    );

    // A non-SSE client still gets a usable buffered result.
    let resp = client
        .post("/mcp")
        .header("accept", "application/json")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params": {"name":"untagged_stream","arguments":{}}
        }))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("chunk 1"), "buffered stream content: {text}");
}

#[tokio::test]
async fn first_progress_arrives_before_the_slow_tool_completes() {
    // Success metric: time-to-first-signal is decoupled from total duration.
    // The handler sleeps after the first event; the first SSE frame must be
    // observable well before the whole tool finishes.
    #[get("/api/slow")]
    #[api_doc(mcp, stream, summary = "Slow streaming tool")]
    async fn slow_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
        let s = stream::once(async { Ok(Event::default().data("started")) }).chain(stream::once(
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(Event::default().data("done"))
            },
        ));
        Sse::new(s)
    }

    // Drive the raw streaming response so we can observe the first frame's
    // arrival time rather than waiting for the buffered whole body.
    let router = TestApp::new()
        .routes(routes![slow_stream])
        .mount_mcp("/mcp")
        .build()
        .into_router();
    let request = http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params": {
                    "name":"slow_stream","arguments":{},
                    "_meta": {"progressToken": 1}
                }
            }))
            .unwrap(),
        ))
        .unwrap();

    let start = std::time::Instant::now();
    let response = tower::ServiceExt::oneshot(router, request).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    // Pull the first non-empty chunk.
    let mut first_at = None;
    while let Some(chunk) = body.next().await {
        let bytes = chunk.unwrap();
        if !bytes.is_empty() {
            first_at = Some(start.elapsed());
            break;
        }
    }
    let elapsed = first_at.expect("a first frame");
    assert!(
        elapsed < Duration::from_millis(250),
        "first signal should precede the 300ms tail; took {elapsed:?}"
    );
}
