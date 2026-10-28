//! Integration tests for the MCP Tasks extension (`io.modelcontextprotocol/tasks`).
//!
//! A task tool returns a task handle instead of a blocking result. The client
//! polls `tasks/get` until the task is terminal.

#![cfg(feature = "mcp")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::Route;
use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink};
use autumn_web::config::AutumnConfig;
use autumn_web::mcp::McpTaskConfig;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TASKS: &str = "io.modelcontextprotocol/tasks";

#[derive(Serialize, Deserialize)]
struct Report {
    rows: u32,
}

#[get("/api/report")]
#[api_doc(mcp, task, summary = "Build a report")]
async fn build_report() -> AutumnResult<Json<Report>> {
    Ok(Json(Report { rows: 3 }))
}

#[get("/api/quick")]
#[api_doc(mcp, summary = "Quick read")]
async fn quick_read() -> AutumnResult<Json<Report>> {
    Ok(Json(Report { rows: 1 }))
}

#[get("/api/broken")]
#[api_doc(mcp, task, summary = "Always fails")]
async fn broken_report() -> AutumnResult<Json<Report>> {
    Err(AutumnError::not_found_msg("no such report"))
}

#[get("/api/panic")]
#[api_doc(mcp, task, summary = "Panics")]
async fn panicky_report() -> AutumnResult<Json<Report>> {
    panic!("report generator crashed");
}

/// Set when the slow handler starts.
static SLOW_STARTED: AtomicBool = AtomicBool::new(false);
/// Set when the slow handler's future is dropped.
static SLOW_DROPPED: AtomicBool = AtomicBool::new(false);

struct DropFlag(&'static AtomicBool);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[get("/api/slow")]
#[api_doc(mcp, task, summary = "Very slow report")]
async fn slow_report() -> AutumnResult<Json<Report>> {
    let _flag = DropFlag(&SLOW_DROPPED);
    SLOW_STARTED.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(60)).await;
    Ok(Json(Report { rows: 9 }))
}

#[get("/api/stalled")]
#[api_doc(mcp, task, summary = "Stalled report")]
async fn stalled_report() -> AutumnResult<Json<Report>> {
    tokio::time::sleep(Duration::from_secs(60)).await;
    Ok(Json(Report { rows: 0 }))
}

#[get("/api/idle")]
#[api_doc(mcp, task, summary = "Idle report")]
async fn idle_report() -> AutumnResult<Json<Report>> {
    tokio::time::sleep(Duration::from_secs(60)).await;
    Ok(Json(Report { rows: 0 }))
}

/// Longer than the global request deadline in `long_work_is_not_cut_by_the_envelope_timeout`.
#[get("/api/long", timeout = "off")]
#[api_doc(mcp, task, summary = "Long report")]
async fn long_report() -> AutumnResult<Json<Report>> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Json(Report { rows: 7 }))
}

// ── Helpers ───────────────────────────────────────────────────────────

fn task_meta() -> Value {
    json!({
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": { TASKS: {} }
        }
    })
}

fn app(routes: Vec<Route>) -> TestClient {
    TestApp::new().routes(routes).mount_mcp("/mcp").build()
}

async fn rpc(client: &TestClient, auth: Option<&str>, body: Value) -> Value {
    let mut req = client.post("/mcp");
    if let Some(auth) = auth {
        req = req.header("authorization", auth);
    }
    let resp = req.json(&body).send().await;
    resp.assert_ok();
    resp.json::<Value>()
}

async fn call(client: &TestClient, name: &str, with_caps: bool) -> Value {
    call_as(client, None, name, with_caps).await
}

async fn call_as(client: &TestClient, auth: Option<&str>, name: &str, with_caps: bool) -> Value {
    let mut params = json!({ "name": name, "arguments": {} });
    if with_caps {
        params["_meta"] = task_meta();
    }
    rpc(
        client,
        auth,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}),
    )
    .await
}

async fn task_rpc(client: &TestClient, auth: Option<&str>, method: &str, task_id: &str) -> Value {
    rpc(
        client,
        auth,
        json!({
            "jsonrpc":"2.0","id":2,"method":method,
            "params": { "taskId": task_id, "_meta": task_meta() }
        }),
    )
    .await
}

async fn get_task(client: &TestClient, task_id: &str) -> Value {
    task_rpc(client, None, "tasks/get", task_id).await
}

