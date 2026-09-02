//! Integration tests for the MCP half of the agent-authority envelope
//! (issue #1691): every `tools/call` is audited against the handler's
//! *compile-known* blast radius, with zero per-handler wiring.
//!
//! Covers the acceptance criteria owned by the MCP/audit slice:
//!
//! 1. A `tools/call` writes **two** audit events — `agent.tool.<name>.attempt`
//!    before dispatch and `agent.tool.<name>` after it — sharing one
//!    correlation id, so an invocation that crashes the process mid-flight
//!    still leaves a record that it was attempted.
//! 2. Both events carry the transport, the grant name, the reversibility and
//!    the tool name; the outcome additionally carries the HTTP status and the
//!    pipeline's own `x-request-id`, so an audit row joins to the access log.
//! 3. An **ungoverned** tool (MCP-exposed, no `#[agent_operable]`) is still
//!    audited, with `reversibility = "unknown"` and no grant — the audit trail
//!    never silently omits an agent action just because nobody annotated it.
//! 4. Metadata records argument *names*, never argument *values*: an audit
//!    sink is not a place to spill request payloads.
//! 5. The actor is never empty — `agent:anonymous` for an unauthenticated call.
//! 6. Fail-closed on a broken sink: when the attempt record cannot be written
//!    and the action is not `reversible`, the tool errors and the handler is
//!    never invoked. A `reversible` action proceeds (the write is warned about,
//!    not enforced).
//! 7. `Extension<AgentInvocation>` is readable inside the handler, so an
//!    application can thread the correlation id into its own records.
//! 8. The outcome describes what actually happened, never the status line
//!    alone: a buffered body that overflows the tool-result cap is a `Failure`
//!    carrying `result`, and a `200` behind it is recorded as the fact it is.
//! 9. A streaming tool's outcome is written when the *projection* ends —
//!    `completed`, `errored` or (from the drop guard, when the client hangs up)
//!    `aborted` — never from the `200` that opened the stream.

#![cfg(feature = "mcp")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use autumn_web::Route;
use autumn_web::agent_authority::{
    AgentAuthority, AgentInvocation, Effect, EffectKind, EffectProvenance, Grant, Reversibility,
    TenantScope,
};
use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink, AuditStatus};
use autumn_web::middleware::RequestId;
use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::{TestApp, TestClient};
use axum::extract::Extension;
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use tower::ServiceExt as _;

// ── Authority fixtures ────────────────────────────────────────────────
//
// The route macro normally emits these statics from `#[agent_operable]`. Built
// by hand here on purpose: this suite is about the *audit* path, and hand-built
// authorities let it exercise every reversibility without depending on the
// analyser proving a matching effect set from a handler body.

/// One construction site for the test grants, so the shape of [`Grant`] is
/// pinned in exactly one place in this file.
const fn test_grant(name: &'static str, reversibility: Reversibility) -> Grant {
    Grant {
        name,
        writes: &["Refund"],
        unbounded_writes: &[],
        tenant_scope: TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility,
        location: "autumn/tests/integration/agent_authority.rs",
    }
}

/// Likewise for [`AgentAuthority`].
const fn test_authority(action: &'static str, grant: &'static Grant) -> AgentAuthority {
    AgentAuthority {
        action,
        module_path: "integration_tests::agent_authority",
        location: "autumn/tests/integration/agent_authority.rs",
        grant,
        effects: &[Effect {
            kind: EffectKind::Write,
            subject: "Refund",
            location: "autumn/tests/integration/agent_authority.rs",
            provenance: EffectProvenance::TypeResolved,
        }],
        asserted_effect_free_sites: 0,
        asserted_effect_free: &[],
    }
}

static COMPENSABLE_GRANT: Grant = test_grant("RefundDrafter", Reversibility::Compensable);
static IRREVERSIBLE_GRANT: Grant = test_grant("PayoutSender", Reversibility::Irreversible);
static REVERSIBLE_GRANT: Grant = test_grant("LineDrafter", Reversibility::Reversible);

static DRAFT_REFUND_AUTHORITY: AgentAuthority = test_authority("draft_refund", &COMPENSABLE_GRANT);
static SEND_PAYOUT_AUTHORITY: AgentAuthority = test_authority("send_payout", &IRREVERSIBLE_GRANT);
static DRAFT_LINE_AUTHORITY: AgentAuthority = test_authority("draft_line", &REVERSIBLE_GRANT);

