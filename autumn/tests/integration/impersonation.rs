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
    self, IMPERSONATED_SESSION_KEY, IMPERSONATOR_ROLE_SESSION_KEY, IMPERSONATOR_SESSION_KEY,
    ImpersonationGate, ImpersonationPolicy, ImpersonationTarget,
};
use autumn_web::authorization::{BoxFuture, PolicyContext};
use autumn_web::config::AutumnConfig;
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
async fn whoami(State(state): State<AppState>, session: Session) -> String {
    let user = session.get("user_id").await.unwrap_or_default();
    let impersonator = impersonation::impersonator_id(&state, &session)
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

/// Build the app under test. `sink` defaults to a `TracingAuditSink`, because
/// `begin_impersonation` refuses outright without one — see
/// `begin_is_refused_when_no_audit_sink_is_configured`.
fn app_with(gate: Option<ImpersonationGate>, sink: Option<Arc<dyn AuditSink>>) -> TestClientAlias {
    let sink = sink.unwrap_or_else(|| Arc::new(autumn_web::audit::TracingAuditSink));
    app_with_optional_sink(gate, Some(sink))
}

fn app_with_optional_sink(
    gate: Option<ImpersonationGate>,
    sink: Option<Arc<dyn AuditSink>>,
) -> TestClientAlias {
    TestApp::new()
        .routes(routes())
        .state_initializer(move |state| {
            if let Some(gate) = gate.clone() {
                state.insert_extension(gate);
            }
            if let Some(sink) = sink.clone() {
                state.insert_extension(AuditLogger::new().with_sink(sink));
            }
        })
        .build()
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

#[tokio::test]
async fn begin_is_refused_when_no_audit_sink_is_configured() {
    // `audit::write_from_state` is a silent no-op with no logger installed, so
    // without an explicit check an app that opted into impersonation but never
    // configured a sink would swap identities with no record at all.
    let client = app_with_optional_sink(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(500);

    assert_eq!(
        who(&client).await,
        "user=admin-1;impersonator=-;actor=admin-1",
        "the session must not have been swapped"
    );
}

#[tokio::test]
async fn begin_is_refused_when_the_audit_logger_has_no_sinks() {
    // An `AuditLogger` that was installed but carries no sinks swallows writes
    // just as silently as no logger at all.
    let client = TestApp::new()
        .routes(routes())
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(AuditLogger::new());
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(500);
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
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
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

// ── Every session-based actor seam applies the same rule ──────

/// Reports the ambient actor from a route guarded by the `RequireAuth`
/// middleware layer rather than by `#[secured]` — a different resolution seam.
#[autumn_web::get("/middleware-guarded")]
async fn middleware_guarded() -> String {
    Current::actor().unwrap_or_else(|| "-".to_owned())
}

/// Reports the ambient actor from a route whose only auth touchpoint is a
/// policy check — the `PolicyContext::from_session` seam, which is what a
/// `#[repository(policy = ...)]` route resolves through.
#[autumn_web::get("/policy-guarded")]
async fn policy_guarded(State(state): State<AppState>, session: Session) -> String {
    let _ctx = PolicyContext::from_request(&state, &session).await;
    Current::actor().unwrap_or_else(|| "-".to_owned())
}

#[tokio::test]
async fn the_require_auth_seam_attributes_to_the_impersonator() {
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop])
        .scoped(
            "/guarded",
            autumn_web::auth::RequireAuth::new("user_id"),
            routes![middleware_guarded],
        )
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();
    assert_eq!(
        client
            .get("/guarded/middleware-guarded")
            .send()
            .await
            .text(),
        "admin-1"
    );

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    assert_eq!(
        client
            .get("/guarded/middleware-guarded")
            .send()
            .await
            .text(),
        "admin-1",
        "the RequireAuth seam must publish the impersonator, not the target"
    );
}

#[tokio::test]
async fn the_policy_context_seam_attributes_to_the_impersonator() {
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop, policy_guarded])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();
    assert_eq!(client.get("/policy-guarded").send().await.text(), "admin-1");

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    assert_eq!(
        client.get("/policy-guarded").send().await.text(),
        "admin-1",
        "a policy-only route (what #[repository(policy = ...)] resolves through) \
         must attribute to the impersonator"
    );
}

// ── The policy sees exactly the id the session receives ───────

/// A policy that protects one specific account by id — the shape the trait's
/// own doc example uses, and the shape a tenancy check takes.
struct ProtectRoot;