/// Poll `tasks/get` until the task is terminal. Panics after 10 s.
async fn wait_terminal(client: &TestClient, task_id: &str) -> Value {
    for _ in 0..1000 {
        let out = get_task(client, task_id).await;
        let status = out["result"]["status"].as_str().unwrap_or_default();
        if matches!(status, "completed" | "failed" | "cancelled") {
            return out;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {task_id} did not reach a terminal status");
}

/// Poll `check` for up to 10 s.
async fn wait_until(check: impl Fn() -> bool) -> bool {
    for _ in 0..1000 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn task_id(created: &Value) -> String {
    created["result"]["taskId"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a CreateTaskResult, got {created}"))
        .to_owned()
}

fn is_iso8601(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
}

// ── Capability negotiation ────────────────────────────────────────────

#[tokio::test]
async fn initialize_advertises_tasks_extension_when_a_task_tool_exists() {
    let client = app(routes![build_report, quick_read]);
    let out = rpc(
        &client,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    )
    .await;
    assert_eq!(
        out["result"]["capabilities"]["extensions"][TASKS],
        json!({}),
        "tasks extension must be advertised: {out}"
    );
}

#[tokio::test]
async fn initialize_omits_tasks_extension_without_task_tools() {
    let client = app(routes![quick_read]);
    let out = rpc(
        &client,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert!(
        out["result"]["capabilities"]["extensions"][TASKS].is_null(),
        "no task tool, so no tasks extension: {out}"
    );
    let get = get_task(&client, "anything").await;
    assert_eq!(get["error"]["code"], -32601, "{get}");
}

#[tokio::test]
async fn route_builder_opts_a_route_in_as_a_task_tool() {
    let routes: Vec<Route> = routes![quick_read]
        .into_iter()
        .map(Route::mcp_task)
        .collect();
    let client = app(routes);
    let created = call(&client, "quick_read", true).await;
    assert_eq!(created["result"]["resultType"], "task", "{created}");
}

// ── Task creation and polling ─────────────────────────────────────────

#[tokio::test]
async fn task_tool_returns_create_task_result() {
    let client = app(routes![build_report]);
    let created = call(&client, "build_report", true).await;
    let result = &created["result"];
    assert_eq!(created["id"], 1);
    assert_eq!(result["resultType"], "task", "{created}");
    assert_eq!(result["status"], "working");
    let id = result["taskId"].as_str().unwrap();
    assert!(
        id.len() >= 32 && id.chars().all(|c| c.is_ascii_hexdigit()),
        "task id must be long and random: {id}"
    );
    assert!(is_iso8601(&result["createdAt"]), "{result}");
    assert!(is_iso8601(&result["lastUpdatedAt"]), "{result}");
    assert!(
        result["ttlMs"].as_u64().is_some_and(|ttl| ttl > 0),
        "{result}"
    );
    assert!(result["pollIntervalMs"].as_u64().is_some(), "{result}");
    assert!(
        result.get("content").is_none(),
        "a task handle has no content"
    );
}

#[tokio::test]
async fn completed_task_inlines_the_tool_result() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    let done = wait_terminal(&client, &id).await;
    let result = &done["result"];
    assert_eq!(result["resultType"], "complete", "{done}");
    assert_eq!(result["taskId"], id.as_str());
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"]["isError"], false);
    let text = result["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!({"rows": 3})
    );
}

#[tokio::test]
async fn task_ids_are_unique() {
    let client = app(routes![build_report]);
    let a = task_id(&call(&client, "build_report", true).await);
    let b = task_id(&call(&client, "build_report", true).await);
    assert_ne!(a, b);
}

#[tokio::test]
async fn tool_error_completes_with_is_error() {
    let client = app(routes![broken_report]);
    let id = task_id(&call(&client, "broken_report", true).await);
    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["result"]["isError"], true, "{done}");
    assert!(done["result"].get("error").is_none());
}

/// The pipeline turns a handler panic into HTTP 500, as for a blocking call.
/// That is a tool error, so the task completes with `isError`.
#[tokio::test]
async fn panicking_handler_completes_with_is_error() {
    let client = app(routes![panicky_report]);
    let id = task_id(&call(&client, "panicky_report", true).await);
    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["result"]["isError"], true, "{done}");
}

#[tokio::test]
async fn invalid_arguments_fail_before_a_task_exists() {
    let client = app(routes![build_report]);
    let out = rpc(
        &client,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
               "params":{"name":"build_report","arguments":"bad","_meta":task_meta()}}),
    )
    .await;
    assert_eq!(out["error"]["code"], -32602, "{out}");
}

// ── Server-directed: blocking paths ───────────────────────────────────

#[tokio::test]
async fn task_tool_blocks_for_a_client_without_the_capability() {
    let client = app(routes![build_report]);
    let out = call(&client, "build_report", false).await;
    assert_ne!(out["result"]["resultType"], "task", "{out}");
    assert_eq!(out["result"]["isError"], false, "{out}");
}