// ── Recording / failing audit sinks ───────────────────────────────────

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
            events.lock().expect("audit sink lock").push(event);
            Ok(())
        })
    }
}

impl RecordingSink {
    fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("audit sink lock").clone()
    }

    /// The events this suite cares about, in write order.
    fn agent_events(&self) -> Vec<AuditEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.action.starts_with("agent.tool."))
            .collect()
    }
}

/// A sink that always fails, to prove the fail-closed attempt record.
struct FailingSink;

impl AuditSink for FailingSink {
    fn write(
        &self,
        _event: AuditEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
    {
        Box::pin(async { Err(AuditError::new("sink down")) })
    }
}

/// A sink whose write never resolves — a remote collector that accepted the
/// connection and went quiet, or a saturated pool. Distinct from
/// [`FailingSink`]: nothing here ever returns an error to react to, so only a
/// deadline can turn it back into a decision.
struct StalledSink;

impl AuditSink for StalledSink {
    fn write(
        &self,
        _event: AuditEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AuditError>> + Send + '_>>
    {
        Box::pin(std::future::pending())
    }
}

// ── Handlers ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct RefundRequest {
    memo: String,
}

/// What the governed handler echoes back, so a test can compare what the
/// handler observed against what the audit trail recorded.
#[derive(Serialize, Deserialize)]
struct RefundReceipt {
    correlation_id: String,
    tool: String,
    grant: Option<String>,
    request_id: String,
}

/// The governed tool. Reads `Extension<AgentInvocation>` to prove the envelope
/// reaches application code, and `Extension<RequestId>` so the test can pin the
/// audit row's `request_id` to the *pipeline's* id rather than to itself.
#[post("/api/refunds")]
#[api_doc(mcp, summary = "Draft a refund")]
async fn draft_refund(
    Extension(invocation): Extension<AgentInvocation>,
    Extension(request_id): Extension<RequestId>,
    Json(_body): Json<RefundRequest>,
) -> AutumnResult<Json<RefundReceipt>> {
    Ok(Json(RefundReceipt {
        correlation_id: invocation.correlation_id.clone(),
        tool: invocation.tool.clone(),
        grant: invocation.grant.map(str::to_owned),
        request_id: request_id.to_string(),
    }))
}

#[derive(Serialize, Deserialize)]
struct NoteRequest {
    text: String,
}

#[derive(Serialize, Deserialize)]
struct NoteReceipt {
    stored: bool,
}

/// The ungoverned tool: MCP-exposed and mutating, with no `#[agent_operable]`.
#[post("/api/notes")]
#[api_doc(mcp, summary = "Write a note")]
async fn write_note(Json(_body): Json<NoteRequest>) -> AutumnResult<Json<NoteReceipt>> {
    Ok(Json(NoteReceipt { stored: true }))
}

static PAYOUT_CALLS: AtomicUsize = AtomicUsize::new(0);
static LINE_CALLS: AtomicUsize = AtomicUsize::new(0);
static STALLED_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct AmountRequest {
    amount: u32,
}

#[derive(Serialize, Deserialize)]
struct AmountReceipt {
    accepted: u32,
}

/// Irreversible tool with an invocation counter: proves the handler is never
/// reached when the attempt record cannot be written.
#[post("/api/payouts")]
#[api_doc(mcp, summary = "Send a payout")]
async fn send_payout(Json(body): Json<AmountRequest>) -> AutumnResult<Json<AmountReceipt>> {
    PAYOUT_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Json(AmountReceipt {
        accepted: body.amount,
    }))
}

/// Irreversible tool used with a sink that never answers, so the counter proves
/// a *deadline* (not an error) stopped the handler.
#[post("/api/wires")]
#[api_doc(mcp, summary = "Send a wire")]
async fn send_wire(Json(body): Json<AmountRequest>) -> AutumnResult<Json<AmountReceipt>> {
    STALLED_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Json(AmountReceipt {
        accepted: body.amount,
    }))
}

/// A governed tool whose handler *fails*, so the outcome event has a non-2xx
/// status to carry.
#[post("/api/voids")]
#[api_doc(mcp, summary = "Void a refund")]
async fn void_refund(Json(_body): Json<RefundRequest>) -> AutumnResult<Json<RefundReceipt>> {
    Err(AutumnError::not_found_msg("no such refund"))
}

