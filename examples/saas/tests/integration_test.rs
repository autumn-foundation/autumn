//! Integration tests for the SaaS starter.
//!
//! ```text
//! cargo test                                   # smoke tests (instant, no Docker)
//! cargo test -- --include-ignored --test-threads=1   # full flow (needs Docker)
//! ```
//!
//! The ignored tests start a Postgres testcontainer and drive the real signup →
//! login → tenant-scoped dashboard flow, then prove one tenant cannot see
//! another's data.

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::middleware::from_fn;
use autumn_web::test::{TestApp, TestClient, TestDb};

use saas::routes;

fn app_routes() -> Vec<autumn_web::Route> {
    routes![
        saas::index,
        routes::auth::signup_form,
        routes::auth::signup,
        routes::auth::login_form,
        routes::auth::login,
        routes::auth::logout,
        routes::dashboard::dashboard,
        routes::dashboard::create_project,
    ]
}

/// Mirror the middleware-driven tenancy from `autumn.toml`: tenant resolved from
/// the session, public pages allowlisted, missing tenant redirected to login.
fn enable_tenancy(config: &mut AutumnConfig) {
    config.tenancy.enabled = true;
    config.tenancy.source = "session".to_string();
    config.tenancy.session_key = "tenant_id".to_string();
    config.tenancy.public_paths = vec![
        "/".to_string(),
        "/login".to_string(),
        "/signup".to_string(),
        "/logout".to_string(),
        "/static".to_string(),
    ];
    config.tenancy.login_redirect = Some("/login".to_string());
}

// ── Smoke tests (no Docker) ──────────────────────────────────────────────────

/// The login page renders without a database — proving the app wires up.
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

/// Regression guard for the multi-tenancy fix: with tenancy middleware enabled,
/// an allowlisted public page like `/login` must still be reachable by an
/// unauthenticated visitor (no session, no tenant) instead of being 401'd.
#[tokio::test]
async fn login_page_is_public_with_tenancy_enabled() {
    let mut config = AutumnConfig::default();
    enable_tenancy(&mut config);
    let client = TestApp::new().routes(app_routes()).config(config).build();
    client
        .get("/login")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Log in");
}

/// A protected route hit without a tenant redirects to the configured login page
/// rather than returning a raw 401. The middleware short-circuits before the
/// handler, so no database is required.
#[tokio::test]
async fn protected_route_redirects_to_login_when_unauthenticated() {
    let mut config = AutumnConfig::default();
    enable_tenancy(&mut config);
    let client = TestApp::new().routes(app_routes()).config(config).build();
    // A navigating browser advertises `Accept: text/html`, so it is bounced to the
    // login page rather than shown a raw 401.
    let resp = client
        .get("/dashboard")
        .header("accept", "text/html")
        .send()
        .await;
    resp.assert_status(303);
    assert_eq!(
        resp.header("location"),
        Some("/login"),
        "missing-tenant protected route should redirect a browser to the login page"
    );
}

/// An API client (`Accept: application/json`) hitting a protected route without a
/// tenant gets a raw 401, not an HTML login redirect, so its error handling works.
#[tokio::test]
async fn protected_route_returns_401_for_api_clients() {
    let mut config = AutumnConfig::default();
    enable_tenancy(&mut config);
    let client = TestApp::new().routes(app_routes()).config(config).build();
    let resp = client
        .get("/dashboard")
        .header("accept", "application/json")
        .send()
        .await;
    resp.assert_status(401);
}

// ── Full flow (requires Docker) ──────────────────────────────────────────────