#[tokio::test]
async fn plain_tool_blocks_even_with_the_capability() {
    let client = app(routes![quick_read]);
    let out = call(&client, "quick_read", true).await;
    assert_ne!(out["result"]["resultType"], "task", "{out}");
    assert_eq!(out["result"]["isError"], false, "{out}");
}

#[tokio::test]
async fn full_store_falls_back_to_a_blocking_call() {
    let client = TestApp::new()
        .routes(routes![build_report])
        .mount_mcp("/mcp")
        .mcp_tasks(McpTaskConfig::default().max_tasks(1))
        .build();
    let first = call(&client, "build_report", true).await;
    assert_eq!(first["result"]["resultType"], "task", "{first}");
    let second = call(&client, "build_report", true).await;
    assert_ne!(second["result"]["resultType"], "task", "{second}");
    assert_eq!(second["result"]["isError"], false, "{second}");
}

#[tokio::test]
async fn task_config_sets_ttl_and_poll_interval() {
    let client = TestApp::new()
        .routes(routes![build_report])
        .mount_mcp("/mcp")
        .mcp_tasks(
            McpTaskConfig::default()
                .ttl(Duration::from_secs(90))
                .poll_interval(Duration::from_millis(250)),
        )
        .build();
    let created = call(&client, "build_report", true).await;
    assert_eq!(created["result"]["ttlMs"], 90_000, "{created}");
    assert_eq!(created["result"]["pollIntervalMs"], 250, "{created}");
}

// ── tasks/* protocol errors ───────────────────────────────────────────

#[tokio::test]
async fn unknown_task_id_is_invalid_params() {
    let client = app(routes![build_report]);
    for method in ["tasks/get", "tasks/cancel", "tasks/update"] {
        let out = task_rpc(&client, None, method, "no-such-task").await;
        assert_eq!(out["error"]["code"], -32602, "{method}: {out}");
    }
}

#[tokio::test]
async fn tasks_methods_require_the_client_capability() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    for method in ["tasks/get", "tasks/cancel", "tasks/update"] {
        let out = rpc(
            &client,
            None,
            json!({"jsonrpc":"2.0","id":3,"method":method,"params":{"taskId":id}}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32021, "{method}: {out}");
        assert_eq!(
            out["error"]["data"]["requiredCapabilities"]["extensions"][TASKS],
            json!({}),
            "{method}: {out}"
        );
    }
}

#[tokio::test]
async fn batched_tasks_get_is_refused() {
    let client = app(routes![build_report]);
    let out = rpc(
        &client,
        None,
        json!([{"jsonrpc":"2.0","id":1,"method":"tasks/get",
                "params":{"taskId":"x","_meta":task_meta()}}]),
    )
    .await;
    assert_eq!(out["error"]["code"], -32600, "{out}");
}

// ── Authorization binding ─────────────────────────────────────────────

#[tokio::test]
async fn another_caller_cannot_read_the_task() {
    let client = app(routes![build_report]);
    let created = call_as(&client, Some("Bearer alice"), "build_report", true).await;
    let id = task_id(&created);

    let alice = task_rpc(&client, Some("Bearer alice"), "tasks/get", &id).await;
    assert!(alice["result"]["status"].is_string(), "{alice}");

    for method in ["tasks/get", "tasks/cancel", "tasks/update"] {
        let mallory = task_rpc(&client, Some("Bearer mallory"), method, &id).await;
        assert_eq!(mallory["error"]["code"], -32602, "{method}: {mallory}");
    }
    let anonymous = get_task(&client, &id).await;
    assert_eq!(anonymous["error"]["code"], -32602, "{anonymous}");
}

// ── Cancellation and update ───────────────────────────────────────────

#[tokio::test]
async fn cancel_stops_a_running_task() {
    let client = app(routes![slow_report]);
    let id = task_id(&call(&client, "slow_report", true).await);
    assert!(
        wait_until(|| SLOW_STARTED.load(Ordering::SeqCst)).await,
        "the handler did not start"
    );
    assert_eq!(get_task(&client, &id).await["result"]["status"], "working");

    let ack = task_rpc(&client, None, "tasks/cancel", &id).await;
    assert_eq!(ack["result"], json!({"resultType": "complete"}), "{ack}");

    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["status"], "cancelled", "{done}");
    assert!(
        wait_until(|| SLOW_DROPPED.load(Ordering::SeqCst)).await,
        "the cancelled handler kept running"
    );
}

