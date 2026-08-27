//! Integration tests for admin user impersonation (issue #1394).
//!
//! Covers the core-auth half of the acceptance criteria:
//!
//! 1. `begin_impersonation` / `end_impersonation` record the effective user and
//!    the original `impersonator_id` distinctly in the session.
//! 2. Current-user resolution (`#[secured]`) returns the **impersonated** user;
//!    a separate accessor exposes the real impersonator.
//! 3. Beginning impersonation is default-deny — an app without a registered
//!    [`ImpersonationGate`] gets `403`, and a user outside the opted-in role
//!    gets `403` too.
//! 4. The session id is rotated on both begin and end.
//! 5. An audit event is written on begin and on end, each carrying
//!    `{impersonator_id, target_id}`.
//! 6. While impersonating, the ambient current actor (#1383) — which seeds
//!    audit/version attribution — is the **real impersonator**, not the target.
//! 7. Nesting is rejected.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink, AuditStatus};
use autumn_web::auth::impersonation::{
    self, IMPERSONATOR_SESSION_KEY, ImpersonationGate, ImpersonationPolicy, ImpersonationTarget,
};
use autumn_web::authorization::{BoxFuture, PolicyContext};
use autumn_web::current::Current;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// ── Recording audit sink ──────────────────────────────────────

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

    fn actions(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.action).collect()
    }
}

/// A sink that always fails, to prove `begin` fails closed when the
/// privileged action cannot be audited.
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

// ── Handlers ──────────────────────────────────────────────────

/// Log in as an admin (role `admin`), the "real" way.
#[autumn_web::post("/login-admin")]
async fn login_admin(session: Session) -> &'static str {
    session.insert("user_id", "admin-1").await;
    session.insert("role", "admin").await;
    "ok"
}

/// Log in as a plain support user with no impersonation grant.
#[autumn_web::post("/login-support")]
async fn login_support(session: Session) -> &'static str {
    session.insert("user_id", "support-1").await;
    session.insert("role", "support").await;
    "ok"
}

#[derive(serde::Deserialize)]
struct TargetForm {
    user_id: String,
}

#[autumn_web::post("/impersonate")]
async fn begin(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<TargetForm>,
) -> AutumnResult<String> {
    let state = impersonation::begin_impersonation(&state, &session, form.user_id).await?;
    Ok(format!(
        "{}->{}",
        state.impersonator_id, state.effective_user_id
    ))
}

#[autumn_web::post("/stop-impersonating")]
async fn stop(State(state): State<AppState>, session: Session) -> AutumnResult<String> {
    let ended = impersonation::end_impersonation(&state, &session).await?;
    Ok(format!(
        "{}<-{}",
        ended.impersonator_id, ended.effective_user_id
    ))
}

/// Secured route reporting who the framework thinks is acting: the resolved
/// current user (the impersonated one), the real impersonator, and the ambient
/// current *actor* that seeds audit/version attribution.
#[autumn_web::get("/whoami")]
#[autumn_web::secured]
async fn whoami(session: Session) -> String {
    let user = session.get("user_id").await.unwrap_or_default();
    let impersonator = impersonation::impersonator_id(&session)
        .await
        .unwrap_or_else(|| "-".to_owned());
    let actor = Current::actor().unwrap_or_else(|| "-".to_owned());
    format!("user={user};impersonator={impersonator};actor={actor}")
}

#[autumn_web::get("/session-id")]
async fn session_id(session: Session) -> String {
    session.touch().await;
    session.id().await
}

fn routes() -> Vec<autumn_web::Route> {
    routes![login_admin, login_support, begin, stop, whoami, session_id]
}

fn app_with(gate: Option<ImpersonationGate>, sink: Option<Arc<dyn AuditSink>>) -> TestClientAlias {
    let mut app = TestApp::new().routes(routes());
    app = app.state_initializer(move |state| {
        if let Some(gate) = gate.clone() {
            state.insert_extension(gate);
        }
        if let Some(sink) = sink.clone() {
            state.insert_extension(AuditLogger::new().with_sink(sink));
        }
    });
    app.build()
}

type TestClientAlias = autumn_web::test::TestClient;

/// Fetch the `/whoami` probe body.
async fn who(client: &TestClientAlias) -> String {
    client.get("/whoami").send().await.text()
}

// ── AC3: default-deny ─────────────────────────────────────────

#[tokio::test]
async fn beginning_impersonation_is_default_deny_without_a_gate() {
    let client = app_with(None, None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(403);
}

#[tokio::test]
async fn a_user_outside_the_opted_in_role_gets_403() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-support").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(403);
}

#[tokio::test]
async fn an_unauthenticated_request_cannot_begin_impersonation() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(401);
}

// ── AC1 + AC2: begin records both ids; resolution returns target ──

#[tokio::test]
async fn begin_swaps_the_effective_user_and_records_the_impersonator() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    assert_eq!(
        who(&client).await,
        "user=admin-1;impersonator=-;actor=admin-1"
    );

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    // Current-user resolution returns the impersonated user; the separate
    // accessor exposes the real impersonator; and the ambient actor that seeds
    // audit/version writes is the impersonator (AC6).
    assert_eq!(
        who(&client).await,
        "user=user-9;impersonator=admin-1;actor=admin-1"
    );
}

#[tokio::test]
async fn end_restores_the_original_admin_session() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    client.post("/stop-impersonating").send().await.assert_ok();

    assert_eq!(
        who(&client).await,
        "user=admin-1;impersonator=-;actor=admin-1"
    );
}

