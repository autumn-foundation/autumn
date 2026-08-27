//! End-to-end tests for the admin plugin's impersonation UI (issue #1394).
//!
//! Uses the in-process test client (the `acting_as` harness from #1359 is the
//! test-side analog) to prove the two acceptance criteria that only the plugin
//! can satisfy:
//!
//! * a **persistent banner** ("Viewing as … — Stop impersonating") renders with
//!   a one-click revert; and
//! * a write performed **while impersonating** is attributed to the real
//!   impersonator, not to the target.
//!
//! Also covers the wiring rules the primitive depends on: the revert route is
//! mounted *outside* the admin role gate (so an operator impersonating a
//! non-admin is never trapped), and the begin route never takes the
//! impersonated role from the request.

use std::sync::{Arc, Mutex};

use autumn_admin_plugin::AdminPlugin;
use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink};
use autumn_web::auth::impersonation::ImpersonationGate;
use autumn_web::current::Current;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};

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
            events.lock().expect("sink lock").push(event);
            Ok(())
        })
    }
}

impl RecordingSink {
    fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("sink lock").clone()
    }
}

// ── App under test ────────────────────────────────────────────

#[autumn_web::post("/login-admin")]
async fn login_admin(session: Session) -> &'static str {
    session.insert("user_id", "admin-1").await;
    session.insert("role", "admin").await;
    "ok"
}

/// An application page that embeds the plugin's banner component in its own
/// layout — the surface an operator actually sees while impersonating.
#[autumn_web::get("/app")]
async fn app_page(State(state): State<AppState>, session: Session) -> Markup {
    let banner =
        autumn_admin_plugin::impersonation_banner_for(&state, &session, "/admin", "", "").await;
    html! {
        body {
            @if let Some(banner) = banner { (banner) }
            main { "the app" }
        }
    }
}

/// A write performed by the *target's* session while impersonating. Records
/// whatever the framework says the current actor is — the value that seeds
/// `#[repository(versioned)]` version rows and audit events (#1383).
#[autumn_web::post("/app/write")]
#[autumn_web::secured]
async fn app_write(State(state): State<AppState>) -> AutumnResult<String> {
    let actor = Current::actor().unwrap_or_else(|| "-".to_owned());
    autumn_web::audit::write_from_state(
        &state,
        AuditEvent::new(
            &actor,
            "note.create",
            "note-1",
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
    Ok(actor)
}

fn build(sink: RecordingSink) -> TestClient {
    TestApp::new()
        .routes(routes![login_admin, app_page, app_write])
        .plugin(
            AdminPlugin::new()
                .require_role("admin".to_owned())
                .with_impersonation(ImpersonationGate::allow_roles(["admin"])),
        )
        .state_initializer(move |state| {
            state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
        })
        .build()
}

// ── The banner renders, with a one-click revert ───────────────

#[tokio::test]
async fn the_banner_renders_with_a_one_click_revert_while_impersonating() {
    let client = build(RecordingSink::default());
    client.post("/login-admin").send().await.assert_ok();

    // No banner before impersonating.
    let before = client.get("/app").send().await.text();
    assert!(
        !before.contains("Stop impersonating"),
        "no banner before impersonating: {before}"
    );

    client
        .post("/admin/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(303);

    let during = client.get("/app").send().await.text();
    assert!(
        during.contains("Viewing as"),
        "banner names the impersonated user: {during}"
    );
    assert!(during.contains("user-9"), "{during}");
    assert!(
        during.contains("Stop impersonating"),
        "banner offers a one-click revert: {during}"
    );
    assert!(
        during.contains(r#"action="/admin/impersonate/stop""#),
        "revert posts to the plugin's stop route: {during}"
    );
}

// ── The revert route is reachable while impersonating ─────────

#[tokio::test]
async fn the_revert_route_is_not_behind_the_admin_role_gate() {
    let client = build(RecordingSink::default());
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/admin/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(303);

    // The impersonated session has no admin role, so the admin panel is closed
    // to it… (`/admin/jobs` rather than `/admin`, which needs a DB pool.)
    client.get("/admin/jobs").send().await.assert_status(403);
    // …but the revert always works, so the operator is never trapped.
    client
        .post("/admin/impersonate/stop")
        .send()
        .await
        .assert_status(303);

    // Back to the admin.
    client.get("/admin/jobs").send().await.assert_ok();
    let after = client.get("/app").send().await.text();
    assert!(!after.contains("Stop impersonating"), "{after}");
}

// ── The plugin's own pages carry the banner ───────────────────

/// A policy that grants the impersonated session the target's real role —
/// here `admin`, so the operator stays inside the admin panel and the banner
/// on the plugin's own pages is reachable.
struct ImpersonateAsAdmin;

impl autumn_web::auth::impersonation::ImpersonationPolicy for ImpersonateAsAdmin {
    fn can_impersonate<'a>(
        &'a self,
        ctx: &'a autumn_web::authorization::PolicyContext,
        _target: &'a autumn_web::auth::impersonation::ImpersonationTarget,
    ) -> autumn_web::authorization::BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") })
    }

    fn target_role<'a>(
        &'a self,
        _ctx: &'a autumn_web::authorization::PolicyContext,
        _target: &'a autumn_web::auth::impersonation::ImpersonationTarget,
    ) -> autumn_web::authorization::BoxFuture<'a, Option<String>> {
        Box::pin(async { Some("admin".to_owned()) })
    }
}

