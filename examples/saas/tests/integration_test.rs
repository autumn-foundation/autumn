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
    db.execute_sql("TRUNCATE users, projects RESTART IDENTITY")
        .await;

    // The forms post normally; disable CSRF so the test does not have to scrape
    // a token out of the rendered HTML.
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    // Drive tenancy through the middleware exactly as `autumn.toml` does.
    enable_tenancy(&mut config);

    TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
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

    // No account was created, so logging in with those credentials fails.
    let login = client
        .post("/login")
        .form("email=weak@acme.test&password=password")
        .send()
        .await;
    login.assert_status(401);
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