/// A governed *streaming* tool. Its outcome is recorded when the projection
/// ends, not when the handler answered, so the pair is only complete once the
/// body has been consumed to the end.
#[get("/api/refund-feed")]
#[api_doc(mcp, stream, summary = "Stream refund events")]
async fn refund_feed() -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = futures::stream::iter(vec![
        Ok(Event::default().data("refund 1")),
        Ok(Event::default().data("refund 2")),
    ]);
    Sse::new(stream)
}

/// A governed streaming tool that pauses between events, so a test can drop the
/// response while the handler still has output to give.
#[get("/api/slow-feed")]
#[api_doc(mcp, stream, summary = "Stream refund events slowly")]
async fn slow_refund_feed() -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = futures::stream::unfold(0_u32, |n| async move {
        if n >= 6 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Some((Ok(Event::default().data(format!("refund {n}"))), n + 1))
    });
    Sse::new(stream)
}

/// A governed streaming tool whose body fails part-way through: one good frame,
/// then a transport error on the stream itself.
///
/// The handler still answers `200` — the failure exists only *inside* the body,
/// which is exactly the ending a status-line outcome could never see (issue
/// #1691 review round 1).
#[get("/api/broken-feed")]
#[api_doc(mcp, stream, summary = "Stream refund events, then fail")]
async fn broken_refund_feed() -> Sse<impl Stream<Item = Result<Event, std::io::Error>>> {
    let stream = futures::stream::iter(vec![
        Ok(Event::default().data("refund 1")),
        Err(std::io::Error::other("the refund feed went away")),
    ]);
    Sse::new(stream)
}

#[derive(Serialize, Deserialize)]
struct BulkReport {
    rows: String,
}

/// A governed tool whose body blows past the MCP tool-result cap.
///
/// The handler itself succeeds — it answers `200` with a well-formed JSON body
/// — so the only place the failure becomes visible is when the buffered path
/// tries to read that body back (issue #1691 review round 2). It returns
/// `Json<T>` rather than a bare `String` because MCP only derives a tool for a
/// route with a response schema.
#[get("/api/oversized")]
#[api_doc(mcp, summary = "Return more than the tool-result cap")]
async fn oversized_report() -> AutumnResult<Json<BulkReport>> {
    // Over the 10 MiB `MAX_TOOL_RESPONSE_BYTES` once serialized. `Accept-
    // Encoding` is not among the headers the MCP replay forwards, so nothing
    // compresses this back under the cap before it is measured.
    Ok(Json(BulkReport {
        rows: "r".repeat(10 * 1024 * 1024 + 1),
    }))
}

/// Reversible twin of [`send_payout`]: a broken sink must NOT stop it.
#[post("/api/lines")]
#[api_doc(mcp, summary = "Draft a refund line")]
async fn draft_line(Json(body): Json<AmountRequest>) -> AutumnResult<Json<AmountReceipt>> {
    LINE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Json(AmountReceipt {
        accepted: body.amount,
    }))
}

// ── Harness ───────────────────────────────────────────────────────────

/// Stamp a hand-built authority onto every route in `routes`, standing in for
/// what `#[agent_operable]` writes into `ApiDoc::agent_authority`.
fn governed_by(routes: Vec<Route>, authority: &'static AgentAuthority) -> Vec<Route> {
    routes
        .into_iter()
        .map(|mut route| {
            route.api_doc.agent_authority = Some(authority);
            route
        })
        .collect()
}

fn app_with_sink(routes: Vec<Route>, sink: Arc<dyn AuditSink>) -> TestClient {
    app_with_sinks(routes, vec![sink])
}

fn app_with_sinks(routes: Vec<Route>, sinks: Vec<Arc<dyn AuditSink>>) -> TestClient {
    TestApp::new()
        .routes(routes)
        .state_initializer(move |state| {
            let logger = sinks
                .into_iter()
                .fold(AuditLogger::new(), AuditLogger::with_sink);
            state.insert_extension(logger);
        })
        .mount_mcp("/mcp")
        .build()
}

async fn call_tool(
    client: &TestClient,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// Decode the JSON a successful `tools/call` returned in its text content.
fn tool_payload(out: &serde_json::Value) -> serde_json::Value {
    assert_ne!(
        out["result"]["isError"], true,
        "unexpected tool error: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result carries no text content: {out}"));
    serde_json::from_str(text).expect("tool result text is JSON")
}

