//! Token-secured JSON API routes for the todo application.
//!
//! Demonstrates how a mobile or CLI client would interact with the todo app:
//!
//! 1. Call `POST /api/tokens` to receive a bearer token.
//! 2. Include `Authorization: Bearer <token>` on every subsequent request.
//!
//! The HTML routes at `/todos` are unaffected — they use session auth as usual.
//!
//! This module also carries a **content-negotiated** route ([`summary`]) that
//! serves the same resource as HTML or JSON from one handler based on the
//! request's `Accept` header. Unlike the token API above it is mounted outside
//! the `/api` bearer-token scope in `main.rs`, so a browser reaches it directly.

use std::convert::Infallible;
use std::time::Duration;

use autumn_web::auth::{DbApiTokenStore, issue_api_token};
use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::models::{NewTodo, Todo};
use crate::schema::todos;

// ── Token issuance ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct IssueTokenRequest {
    /// Arbitrary principal identifier, e.g. `"user:42"` or `"mobile-client"`.
    pub principal_id: String,
}

/// Issue a new Bearer token for the given principal.
///
/// Returns the raw token as plain text. **It is shown only once — store it
/// securely.** In production, protect this endpoint with session auth or an
/// admin guard; it is left open here for demonstration clarity.
#[post("/api/tokens")]
pub async fn issue_token(
    State(state): State<AppState>,
    body: Json<IssueTokenRequest>,
) -> AutumnResult<String> {
    let pool = state
        .pool()
        .expect("database required for API token issuance")
        .clone();
    let store = DbApiTokenStore::new(pool);
    issue_api_token(&store, &body.principal_id).await
}

// ── Protected todo endpoints ──────────────────────────────────────────────────
//
// These handlers are mounted behind `RequireApiToken` in main.rs.
// Requests without a valid `Authorization: Bearer <token>` header never reach
// them — the middleware rejects them with `401 Unauthorized` first.

/// Return all todos as a JSON array.
///
/// Tagged `#[api_doc(mcp)]` so it is projected as a read-only MCP tool: an AI
/// agent can call `list_json` through the same bearer-token-protected pipeline
/// a mobile or CLI client uses. The route is scoped under `/api` in
/// `main.rs`, so the served URL stays `/api/todos`.
#[get("/todos")]
#[api_doc(mcp, summary = "List all todos")]
pub async fn list_json(ApiToken(caller): ApiToken, mut db: Db) -> AutumnResult<Json<Vec<Todo>>> {
    let _ = caller; // available for per-user filtering; unused in this demo
    let all_todos = Todo::all(&mut db).await?;
    Ok(Json(all_todos))
}

/// Create a new todo from a JSON body, return the created todo as JSON.
///
/// Tagged `#[api_doc(mcp)]` to expose it as a write tool. Mutating verbs are
/// never exposed implicitly — this explicit opt-in is what allows an agent to
/// call it, even under the whole-API hatch.
#[post("/todos")]
#[api_doc(mcp, summary = "Create a new todo")]
pub async fn create_json(
    ApiToken(caller): ApiToken,
    mut db: Db,
    body: Json<NewTodo>,
) -> AutumnResult<Json<Todo>> {
    let _ = caller;
    let new_todo = body.0.validated()?;
    let created: Todo = diesel::insert_into(todos::table)
        .values(&new_todo)
        .returning(Todo::as_returning())
        .get_result(&mut db)
        .await?;
    Ok(Json(created))
}

// ── Streaming MCP tool (issue #1118) ────────────────────────────────────────────
//
// A slow tool — imagine scanning a large codebase rather than a todo list —
// feels broken if the agent waits in silence for the whole result. `stream`
// projects this handler's *normal Autumn `Sse` stream* onto the MCP
// Streamable-HTTP SSE channel: each yielded `Event` becomes a
// `notifications/progress` message (when the client sends `_meta.progressToken`),
// and the stream is terminated by the final `tools/call` result. The developer
// writes zero JSON-RPC/SSE framing — just a stream of events.

