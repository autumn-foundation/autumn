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

#![cfg(feature = "mcp")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use autumn_web::Route;
use autumn_web::agent_authority::{
    AgentAuthority, AgentInvocation, Effect, EffectKind, EffectProvenance, Grant, Reversibility,
    TenantScope,
};
use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink};
use autumn_web::middleware::RequestId;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use axum::extract::Extension;
use serde::{Deserialize, Serialize};

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
    TestApp::new()
        .routes(routes)
        .state_initializer(move |state| {
            state.insert_extension(AuditLogger::new().with_sink(sink));
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