impl ImpersonationPolicy for ProtectRoot {
    fn can_impersonate<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        target: &'a ImpersonationTarget,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") && target.user_id() != "root" })
    }
}

#[tokio::test]
async fn a_whitespace_padded_target_cannot_slip_past_the_policy() {
    // The id the policy authorizes and the id written to the session must be
    // the same string. If `begin_impersonation` trimmed only on the way into
    // the session, `" root "` would be authorized (it is != "root") and then
    // land as `root` — the account the policy exists to protect.
    let client = TestApp::new()
        .routes(routes())
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::custom(ProtectRoot));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=%20root%20")
        .send()
        .await
        .assert_status(403);

    assert_eq!(
        who(&client).await,
        "user=admin-1;impersonator=-;actor=admin-1",
        "the padded id must not have been swapped in"
    );

    // The unpadded form is refused too, so the test is not passing for the
    // wrong reason.
    client
        .post("/impersonate")
        .form("user_id=root")
        .send()
        .await
        .assert_status(403);
}

#[tokio::test]
async fn a_blank_target_is_rejected() {
    let client = app_with(Some(ImpersonationGate::allow_roles(["admin"])), None);
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/impersonate")
        .form("user_id=%20%20")
        .send()
        .await
        .assert_status(400);
}

// ── An already-resolved actor is never clobbered ──────────────

/// Begins an impersonation inside an explicit `with_actor(...)` scope — the
/// shape a background job or an API-token-authenticated route takes — and
/// reports the ambient actor from inside that scope.
#[autumn_web::post("/impersonate-in-actor-scope")]
async fn begin_in_actor_scope(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<TargetForm>,
) -> AutumnResult<String> {
    autumn_web::current::with_actor("outer-principal", async {
        impersonation::begin_impersonation(&state, &session, form.user_id).await?;
        Ok(Current::actor().unwrap_or_else(|| "-".to_owned()))
    })
    .await
}

#[tokio::test]
async fn beginning_impersonation_does_not_clobber_a_stronger_actor() {
    // The three session seams all seed the actor only when none is set, so an
    // API-token bearer or an explicit `with_actor(...)` scope wins. Beginning
    // an impersonation must follow the same rule, or the rest of the handler's
    // writes would be misattributed to the session user.
    let client = TestApp::new()
        .routes(routes![login_admin, begin_in_actor_scope, stop, whoami])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();

    let actor = client
        .post("/impersonate-in-actor-scope")
        .form("user_id=user-9")
        .send()
        .await
        .text();
    assert_eq!(
        actor, "outer-principal",
        "the outermost resolved principal must survive the swap"
    );

    // The impersonation itself still took effect; only the attribution rule
    // changed, and a fresh request resolves the impersonator as usual.
    assert_eq!(
        who(&client).await,
        "user=user-9;impersonator=admin-1;actor=admin-1"
    );
}

// ── A self-destructive auth session key is refused ────────────

#[tokio::test]
async fn an_auth_session_key_that_collides_with_a_reserved_key_is_refused() {
    // With `auth.session_key = "impersonator_id"`, writing the target through
    // the auth key would overwrite the record naming the operator: attribution
    // would follow the target and the revert could not restore the admin. The
    // misconfiguration is refused rather than silently corrupting the session.
    for reserved in autumn_web::auth::impersonation::RESERVED_SESSION_KEYS {
        let mut config = AutumnConfig::default();
        config.auth.session_key = reserved.to_owned();
        let client = TestApp::new()
            .routes(routes())
            .config(config)
            .state_initializer(|state| {
                state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
                state.insert_extension(
                    AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
                );
            })
            .build();
        client.post("/login-admin").send().await.assert_ok();

        client
            .post("/impersonate")
            .form("user_id=user-9")
            .send()
            .await
            .assert_status(500);
        client
            .post("/stop-impersonating")
            .send()
            .await
            .assert_status(500);
    }
}

// ── Step-up does not carry into the impersonated identity ─────

#[autumn_web::post("/step-up")]
async fn stamp_step_up(session: Session) -> &'static str {
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    "stamped"
}

#[autumn_web::get("/step-up-claim")]
async fn step_up_claim(session: Session) -> String {
    session
        .get(autumn_web::step_up::STEP_UP_SESSION_KEY)
        .await
        .map_or_else(|| "-".to_owned(), |_| "present".to_owned())
}