/// Scan all todos, emitting incremental progress per item, then a final summary.
///
/// Tagged `#[api_doc(mcp, stream)]`: the `stream` flag opts this tool into
/// progressive output and exempts it from the JSON-response eligibility gate
/// (an `Sse` handler has no JSON response schema). It dispatches through the
/// same `RequireApiToken` pipeline as the buffered tools above, so an agent's
/// bearer token is still enforced.
#[get("/todos/scan")]
#[api_doc(mcp, stream, summary = "Scan todos, streaming progress")]
pub async fn scan_json(
    ApiToken(caller): ApiToken,
    mut db: Db,
) -> AutumnResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let _ = caller; // available for per-user filtering; unused in this demo
    let all = Todo::all(&mut db).await?;
    let total = all.len();

    // A normal Autumn stream. `.then` simulates incremental work so the first
    // progress frame reaches the agent long before the scan completes — the
    // success metric for #1118 (time-to-first-signal decoupled from duration).
    let stream = stream::iter(all.into_iter().enumerate()).then(move |(index, todo)| async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Structured progress: a numeric `progress`/`total` plus a human
        // message are forwarded into the `notifications/progress` params.
        let frame = serde_json::json!({
            "progress": index + 1,
            "total": total,
            "message": format!("scanned todo #{}: {}", todo.id, todo.title),
        });
        Ok(Event::default().data(frame.to_string()))
    });

    Ok(Sse::new(stream))
}

// ── Content-negotiated resource (issue #2099) ───────────────────────────────────
//
// One handler, two representations. The `Negotiate` extractor reads the
// request's `Accept` header, and `.respond(html_closure, json_value)` serves the
// Maud markup to a browser (`Accept: text/html`) and the JSON value to an API
// client (`Accept: application/json`) from a single source of truth — the same
// `Summary` is rendered once and serialized once, never both. `Negotiated` also
// sets `Vary: Accept` so shared caches key the two representations separately.
//
// Unlike the endpoints above, this route is mounted OUTSIDE the `/api`
// bearer-token scope in `main.rs`, so a browser can reach it without a token and
// see the HTML branch. Try both representations against a running server:
//
//   curl localhost:3000/todos/summary                                # HTML
//   curl -H 'Accept: application/json' localhost:3000/todos/summary   # JSON

/// A small aggregate over the todo list.
///
/// `Copy` so the HTML closure and the JSON value below can each own a copy — the
/// closure captures one by copy while the original is handed to `.respond` as
/// the serialized body, avoiding a move/borrow conflict over a single value.
#[derive(Clone, Copy, Serialize)]
pub struct Summary {
    pub total: i64,
    pub completed: i64,
    pub pending: i64,
}

/// Serve the todo summary as HTML to browsers and JSON to API clients.
///
/// `Negotiate` captures the `Accept` preference; `.respond` picks the
/// representation and renders exactly one branch — the `html!` closure never
/// runs for a JSON response, and the JSON value is never serialized for an HTML
/// response. A client that forbids both (`Accept: text/html;q=0,
/// application/json;q=0`) gets `406 Not Acceptable`.
#[get("/todos/summary")]
pub async fn summary(negotiate: Negotiate, mut db: Db) -> AutumnResult<impl IntoResponse> {
    let all = Todo::all(&mut db).await?;
    let total = all.len() as i64;
    let completed = all.iter().filter(|t| t.completed).count() as i64;
    let summary = Summary {
        total,
        completed,
        pending: total - completed,
    };

    Ok(negotiate.respond(
        // `move` copies the `Copy` summary into the closure; the binding below
        // stays valid to hand to the JSON arm.
        move || {
            html! {
                (autumn_web::PreEscaped("<!DOCTYPE html>"))
                html lang="en" {
                    head {
                        meta charset="utf-8";
                        title { "Todo summary" }
                    }
                    body {
                        h1 { "Todo summary" }
                        ul {
                            li { "Total: " (summary.total) }
                            li { "Completed: " (summary.completed) }
                            li { "Pending: " (summary.pending) }
                        }
                    }
                }
            }
        },
        summary,
    ))
}