#[tokio::test]
async fn ending_the_impersonation_restores_the_admin_role() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    // The impersonated session must not carry the admin's role.
    client
        .post("/impersonate")
        .form("user_id=user-8")
        .send()
        .await
        .assert_status(409);

    client.post("/stop-impersonating").send().await.assert_ok();

    // Back to the admin — the gate (role `admin`) admits them again.
    client
        .post("/impersonate")
        .form("user_id=user-8")
        .send()
        .await
        .assert_ok();
}

#[tokio::test]
async fn ending_without_impersonating_is_rejected() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/stop-impersonating")
        .send()
        .await
        .assert_status(400);
}

// ── AC8: no nesting ───────────────────────────────────────────

#[tokio::test]
async fn nesting_impersonation_is_rejected() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-8")
        .send()
        .await
        .assert_status(409);

    // Still impersonating the original target — the chain did not advance.
    assert_eq!(
        who(&client).await,
        "user=user-9;impersonator=admin-1;actor=admin-1"
    );
}

#[tokio::test]
async fn impersonating_yourself_is_rejected() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=admin-1")
        .send()
        .await
        .assert_status(400);
}

// ── AC4: session rotation ─────────────────────────────────────

#[tokio::test]
async fn session_id_is_rotated_on_begin_and_on_end() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    let before = client.get("/session-id").send().await.text();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    let during = client.get("/session-id").send().await.text();
    assert_ne!(before, during, "begin must rotate the session id");

    client.post("/stop-impersonating").send().await.assert_ok();
    let after = client.get("/session-id").send().await.text();
    assert_ne!(during, after, "end must rotate the session id");
    assert_ne!(before, after, "the original id must not be reinstated");
}

// ── AC5: audit events on begin and end ────────────────────────

#[tokio::test]
async fn begin_and_end_each_write_an_audit_event_naming_both_parties() {
    let sink = RecordingSink::default();
    let client = app_with(
        Some(ImpersonationGate::allow_roles(["admin"])),
        Some(Arc::new(sink.clone())),
    );
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    client.post("/stop-impersonating").send().await.assert_ok();

    let events = sink.events();
    assert_eq!(
        sink.actions(),
        vec![
            "auth.impersonation.begin".to_owned(),
            "auth.impersonation.end".to_owned()
        ],
        "one begin event and one end event, in order"
    );
    for event in &events {
        assert_eq!(
            event.actor_id, "admin-1",
            "the audit actor is the real impersonator, not the target"
        );
        assert_eq!(event.target_resource_id, "user-9");
        assert_eq!(event.status, AuditStatus::Success);
        let _: Option<IpAddr> = event.ip_address;
    }
}

#[tokio::test]
async fn a_denied_begin_is_audited_as_a_failure() {
    let sink = RecordingSink::default();
    let client = app_with(
        Some(ImpersonationGate::allow_roles(["admin"])),
        Some(Arc::new(sink.clone())),
    );
    client.post("/login-support").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(403);

    let events = sink.events();
    assert_eq!(events.len(), 1, "the denial is audited");
    assert_eq!(events[0].action, "auth.impersonation.begin");
    assert_eq!(events[0].actor_id, "support-1");
    assert_eq!(events[0].target_resource_id, "user-9");
    assert_eq!(events[0].status, AuditStatus::Failure);
}

#[tokio::test]
async fn begin_fails_closed_when_the_audit_write_fails() {
    let client = app_with(
        Some(ImpersonationGate::allow_roles(["admin"])),
        Some(Arc::new(FailingSink)),
    );
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(500);

    // The session was not swapped: an unauditable privileged action must not
    // take effect.
    assert_eq!(
        who(&client).await,
        "user=admin-1;impersonator=-;actor=admin-1"
    );
}

// ── Custom policy ─────────────────────────────────────────────

struct OnlyOneTarget;

impl ImpersonationPolicy for OnlyOneTarget {
    fn can_impersonate<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") && target.user_id() == "user-9" })
    }

    fn target_role<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { Some("member".to_owned()) })
    }
}

#[autumn_web::get("/my-role")]
async fn my_role(session: Session) -> String {
    session.get("role").await.unwrap_or_else(|| "-".to_owned())
}

#[tokio::test]
async fn a_custom_policy_decides_the_target_and_its_role() {
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop, my_role])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::custom(OnlyOneTarget));
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-8")
        .send()
        .await
        .assert_status(403);

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    // The role is resolved server-side by the policy — never taken from the
    // request — so an operator cannot mint a privileged impersonated session.
    assert_eq!(client.get("/my-role").send().await.text(), "member");

    client.post("/stop-impersonating").send().await.assert_ok();
    assert_eq!(client.get("/my-role").send().await.text(), "admin");
}

// ── Session key hygiene ───────────────────────────────────────

#[autumn_web::get("/raw-impersonator")]
async fn raw_impersonator(session: Session) -> String {
    session
        .get(IMPERSONATOR_SESSION_KEY)
        .await
        .unwrap_or_else(|| "-".to_owned())
}

#[tokio::test]
async fn the_impersonator_key_is_cleared_on_end() {
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop, raw_impersonator])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    assert_eq!(
        client.get("/raw-impersonator").send().await.text(),
        "admin-1"
    );

    client.post("/stop-impersonating").send().await.assert_ok();
    assert_eq!(client.get("/raw-impersonator").send().await.text(), "-");
}