#[tokio::test]
async fn the_operators_step_up_claim_does_not_follow_them_into_the_target() {
    // `last_strong_auth_at` is a bare timestamp with no identity bound to it.
    // Carrying it across the swap would let a `#[step_up]` route run a
    // destructive action on the *target's* account on the strength of the
    // operator's re-authentication.
    let client = TestApp::new()
        .routes(routes![
            login_admin,
            begin,
            stop,
            stamp_step_up,
            step_up_claim
        ])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build();
    client.post("/login-admin").send().await.assert_ok();
    client.post("/step-up").send().await.assert_ok();
    assert_eq!(
        client.get("/step-up-claim").send().await.text(),
        "present",
        "the operator re-authenticated"
    );

    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    assert_eq!(
        client.get("/step-up-claim").send().await.text(),
        "-",
        "the impersonated session must not inherit the operator's freshness"
    );

    client.post("/stop-impersonating").send().await.assert_ok();
    assert_eq!(
        client.get("/step-up-claim").send().await.text(),
        "present",
        "reverting restores the operator's own claim"
    );
}

// ── A stale record is never honoured ──────────────────────────

/// Re-authenticates the session as somebody else *without* clearing the
/// impersonation keys — the shape of a hand-rolled login that writes the auth
/// key directly, which is how a stale record comes to exist.
#[autumn_web::post("/login-as-someone-else")]
async fn login_as_someone_else(session: Session) -> &'static str {
    session.rotate_id().await;
    session.insert("user_id", "carol").await;
    "ok"
}

#[autumn_web::post("/scrubbed-login")]
async fn scrubbed_login(session: Session) -> &'static str {
    session.rotate_id().await;
    impersonation::clear(&session).await;
    session.insert("user_id", "carol").await;
    "ok"
}

fn stale_record_app() -> TestClientAlias {
    TestApp::new()
        .routes(routes![
            login_admin,
            begin,
            stop,
            whoami,
            login_as_someone_else,
            scrubbed_login
        ])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
        })
        .build()
}

#[tokio::test]
async fn a_stale_record_does_not_hand_the_next_user_the_operators_identity() {
    let client = stale_record_app();
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    // The operator walks away without reverting; somebody else logs in on the
    // same session, and the app forgets to scrub the impersonation keys.
    client
        .post("/login-as-someone-else")
        .send()
        .await
        .assert_ok();

    // Their work is theirs — not attributed to the operator.
    assert_eq!(
        who(&client).await,
        "user=carol;impersonator=-;actor=carol",
        "a record describing user-9 must not apply to carol"
    );

    // And "Stop impersonating" is not a credential-free path to the admin
    // account: the stale record is dropped, not honoured.
    client
        .post("/stop-impersonating")
        .send()
        .await
        .assert_status(400);
    assert_eq!(who(&client).await, "user=carol;impersonator=-;actor=carol");
}

#[tokio::test]
async fn a_stale_record_does_not_wedge_the_session_at_409() {
    let client = stale_record_app();
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();
    client
        .post("/login-as-someone-else")
        .send()
        .await
        .assert_ok();

    // carol is not an admin, so this is a 403 (the gate), never a 409 (the
    // nesting guard tripping over a record that no longer describes her).
    client
        .post("/impersonate")
        .form("user_id=user-8")
        .send()
        .await
        .assert_status(403);
}

#[tokio::test]
async fn clear_removes_every_reserved_key() {
    let client = stale_record_app();
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_ok();

    client.post("/scrubbed-login").send().await.assert_ok();
    assert_eq!(who(&client).await, "user=carol;impersonator=-;actor=carol");
    client
        .post("/stop-impersonating")
        .send()
        .await
        .assert_status(400);
}

// ── Session key hygiene ───────────────────────────────────────

#[autumn_web::get("/raw-impersonator")]
async fn raw_impersonator(session: Session) -> String {
    let mut parts = Vec::new();
    for key in [
        IMPERSONATOR_SESSION_KEY,
        IMPERSONATED_SESSION_KEY,
        IMPERSONATOR_ROLE_SESSION_KEY,
    ] {
        parts.push(session.get(key).await.unwrap_or_else(|| "-".to_owned()));
    }
    parts.join(",")
}

#[tokio::test]
async fn the_impersonator_key_is_cleared_on_end() {
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop, raw_impersonator])
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(
                AuditLogger::new().with_sink(Arc::new(autumn_web::audit::TracingAuditSink)),
            );
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
        "admin-1,user-9,admin",
        "the record names both parties and stashes the operator's role"
    );

    client.post("/stop-impersonating").send().await.assert_ok();
    assert_eq!(
        client.get("/raw-impersonator").send().await.text(),
        "-,-,-",
        "every reserved key is cleared on revert"
    );
}
