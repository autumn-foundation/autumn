//! Integration tests for the teams starter (issue #1261).
//!
//! ```text
//! cargo test -p teams                                   # smoke tests (instant, no Docker)
//! cargo test -p teams -- --include-ignored --test-threads=1   # full flow (needs Docker)
//! ```
//!
//! The ignored tests start a Postgres testcontainer and drive the real
//! signup → invite → accept → role-gated member-management round trip.

use autumn_web::config::AutumnConfig;
use autumn_web::mail::Transport;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient, TestDb, TestResponse};

use teams::routes;

fn app_routes() -> Vec<autumn_web::Route> {
    routes![
        teams::index,
        routes::auth::signup_form,
        routes::auth::signup,
        routes::auth::login_form,
        routes::auth::login,
        routes::auth::logout,
        routes::organizations::create_organization,
        routes::organizations::switch_organization,
        routes::invitations::create_invitation,
        routes::invitations::show_invitation,
        routes::invitations::accept_invitation,
        routes::invitations::revoke_invitation,
        routes::invitations::resend_invitation,
        routes::members::list_members,
        routes::members::change_role,
        routes::members::remove_member,
    ]
}

fn enable_tenancy(config: &mut AutumnConfig) {
    config.tenancy.enabled = true;
    config.tenancy.source = "session".to_string();
    config.tenancy.session_key = "organization_id".to_string();
    config.tenancy.public_paths = vec![
        "/".to_string(),
        "/login".to_string(),
        "/signup".to_string(),
        "/logout".to_string(),
        "/static".to_string(),
        "/invite".to_string(),
    ];
    config.tenancy.login_redirect = Some("/login".to_string());
}

// ── Smoke tests (no Docker) ──────────────────────────────────────────────────

#[tokio::test]
async fn login_page_renders() {
    let client = TestApp::new().routes(app_routes()).build();
    client
        .get("/login")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Log in");
}

#[tokio::test]
async fn signup_page_renders() {
    let client = TestApp::new().routes(app_routes()).build();
    client
        .get("/signup")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Create your account");
}

#[tokio::test]
async fn protected_route_redirects_to_login_when_unauthenticated() {
    let mut config = AutumnConfig::default();
    enable_tenancy(&mut config);
    let client = TestApp::new().routes(app_routes()).config(config).build();
    let resp = client
        .get("/members")
        .header("accept", "text/html")
        .send()
        .await;
    resp.assert_status(303);
    assert_eq!(resp.header("location"), Some("/login"));
}

// ── Full flow (requires Docker) ──────────────────────────────────────────────

/// Create the schema and return a CSRF-disabled, DB-backed client. `mail_dir`
/// captures every invite email as an `.eml` file (AC4's "delivered to the dev
/// mailbox").
async fn db_client(mail_dir: &std::path::Path) -> TestClient {
    let db = TestDb::shared().await;
    db.execute_sql(include_str!(
        "../migrations/00000000000000_create_teams/up.sql"
    ))
    .await;
    db.execute_sql("TRUNCATE invitations, memberships, organizations, users RESTART IDENTITY")
        .await;

    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    enable_tenancy(&mut config);
    config.mail.transport = Transport::File;
    config.mail.file_dir = mail_dir.to_path_buf();
    config.mail.from = Some("Teams <noreply@example.com>".to_string());

    TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .build()
}

fn session_cookie(resp: &TestResponse) -> String {
    resp.header("set-cookie")
        .expect("response should set a session cookie")
        .split(';')
        .next()
        .expect("cookie has a name=value pair")
        .to_owned()
}

fn count_emls(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |rd| {
        rd.filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("eml"))
            .count()
    })
}

/// Pull the raw invite token out of the most recently written `.eml` file's
/// `/invite/{token}` link.
fn latest_invite_token(dir: &std::path::Path) -> String {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("mail dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("eml"))
        .collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    let latest = entries.last().expect("at least one .eml written");
    let body = std::fs::read_to_string(latest.path()).expect("read .eml");
    let marker = "/invite/";
    let start = body.find(marker).expect("accept link present in email") + marker.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '=')
        .unwrap_or(rest.len());
    rest[..end].trim().to_owned()
}

async fn signup(client: &TestClient, email: &str) -> String {
    let resp = client
        .post("/signup")
        .form(&format!(
            "email={email}&password=Tr0ubad0ur-Xy7-correct-horse"
        ))
        .send()
        .await;
    resp.assert_status(303);
    session_cookie(&resp)
}

/// AC3: signing up creates a personal organization and makes the signer its
/// Owner — proven by the member list showing exactly one `owner` row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn signup_creates_organization_with_owner_membership() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let cookie = signup(&client, "owner@acme.test").await;

    client
        .get("/members")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("owner@acme.test")
        .assert_body_contains("owner");
}

