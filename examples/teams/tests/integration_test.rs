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

/// Every recoverable signup/login failure mode redisplays the form inline
/// (HTTP 200, same page, error adjacent to the fields) with the entered email
/// preserved, instead of navigating the user to a generic error page and
/// losing what they typed (Wayfinder: error-path inventory).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn signup_and_login_failures_redisplay_the_form_with_email_preserved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;

    // Malformed email at signup.
    let resp = client
        .post("/signup")
        .form("email=not-an-email&password=Tr0ubad0ur-Xy7-correct-horse")
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("Enter a valid email address"));
    assert!(
        resp.text().contains(r#"value="not-an-email""#),
        "expected the entered email to be preserved in the re-rendered form, got: {}",
        resp.text()
    );

    // Over-long password at signup.
    let long_password = "x".repeat(129);
    let resp = client
        .post("/signup")
        .form(&format!("email=long@acme.test&password={long_password}"))
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("at most 128 characters"));
    assert!(resp.text().contains(r#"value="long@acme.test""#));

    // Weak password at signup (Codex review finding: teams had no
    // weak-password redisplay coverage at all, unlike saas's pre-existing
    // `signup_rejects_weak_password`; asserted here alongside email
    // preservation, which no prior test checked for either app's
    // weak-password branch).
    let resp = client
        .post("/signup")
        .form("email=weak@acme.test&password=password")
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("too common"));
    assert!(resp.text().contains(r#"value="weak@acme.test""#));

    // Duplicate email at signup: the first signup succeeds, the second is
    // redisplayed inline rather than dropped onto a generic error page.
    let _ = signup(&client, "dupe@acme.test").await;
    let resp = client
        .post("/signup")
        .form("email=dupe@acme.test&password=Tr0ubad0ur-Xy7-correct-horse-2")
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("Could not create account"));
    assert!(resp.text().contains(r#"value="dupe@acme.test""#));

    // Invalid credentials at login preserve the entered (nonexistent) email.
    let resp = client
        .post("/login")
        .form("email=nobody@acme.test&password=wrong-password-entirely")
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("Invalid email or password"));
    assert!(resp.text().contains(r#"value="nobody@acme.test""#));

    // Over-long input at login (Codex review finding: the "invalid
    // credentials" case above exercises the same `login_page` call but not
    // this distinct length-guard branch).
    let resp = client
        .post("/login")
        .form(&format!("email=toolong@acme.test&password={long_password}"))
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("Invalid email or password"));
    assert!(resp.text().contains(r#"value="toolong@acme.test""#));
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

/// Two concurrent submissions of the same signup-and-join accept form (a
/// double-submit, or a browser retry) must not leave one of them with a
/// raw 422 — the invitation lock must serialize the two requests, so the
/// loser recognizes the account the winner just created and joins it too,
/// rather than racing the winner to the `users.email` unique constraint
/// and failing before it ever reaches the invitation lock (Codex review
/// finding).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_signup_and_accept_for_the_same_new_email_both_succeed() {
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

    let (first, second) = tokio::join!(
        client
            .post(&format!("/invite/{token}/accept"))
            .form("password=An0ther-Str0ng-Pa55word")
            .send(),
        client
            .post(&format!("/invite/{token}/accept"))
            .form("password=An0ther-Str0ng-Pa55word")
            .send(),
    );

    assert_eq!(
        [first.status.as_u16(), second.status.as_u16()],
        [303, 303],
        "both concurrent submissions of the same accept-and-join should succeed"
    );

    let new_member_cookie = session_cookie(&first);
    let members_body = client
        .get("/members")
        .header("cookie", &new_member_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "concurrent double-submit must not create two accounts or two memberships: {members_body}"
    );
}

/// Security regression: two *different* requests racing the same unused
/// invitation link with different passwords must not both succeed — the
/// loser must never be silently logged in to the account the winner just
/// created, since that account's password is the winner's, not theirs. This
/// would let anyone who obtains a not-yet-accepted invitation token (a
/// forwarded link, a mail-scanner fetch, a shared clipboard) race the real
/// invitee's legitimate accept and get authenticated into their brand-new
/// account without ever knowing its password (Codex review finding).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_accept_with_different_passwords_does_not_authenticate_the_loser() {
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

    let (first, second) = tokio::join!(
        client
            .post(&format!("/invite/{token}/accept"))
            .form("password=First-Str0ng-Pa55word")
            .send(),
        client
            .post(&format!("/invite/{token}/accept"))
            .form("password=Second-Different-Pa55word")
            .send(),
    );

    let statuses = [first.status.as_u16(), second.status.as_u16()];
    assert!(
        statuses.contains(&303) && statuses.contains(&401),
        "exactly one request (whichever wins the account-creation race) \
         should succeed (303); the other, submitting a password that \
         doesn't match the account just created, must be rejected (401) \
         rather than silently authenticated into it — got {statuses:?}"
    );

    // Only one membership/account exists either way.
    let winner_cookie = if first.status.as_u16() == 303 {
        session_cookie(&first)
    } else {
        session_cookie(&second)
    };
    let members_body = client
        .get("/members")
        .header("cookie", &winner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "{members_body}"
    );
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

/// `Owner > Admin` must hold regardless of headcount: an Admin may never
/// demote or remove an Owner, even when the organization has more than one
/// (only the *last*-owner protection is about headcount).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn admin_cannot_demote_or_remove_an_owner_even_with_multiple_owners() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    // A second Owner (member id 2) — only the founding Owner can invite as
    // owner, so this is done by the owner_cookie.
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=co-owner@acme.test&role=owner")
        .send()
        .await
        .assert_status(303);
    let co_owner_token = latest_invite_token(dir.path());
    client
        .post(&format!("/invite/{co_owner_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await
        .assert_status(303);

    // An Admin (member id 3).
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=admin-user@acme.test&role=admin")
        .send()
        .await
        .assert_status(303);
    let admin_token = latest_invite_token(dir.path());
    let admin_accept = client
        .post(&format!("/invite/{admin_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    let admin_cookie = session_cookie(&admin_accept);

    // Two owners now exist, so the last-owner protection alone would let
    // this through — the Owner-vs-Admin check must be what blocks it.
    let demote_resp = client
        .post("/members/2/role")
        .header("cookie", &admin_cookie)
        .form("role=member")
        .send()
        .await;
    demote_resp.assert_status(403);

    let remove_resp = client
        .post("/members/2/remove")
        .header("cookie", &admin_cookie)
        .send()
        .await;
    remove_resp.assert_status(403);
}

/// Two Owners concurrently demoting *each other* must not both succeed —
/// each request's last-owner check ("is this the only Owner left?") has to
/// run atomically with its own demotion, or both requests can read the same
/// two-owner snapshot, both pass, and leave the organization with zero
/// Owners (Codex review finding: the last-owner check and the mutation were
/// previously a read-then-write race, not one locked transaction).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_mutual_demotion_of_the_last_two_owners_does_not_leave_zero_owners() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    // A second Owner (member id 2).
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=co-owner@acme.test&role=owner")
        .send()
        .await
        .assert_status(303);
    let co_owner_token = latest_invite_token(dir.path());
    let co_owner_accept = client
        .post(&format!("/invite/{co_owner_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    let co_owner_cookie = session_cookie(&co_owner_accept);

    // Owner (id 1) demotes co-owner (id 2), and co-owner (id 2) demotes
    // owner (id 1), at the same time.
    let (first, second) = tokio::join!(
        client
            .post("/members/2/role")
            .header("cookie", &owner_cookie)
            .form("role=member")
            .send(),
        client
            .post("/members/1/role")
            .header("cookie", &co_owner_cookie)
            .form("role=member")
            .send(),
    );

    let statuses = [first.status.as_u16(), second.status.as_u16()];
    assert!(
        statuses.contains(&303) && statuses.contains(&409),
        "exactly one concurrent mutual demotion should succeed (303) and \
         the other should be rejected as the last owner (409), got {statuses:?}"
    );

    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body
            .matches("text-xs uppercase text-gray-400\">owner<")
            .count(),
        1,
        "exactly one owner must remain after the race: {members_body}"
    );
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

/// A second, independent pending invitation to an email that's already a
/// member (e.g. invited twice, or already joined some other way) must be
/// consumed on accept, not left dangling — a lingering `pending` token would
/// stay redeemable indefinitely and could regrant membership after removal.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn accepting_a_lingering_invite_for_an_existing_member_consumes_it() {
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
    let first_token = latest_invite_token(dir.path());

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let second_token = latest_invite_token(dir.path());

    let accept = client
        .post(&format!("/invite/{first_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    accept.assert_status(303);
    let member_cookie = session_cookie(&accept);

    // Accepting the second, still-pending invite while already a member
    // must consume it too — not leave it pending forever.
    client
        .post(&format!("/invite/{second_token}/accept"))
        .header("cookie", &member_cookie)
        .send()
        .await
        .assert_status(303);

    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "the second invitation must be consumed (accepted), not left listed as pending: {members_body}"
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

/// A double-clicked (or concurrent) resend must not mint two live pending
/// invitations for the same original one — the second resend of an
/// already-resent (now revoked) invitation is rejected, not silently
/// treated as still pending.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn double_resend_does_not_create_two_pending_invitations() {
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

    // Only one invitation exists yet, so its id is deterministically 1.
    let first = client
        .post("/invitations/1/resend")
        .header("cookie", &owner_cookie)
        .send()
        .await;
    first.assert_status(303);

    // A second resend of the same (now-revoked) original invitation must be
    // rejected, not silently mint a second live pending replacement.
    let second = client
        .post("/invitations/1/resend")
        .header("cookie", &owner_cookie)
        .send()
        .await;
    second.assert_status(409);

    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "a rejected double-resend must not leave two pending invitations for the same invitee"
    );
}

/// A second `create_invitation` for the same organization/email while an
/// earlier one is still pending must revoke the older one instead of
/// leaving two independently valid tokens live — otherwise accepting one
/// would leave the other pending indefinitely (redeemable even after the
/// member is later removed).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn second_invite_to_same_email_revokes_the_first() {
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
    let first_token = latest_invite_token(dir.path());

    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let second_token = latest_invite_token(dir.path());

    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "a second invite to the same email must revoke the first, not leave two pending: {members_body}"
    );

    // The first (now-revoked) token no longer works...
    client
        .get(&format!("/invite/{first_token}"))
        .send()
        .await
        .assert_status(410);

    // ...but the second (current) token does.
    client
        .post(&format!("/invite/{second_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await
        .assert_status(303);
}

/// Two concurrent `create_invitation` requests for the same organization and
/// email must not both leave a live pending token: each request's
/// revoke-then-insert transaction sees no prior pending row to lock (there
/// isn't one yet), so the transaction alone can't serialize them.
/// `idx_invitations_pending_email` (a partial unique index on
/// `(tenant_id, email) WHERE status = 'pending'`) is the real backstop —
/// exercised here via genuinely concurrent requests against a real Postgres
/// instance, not just sequential calls (Codex review finding).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_invites_to_the_same_email_do_not_both_stay_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    let (first, second) = tokio::join!(
        client
            .post("/invitations")
            .header("cookie", &owner_cookie)
            .form("email=newbie@acme.test&role=member")
            .send(),
        client
            .post("/invitations")
            .header("cookie", &owner_cookie)
            .form("email=newbie@acme.test&role=member")
            .send(),
    );

    let statuses = [first.status.as_u16(), second.status.as_u16()];
    assert!(
        statuses.contains(&303) && statuses.contains(&409),
        "exactly one concurrent invite should succeed (303) and the other \
         should be rejected as a conflict (409), got {statuses:?}"
    );

    let members_body = client
        .get("/members")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert_eq!(
        members_body.matches("newbie@acme.test").count(),
        1,
        "only one pending invitation should survive two concurrent creates \
         for the same email: {members_body}"
    );
}

/// Posting a *revoked* invitation token must still be rejected with the
/// documented "no longer available" error even when the caller already
/// belongs to that organization through some other (later, still-valid)
/// invitation — the existing-membership shortcut must not let a dead token
/// silently succeed and switch the caller's active organization (Codex
/// review finding).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn accepting_a_revoked_token_is_rejected_even_for_an_existing_member() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = db_client(dir.path()).await;
    let owner_cookie = signup(&client, "owner@acme.test").await;

    // First invite to newbie@acme.test — revoke it before it's ever used.
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let revoked_token = latest_invite_token(dir.path());
    client
        .post("/invitations/1/revoke")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_status(303);

    // A second, later invite to the same email is accepted for real —
    // newbie is now a genuine member of the organization.
    client
        .post("/invitations")
        .header("cookie", &owner_cookie)
        .form("email=newbie@acme.test&role=member")
        .send()
        .await
        .assert_status(303);
    let live_token = latest_invite_token(dir.path());
    let accept = client
        .post(&format!("/invite/{live_token}/accept"))
        .form("password=An0ther-Str0ng-Pa55word")
        .send()
        .await;
    accept.assert_status(303);
    let newbie_cookie = session_cookie(&accept);

    // The first invitation's token is dead (revoked), even though newbie is
    // now a member of this exact organization — POSTing it must not be
    // silently treated as a successful (re)join via the existing-membership
    // shortcut inside `accept_invitation`'s transaction.
    client
        .post(&format!("/invite/{revoked_token}/accept"))
        .header("cookie", &newbie_cookie)
        .send()
        .await
        .assert_status(410)
        .assert_body_contains("no longer available");
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

/// `require_role` must revalidate against the live `Membership` row, not the
/// session's cached `"role"` string — otherwise a removed member's still-live
/// session cookie would keep authorizing requests until it expired.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn removed_member_loses_access_immediately_despite_cached_session() {
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

    // The member's own view of the roster works before removal.
    client
        .get("/members")
        .header("cookie", &member_cookie)
        .send()
        .await
        .assert_ok();

    // The owner removes them (member id 2, after the owner's own id 1), but
    // the removed member's session cookie is still live — nothing signs them
    // out server-side.
    client
        .post("/members/2/remove")
        .header("cookie", &owner_cookie)
        .send()
        .await
        .assert_status(303);

    // The stale cookie must no longer authorize anything: their `Membership`
    // row is gone, so `require_role` rejects them even though the session
    // still carries the old `"role"` value from login.
    client
        .get("/members")
        .header("cookie", &member_cookie)
        .send()
        .await
        .assert_status(401);
}