fn meta<'a>(event: &'a AuditEvent, key: &str) -> &'a str {
    event
        .metadata
        .get(key)
        .unwrap_or_else(|| {
            panic!(
                "audit event `{}` is missing metadata key `{key}`: {:?}",
                event.action, event.metadata
            )
        })
        .as_str()
}

// ── 1 + 2: the attempt/outcome pair ───────────────────────────────────

#[tokio::test]
async fn governed_tools_call_writes_an_attempt_and_an_outcome_event() {
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![draft_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let out = call_tool(
        &client,
        "draft_refund",
        serde_json::json!({ "body": { "memo": "late shipment" } }),
    )
    .await;
    let payload = tool_payload(&out);

    let events = sink.agent_events();
    assert_eq!(
        events.len(),
        2,
        "one tools/call writes exactly an attempt and an outcome: {events:#?}"
    );
    assert_eq!(events[0].action, "agent.tool.draft_refund.attempt");
    assert_eq!(events[1].action, "agent.tool.draft_refund");
    // `phase` is what tells the rows apart once they are in the sink: the
    // attempt is deliberately `Success` (the *write* succeeded; nothing has run
    // yet), so `status` alone cannot distinguish it from a completed call.
    assert_eq!(meta(&events[0], "phase"), "attempt");
    assert_eq!(meta(&events[1], "phase"), "outcome");
    assert_eq!(events[0].status, AuditStatus::Success);
    assert_eq!(events[1].status, AuditStatus::Success);

    // The correlation id is minted before dispatch and ties the pair together.
    let correlation = meta(&events[0], "correlation_id");
    assert!(
        !correlation.is_empty(),
        "correlation id must never be empty"
    );
    assert_eq!(
        meta(&events[1], "correlation_id"),
        correlation,
        "both events describe ONE invocation"
    );

    for event in &events {
        assert_eq!(meta(event, "transport"), "mcp");
        assert_eq!(meta(event, "tool"), "draft_refund");
        assert_eq!(meta(event, "grant"), "RefundDrafter");
        assert_eq!(meta(event, "reversibility"), "compensable");
        assert!(
            meta(event, "effects").contains("Refund"),
            "the proved effect set travels with the record: {:?}",
            event.metadata
        );
        assert_eq!(
            event.target_resource_id, "/api/refunds",
            "the target is the route template, not the tool name"
        );
    }

    // Only the outcome knows how the call ended.
    assert!(
        !events[0].metadata.contains_key("http_status"),
        "the attempt is written BEFORE dispatch, so it cannot know the status"
    );
    assert_eq!(meta(&events[1], "http_status"), "200");

    // …and it joins to the access log through the pipeline's own request id.
    assert_eq!(
        meta(&events[1], "request_id"),
        payload["request_id"]
            .as_str()
            .expect("handler echoes its request id"),
        "the outcome's request_id is the replayed pipeline's x-request-id"
    );
}

// ── 3: ungoverned tools are still audited ─────────────────────────────

#[tokio::test]
async fn ungoverned_tool_is_audited_with_unknown_reversibility() {
    let sink = RecordingSink::default();
    let client = app_with_sink(routes![write_note], Arc::new(sink.clone()));

    let out = call_tool(
        &client,
        "write_note",
        serde_json::json!({ "body": { "text": "hello" } }),
    )
    .await;
    assert_ne!(out["result"]["isError"], true);

    let events = sink.agent_events();
    assert_eq!(
        events.len(),
        2,
        "an ungoverned tool is audited exactly like a governed one: {events:#?}"
    );
    assert_eq!(meta(&events[0], "phase"), "attempt");
    assert_eq!(meta(&events[1], "phase"), "outcome");
    for event in &events {
        assert_eq!(meta(event, "transport"), "mcp");
        assert_eq!(meta(event, "tool"), "write_note");
        assert_eq!(
            meta(event, "reversibility"),
            "unknown",
            "no grant means the blast radius is unknown — never assume reversible"
        );
        assert!(
            !event.metadata.contains_key("grant"),
            "an ungoverned tool must not claim a grant: {:?}",
            event.metadata
        );
    }
}

// ── 4: argument names, never argument values ──────────────────────────

#[tokio::test]
async fn metadata_records_argument_names_but_never_values() {
    const SENTINEL: &str = "SENTINEL-DO-NOT-LOG-8f3a";

    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![draft_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let out = call_tool(
        &client,
        "draft_refund",
        serde_json::json!({ "body": { "memo": SENTINEL } }),
    )
    .await;
    assert_ne!(out["result"]["isError"], true);

    let events = sink.agent_events();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(
            meta(event, "argument_names"),
            "body",
            "the shape of the call is recorded…"
        );
        for (key, value) in &event.metadata {
            assert!(
                !value.contains(SENTINEL),
                "…but never its contents: metadata[{key}] leaked the argument value"
            );
        }
        assert!(
            !event.target_resource_id.contains(SENTINEL),
            "the target must not carry argument values either"
        );
    }
}

#[tokio::test]
async fn metadata_never_echoes_a_caller_chosen_argument_key() {
    // The caller picks the JSON keys and nothing validates them against the
    // advertised schema, so an un-intersected name list would be an
    // attacker-controlled string landing in a durable audit row — with an
    // embedded newline forging a log line, and a key that is itself PII
    // smuggling the very payload the names-only rule exists to keep out
    // (issue #1691 review, P2-2).
    const FORGED_LINE: &str =
        "\n2026-09-02T00:00:00Z  INFO autumn.agent: agent tool call actor=admin tool=send_payout";
    const PII_KEY: &str = "ssn-123-45-6789";

    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![draft_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let mut args = serde_json::Map::new();
    args.insert("body".to_owned(), serde_json::json!({ "memo": "ordinary" }));
    args.insert(FORGED_LINE.to_owned(), serde_json::json!(1));
    args.insert(PII_KEY.to_owned(), serde_json::json!(2));

    let out = call_tool(&client, "draft_refund", serde_json::Value::Object(args)).await;
    // The extra keys are ignored by dispatch, so the call still succeeds.
    assert_ne!(out["result"]["isError"], true);

    let events = sink.agent_events();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(
            meta(event, "argument_names"),
            "body,+2 unknown",
            "the recognised name is kept and the rest are counted, never quoted"
        );
        for (key, value) in &event.metadata {
            assert!(
                !value.contains("ssn-") && !value.contains("actor=admin"),
                "metadata[{key}] echoed a caller-chosen key: {value:?}"
            );
            assert!(
                !value.contains('\n'),
                "metadata[{key}] carries a newline a caller supplied: {value:?}"
            );
        }
    }
}

// ── 5: the actor is never empty ───────────────────────────────────────

#[tokio::test]
async fn unauthenticated_call_is_attributed_to_the_anonymous_agent() {
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![draft_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    call_tool(
        &client,
        "draft_refund",
        serde_json::json!({ "body": { "memo": "anon" } }),
    )
    .await;

    let events = sink.agent_events();
    assert_eq!(
        events.len(),
        2,
        "the call must be audited before its actor can be checked: {events:#?}"
    );
    for event in events {
        assert_eq!(
            event.actor_id, "agent:anonymous",
            "an unattributable agent call still names an actor"
        );
    }
}

// ── 6: fail-closed when the attempt record cannot be written ──────────

#[tokio::test]
async fn irreversible_tool_refuses_to_dispatch_when_the_attempt_cannot_be_audited() {
    let before = PAYOUT_CALLS.load(Ordering::SeqCst);
    let client = app_with_sink(
        governed_by(routes![send_payout], &SEND_PAYOUT_AUTHORITY),
        Arc::new(FailingSink),
    );

    let out = call_tool(
        &client,
        "send_payout",
        serde_json::json!({ "body": { "amount": 500 } }),
    )
    .await;

    assert_eq!(
        out["result"]["isError"], true,
        "an unauditable irreversible action must fail closed: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("audit"),
        "the tool error must say the audit record could not be written: {text}"
    );
    assert_eq!(
        PAYOUT_CALLS.load(Ordering::SeqCst),
        before,
        "the handler must never run when the attempt could not be recorded"
    );
}

#[tokio::test]
async fn a_refusal_is_recorded_by_whichever_sinks_are_still_healthy() {
    // `AuditLogger::write` attempts every sink and joins the errors, so ONE
    // broken sink fails the whole write — while the healthy ones would happily
    // have taken the row. The refusal is the single most interesting thing that
    // happened, so it is re-offered to them (issue #1691 review, P3-5).
    let healthy = RecordingSink::default();
    let client = app_with_sinks(
        governed_by(routes![send_payout], &SEND_PAYOUT_AUTHORITY),
        vec![Arc::new(FailingSink), Arc::new(healthy.clone())],
    );

    let out = call_tool(
        &client,
        "send_payout",
        serde_json::json!({ "body": { "amount": 500 } }),
    )
    .await;
    assert_eq!(out["result"]["isError"], true);

    let events = healthy.agent_events();
    let refusal = events
        .iter()
        .find(|e| e.metadata.get("phase").map(String::as_str) == Some("refused"))
        .unwrap_or_else(|| panic!("the healthy sink must receive the refusal: {events:#?}"));
    assert_eq!(refusal.action, "agent.tool.send_payout.refused");
    assert_eq!(refusal.status, AuditStatus::Failure);
    assert_eq!(meta(refusal, "tool"), "send_payout");
    assert_eq!(meta(refusal, "reversibility"), "irreversible");
    assert!(
        !refusal.metadata.contains_key("http_status"),
        "nothing was dispatched, so there is no status to report: {:?}",
        refusal.metadata
    );
    // …and it joins to the attempt that could not be completed.
    assert_eq!(
        meta(refusal, "correlation_id"),
        meta(&events[0], "correlation_id")
    );
}

#[tokio::test(start_paused = true)]
async fn a_sink_that_never_answers_refuses_an_irreversible_call_on_a_deadline() {
    // Without a deadline this write would hang the request path until the
    // envelope's request timeout fired, with the handler still unrun — a slow
    // sink stalling every agent call (issue #1691 review, P3-6). The paused
    // clock makes the wait virtual, so the test proves the deadline exists
    // without spending it.
    let before = STALLED_CALLS.load(Ordering::SeqCst);
    let client = app_with_sink(
        governed_by(routes![send_wire], &SEND_PAYOUT_AUTHORITY),
        Arc::new(StalledSink),
    );

    let out = call_tool(
        &client,
        "send_wire",
        serde_json::json!({ "body": { "amount": 900 } }),
    )
    .await;

    assert_eq!(
        out["result"]["isError"], true,
        "a sink that never answers is not a reason to serve an irreversible \
         action unrecorded: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("audit"),
        "the tool error must name the audit record: {text}"
    );
    assert_eq!(
        STALLED_CALLS.load(Ordering::SeqCst),
        before,
        "the handler must never run when the attempt could not be recorded in time"
    );
}

#[tokio::test]
async fn reversible_tool_still_dispatches_when_the_attempt_cannot_be_audited() {
    let before = LINE_CALLS.load(Ordering::SeqCst);
    let client = app_with_sink(
        governed_by(routes![draft_line], &DRAFT_LINE_AUTHORITY),
        Arc::new(FailingSink),
    );

    let out = call_tool(
        &client,
        "draft_line",
        serde_json::json!({ "body": { "amount": 7 } }),
    )
    .await;

    let payload = tool_payload(&out);
    assert_eq!(
        payload["accepted"], 7,
        "a reversible action is not worth refusing over a broken sink: {out}"
    );
    assert_eq!(
        LINE_CALLS.load(Ordering::SeqCst),
        before + 1,
        "the handler must still be invoked"
    );
}

// ── 8: the outcome reports what actually happened ─────────────────────

#[tokio::test]
async fn a_failing_handler_records_a_failure_outcome_with_its_status() {
    // The audit `status` follows the HTTP status, not whether the dispatch
    // mechanism worked: an action the application refused is a *failed* agent
    // action, and an operator filtering `status = Failure` must see it.
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![void_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let out = call_tool(
        &client,
        "void_refund",
        serde_json::json!({ "body": { "memo": "gone" } }),
    )
    .await;
    assert_eq!(out["result"]["isError"], true, "the 404 reaches the caller");

    let events = sink.agent_events();
    assert_eq!(events.len(), 2, "a failed call is still a full pair");

    // The attempt was written before dispatch, so it cannot know any of this.
    assert_eq!(meta(&events[0], "phase"), "attempt");
    assert_eq!(events[0].status, AuditStatus::Success);
    assert!(!events[0].metadata.contains_key("http_status"));

    let outcome = &events[1];
    assert_eq!(meta(outcome, "phase"), "outcome");
    assert_eq!(outcome.status, AuditStatus::Failure);
    assert_eq!(meta(outcome, "http_status"), "404");
    assert_eq!(
        meta(outcome, "correlation_id"),
        meta(&events[0], "correlation_id"),
        "a failed call still joins to its attempt"
    );
}

#[tokio::test]
async fn a_body_that_overflows_the_tool_result_cap_records_a_failure() {
    // The handler answered `200` and the dispatch mechanism worked, but the
    // buffered path could not hand the body to the agent: the agent gets a tool
    // error, so the audit row must not claim a success behind it (issue #1691
    // review round 2).
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![oversized_report], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let out = call_tool(&client, "oversized_report", serde_json::json!({})).await;
    assert_eq!(
        out["result"]["isError"], true,
        "an unreadable body is a tool error: {out}"
    );

    let events = sink.agent_events();
    assert_eq!(events.len(), 2, "an overflow is still a full pair");

    let outcome = &events[1];
    assert_eq!(meta(outcome, "phase"), "outcome");
    assert_eq!(
        outcome.status,
        AuditStatus::Failure,
        "a result the agent never received is not a successful action"
    );
    assert_eq!(meta(outcome, "result"), "body_overflow");
    assert_eq!(
        meta(outcome, "http_status"),
        "200",
        "the handler's own status is still recorded as the fact it is"
    );
    assert_eq!(
        meta(outcome, "correlation_id"),
        meta(&events[0], "correlation_id"),
        "the overflow still joins to its attempt"
    );
}

// ── 9: streaming tools are audited like any other ─────────────────────

#[tokio::test]
async fn a_streaming_tool_is_audited_without_disturbing_its_stream() {
    // The outcome is recorded when the SSE projection *ends*, not from the
    // `200` that opened it — so the pair is complete only after the body has
    // been consumed, and the projection itself must be untouched by the audit
    // riding along with it.
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![refund_feed], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let resp = client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "refund_feed",
                "arguments": {},
                // The projection correlates progress notifications to this
                // token; without one the client gets only the final result.
                "_meta": { "progressToken": "tok-1" },
            },
        }))
        .send()
        .await;
    resp.assert_ok();

    // The stream is untouched: the handler's events still arrive as
    // `notifications/progress`, terminated by the tool result.
    let body = resp.text();
    let messages: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|s| serde_json::from_str(s.trim()).ok())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m["method"] == "notifications/progress"),
        "the SSE projection must be unaffected: {body}"
    );
    assert!(
        messages.iter().any(|m| m.get("result").is_some()),
        "the stream must still terminate with the tool result: {body}"
    );

    let events = sink.agent_events();
    assert_eq!(
        events.len(),
        2,
        "a streaming tool writes exactly one attempt and one outcome: {events:#?}"
    );
    assert_eq!(events[0].action, "agent.tool.refund_feed.attempt");
    assert_eq!(meta(&events[0], "phase"), "attempt");

    // The outcome is written when the *projection* ends, not when the handler
    // answered `200` — a streaming handler returns that before it has produced
    // anything. Reading the sink only now (after the body above was consumed to
    // the end) is the point: an up-front record would have been sitting here
    // claiming success before a single event was emitted.
    let outcome = &events[1];
    assert_eq!(outcome.action, "agent.tool.refund_feed");
    assert_eq!(meta(outcome, "phase"), "outcome");
    assert_eq!(outcome.status, AuditStatus::Success);
    assert_eq!(meta(outcome, "http_status"), "200");
    assert_eq!(meta(outcome, "grant"), "RefundDrafter");
    assert_eq!(
        meta(outcome, "stream_state"),
        "completed",
        "the handler's body ended normally"
    );
}