#[tokio::test]
async fn cancel_does_not_change_a_completed_task() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    wait_terminal(&client, &id).await;
    let ack = task_rpc(&client, None, "tasks/cancel", &id).await;
    assert_eq!(ack["result"], json!({"resultType": "complete"}), "{ack}");
    assert_eq!(
        get_task(&client, &id).await["result"]["status"],
        "completed"
    );
}

#[tokio::test]
async fn update_is_acknowledged() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    let ack = rpc(
        &client,
        None,
        json!({"jsonrpc":"2.0","id":4,"method":"tasks/update",
               "params":{"taskId":id,"inputResponses":{"k":{"action":"accept"}},"_meta":task_meta()}}),
    )
    .await;
    assert_eq!(ack["result"], json!({"resultType": "complete"}), "{ack}");
}

#[tokio::test]
async fn update_needs_an_input_responses_object() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    for params in [
        json!({"taskId": id, "_meta": task_meta()}),
        json!({"taskId": id, "inputResponses": [], "_meta": task_meta()}),
    ] {
        let out = rpc(
            &client,
            None,
            json!({"jsonrpc":"2.0","id":5,"method":"tasks/update","params":params}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32602, "{out}");
    }
}

// ── TTL ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn expired_task_is_not_found() {
    use chrono::TimeZone as _;
    let clock = autumn_web::time::TickingClock::starting_at(
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    );
    let client = TestApp::new()
        .routes(routes![build_report])
        .with_clock(clock)
        .mount_mcp("/mcp")
        .mcp_tasks(McpTaskConfig::default().ttl(Duration::from_secs(60)))
        .build();
    let created = call(&client, "build_report", true).await;
    assert_eq!(created["result"]["createdAt"], "2026-01-01T00:00:00.000Z");
    let id = task_id(&created);
    wait_terminal(&client, &id).await;

    client.advance_clock(Duration::from_secs(61));
    let out = get_task(&client, &id).await;
    assert_eq!(out["error"]["code"], -32602, "{out}");
}

// ── Long work ─────────────────────────────────────────────────────────

#[tokio::test]
async fn long_work_is_not_cut_by_the_envelope_timeout() {
    let mut config = AutumnConfig::default();
    config.server.timeouts.request_timeout_ms = Some(100);
    let client = TestApp::new()
        .routes(routes![long_report])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    // Blocking: the envelope deadline cuts the call.
    client
        .post("/mcp")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                      "params":{"name":"long_report","arguments":{}}}))
        .send()
        .await
        .assert_status(503);

    // Task: only the route's own timeout (`off`) applies.
    let id = task_id(&call(&client, "long_report", true).await);
    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["result"]["isError"], false, "{done}");
}

// ── Audit ─────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct RecordingSink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl AuditSink for RecordingSink {
    fn write(
        &self,
        event: AuditEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
    {
        let events = Arc::clone(&self.events);
        Box::pin(async move {
            events.lock().expect("audit lock").push(event);
            Ok(())
        })
    }
}

impl RecordingSink {
    fn actions(&self) -> Vec<(String, Option<String>)> {
        self.events
            .lock()
            .expect("audit lock")
            .iter()
            .map(|e| (e.action.clone(), e.metadata.get("result").cloned()))
            .collect()
    }