/// Create the schema and return a CSRF-disabled, DB-backed client.
async fn db_client() -> TestClient {
    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS users (
            id            BIGSERIAL PRIMARY KEY,
            email         TEXT      NOT NULL UNIQUE,
            password_hash TEXT      NOT NULL,
            tenant_id     TEXT      NOT NULL,
            created_at    TIMESTAMP NOT NULL DEFAULT NOW()
        )",
    )
    .await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS projects (
            id         BIGSERIAL PRIMARY KEY,
            tenant_id  TEXT      NOT NULL,
            name       TEXT      NOT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT NOW()
        )",
    )
    .await;
    // Persistent "remember-me" chains (issue #1397).
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS remember_tokens (
            series       TEXT        PRIMARY KEY,
            user_id      BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
            tenant_id    TEXT        NOT NULL,
            token_hash   TEXT        NOT NULL,
            previous_token_hash TEXT NULL,
            rotated_at   TIMESTAMPTZ NULL,
            expires_at   TIMESTAMPTZ NOT NULL,
            ip           TEXT        NOT NULL DEFAULT '',
            user_agent   TEXT        NOT NULL DEFAULT '',
            ua_family    TEXT        NOT NULL DEFAULT '',
            ua_os        TEXT        NOT NULL DEFAULT '',
            ua_device    TEXT        NOT NULL DEFAULT '',
            label        TEXT,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_used_at TIMESTAMPTZ
        )",
    )
    .await;
    db.execute_sql("TRUNCATE users, projects, remember_tokens RESTART IDENTITY")
        .await;

    // The forms post normally; disable CSRF so the test does not have to scrape
    // a token out of the rendered HTML.
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    // Drive tenancy through the middleware exactly as `autumn.toml` does.
    enable_tenancy(&mut config);

    // Hand the remember middleware the shared pool and resolved config, exactly
    // as `main`'s startup hook does, and register the middleware layer so a
    // remember cookie rotates into a session before the tenancy gate.
    saas::remember::init_remember_pool(db.pool(), autumn_web::auth::RememberConfig::default());

    TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .layer(from_fn(saas::remember::remember_me_middleware))
        .build()
}

/// Pull the session cookie pair (`name=value`) out of a `Set-Cookie` response.
fn session_cookie(resp: &autumn_web::test::TestResponse) -> String {
    let set_cookie = resp
        .header("set-cookie")
        .expect("auth response should set a session cookie");
    set_cookie
        .split(';')
        .next()
        .expect("cookie has a name=value pair")
        .to_owned()
}

/// Return the `name=value` value half of a named cookie from any of the
/// response's `Set-Cookie` headers (a login with remember-me sets two).
fn named_cookie(resp: &autumn_web::test::TestResponse, name: &str) -> Option<String> {
    for (key, value) in &resp.headers {
        if key.eq_ignore_ascii_case("set-cookie") {
            let pair = value.split(';').next().unwrap_or("");
            if let Some((cookie_name, cookie_value)) = pair.split_once('=')
                && cookie_name.trim() == name
            {
                return Some(cookie_value.trim().to_owned());
            }
        }
    }
    None
}

/// Sign up a user and return their session cookie.
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn signup_login_dashboard_returns_200() {
    let client = db_client().await;
    let cookie = signup(&client, "founder@acme.test").await;

    // The tenant-scoped dashboard renders for the signed-in session.
    client
        .get("/dashboard")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Projects");

    // Logging back in lands on the same tenant dashboard.
    let login = client
        .post("/login")
        .form("email=founder@acme.test&password=Tr0ubad0ur-Xy7-correct-horse")
        .send()
        .await;
    login.assert_status(303);
    let cookie = session_cookie(&login);
    client
        .get("/dashboard")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok();
}

/// A weak password (present in the bundled common-password corpus) is rejected
/// at signup: the form re-renders with the policy error at HTTP 200 — not a
/// redirect and not a 5xx — and no account is created, so a follow-up login
/// attempt with those credentials is unauthorized.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn signup_rejects_weak_password() {
    let client = db_client().await;

    let resp = client
        .post("/signup")
        .form("email=weak@acme.test&password=password")
        .send()
        .await;
    // Re-rendered form with the field error, not an accepted account (303) or a
    // server error.
    resp.assert_ok();
    assert!(
        resp.text().contains("too common"),
        "expected the weak-password policy error in the re-rendered form, got: {}",
        resp.text()
    );

    // No account was created, so logging in with those credentials fails —
    // the login form re-renders inline with the generic message at HTTP 200,
    // the same convention as the signup failure above (Wayfinder: error-path
    // inventory), rather than a full navigation to a generic error page.
    let login = client
        .post("/login")
        .form("email=weak@acme.test&password=password")
        .send()
        .await;
    login.assert_ok();
    assert!(
        login.text().contains("Invalid email or password"),
        "expected the login form to redisplay inline with the auth error, got: {}",
        login.text()
    );
}