#[tokio::test]
async fn a_stream_that_fails_mid_flight_is_recorded_as_errored() {
    // The other way a `200` lies: the handler opened its stream, emitted part of
    // its output, and then the body itself errored. The projection reaches a
    // terminal state on its own here — no client had to go away — so the outcome
    // is recorded inline rather than by the drop guard, and it must still be a
    // `Failure` (issue #1691 review round 1).
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![broken_refund_feed], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let resp = client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "broken_refund_feed",
                "arguments": {},
                "_meta": { "progressToken": "tok-1" },
            },
        }))
        .send()
        .await;
    resp.assert_ok();

    // Whatever the handler managed to emit before failing still reaches the
    // agent: a broken stream is audited as a failure, not censored.
    let body = resp.text();
    assert!(
        body.contains("refund 1"),
        "the frames that did arrive must still be projected: {body}"
    );

    let events = sink.agent_events();
    assert_eq!(
        events.len(),
        2,
        "a failed stream is still exactly one attempt and one outcome: {events:#?}"
    );

    let outcome = &events[1];
    assert_eq!(outcome.action, "agent.tool.broken_refund_feed");
    assert_eq!(meta(outcome, "phase"), "outcome");
    assert_eq!(
        outcome.status,
        AuditStatus::Failure,
        "a stream that broke part-way through did not deliver the action"
    );
    assert_eq!(meta(outcome, "stream_state"), "errored");
    assert_eq!(
        meta(outcome, "http_status"),
        "200",
        "the status the handler answered with is still recorded as the fact it is"
    );
    assert_eq!(
        meta(outcome, "correlation_id"),
        meta(&events[0], "correlation_id"),
        "the failure still joins to its attempt"
    );
}