#[tokio::test]
async fn admin_pages_render_the_banner_while_impersonating() {
    let client = TestApp::new()
        .routes(routes![login_admin])
        .plugin(
            AdminPlugin::new()
                .require_role("admin".to_owned())
                .with_impersonation(ImpersonationGate::custom(ImpersonateAsAdmin)),
        )
        .build();
    client.post("/login-admin").send().await.assert_ok();

    let before = client.get("/admin/jobs").send().await.text();
    assert!(!before.contains("Stop impersonating"), "{before}");

    client
        .post("/admin/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(303);

    let during = client.get("/admin/jobs").send().await.text();
    assert!(during.contains("Viewing as"), "{during}");
    assert!(during.contains("user-9"), "{during}");
    assert!(during.contains("Stop impersonating"), "{during}");
    assert!(
        during.contains(".autumn-impersonation-banner"),
        "the layout ships the banner styles: {during}"
    );
}

// ── Writes are attributed to the real impersonator ────────────

#[tokio::test]
async fn a_write_while_impersonating_is_attributed_to_the_impersonator() {
    let sink = RecordingSink::default();
    let client = build(sink.clone());
    client.post("/login-admin").send().await.assert_ok();
    client
        .post("/admin/impersonate")
        .form("user_id=user-9")
        .send()
        .await
        .assert_status(303);

    let actor = client.post("/app/write").send().await.text();
    assert_eq!(
        actor, "admin-1",
        "the write performed as user-9 must carry the real impersonator"
    );

    let writes: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|e| e.action == "note.create")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].actor_id, "admin-1");
}

// ── The impersonated role is never taken from the request ─────

#[tokio::test]
async fn the_begin_route_ignores_a_role_supplied_by_the_client() {
    let client = build(RecordingSink::default());
    client.post("/login-admin").send().await.assert_ok();

    client
        .post("/admin/impersonate")
        .form("user_id=user-9&role=admin")
        .send()
        .await
        .assert_status(303);

    // The gate's policy did not grant a role, so the smuggled `role=admin`
    // must not have elevated the impersonated session.
    client.get("/admin").send().await.assert_status(403);
}

// ── Default-deny: no gate configured, no route ────────────────

#[tokio::test]
async fn without_with_impersonation_the_routes_are_not_mounted() {
    let client = TestApp::new()
        .routes(routes![login_admin, app_page])
        .plugin(AdminPlugin::new().require_role("admin".to_owned()))
        .build();
    client.post("/login-admin").send().await.assert_ok();

    // Not a 404: `/admin/{slug}` is a catch-all, so the request lands on the
    // model-create handler instead. What matters is that nothing impersonates.
    let response = client
        .post("/admin/impersonate")
        .form("user_id=user-9")
        .send()
        .await;
    assert_ne!(response.status.as_u16(), 303, "no impersonation redirect");

    let page = client.get("/app").send().await.text();
    assert!(
        !page.contains("Stop impersonating"),
        "no impersonation without the opt-in: {page}"
    );

    // The revert route is not mounted either — the request falls through to the
    // `/{slug}/{id}` catch-all rather than redirecting.
    let stop = client.post("/admin/impersonate/stop").send().await;
    assert_ne!(stop.status.as_u16(), 303, "no revert redirect");
}