/// AC4 + AC5(a) + success metric: inviting sends a real email (captured as an
/// `.eml`), and accepting via the tokened link as a brand-new user creates
/// the account and the membership in one step.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn invite_and_accept_as_new_user() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    let before = count_emls(dir.path());
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    assert_eq!(
        count_emls(dir.path()),
        before + 1,
        "AC4: an invite email must be delivered to the dev mailbox in the same request cycle"
    );

    let token = latest_invite_token(dir.path());

    // The accept page shows a signup form for an unknown email.
    client
        .get(&format!("/invite/{token}"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Create an account to accept");

    let accept = client
        .post(&format!("/invite/{token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    accept.assert_status(303);
    let new_member_cookie = session_cookie(&accept);

    client
        .get("/members")
        .header("cookie", &new_member_cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("newbie@acme.test")
        .assert_body_contains("member");
}

/// AC5(b): an already-authenticated user accepting an invite joins directly,
/// no signup step.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn invite_and_accept_as_existing_authenticated_user() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    let other_cookie = signup(&client, "second-owner@acme.test").await;

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=second-owner@acme.test&role=admin")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());

    // Signed in as second-owner (owner of their *own* org), the accept page
    // shows a direct one-click accept, not a signup form.
    client
        .get(&format!("/invite/{token}"))
        .header("cookie", &other_cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Accept invitation");

    let accept = client
        .post(&format!("/invite/{token}/accept"))
        .header("cookie", &other_cookie)
        .send()
        .await;
    accept.assert_status(303);

    let switched_cookie = session_cookie(&accept);
    client
        .get("/members")
        .header("cookie", &switched_cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("second-owner@acme.test")
        .assert_body_contains("admin");
}

/// Security regression: an unauthenticated visitor must never be able to
/// silently accept-as (and thereby log in as) an account that already exists
/// for the invited email, just by possessing the token — no password check.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn accept_without_session_for_existing_account_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    // The invited email already has its own account (and its own org).
    signup(&client, "existing@acme.test").await;

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=existing@acme.test&role=admin")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());

    // No session cookie, no password field: must be rejected, not silently
    // logged in as the existing account.
    let resp = client.post(&format!("/invite/{token}/accept")).send().await;
    resp.assert_status(401);
    assert!(
        resp.header("set-cookie").is_none(),
        "a rejected accept must not establish a session"
    );
}

/// Security regression: an authenticated user may only redeem an invite
/// addressed to their own email — not one meant for a different account.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn accept_rejects_authenticated_user_with_mismatched_email() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    let bystander_cookie = signup(&client, "bystander@acme.test").await;

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=intended@acme.test&role=admin")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());

    let resp = client
        .post(&format!("/invite/{token}/accept"))
        .header("cookie", &bystander_cookie)
        .send()
        .await;
    resp.assert_status(403);

    // The bystander must not have joined the inviting org under any role.
    let body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(!body.contains("bystander@acme.test"));
}

/// Security regression: an Admin (not Owner) must not be able to grant the
/// `Owner` role to anyone, whether via a fresh invite or an existing member —
/// otherwise Admin -> Owner is a one-request privilege escalation.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn admin_cannot_grant_owner_role() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=admin-user@acme.test&role=admin")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());
    let accept = client
        .post(&format!("/invite/{token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    let admin_cookie = session_cookie(&accept);

    // The Admin cannot invite a fresh account straight in as Owner...
    let invite_resp = client
        .post("/invitations")
        .header("cookie", &admin_cookie)
        .form("email=wannabe-owner@acme.test&role=owner")
        .send()
        .await;
    invite_resp.assert_status(403);

    // ...nor promote themselves (member id 2, after the owner's own id 1) to Owner.
    let promote_resp = client
        .post("/members/2/role")
        .header("cookie", &admin_cookie)
        .form("role=owner")
        .send()
        .await;
    promote_resp.assert_status(403);
}

/// AC5: a double-clicked accept link is a no-op, not a duplicate membership
/// or a 500 — the success metric's exact wording.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn double_accept_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());

    let first = client
        .post(&format!("/invite/{token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    first.assert_status(303);
    let cookie = session_cookie(&first);

    // Second click reuses the now-authenticated session rather than
    // resubmitting the signup form (mirrors a browser back-button replay).
    let second = client
        .post(&format!("/invite/{token}/accept"))
        .header("cookie", &cookie)
        .send()
        .await;
    second.assert_status(303);

    let body = client
        .get("/members")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        body.matches("newbie@acme.test").count(),
        1,
        "double-accept must not create a second membership row"
    );
}

/// AC6: a revoked invitation's link renders a clear error, not a panic.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn revoked_invitation_shows_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());

    // Only one invitation exists yet, so its id is deterministically 1.
    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(members_body.contains("newbie@acme.test"));

    client
        .post("/invitations/1/revoke")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_status(303);

    client
        .get(&format!("/invite/{token}"))
        .send()
        .await
        .assert_status(410)
        .assert_body_contains("revoked");
}

/// AC7: the last Owner cannot be removed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn last_owner_cannot_be_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    // The owner's own membership row is id 1 (first row inserted).
    let resp = client
        .post("/members/1/remove")
        .header("cookie", &owner_cookie)
        .send()
        .await;
    resp.assert_status(409);
}

/// AC7: a plain Member cannot manage the roster (invite gated Admin+).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn member_cannot_invite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let token = latest_invite_token(dir.path());
    let accept = client
        .post(&format!("/invite/{token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    let member_cookie = session_cookie(&accept);

    let resp = client
        .post("/invitations")
        .header("cookie", &member_cookie)
        .form("email=someone-else@acme.test&role=member")
        .send()
        .await;
    resp.assert_status(403);
}