#[tokio::test]
async fn a_stream_cut_off_mid_flight_is_recorded_as_aborted() {
    // The failure the up-front record could not express: the handler answered
    // `200`, emitted part of its output, and then the client went away. Nothing
    // about the status line says that happened, so the outcome has to come from
    // the projection's own fate (issue #1691 review round 1).
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![slow_refund_feed], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    // Driven through the router directly rather than `TestClient`: that helper
    // drains the whole body with `to_bytes(.., usize::MAX)` before handing back
    // a response, so it can never model a client that goes away mid-stream —
    // which is precisely the case under test.
    let router = client.into_router();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "slow_refund_feed",
                    "arguments": {},
                    "_meta": { "progressToken": "tok-1" },
                },
            }))
            .expect("serialize request"),
        ))
        .expect("build request");

    let response = router.oneshot(request).await.expect("dispatch");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // Take one projected frame so the handler is genuinely mid-stream — it
    // sleeps between events — then hang up without draining the rest. The
    // projection never reaches a terminal state, so the drop guard is the only
    // thing left that can record the outcome.
    let mut body = response.into_body().into_data_stream();
    assert!(
        body.next().await.is_some(),
        "the projection should have emitted at least one frame"
    );
    drop(body);

    // The guard cannot await inside `Drop`, so it spawns the write; give that
    // task a turn.
    for _ in 0..50 {
        if sink
            .agent_events()
            .iter()
            .any(|e| e.metadata.get("phase").map(String::as_str) == Some("outcome"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let events = sink.agent_events();
    let outcome = events
        .iter()
        .find(|e| e.metadata.get("phase").map(String::as_str) == Some("outcome"))
        .unwrap_or_else(|| panic!("an abandoned stream must still record an outcome: {events:#?}"));

    assert_eq!(outcome.action, "agent.tool.slow_refund_feed");
    assert_eq!(
        outcome.status,
        AuditStatus::Failure,
        "a stream that never finished is not a successful action"
    );
    assert_eq!(meta(outcome, "stream_state"), "aborted");
    assert_eq!(
        meta(outcome, "http_status"),
        "200",
        "the 200 is still a fact"
    );
    assert_eq!(
        meta(outcome, "correlation_id"),
        meta(&events[0], "correlation_id"),
        "the abort still joins to its attempt"
    );

    // Exactly one outcome, however the projection ended.
    assert_eq!(
        events
            .iter()
            .filter(|e| e.metadata.get("phase").map(String::as_str) == Some("outcome"))
            .count(),
        1,
        "exactly one outcome per invocation: {events:#?}"
    );
}

// ── 7: the invocation reaches the handler ─────────────────────────────

#[tokio::test]
async fn handler_can_read_the_agent_invocation_extension() {
    let sink = RecordingSink::default();
    let client = app_with_sink(
        governed_by(routes![draft_refund], &DRAFT_REFUND_AUTHORITY),
        Arc::new(sink.clone()),
    );

    let out = call_tool(
        &client,
        "draft_refund",
        serde_json::json!({ "body": { "memo": "threaded" } }),
    )
    .await;
    let payload = tool_payload(&out);

    assert_eq!(payload["tool"], "draft_refund");
    assert_eq!(payload["grant"], "RefundDrafter");

    let events = sink.agent_events();
    assert!(
        !events.is_empty(),
        "the invocation must have been audited at all"
    );
    let recorded = meta(&events[0], "correlation_id");
    assert_eq!(
        payload["correlation_id"].as_str(),
        Some(recorded),
        "the handler sees the SAME correlation id the audit trail recorded, so an \
         application can join its own records to the agent's invocation"
    );
}