/// Every recoverable signup/login failure mode redisplays the form inline
/// (HTTP 200, same page, error adjacent to the fields) with the entered email
/// preserved, instead of navigating the user to a generic error page and
/// losing what they typed (Wayfinder: error-path inventory).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn signup_and_login_failures_redisplay_the_form_with_email_preserved() {
    let client = db_client().await;

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

    // Weak password at signup (the one branch that already redisplayed
    // before this PR, but — per Codex review — was never itself asserted to
    // preserve the entered email; `signup_rejects_weak_password` above only
    // checks the policy message).
    let resp = client
        .post("/signup")
        .form("email=weak2@acme.test&password=password")
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("too common"));
    assert!(resp.text().contains(r#"value="weak2@acme.test""#));

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

    // Over-long input at login (Codex review finding: the pre-existing
    // "invalid credentials" case above exercises the same `login_page` call
    // but not this distinct length-guard branch). Reuses the over-long
    // password minted for the signup case above.
    let resp = client
        .post("/login")
        .form(&format!("email=toolong@acme.test&password={long_password}"))
        .send()
        .await;
    resp.assert_ok();
    assert!(resp.text().contains("Invalid email or password"));
    assert!(resp.text().contains(r#"value="toolong@acme.test""#));
}

/// A "Remember me" opt-in must survive a rejected login (Codex review
/// finding): if the user ticks the box, mistypes the password, and corrects
/// it on the redisplayed form without re-checking a box they already
/// checked, the eventual successful login must still honor their original
/// choice — so the checkbox itself must come back checked.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn login_failure_preserves_the_remember_me_choice() {
    let client = db_client().await;

    let checked = client
        .post("/login")
        .form("email=nobody@acme.test&password=wrong-password&remember=on")
        .send()
        .await;
    checked.assert_ok();
    assert!(
        checked.text().contains("checked"),
        "expected the Remember-me checkbox to stay checked after a rejected login, got: {}",
        checked.text()
    );

    let unchecked = client
        .post("/login")
        .form("email=nobody@acme.test&password=wrong-password")
        .send()
        .await;
    unchecked.assert_ok();
    assert!(
        !unchecked.text().contains("checked"),
        "an unticked box must not come back checked, got: {}",
        unchecked.text()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn tenants_are_isolated() {
    let client = db_client().await;

    let acme = signup(&client, "a@acme.test").await;
    let globex = signup(&client, "b@globex.test").await;

    // Acme creates a project.
    client
        .post("/dashboard/projects")
        .header("cookie", &acme)
        .form("name=Alpha")
        .send()
        .await
        .assert_status(303);

    // Acme sees its project …
    client
        .get("/dashboard")
        .header("cookie", &acme)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Alpha");

    // … but Globex (a different tenant) does not.
    let globex_view = client
        .get("/dashboard")
        .header("cookie", &globex)
        .send()
        .await;
    globex_view.assert_ok();
    assert!(
        !globex_view.text().contains("Alpha"),
        "tenant isolation breached: Globex can see Acme's project"
    );
}

/// AC8 (issue #1397): the whole persistent remember-me lifecycle, under the
/// grace-window rotation model —
///   1. `POST /login` with `remember=on` sets BOTH a session cookie and a
///      remember cookie (T0);
///   2. after a browser restart (session cookie dropped, remember cookie kept),
///      a protected route authenticates via the remember chain and ROTATES the
///      remember cookie to a fresh value (T1);
///   3. a SECOND restart rotates again (T2), pushing the ORIGINAL T0 out of the
///      one-slot previous-token window — so replaying T0 can no longer be
///      confused with a benign concurrent request;
///   4. replaying the ORIGINAL T0 (matching neither current T2 nor previous T1)
///      is detected as theft — the request is unauthorized and the whole chain
///      is revoked, so even the latest rotated cookie (T2) no longer
///      authenticates.
///
/// This demonstrates theft in a grace-INDEPENDENT way: an immediate replay of
/// the just-rotated-out token is `Accept` (see the grace test below), so theft
/// is proven by replaying a token that is two rotations stale.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn remember_me_rotates_across_restart_and_detects_theft() {
    let client = db_client().await;

    // Seed a confirmed account. `signup` logs the browser in; drop that session
    // so the next login is a clean "remember me" opt-in.
    signup(&client, "remember@acme.test").await;
    client.log_out();

    // 1. Log in WITH the remember box ticked.
    let login = client
        .post("/login")
        .form("email=remember@acme.test&password=Tr0ubad0ur-Xy7-correct-horse&remember=on")
        .send()
        .await;
    login.assert_status(303);
    let original_remember = named_cookie(&login, "autumn.remember")
        .expect("login with remember=on must set a remember cookie");
    assert!(
        named_cookie(&login, "autumn.sid").is_some(),
        "login must also set a session cookie"
    );

    // Sanity: the freshly-authenticated jar reaches the dashboard.
    client
        .get("/dashboard")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Projects");

    // 2. Browser restart: the session cookie (no Max-Age) is cleared but the
    //    persistent remember cookie survives. A protected route must now
    //    authenticate via the remember chain and rotate the cookie T0 → T1.
    client.log_out(); // clears only the session cookie
    let restart = client.get("/dashboard").send().await;
    restart.assert_ok();
    let rotated_once = named_cookie(&restart, "autumn.remember")
        .expect("a rotated request must set a fresh remember cookie");
    assert_ne!(
        rotated_once, original_remember,
        "the remember cookie must rotate on use"
    );

    // 3. Second restart: rotate again T1 → T2. Now the stored previous token is
    //    T1, and the ORIGINAL T0 is beyond the one-slot grace window.
    client.log_out();
    let restart2 = client.get("/dashboard").send().await;
    restart2.assert_ok();
    let rotated_twice = named_cookie(&restart2, "autumn.remember")
        .expect("a second rotated request must set another fresh remember cookie");
    assert_ne!(
        rotated_twice, rotated_once,
        "the remember cookie must rotate again on the second use"
    );

    // 4. Theft: replay the ORIGINAL (two-rotations-stale) remember cookie with
    //    no session cookie (an explicit `cookie` header bypasses the jar). It
    //    matches neither the current nor the previous token → theft:
    //    unauthorized, chain nuked.
    client.log_out();
    let theft = client
        .get("/dashboard")
        .header("accept", "application/json")
        .header("cookie", &format!("autumn.remember={original_remember}"))
        .send()
        .await;
    theft.assert_status(401);

    // The chain was revoked, so even the *latest* rotated cookie (T2) no longer
    // authenticates.
    let after_theft = client
        .get("/dashboard")
        .header("accept", "application/json")
        .header("cookie", &format!("autumn.remember={rotated_twice}"))
        .send()
        .await;
    after_theft.assert_status(401);
}

/// AC8 grace path (issue #1397): immediately replaying the just-rotated-out
/// token — before any second rotation — is a benign concurrent request within
/// the grace window (`Accept`), so it still authenticates and does NOT trip
/// theft detection. This documents the intended concurrency behaviour that the
/// theft test above deliberately steps around.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn remember_me_accepts_previous_token_within_grace() {
    let client = db_client().await;

    signup(&client, "grace@acme.test").await;
    client.log_out();

    // Log in with remember, capture T0.
    let login = client
        .post("/login")
        .form("email=grace@acme.test&password=Tr0ubad0ur-Xy7-correct-horse&remember=on")
        .send()
        .await;
    login.assert_status(303);
    let original_remember = named_cookie(&login, "autumn.remember")
        .expect("login with remember=on must set a remember cookie");

    // Browser restart: one rotation T0 → T1.
    client.log_out();
    let restart = client.get("/dashboard").send().await;
    restart.assert_ok();
    let rotated_once = named_cookie(&restart, "autumn.remember")
        .expect("a rotated request must set a fresh remember cookie");
    assert_ne!(rotated_once, original_remember);

    // Immediately replay the just-rotated-out T0 with no session. It is the
    // stored previous token, still inside the grace window → Accept: the
    // dashboard renders (authenticated) rather than 401.
    client.log_out();
    let within_grace = client
        .get("/dashboard")
        .header("cookie", &format!("autumn.remember={original_remember}"))
        .send()
        .await;
    within_grace.assert_ok().assert_body_contains("Projects");

    // The chain is intact, so the latest rotated cookie T1 still authenticates.
    client.log_out();
    client
        .get("/dashboard")
        .header("cookie", &format!("autumn.remember={rotated_once}"))
        .send()
        .await
        .assert_ok();
}