    async fn wait_for(&self, action: &str) -> Vec<(String, Option<String>)> {
        for _ in 0..1000 {
            let actions = self.actions();
            if actions.iter().any(|(a, _)| a == action) {
                return actions;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no `{action}` audit event: {:?}", self.actions());
    }
}

fn audited_app(routes: Vec<Route>, sink: RecordingSink) -> TestClient {
    TestApp::new()
        .routes(routes)
        .state_initializer(move |state| {
            state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
        })
        .mount_mcp("/mcp")
        .build()
}

#[tokio::test]
async fn task_call_writes_attempt_and_outcome() {
    let sink = RecordingSink::default();
    let client = audited_app(routes![build_report], sink.clone());
    let id = task_id(&call(&client, "build_report", true).await);
    wait_terminal(&client, &id).await;
    let actions = sink.wait_for("agent.tool.build_report").await;
    assert!(
        actions
            .iter()
            .any(|(a, _)| a == "agent.tool.build_report.attempt"),
        "{actions:?}"
    );
}

#[tokio::test]
async fn cancelled_task_writes_an_aborted_outcome() {
    let sink = RecordingSink::default();
    let client = audited_app(routes![stalled_report], sink.clone());
    let id = task_id(&call(&client, "stalled_report", true).await);
    sink.wait_for("agent.tool.stalled_report.attempt").await;
    task_rpc(&client, None, "tasks/cancel", &id).await;
    let actions = sink.wait_for("agent.tool.stalled_report").await;
    let outcome = actions
        .iter()
        .find(|(a, _)| a == "agent.tool.stalled_report")
        .unwrap();
    assert_eq!(outcome.1.as_deref(), Some("task_aborted"), "{actions:?}");
}

/// Stalls the outcome write of `build_report` and flags its start.
#[derive(Clone, Default)]
struct StallingOutcomeSink {
    outcome_started: Arc<AtomicBool>,
}

impl AuditSink for StallingOutcomeSink {
    fn write(
        &self,
        event: AuditEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
    {
        let started = Arc::clone(&self.outcome_started);
        Box::pin(async move {
            if event.action == "agent.tool.build_report" {
                started.store(true, Ordering::SeqCst);
                // The framework stops waiting after its audit timeout.
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn cancel_during_the_outcome_write_keeps_the_result() {
    let sink = StallingOutcomeSink::default();
    let started = Arc::clone(&sink.outcome_started);
    let client = TestApp::new()
        .routes(routes![build_report])
        .state_initializer(move |state| {
            state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
        })
        .mount_mcp("/mcp")
        .build();
    let id = task_id(&call(&client, "build_report", true).await);
    assert!(
        wait_until(|| started.load(Ordering::SeqCst)).await,
        "no outcome write"
    );
    task_rpc(&client, None, "tasks/cancel", &id).await;
    let task = get_task(&client, &id).await;
    assert_eq!(task["result"]["status"], "completed", "{task}");
    assert_eq!(task["result"]["result"]["isError"], false, "{task}");
}

#[tokio::test]
async fn ttl_stops_a_task_without_a_new_request() {
    let sink = RecordingSink::default();
    let client = TestApp::new()
        .routes(routes![idle_report])
        .state_initializer({
            let sink = sink.clone();
            move |state| {
                state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
            }
        })
        .mount_mcp("/mcp")
        .mcp_tasks(McpTaskConfig::default().ttl(Duration::from_secs(1)))
        .build();
    task_id(&call(&client, "idle_report", true).await);
    // No more requests: the job must still stop when its TTL ends.
    let actions = sink.wait_for("agent.tool.idle_report").await;
    let outcome = actions
        .iter()
        .find(|(a, _)| a == "agent.tool.idle_report")
        .unwrap();
    assert_eq!(outcome.1.as_deref(), Some("task_aborted"), "{actions:?}");
}

#[tokio::test]
async fn one_client_cannot_take_every_task_slot() {
    let client = TestApp::new()
        .routes(routes![build_report])
        .mount_mcp("/mcp")
        .mcp_tasks(McpTaskConfig::default().max_tasks_per_caller(1))
        .build();
    let first = call_as(&client, Some("Bearer alice"), "build_report", true).await;
    assert_eq!(first["result"]["resultType"], "task", "{first}");
    let second = call_as(&client, Some("Bearer alice"), "build_report", true).await;
    assert_ne!(second["result"]["resultType"], "task", "{second}");
    // A new `Authorization` value from the same client gets no new quota.
    let rotated = call_as(&client, Some("Bearer forged-1"), "build_report", true).await;
    assert_ne!(rotated["result"]["resultType"], "task", "{rotated}");
}

#[tokio::test]
async fn result_over_the_byte_budget_completes_with_is_error() {
    let client = TestApp::new()
        .routes(routes![build_report])
        .mount_mcp("/mcp")
        .mcp_tasks(McpTaskConfig::default().max_stored_bytes(8))
        .build();
    let id = task_id(&call(&client, "build_report", true).await);
    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["result"]["isError"], true, "{done}");
}

#[tokio::test]
async fn completed_result_carries_result_type() {
    let client = app(routes![build_report]);
    let id = task_id(&call(&client, "build_report", true).await);
    let done = wait_terminal(&client, &id).await;
    assert_eq!(done["result"]["result"]["resultType"], "complete", "{done}");
}

// ── Transport ─────────────────────────────────────────────────────────

#[tokio::test]
async fn preflight_allows_task_routing_headers() {
    let mut config = AutumnConfig::default();
    config.cors.allowed_origins = vec!["https://app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![build_report])
        .config(config)
        .mount_mcp("/mcp")
        .build();
    let resp = client
        .options("/mcp")
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    resp.assert_status(204);
    let allowed = resp
        .header("access-control-allow-headers")
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(allowed.contains("mcp-name"), "{allowed}");
    assert!(allowed.contains("mcp-method"), "{allowed}");
}
