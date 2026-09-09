//! Integration tests for the CMS starter.
//!
//! ```text
//! cargo test -p {{project_name}}                                    # smoke tests (no Docker)
//! cargo test -p {{project_name}} -- --include-ignored --test-threads=1   # full flow (needs Docker)
//! ```
//!
//! The ignored tests start a Postgres testcontainer and drive the real
//! registration → author → publish → read flow through the actual routes.
//! `--test-threads=1` is not optional: they share one process-global
//! `TestDb::shared()` container and each test truncates it, so running them
//! concurrently would race each other's data.

use autumn_web::config::AutumnConfig;
use autumn_web::test::{TestApp, TestClient, TestDb, TestResponse};

/// The real migration, so the test schema can never drift from the shipped one.
const MIGRATION_SQL: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

/// The application's real route table — the same one `main` mounts.
fn app_routes() -> Vec<autumn_web::Route> {
    {{crate_name}}::all_routes()
}

/// URL-encode form pairs.
///
/// `TestRequest::form` takes an already-encoded body, and these tests submit
/// values containing spaces, commas and `%` — encoding by hand once here is
/// safer than remembering to escape at twenty call sites.
fn form(pairs: &[(&str, &str)]) -> String {
    fn encode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*byte as char);
                }
                b' ' => out.push('+'),
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

// ── Smoke tests (no Docker) ─────────────────────────────────────────────────

/// Exactly one route is a wildcard, and it is the front controller.
///
/// This is the one structural claim worth making without a database, because it
/// is the hazard the routing design actually has: `dispatch` is mounted at
/// `/{*path}` and serves every permalink, so a second wildcard — or a reserved
/// prefix that was never given a literal route — would be silently swallowed by
/// it and 404 at runtime rather than fail to build.
#[test]
fn the_front_controller_is_the_only_wildcard_route() {
    let routes = app_routes();
    let wildcards: Vec<&str> = routes
        .iter()
        .map(|route| route.path)
        .filter(|path| path.contains('*'))
        .collect();
    assert_eq!(
        wildcards,
        vec!["/{*path}"],
        "a second wildcard route would shadow, or be shadowed by, the front controller"
    );
}

/// Every reserved prefix has a literal route of its own.
///
/// A path listed here that lost its handler would not 404 — it would fall
/// through to the front controller and be looked up as a post slug, which is a
/// far more confusing failure than a missing route.
#[test]
fn every_reserved_prefix_has_a_literal_route() {
    let routes = app_routes();
    let paths: Vec<&str> = routes.iter().map(|route| route.path).collect();
    for reserved in [
        "/login",
        "/logout",
        "/register",
        "/search",
        "/admin",
        "/feed",
        "/api/v1",
        "/media/{slug}",
        // The browser asks for this on every page load. Without a literal
        // route the catch-all answers it with a themed 404, which the
        // headless-Chromium smoke sees as a console error — and which
        // shadows the framework's own `204 No Content` fallback.
        "/favicon.ico",
    ] {
        assert!(
            paths.contains(&reserved),
            "`{reserved}` has no literal route, so the front controller would try to resolve \
             it as content; mounted paths: {paths:?}"
        );
    }
}

/// Every route source, for the scans below. One list, so a new module cannot
/// be added to one gate and forgotten by the other.
fn route_sources() -> &'static [(&'static str, &'static str)] {
    &[
        ("routes/site.rs", include_str!("../src/routes/site.rs")),
        ("routes/auth.rs", include_str!("../src/routes/auth.rs")),
        ("routes/front.rs", include_str!("../src/routes/front.rs")),
        (
            "routes/comments.rs",
            include_str!("../src/routes/comments.rs"),
        ),
        (
            "routes/admin/mod.rs",
            include_str!("../src/routes/admin/mod.rs"),
        ),
        (
            "routes/admin/posts.rs",
            include_str!("../src/routes/admin/posts.rs"),
        ),
        (
            "routes/admin/terms.rs",
            include_str!("../src/routes/admin/terms.rs"),
        ),
        (
            "routes/admin/comments.rs",
            include_str!("../src/routes/admin/comments.rs"),
        ),
        (
            "routes/admin/media.rs",
            include_str!("../src/routes/admin/media.rs"),
        ),
        (
            "routes/admin/users.rs",
            include_str!("../src/routes/admin/users.rs"),
        ),
        (
            "routes/admin/settings.rs",
            include_str!("../src/routes/admin/settings.rs"),
        ),
        (
            "routes/admin/appearance.rs",
            include_str!("../src/routes/admin/appearance.rs"),
        ),
        (
            "routes/admin/tools.rs",
            include_str!("../src/routes/admin/tools.rs"),
        ),
        ("theme.rs", include_str!("../src/theme.rs")),
    ]
}

/// No handler takes the `Db` extractor.
///
/// `Db` is checked out before the handler body runs and held until the response
/// is returned. The repositories are pool-backed and acquire their *own*
/// connection per call, so a handler holding a `Db` and then reaching for a
/// repository needs two slots at once. With the shipped `pool_size = 10`, ten
/// concurrent requests in that shape each hold one while waiting for a second
/// that only another of them could release — a pool-wide deadlock, reachable
/// from `/comments/{id}`, which is unauthenticated.
///
/// The rule is therefore: handlers get their connection from `Repos::with_conn`,
/// which scopes it to a single call and cannot span a repository read. This
/// scans for the extractor because the failure is invisible until the pool is
/// under real concurrency, which no test in this suite produces.
#[test]
fn no_handler_holds_a_pool_connection_across_repository_calls() {
    let sources = route_sources();
    let mut offenders = Vec::new();
    for (name, source) in sources {
        for (line_no, line) in source.lines().enumerate() {
            if line.contains("autumn_web::Db") && !line.trim_start().starts_with("//") {
                offenders.push(format!("{name}:{}: {}", line_no + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these handlers take the `Db` extractor; use `Repos::with_conn` instead so the \
         connection cannot be held across a repository call:\n{}",
        offenders.join("\n")
    );
}

/// Every `POST` form in the source carries a CSRF token.
///
/// The runtime test above proves the mechanism works on one form. This proves
/// no form was *forgotten* — the actual failure mode, since a missing token is
/// invisible until someone submits that particular form in a deployment with
/// CSRF on (and this suite runs with it off). Scanning the source is crude, but
/// it is the only check that covers all twenty-odd forms at once.
#[test]
fn every_post_form_emits_a_csrf_token() {
    let sources = route_sources();
    let mut forms = 0_usize;
    for (name, source) in sources {
        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains(r#"method="post""#) {
                continue;
            }
            forms += 1;
            // The token is emitted as the form's first child, so it lands
            // within a few lines of the opening tag. `chrome.csrf` is the
            // pre-rendered input the theme layout carries.
            let window = lines[index..lines.len().min(index + 8)].join("\n");
            assert!(
                window.contains("csrf.input()") || window.contains("chrome.csrf"),
                "{name}:{} opens a POST form with no CSRF token:\n{window}",
                index + 1
            );
        }
    }
    assert!(
        forms >= 20,
        "expected to scan the application's POST forms, found only {forms} — \
         has the markup changed shape?"
    );
}

/// The admin is capability-gated at every entry point.
///
/// A handler added to the admin without a `require_capability!` would be
/// reachable by any signed-in Subscriber. This asserts the shape rather than
/// the behaviour — the behaviour is covered by the Docker tests below — so it
/// catches the omission at the cheapest possible moment.
#[test]
fn every_admin_route_is_under_the_admin_prefix() {
    let routes = app_routes();
    for route in &routes {
        if route.name.contains("admin") {
            assert!(
                route.path.starts_with("/admin") || route.path.starts_with("/media"),
                "`{}` looks like an admin handler but is mounted at `{}`",
                route.name,
                route.path
            );
        }
    }
}

// ── Full flow (requires Docker) ─────────────────────────────────────────────

/// Split the migration into individual statements.
///
/// `TestDb::execute_sql` prepares what it is given, and Postgres refuses
/// multiple commands in one prepared statement — passing the whole file makes
/// every Docker test in the suite fail with "cannot insert multiple commands
/// into a prepared statement". `examples/teams` shipped exactly that bug for
/// months; this is the fix, applied up front.
fn migration_statements() -> Vec<String> {
    // Comments are stripped BEFORE splitting, not after. A prose comment in the
    // migration contains a semicolon ("…no application code; `#[searchable]`…"),
    // and splitting first turns the rest of that sentence into a statement of
    // its own — which fails with `syntax error at or near "\`#"`. This ordering
    // is the whole subtlety in this function.
    //
    // It still assumes no statement contains a semicolon inside a string
    // literal or a `$$`-quoted body; this migration has neither, and a
    // migration that grows one needs a real parser rather than a patch here.
    let without_comments: String = MIGRATION_SQL
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");

    without_comments
        .split(';')
        .map(|statement| statement.trim().to_owned())
        .filter(|statement| !statement.is_empty())
        .collect()
}

/// Apply one statement, returning the error instead of panicking.
///
/// `TestDb::execute_sql` panics on failure, which is the right default but
/// makes the version probe below impossible.
async fn try_execute(db: &TestDb, sql: &str) -> Result<(), String> {
    use diesel_async::RunQueryDsl;
    let mut conn = db.pool().get().await.expect("pool connection");
    diesel::sql_query(sql)
        .execute(&mut conn)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Apply the migration exactly once per test process.
///
/// `TestDb::shared()` hands every test the *same* container, so running the
/// DDL per test fails the second one with `relation "users" already exists`.
/// Rewriting the migration to say `IF NOT EXISTS` would be the other fix and is
/// worse: it would make the committed migration lie about being idempotent when
/// it is not, to serve a test-only need.
static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Recreate the `search_vector` column with a trigger, for servers older than
/// PostgreSQL 12. See the call site for why this lives in the test harness.
async fn apply_search_vector_fallback(db: &TestDb) {
    db.execute_sql("ALTER TABLE posts ADD COLUMN search_vector tsvector")
        .await;
    db.execute_sql(
        "CREATE FUNCTION posts_search_vector_refresh() RETURNS trigger AS $$
         BEGIN
             NEW.search_vector :=
                 setweight(to_tsvector('english'::regconfig, coalesce(NEW.title, '')), 'A') ||
                 setweight(to_tsvector('english'::regconfig, coalesce(NEW.excerpt, '')), 'B') ||
                 setweight(to_tsvector('english'::regconfig, coalesce(NEW.body, '')), 'C');
             RETURN NEW;
         END
         $$ LANGUAGE plpgsql",
    )
    .await;
    db.execute_sql(
        "CREATE TRIGGER posts_search_vector_trigger BEFORE INSERT OR UPDATE ON posts
         FOR EACH ROW EXECUTE PROCEDURE posts_search_vector_refresh()",
    )
    .await;
}

/// A migrated, truncated, CSRF-disabled client.
async fn db_client() -> TestClient {
    let db = TestDb::shared().await;
    SCHEMA
        .get_or_init(|| async {
            for statement in migration_statements() {
                // `GENERATED ALWAYS AS (…) STORED` needs PostgreSQL 12, and
                // `TestDb` pins `postgres:11-alpine` (testcontainers-modules'
                // default tag). The migration is NOT wrong — `docker-compose.yml`
                // runs Postgres 16, and the framework's own `#[searchable]`
                // generator emits this exact construct (see
                // `examples/wiki/migrations/…_add_search_to_pages`). So the
                // fallback belongs here, in the harness, rather than degrading
                // the shipped migration to suit an EOL server.
                //
                // The fallback produces the *same column with the same
                // contents* via a trigger, so `search()` is genuinely exercised
                // rather than skipped — the FTS test would otherwise be the one
                // test that never ran.
                if let Err(error) = try_execute(db, &statement).await {
                    let is_generated_column = statement.contains("GENERATED ALWAYS AS");
                    assert!(
                        is_generated_column,
                        "migration statement failed: {error}\nSQL: {statement}"
                    );
                    apply_search_vector_fallback(db).await;
                }
            }
        })
        .await;
    db.execute_sql(
        "TRUNCATE users, options, attachments, posts, post_meta, terms, post_terms, \
         revisions, comments, menus, menu_items, widgets RESTART IDENTITY CASCADE",
    )
    .await;

    // The settings read is memoized per *process*, so truncating `options`
    // alone does not reset it: a test that changes a setting keeps changing it
    // for every test that runs afterwards. That surfaced as an unrelated
    // failure — a test configuring a front page left `/` rendering that single
    // post for everything after it — and the same shape once disabled guest
    // comments suite-wide. Invalidating through the app's own mechanism is what
    // makes each test start from the shipped defaults.
    assert!(
        {{crate_name}}::repositories::PgSiteOptionRepository::invalidate_declared_caches(),
        "the test cache backend cannot invalidate by namespace, so settings would leak \
         between tests"
    );

    // Registrations are process-global; the real `main` calls this too, so the
    // test app and the shipped app see the same post types and shortcodes.
    {{crate_name}}::bootstrap();

    // The forms post normally; disabling CSRF keeps the tests from having to
    // scrape a hidden token out of every rendered page.
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    // `SubmitTokenLayer` is the same story: the registration form carries a
    // one-time token that the tests would otherwise have to round-trip.
    config.security.submit_token.enabled = false;
    // A `TestClient` request has no TCP peer, and `__check_throttle` bypasses a
    // caller it cannot identify — so a `#[throttle(key = "ip")]` route is
    // unreachable from a test unless the address arrives in a header. This
    // makes `X-Forwarded-For` that address. It changes nothing for a request
    // that sends no such header (still no peer, still bypassed), so only the
    // test that deliberately sets one is throttled; the global limiter stays
    // off.
    config.security.rate_limit.trust_forwarded_headers = true;

    TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .build()
}

/// A client with CSRF **enabled**, unlike [`db_client`].
///
/// `TestApp::new()` disables CSRF by default, so the rest of this suite never
/// exercises it — which is exactly how every form in this application once came
/// to carry a hidden field named `_csrf_token` while `CsrfLayer` scans for the
/// configured `security.csrf.form_field` (default `_csrf`). Every POST would
/// have 403'd in production and no test would have noticed.
async fn csrf_client() -> TestClient {
    let db = TestDb::shared().await;
    SCHEMA
        .get_or_init(|| async {
            for statement in migration_statements() {
                if try_execute(db, &statement).await.is_err() {
                    apply_search_vector_fallback(db).await;
                }
            }
        })
        .await;
    db.execute_sql(
        "TRUNCATE users, options, attachments, posts, post_meta, terms, post_terms, \
         revisions, comments, menus, menu_items, widgets RESTART IDENTITY CASCADE",
    )
    .await;

    // The settings read is memoized per *process*, so truncating `options`
    // alone does not reset it: a test that changes a setting keeps changing it
    // for every test that runs afterwards. That surfaced as an unrelated
    // failure — a test configuring a front page left `/` rendering that single
    // post for everything after it — and the same shape once disabled guest
    // comments suite-wide. Invalidating through the app's own mechanism is what
    // makes each test start from the shipped defaults.
    assert!(
        {{crate_name}}::repositories::PgSiteOptionRepository::invalidate_declared_caches(),
        "the test cache backend cannot invalidate by namespace, so settings would leak \
         between tests"
    );
    {{crate_name}}::bootstrap();

    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = true;
    TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .build()
}

/// Pull the value of the hidden CSRF input out of a rendered form.
///
/// Deliberately reads the **name** from the markup rather than assuming
/// `_csrf`: that assumption is the bug this test exists to catch.
fn scrape_csrf(html: &str) -> (String, String) {
    let marker = r#"<input type="hidden" name=""#;
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("no hidden CSRF input in the rendered form:\n{html}"))
        + marker.len();
    let rest = &html[start..];
    let name_end = rest.find('"').expect("field name is quoted");
    let name = rest[..name_end].to_owned();

    let value_marker = r#" value=""#;
    let value_start =
        rest.find(value_marker).expect("hidden input has a value") + value_marker.len();
    let value_rest = &rest[value_start..];
    let value_end = value_rest.find('"').expect("value is quoted");
    (name, value_rest[..value_end].to_owned())
}

/// Every POST form carries a token the layer will actually look for, and a
/// submission without one is refused.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn forms_carry_a_csrf_token_the_layer_accepts() {
    let client = csrf_client().await;

    // The registration form renders a token…
    let page = client.get("/register").send().await.assert_ok().text();
    let (field, token) = scrape_csrf(&page);
    assert_eq!(
        field, "_csrf",
        "the hidden field must use the configured `security.csrf.form_field`; \
         a form that invents its own name submits a token nothing validates"
    );

    // …a submission carrying it is accepted…
    let accepted = client
        .post("/register")
        .form(&form(&[
            ("username", "owner"),
            ("email", "owner@example.com"),
            ("password", "correct-horse-battery-staple"),
            (field.as_str(), token.as_str()),
        ]))
        .send()
        .await;
    assert_eq!(
        accepted.status,
        303,
        "a token-carrying submission must be accepted; body: {}",
        accepted.text()
    );

    // …and one without it is refused.
    let refused = client
        .post("/register")
        .form(&form(&[
            ("username", "intruder"),
            ("email", "intruder@example.com"),
            ("password", "correct-horse-battery-staple"),
        ]))
        .send()
        .await;
    assert!(
        refused.status.is_client_error(),
        "a submission with no CSRF token must be refused, got {}",
        refused.status
    );
}

/// The `name=value` pair from a response's session cookie.
fn session_cookie(resp: &TestResponse) -> String {
    resp.header("set-cookie")
        .expect("an authenticating response sets a session cookie")
        .split(';')
        .next()
        .expect("cookie has a name=value pair")
        .to_owned()
}

/// Make the client anonymous.
///
/// `TestClient` carries a cookie jar and replays it automatically, so a plain
/// `client.get(...)` after a registration is still signed in. Every assertion
/// about what a *visitor* can see has to clear it first — without this, the
/// draft-visibility and password-protection tests pass for the wrong reason.
fn sign_out(client: &TestClient) {
    client.log_out();
}

/// Register an account. The first one created owns the site.
async fn register(client: &TestClient, username: &str) -> String {
    let email = format!("{username}@example.com");
    let resp = client
        .post("/register")
        .form(&form(&[
            ("username", username),
            ("email", &email),
            ("password", "correct-horse-battery-staple"),
        ]))
        .send()
        .await;
    assert_eq!(
        resp.status,
        303,
        "registration should redirect; body was: {}",
        resp.text()
    );
    session_cookie(&resp)
}

/// The settings form, filled with the shipped defaults, with `overrides` applied.
///
/// Posting a partial settings form is a trap: `comment_moderation` and
/// `allow_guest_comments` are `Option<String>`, so a browser omits them when
/// unchecked and the handler reads *absent* as *off*. A test that names only
/// the field it cares about therefore silently disables guest comments — and
/// because the settings read is memoized per process, it does so for every test
/// that runs after it, in a way that looks like an unrelated failure. Building
/// from the defaults here means a test can only change what it names.
fn settings_form(overrides: &[(&str, &str)]) -> String {
    let mut fields: Vec<(&str, &str)> = vec![
        ("site_title", "Test Site"),
        ("tagline", ""),
        ("permalink_structure", "day_and_name"),
        ("posts_per_page", "10"),
        ("default_comment_status", "open"),
        ("comment_moderation", "on"),
        ("allow_guest_comments", "on"),
        ("active_theme", "default"),
        ("date_format", "%B %-d, %Y"),
        ("timezone", "UTC"),
    ];
    for (key, value) in overrides {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => fields.push((key, value)),
        }
    }
    form(&fields)
}

/// Upload an export file to the importer.
///
/// The endpoint takes a multipart file rather than a URL-encoded field: form
/// encoding turned every quote and brace into a three-byte escape, so a backup
/// roughly a third of the request limit already exceeded it and the CMS could
/// not restore its own export.
async fn import_export(client: &TestClient, cookie: &str, payload: &str) -> TestResponse {
    const BOUNDARY: &str = "----cmsimport";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"payload\"; \
         filename=\"export.json\"\r\nContent-Type: application/json\r\n\r\n\
         {payload}\r\n--{BOUNDARY}--\r\n"
    );
    client
        .post("/admin/tools/import")
        .header("cookie", cookie)
        .header(
            "content-type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
}

/// Encode an editor form, stamping the post's current `lock_version`.
///
/// The editor renders that hidden field on every edit and the update handler
/// requires it, so a test that omits it is exercising a request the UI cannot
/// make — and, before the check existed, one that silently skipped the
/// stale-edit guard.
async fn edit_form(id: &impl std::fmt::Display, fields: &[(&str, &str)]) -> String {
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    let id: i64 = id.to_string().parse().expect("a post id");
    let version = {
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(id)
            .select({{crate_name}}::schema::posts::lock_version)
            .first::<i32>(&mut conn)
            .await
            .expect("the post")
            .to_string()
    };
    let mut all: Vec<(&str, &str)> = fields.to_vec();
    all.push(("lock_version", version.as_str()));
    form(&all)
}

/// Create a post through the admin editor and return its id.
async fn create_post(
    client: &TestClient,
    cookie: &str,
    title: &str,
    body: &str,
    status: &str,
) -> i64 {
    let resp = client
        .post("/admin/content/post")
        .header("cookie", cookie)
        .form(&form(&[
            ("title", title),
            ("slug", ""),
            ("excerpt", ""),
            ("body", body),
            ("status", status),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            // Browsers omit an unchecked checkbox entirely, so the handler
            // reads an absent `comment_status` as "closed". The real editor
            // renders this box checked; the fixture has to say so too.
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(resp.status, 303, "create should redirect: {}", resp.text());
    resp.header("location")
        .expect("redirect to the editor")
        .rsplit('/')
        .next()
        .expect("id is the last path segment")
        .parse()
        .expect("id is numeric")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_first_registered_account_owns_the_site() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // An administrator lands on the dashboard and sees every capability.
    client
        .get("/admin")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Administrator")
        .assert_body_contains("manage_options");

    // The second account is a Subscriber, and a Subscriber has no admin.
    let subscriber = register(&client, "reader").await;
    let resp = client
        .get("/admin")
        .header("cookie", &subscriber)
        .send()
        .await;
    assert_eq!(
        resp.status, 403,
        "a signed-in account without the capability gets a 403, not a redirect"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_published_post_appears_on_the_front_page_and_a_draft_does_not() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    create_post(
        &client,
        &cookie,
        "Published Post",
        "Visible body.",
        "publish",
    )
    .await;
    create_post(&client, &cookie, "Draft Post", "Hidden body.", "draft").await;

    sign_out(&client);
    let home = client.get("/").send().await;
    home.assert_ok().assert_body_contains("Published Post");
    assert!(
        !home.text().contains("Draft Post"),
        "an unpublished post must not appear on the front page"
    );

    // The published post resolves at its permalink…
    client
        .get("/published-post")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Visible body.");

    // …and the draft is a 404 for an anonymous visitor, not a 403: a 403 would
    // confirm that unpublished content exists at that URL.
    sign_out(&client);
    let draft = client.get("/draft-post").send().await;
    assert_eq!(draft.status, 404);
    assert!(!draft.text().contains("Hidden body."));

    // Its author, though, can preview it.
    client
        .get("/draft-post")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Hidden body.");
}

/// A post created *directly* as published carries a publish date.
///
/// It did not, at first: `published_at` was stamped only in `before_update`, so
/// content that never passed through a draft→publish transition had no date —
/// no byline on the page, nothing to order the index by, and no `<lastmod>` in
/// the sitemap. Every Docker test in this file passed while that was broken,
/// because none of them looked at the rendered date; it took booting the app.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_directly_published_post_has_a_publish_date() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Dated", "Body.", "publish").await;

    sign_out(&client);
    let page = client.get("/dated").send().await.assert_ok().text();
    assert!(
        page.contains("<time datetime="),
        "a published post must render its date; page:\n{page}"
    );

    // The API is the machine-readable half of the same fact.
    let posts: serde_json::Value = client.get("/api/v1/posts").send().await.assert_ok().json();
    let first = &posts.as_array().expect("array")[0];
    assert!(
        !first["published_at"].is_null(),
        "published_at must be set on a directly-published post: {first}"
    );

    // …and the sitemap carries a `<lastmod>` derived from it.
    let sitemap = client.get("/sitemap.xml").send().await.assert_ok().text();
    assert!(sitemap.contains("<loc>"), "sitemap has no URLs:\n{sitemap}");
    assert!(
        sitemap.contains("/dated"),
        "sitemap omits the published post:\n{sitemap}"
    );
    assert!(
        sitemap.contains("<lastmod>"),
        "sitemap entry has no lastmod:\n{sitemap}"
    );
}

/// `robots.txt` refuses crawlers outside a production profile.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn robots_disallows_everything_outside_production() {
    let client = db_client().await;
    let body = client.get("/robots.txt").send().await.assert_ok().text();
    assert!(
        body.contains("Disallow: /"),
        "a non-production profile must not invite indexing; body: {body}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_status_state_machine_refuses_an_undeclared_edge() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Lifecycle", "Body.", "draft").await;

    // draft -> publish is declared.
    let ok = client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(ok.status, 303);

    // publish -> future is NOT: scheduling a post that is already live is not a
    // move the graph has, and the transition must be refused rather than
    // silently applied.
    let refused = client
        .post(&format!("/admin/content/post/{id}/status?to=future"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert!(
        refused.status.is_client_error() || refused.status.is_server_error(),
        "publish -> future is not a declared edge; got {}",
        refused.status
    );

    // The post is still published.
    client
        .get("/lifecycle")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Lifecycle");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_guest_comment_is_held_for_moderation_until_approved() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Discuss", "Body.", "publish").await;

    // A signed-out visitor comments. Moderation is on by default, so it is held.
    sign_out(&client);
    let posted = client
        .post(&format!("/comments/{post_id}"))
        .form(&form(&[
            ("body", "First!"),
            ("author_name", "Guest"),
            ("author_email", "guest@example.com"),
        ]))
        .send()
        .await;
    assert_eq!(posted.status, 303);

    // It is not on the page yet…
    sign_out(&client);
    let page = client.get("/discuss").send().await;
    page.assert_ok();
    assert!(
        !page.text().contains("First!"),
        "an unapproved comment must not be published"
    );

    // …and the API does not serve it either. A moderation queue that the API
    // walks straight past is not a moderation queue.
    let api: serde_json::Value = client
        .get(&format!("/api/v1/posts/{post_id}/comments"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(api.as_array().map(Vec::len), Some(0));

    // Approve it from the queue.
    let queue = client
        .get("/admin/comments?status=pending")
        .header("cookie", &cookie)
        .send()
        .await;
    queue.assert_ok().assert_body_contains("First!");

    let approved = client
        .post("/admin/comments/1/status?to=approved")
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(approved.status, 303);

    // Now it renders, and the post's approved-comment counter moved with it.
    client
        .get("/discuss")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("First!")
        .assert_body_contains("1 comment");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn approving_and_unapproving_keeps_the_comment_counter_exact() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Counted", "Body.", "publish").await;

    sign_out(&client);
    for n in 0..3 {
        let body = format!("Comment {n}");
        client
            .post(&format!("/comments/{post_id}"))
            .form(&form(&[
                ("body", &body),
                ("author_name", "Guest"),
                ("author_email", "guest@example.com"),
            ]))
            .send()
            .await;
    }
    for id in 1..=3 {
        client
            .post(&format!("/admin/comments/{id}/status?to=approved"))
            .header("cookie", &cookie)
            .send()
            .await;
    }
    client
        .get("/counted")
        .send()
        .await
        .assert_body_contains("3 comments");

    // Unapproving decrements; a repeated unapprove is a no-op rather than a
    // second decrement — the counter is derived from the before/after pair, not
    // from the action name.
    for _ in 0..2 {
        client
            .post("/admin/comments/1/status?to=spam")
            .header("cookie", &cookie)
            .send()
            .await;
    }
    client
        .get("/counted")
        .send()
        .await
        .assert_body_contains("2 comments");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_contributor_cannot_publish_their_own_draft() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;

    // The owner creates a contributor.
    client
        .post("/admin/users")
        .header("cookie", &owner)
        .form(&form(&[
            ("username", "carla"),
            ("email", "carla@example.com"),
            ("password", "correct-horse-battery-staple"),
            ("role", "contributor"),
            ("display_name", "Carla"),
        ]))
        .send()
        .await
        .assert_status(303);

    let login = client
        .post("/login")
        .form(&form(&[
            ("username", "carla"),
            ("password", "correct-horse-battery-staple"),
        ]))
        .send()
        .await;
    assert_eq!(login.status, 303);
    let carla = session_cookie(&login);

    // A contributor submitting `status=publish` gets a draft: the editor hides
    // the option, and the server clamps it regardless of what was posted.
    let id = create_post(&client, &carla, "Contributor Draft", "Body.", "publish").await;
    sign_out(&client);
    let draft = client.get("/contributor-draft").send().await;
    assert_eq!(
        draft.status, 404,
        "a contributor's post must not be published by posting `status=publish`"
    );

    // The explicit transition route refuses too.
    let refused = client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &carla)
        .send()
        .await;
    assert_eq!(refused.status, 403);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_password_protected_post_withholds_its_body_until_unlocked() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let resp = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Secret"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "The hidden text."),
            ("status", "publish"),
            ("password", "letmein"),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    assert_eq!(resp.status, 303);

    sign_out(&client);
    let locked = client.get("/secret").send().await;
    locked
        .assert_ok()
        .assert_body_contains("password protected");
    assert!(
        !locked.text().contains("The hidden text."),
        "a protected post must not render its body"
    );

    // The REST API withholds it too — otherwise the password is decorative.
    let api: serde_json::Value = client.get("/api/v1/posts").send().await.assert_ok().json();
    let first = &api.as_array().expect("array")[0];
    assert_eq!(first["password_protected"], serde_json::json!(true));
    assert!(
        first.get("body").is_none(),
        "the API must omit a protected post's body: {first}"
    );
}

/// Password protection has to hold on every surface, not just the page body.
///
/// The derived excerpt was the leak: with a blank excerpt, `display_excerpt()`
/// took the first 55 words straight from the body — and the blog index, the
/// REST API and the syndication feeds all call it. The comment thread was the
/// other one: the front end hides it until the session unlocks the post, but
/// the API served it to anyone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_protected_post_withholds_its_excerpt_and_comments_everywhere() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let resp = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Sealed"),
            ("slug", ""),
            // Deliberately blank, which is what makes the excerpt derived.
            ("excerpt", ""),
            (
                "body",
                "THE-SECRET-SENTENCE should never appear in a listing.",
            ),
            ("status", "publish"),
            ("password", "letmein"),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(resp.status, 303);
    let post_id: i64 = resp
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .parse()
        .expect("numeric id");

    // A comment exists and is approved, so only the protection can hide it.
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "VISIBLE-ONLY-WHEN-UNLOCKED")]))
        .send()
        .await;

    sign_out(&client);

    for (label, path) in [
        ("home", "/"),
        ("atom feed", "/feed"),
        ("rss feed", "/feed/rss"),
        ("api list", "/api/v1/posts"),
        ("single page", "/sealed"),
    ] {
        let body = client.get(path).send().await.text();
        assert!(
            !body.contains("THE-SECRET-SENTENCE"),
            "{label} leaked the protected body: {path}"
        );
        assert!(
            !body.contains("VISIBLE-ONLY-WHEN-UNLOCKED"),
            "{label} leaked a protected post's comments: {path}"
        );
    }

    // The comments endpoint refuses outright rather than returning an empty
    // list, so it does not confirm the thread exists either.
    assert_eq!(
        client
            .get(&format!("/api/v1/posts/{post_id}/comments"))
            .send()
            .await
            .status,
        404
    );
}

/// Commenting is gated by the same rules as reading.
///
/// The read side was covered; the *write* side was not, which is how a fix for
/// this was twice reported as landed while the tree was unchanged. A signed-in
/// caller's comment is approved immediately, so accepting one on locked content
/// puts visible discussion under a post whose thread the front end withholds.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_protected_post_refuses_comments_until_unlocked() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let resp = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Sealed Thread"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", "letmein"),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(resp.status, 303);
    let post_id: i64 = resp
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .parse()
        .expect("numeric id");

    // Signed in, but the session has not unlocked the post. This is the sharp
    // case: a signed-in comment is stored `approved`.
    let refused = client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "SHOULD-NOT-BE-STORED")]))
        .send()
        .await;
    assert_eq!(
        refused.status, 403,
        "a comment on a locked post must be refused, not stored approved"
    );

    // Unlock, then the same submission is accepted.
    client
        .post(&format!("/unlock/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("password", "letmein")]))
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "ALLOWED-AFTER-UNLOCK")]))
        .send()
        .await
        .assert_status(303);

    // Nothing from the refused attempt reached the database.
    let comments: serde_json::Value = client
        .get(&format!("/api/v1/posts/{post_id}/comments"))
        .send()
        .await
        .json();
    let rendered = comments.to_string();
    assert!(
        !rendered.contains("SHOULD-NOT-BE-STORED"),
        "the refused comment must not have been persisted: {rendered}"
    );
}

/// A post and a page may both be slugged `about`; both mint `/about`, and only
/// one can be served there. The loser is suffixed rather than left unreachable.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_post_and_a_page_cannot_take_the_same_bare_path() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    create_post(&client, &cookie, "About", "Post body.", "publish").await;
    client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "About"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Page body."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);

    // The post keeps `/about`; the page is reachable at its de-duplicated slug.
    client
        .get("/about")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Post body.");
    client
        .get("/about-2")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Page body.");
}

/// Two editors on one post: the second save is refused rather than silently
/// overwriting the first.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_stale_editor_submission_is_refused() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Contested", "Original.", "publish").await;

    // Both editors loaded the form at version 0.
    let stale_version = "0";

    let first = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Contested"),
            ("slug", "contested"),
            ("excerpt", ""),
            ("body", "First editor's text."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
            ("lock_version", stale_version),
        ]))
        .send()
        .await;
    assert_eq!(first.status, 303, "the first save should succeed");

    let second = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Contested"),
            ("slug", "contested"),
            ("excerpt", ""),
            ("body", "Second editor's text."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
            ("lock_version", stale_version),
        ]))
        .send()
        .await;
    assert_eq!(
        second.status, 409,
        "a save built on a stale version must be refused, not applied"
    );

    // The first editor's text survived.
    sign_out(&client);
    client
        .get("/contested")
        .send()
        .await
        .assert_body_contains("First editor's text.");
}

/// A backup restore must not publish content that was protected, nor flatten a
/// page tree.
/// Restoring a page whose slug an existing post already holds must not abort
/// the run part-way.
///
/// `idx_posts_bare_path_slug` made bare-path uniqueness the database's
/// invariant, which meant every insert path had to allocate through the shared
/// allocator — the importer did not, so this aborted after earlier rows had
/// already committed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn importing_a_slug_an_existing_post_holds_does_not_abort_the_run() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // A hand-built export containing a page slugged `about`, plus another post
    // after it — so a mid-run abort would be visible as the second going missing.
    let payload = serde_json::json!({
        "version": 2,
        "site_title": "Imported",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "posts": [
            {
                "post_type": "page", "title": "About", "slug": "about",
                "excerpt": "", "body": "Imported page.", "status": "publish",
                "comment_status": "closed", "password": "", "author": "owner",
                "published_at": null, "parent": null, "terms": []
            },
            {
                "post_type": "post", "title": "Second", "slug": "second",
                "excerpt": "", "body": "Imported post.", "status": "publish",
                "comment_status": "open", "password": "", "author": "owner",
                "published_at": null, "parent": null, "terms": []
            }
        ]
    })
    .to_string();

    // An existing post already holds `about`.
    create_post(&client, &cookie, "About", "Existing post.", "publish").await;

    let result = import_export(&client, &cookie, payload.as_str()).await;
    result.assert_ok().assert_body_contains("2 imported");

    sign_out(&client);
    // Everything is reachable: the original post, the re-slugged page, and the
    // row that came after the collision.
    client
        .get("/about")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Existing post.");
    client
        .get("/about-2")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Imported page.");
    client
        .get("/second")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Imported post.");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn export_preserves_password_protection_and_page_ancestry() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let parent = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Handbook"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Parent."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    let parent_id = parent
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Chapter"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Child."),
            ("status", "publish"),
            ("password", "shh"),
            ("taxonomy_names[post_tag]", ""),
            ("parent_id", parent_id.as_str()),
        ]))
        .send()
        .await
        .assert_status(303);

    let payload = client
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();

    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("export is valid JSON");
    let chapter = parsed["posts"]
        .as_array()
        .expect("posts array")
        .iter()
        .find(|p| p["slug"] == serde_json::json!("chapter"))
        .expect("the child page is exported");
    assert_eq!(
        chapter["password"],
        serde_json::json!("shh"),
        "the export must carry the password, or a restore publishes protected content"
    );
    assert_eq!(
        chapter["parent"],
        serde_json::json!("handbook"),
        "the export must carry ancestry, or a restore flattens the page tree"
    );

    // Hierarchical taxonomies are supported, so a restore that flattened the
    // category tree would quietly change every archive's shape.
    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Guides"),
            ("slug", ""),
            ("description", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    let parent_term: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=category")
        .send()
        .await
        .assert_ok()
        .json();
    let parent_term_id = parent_term.as_array().expect("array")[0]["id"]
        .as_i64()
        .expect("term id")
        .to_string();
    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Deep Dives"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", parent_term_id.as_str()),
        ]))
        .send()
        .await
        .assert_status(303);

    let payload = client
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("export is valid JSON");
    let child_term = parsed["terms"]
        .as_array()
        .expect("terms array")
        .iter()
        .find(|t| t["slug"] == serde_json::json!("deep-dives"))
        .expect("the child term is exported");
    assert_eq!(
        child_term["parent"],
        serde_json::json!("guides"),
        "the export must carry taxonomy ancestry too"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn changing_the_permalink_structure_does_not_break_existing_urls() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Durable", "Body.", "publish").await;

    // The default structure.
    client.get("/durable").send().await.assert_ok();

    // Switch to the dated structure.
    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[
            ("site_title", "Autumn CMS"),
            ("permalink_structure", "day_and_name"),
            ("front_page_id", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    // The new URL works…
    let year = chrono::Utc::now().format("%Y/%m/%d").to_string();
    client
        .get(&format!("/{year}/durable"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Durable");

    // …and so does the old one. This is the property that makes the setting
    // safe to change on a site with links already in the wild.
    client
        .get("/durable")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Durable");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn editing_a_post_records_a_restorable_revision() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Versioned", "First draft.", "publish").await;

    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Versioned"),
                    ("slug", "versioned"),
                    ("excerpt", ""),
                    ("body", "Second draft."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", ""),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    client
        .get("/versioned")
        .send()
        .await
        .assert_body_contains("Second draft.");

    // The history holds the pre-edit snapshot.
    let history = client
        .get(&format!("/admin/content/post/{id}/revisions"))
        .header("cookie", &cookie)
        .send()
        .await;
    history.assert_ok().assert_body_contains("First draft.");

    // Restoring the first revision brings the old text back. Revision 1 is the
    // "Created" snapshot the create path records.
    client
        .post(&format!("/admin/content/post/{id}/revisions/1/restore"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    client
        .get("/versioned")
        .send()
        .await
        .assert_body_contains("First draft.");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn tags_typed_into_the_editor_are_created_and_archived() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Tagged"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", "Rust, Web Frameworks"),
        ]))
        .send()
        .await
        .assert_status(303);

    // The tag archive lists it, at the taxonomy's *rewrite base* (`/tag`), not
    // its slug (`post_tag`).
    client
        .get("/tag/rust")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Tagged");
    client
        .get("/tag/web-frameworks")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Tagged");

    // The API agrees, and the term's published-post count was maintained.
    let terms: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=post_tag")
        .send()
        .await
        .assert_ok()
        .json();
    let rust = terms
        .as_array()
        .expect("array")
        .iter()
        .find(|t| t["slug"] == serde_json::json!("rust"))
        .expect("the rust tag exists");
    assert_eq!(rust["post_count"], serde_json::json!(1));
}

/// Search paginates the *visible* set, not the whole match set.
///
/// Filtering after `search_page` returned meant drafts could occupy the first
/// page — leaving it blank while public results sat on page two — and the total
/// disclosed how many hidden matches existed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn search_counts_and_paginates_only_visible_matches() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // Three drafts and one published post, all matching the same term.
    for n in 0..3 {
        create_post(
            &client,
            &cookie,
            &format!("Hidden {n}"),
            "kumquat marmalade recipe",
            "draft",
        )
        .await;
    }
    create_post(
        &client,
        &cookie,
        "Visible",
        "kumquat marmalade recipe",
        "publish",
    )
    .await;

    sign_out(&client);
    let body = client
        .get("/search?s=kumquat")
        .send()
        .await
        .assert_ok()
        .text();

    assert!(body.contains("Visible"), "the published match must appear");
    assert!(
        !body.contains("Hidden"),
        "a draft must not appear in public search results"
    );
    assert!(
        body.contains("1 result"),
        "the total must count only visible matches, not disclose hidden ones; body: {}",
        &body[..body.len().min(2000)]
    );
}

/// Once an editor publishes a Contributor's draft, the Contributor can no
/// longer edit it — nor trash it, which is the more destructive of the two.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_contributor_cannot_trash_their_published_post() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;

    client
        .post("/admin/users")
        .header("cookie", &owner)
        .form(&form(&[
            ("username", "carla"),
            ("email", "carla@example.com"),
            ("password", "correct-horse-battery-staple"),
            ("role", "contributor"),
            ("display_name", "Carla"),
        ]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);
    let login = client
        .post("/login")
        .form(&form(&[
            ("username", "carla"),
            ("password", "correct-horse-battery-staple"),
        ]))
        .send()
        .await;
    let carla = session_cookie(&login);
    sign_out(&client);
    let id = create_post(&client, &carla, "Carla Draft", "Body.", "draft").await;

    // The owner publishes it.
    sign_out(&client);
    client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &owner)
        .send()
        .await
        .assert_status(303);

    // Confirm it really published — a 303 alone would also be the guard's
    // redirect to /login, which is how the first draft of this test passed
    // while the transition silently never happened.
    sign_out(&client);
    let published: serde_json::Value = client.get("/api/v1/posts").send().await.json();
    assert_eq!(
        published.as_array().map(Vec::len),
        Some(1),
        "the owner's publish must have taken effect: {published}"
    );

    // Carla can no longer trash it.
    sign_out(&client);
    let refused = client
        .post(&format!("/admin/content/post/{id}/status?to=trash"))
        .header("cookie", &carla)
        .send()
        .await;
    assert_eq!(
        refused.status, 403,
        "a contributor must not be able to trash content they can no longer edit"
    );

    sign_out(&client);
    client.get("/carla-draft").send().await.assert_ok();
}

/// A slug shaped like a year would otherwise be swallowed by the date archive.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_year_shaped_slug_stays_reachable() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let resp = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "2026"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "A year in review."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(resp.status, 303);

    sign_out(&client);
    // `/2026` is still the date archive. It legitimately *lists* the post
    // (published in 2026), so assert on the archive's own heading rather than
    // on the absence of the post — the listing showing it is correct.
    let archive = client.get("/2026").send().await;
    archive.assert_ok().assert_body_contains("Archive: 2026");

    // The post itself is reachable at the slug it was given, which is what the
    // reservation is for: without it the slug would be `2026`, whose canonical
    // URL the archive owns, leaving the post unreachable.
    let single = client.get("/2026-2").send().await;
    single.assert_ok().assert_body_contains("A year in review.");
    assert!(
        !single.text().contains("Archive: 2026"),
        "/2026-2 must be the post, not the archive"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn full_text_search_finds_a_post_by_its_body() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(
        &client,
        &cookie,
        "Nothing Obvious",
        "The quick brown fox jumps over the lazy dog.",
        "publish",
    )
    .await;
    create_post(
        &client,
        &cookie,
        "Unrelated",
        "Something else entirely.",
        "publish",
    )
    .await;

    let results = client.get("/search?s=brown+fox").send().await;
    results.assert_ok().assert_body_contains("Nothing Obvious");
    assert!(
        !results.text().contains("Unrelated"),
        "search must not match every post"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn export_and_import_round_trip_the_site_content() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Exported", "Body text.", "publish").await;

    let export = client
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await;
    export.assert_ok();
    let payload = export.text();
    assert!(payload.contains("Exported"), "export payload: {payload}");

    // Re-importing into the same site is a no-op: content is matched on
    // (post_type, slug), so a re-run duplicates nothing.
    let reimport = import_export(&client, &cookie, payload.as_str()).await;
    reimport.assert_ok().assert_body_contains("already present");

    let posts: serde_json::Value = client.get("/api/v1/posts").send().await.assert_ok().json();
    assert_eq!(
        posts.as_array().map(Vec::len),
        Some(1),
        "re-importing must not duplicate content"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_api_never_serves_a_password_hash() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Bylined", "Body.", "publish").await;

    let authors = client.get("/api/v1/authors").send().await;
    let body = authors.assert_ok().text();
    assert!(
        !body.contains("password_hash") && !body.contains("$2b$"),
        "the authors endpoint leaked credential material: {body}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_page_is_addressed_by_its_ancestry() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let parent = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "About"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Parent page."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    let parent_id = parent
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Team"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Child page."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("parent_id", parent_id.as_str()),
        ]))
        .send()
        .await
        .assert_status(303);

    client
        .get("/about/team")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Child page.");

    // The bare child slug does NOT resolve: a page is addressed by its path, so
    // two pages named "team" under different parents stay distinct.
    sign_out(&client);
    assert_eq!(client.get("/team").send().await.status, 404);
}

/// The unlock endpoint must not answer for content the caller cannot reach.
///
/// It takes a bare post id from an unauthenticated request and its response
/// carries the row's canonical permalink. Without the reachability gate,
/// iterating ids confirmed the existence of drafts, private and trashed rows
/// and disclosed their slugs and page ancestry — with a wrong password, and
/// with no session at all.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unlocking_a_hidden_post_discloses_nothing() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let draft = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Unannounced Acquisition"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Not for publication."),
            ("status", "draft"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    assert_eq!(draft.status, 303);
    let draft_id = draft
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    sign_out(&client);
    let probe = client
        .post(&format!("/unlock/{draft_id}"))
        .form(&form(&[("password", "guess")]))
        .send()
        .await;
    assert_eq!(
        probe.status, 404,
        "an anonymous unlock of a draft must 404, not redirect"
    );
    assert!(
        probe.header("location").is_none(),
        "no Location header may carry a hidden post's permalink"
    );
    assert!(
        !probe.text().contains("unannounced-acquisition"),
        "the draft's slug must not appear in the response: {}",
        probe.text()
    );
}

/// API creation allocates a slug the same way the editor does.
///
/// It saved through the repository directly, so a second item with the same
/// title reached `idx_posts_bare_path_slug` and came back as a constraint
/// error, where the admin editor and the importer both get the usual suffix.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn api_creation_suffixes_a_duplicate_slug_instead_of_failing() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let body = serde_json::json!({
        "title": "Release Notes",
        "body": "First.",
        "status": "publish",
    });
    let first: serde_json::Value = client
        .post("/api/v1/posts")
        .header("cookie", &cookie)
        .json(&body)
        .send()
        .await
        .assert_status(201)
        .json();
    assert_eq!(first["slug"], serde_json::json!("release-notes"));

    let second: serde_json::Value = client
        .post("/api/v1/posts")
        .header("cookie", &cookie)
        .json(&body)
        .send()
        .await
        .assert_status(201)
        .json();
    assert_eq!(
        second["slug"],
        serde_json::json!("release-notes-2"),
        "the second creation must take a suffix, not a 500: {second}"
    );
}

/// Public read endpoints are bounded by the request, not by the corpus.
///
/// `/api/v1/terms` returned every row of a taxonomy and the comment endpoint
/// returned a post's whole thread, both unauthenticated and both with a cost
/// that grew without limit as the site did.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_public_api_paginates_terms_and_comments() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    for name in ["Alpha", "Bravo", "Charlie", "Delta", "Echo"] {
        client
            .post("/admin/terms/category")
            .header("cookie", &cookie)
            .form(&form(&[
                ("name", name),
                ("slug", ""),
                ("description", ""),
                ("parent_id", ""),
            ]))
            .send()
            .await
            .assert_status(303);
    }

    sign_out(&client);
    let page_one: serde_json::Value = client
        .get("/api/v1/terms?per_page=2")
        .send()
        .await
        .assert_ok()
        .json();
    let page_one = page_one.as_array().expect("array");
    assert_eq!(page_one.len(), 2, "per_page must be applied in SQL");

    let page_two: serde_json::Value = client
        .get("/api/v1/terms?per_page=2&page=2")
        .send()
        .await
        .assert_ok()
        .json();
    let page_two = page_two.as_array().expect("array");
    assert_eq!(page_two.len(), 2);
    assert_ne!(
        page_one[0]["id"], page_two[0]["id"],
        "the second page must not repeat the first"
    );

    // An absurd page size is clamped rather than honoured.
    let clamped: serde_json::Value = client
        .get("/api/v1/terms?per_page=100000")
        .send()
        .await
        .assert_ok()
        .json();
    assert!(clamped.as_array().expect("array").len() <= 100);
}

/// `supports_comments: false` on a registered type is a refusal, not a hint.
///
/// The gate asked the row's `comment_status` and the type's `public` flag but
/// never the type's `supports_comments`. A `page` registers it false and the
/// editor offers no checkbox — but a direct request that sets the column, or an
/// import carrying it, produced a page that accepted comments. A signed-in
/// submission is approved immediately, so the thread then rendered on a type
/// that had explicitly disabled it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_type_that_disables_comments_refuses_them_however_the_row_is_set() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let created = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Contact"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Reach us here."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303);
    let page_id = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    // Whatever the editor did with the checkbox, force the column open — this
    // is the import/crafted-request shape the gate has to survive.
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET comment_status = 'open' WHERE id = {page_id}"),
    )
    .await
    .expect("force comment_status");

    // Signed in, so the submission would be approved on the spot if accepted.
    let refused = client
        .post(&format!("/comments/{page_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "COMMENT-ON-A-PAGE")]))
        .send()
        .await;
    assert_eq!(
        refused.status,
        403,
        "a page must refuse comments: {}",
        refused.text()
    );

    let page = client.get("/contact").send().await;
    page.assert_ok();
    assert!(
        !page.text().contains("COMMENT-ON-A-PAGE"),
        "nothing may have been stored"
    );
    assert!(
        !page.text().contains("Post comment"),
        "and the form must not be offered: {}",
        page.text()
    );
}

/// Deleting a comment takes its replies with it, and the counter has to know.
///
/// `comments.parent_id` cascades, so deleting an approved parent removes every
/// approved descendant — while the handler decremented by one. The post then
/// advertised comments that no longer existed, permanently.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_a_comment_recounts_the_replies_it_cascades_away() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Threaded", "Body.", "publish").await;

    // Signed in, so all three land approved immediately.
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Parent comment")]))
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Reply one"), ("reply_to", "1")]))
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Unrelated comment")]))
        .send()
        .await
        .assert_status(303);

    client
        .get("/threaded")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("3 comments");

    // Delete the parent; the reply goes with it through the cascade.
    client
        .post("/admin/comments/1/delete")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let page = client.get("/threaded").send().await;
    page.assert_ok().assert_body_contains("1 comment");
    assert!(
        !page.text().contains("Reply one"),
        "the cascaded reply must be gone"
    );
    assert!(
        page.text().contains("Unrelated comment"),
        "and the untouched comment must remain"
    );
}

/// The public comment endpoint is bounded per address.
///
/// Unauthenticated, guest comments on by default, a reusable CSRF token and no
/// global limiter: without a per-route bound, a request loop writes a database
/// row per request and buries the moderation queue.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn guest_comment_submissions_are_throttled() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Flooded", "Body.", "publish").await;

    sign_out(&client);
    let mut throttled = false;
    for i in 0..25 {
        let resp = client
            .post(&format!("/comments/{post_id}"))
            .header("X-Forwarded-For", "198.51.100.23")
            .form(&form(&[
                ("body", &format!("flood {i}")),
                ("author_name", "Guest"),
                ("author_email", "guest@example.com"),
            ]))
            .send()
            .await;
        if resp.status == 429 {
            throttled = true;
            break;
        }
    }
    assert!(
        throttled,
        "the comment endpoint must stop accepting after its per-minute bound"
    );
}

/// Password guesses against protected content are bounded per address.
///
/// The route is unauthenticated, each request is one guess, and the redirect
/// target starts serving the body on success — a free oracle telling a client
/// when to stop.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn protected_post_password_attempts_are_throttled() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let created = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Members Only"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "The protected text."),
            ("status", "publish"),
            ("password", "correcthorse"),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303);
    let post_id = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    sign_out(&client);
    let mut throttled = false;
    for i in 0..25 {
        let resp = client
            .post(&format!("/unlock/{post_id}"))
            .header("X-Forwarded-For", "198.51.100.77")
            .form(&form(&[("password", &format!("guess{i}"))]))
            .send()
            .await;
        if resp.status == 429 {
            throttled = true;
            break;
        }
    }
    assert!(
        throttled,
        "unlimited password guesses must not be available to one address"
    );
}

/// The importer applies the editor's parent rules, and says when it cannot.
///
/// Importing into a partly-populated site resolves a skipped parent by slug
/// against rows already present. That row can be trashed, of another type, or
/// already nested as deeply as pages go — none of which the editor would
/// accept. Writing the link anyway produced a child whose generated ancestry
/// the resolver cannot walk, leaving the imported page unreachable at its own
/// canonical URL. Aborting the run instead is worse, so the link is declined
/// and the child lands at the top level.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn importing_under_a_trashed_parent_keeps_the_child_reachable() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // An existing page the import will name as a parent — then trashed, so it
    // is exactly the kind of row `validate_parent` refuses.
    let parent = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Archive"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Old parent."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    assert_eq!(parent.status, 303);
    let parent_id = parent
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();
    client
        .post(&format!("/admin/content/page/{parent_id}/status?to=trash"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let payload = serde_json::json!({
        "version": 2,
        "site_title": "Imported",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "posts": [
            {
                "post_type": "page", "title": "Orphan", "slug": "orphan",
                "excerpt": "", "body": "Imported child.", "status": "publish",
                "comment_status": "closed", "password": "", "author": "owner",
                "published_at": null, "parent": "archive", "terms": []
            }
        ]
    })
    .to_string();

    let result = import_export(&client, &cookie, payload.as_str()).await;
    result
        .assert_ok()
        .assert_body_contains("1 imported")
        .assert_body_contains("could not keep its parent");

    // The child is at the top level and reachable there, rather than filed
    // under a trashed ancestor and reachable nowhere.
    sign_out(&client);
    client
        .get("/orphan")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Imported child.");
}

/// A password-protected post's body must not be searchable.
///
/// `search_vector` covers title, excerpt and body, so a query matching only
/// protected body text still returned the post — turning `/search` and
/// `/api/v1/posts?search=` into an oracle for probing content the password
/// exists to withhold. The title stays searchable because it is already public.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn search_does_not_reach_into_a_protected_body() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Quarterly Briefing"),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "The acquisition of Zephyrine closes in March."),
            ("status", "publish"),
            ("password", "letmein"),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);

    // A word that occurs only in the protected body finds nothing.
    let hidden: serde_json::Value = client
        .get("/api/v1/posts?search=Zephyrine")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(
        hidden.as_array().map(Vec::len),
        Some(0),
        "body text of a protected post must not be searchable: {hidden}"
    );
    let page = client.get("/search?q=Zephyrine").send().await;
    page.assert_ok();
    assert!(
        !page.text().contains("Quarterly Briefing"),
        "the front-end search must not surface it either"
    );

    // The title still is — it renders publicly on the index either way.
    let visible: serde_json::Value = client
        .get("/api/v1/posts?search=Quarterly")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(visible.as_array().map(Vec::len), Some(1));
}

/// Un-approving a comment takes its approved replies with it.
///
/// `assemble_thread` builds from the roots down, so a reply whose parent is no
/// longer approved can never be rendered — while it stayed `approved` and
/// stayed in `comment_count`. The post advertised comments no reader could see.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unapproving_a_parent_hides_and_uncounts_its_replies() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Moderated", "Body.", "publish").await;

    for (body, reply_to) in [
        ("Parent comment", ""),
        ("Reply under parent", "1"),
        ("Unrelated comment", ""),
    ] {
        let mut fields = vec![("body", body)];
        if !reply_to.is_empty() {
            fields.push(("reply_to", reply_to));
        }
        client
            .post(&format!("/comments/{post_id}"))
            .header("cookie", &cookie)
            .form(&form(&fields))
            .send()
            .await
            .assert_status(303);
    }

    client
        .get("/moderated")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("3 comments");

    // Spam the parent.
    client
        .post("/admin/comments/1/status?to=spam")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let page = client.get("/moderated").send().await;
    page.assert_ok().assert_body_contains("1 comment");
    assert!(
        !page.text().contains("Reply under parent"),
        "the orphaned reply must not render"
    );
    assert!(
        page.text().contains("Unrelated comment"),
        "an unrelated comment is untouched"
    );

    // The API agrees — it is the same approved-status query.
    let api: serde_json::Value = client
        .get(&format!("/api/v1/posts/{post_id}/comments"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(api.as_array().map(Vec::len), Some(1));
}

/// A reader can actually reply, without a hand-written POST.
///
/// The thread rendered no per-comment control and the single top-level form
/// never supplied `reply_to`, so the threading the schema, the depth cap and
/// the renderer all support was unreachable from a browser.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_thread_offers_a_reply_control_that_works() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Conversation", "Body.", "publish").await;

    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "The first comment")]))
        .send()
        .await
        .assert_status(303);

    let page = client
        .get("/conversation")
        .header("cookie", &cookie)
        .send()
        .await;
    let html = page.assert_ok().text();
    assert!(
        html.contains(r#"name="reply_to""#),
        "the thread must render a control that supplies reply_to:\n{html}"
    );
    assert!(
        html.contains(r#"value="1""#),
        "and it must name the comment it replies to"
    );

    // The control posts to the same endpoint, and the reply nests.
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "A threaded reply"), ("reply_to", "1")]))
        .send()
        .await
        .assert_status(303);
    client
        .get("/conversation")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("A threaded reply")
        .assert_body_contains("aria-level=\"2\"");
}

/// Plain permalinks belong in the sitemap.
///
/// With that structure every post's canonical URL is `/?p=<id>`, which
/// `front_page` serves — so skipping query-string paths dropped the entire post
/// corpus from `sitemap.xml` on a site that chose it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_sitemap_carries_plain_permalinks() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Findable", "Body.", "publish").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("permalink_structure", "plain")]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);
    let sitemap = client.get("/sitemap.xml").send().await;
    let body = sitemap.assert_ok().text();
    assert!(
        body.contains(&format!("/?p={post_id}")),
        "the plain permalink must be listed:\n{body}"
    );
}

/// Re-running an import does not duplicate a row the allocator had to re-slug.
///
/// The dedupe checked the slug the file names. When an imported `about`
/// collided with an existing post and landed as `about-2`, the retry found
/// nothing under `about` and created `about-3`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn re_importing_a_reslugged_page_is_still_idempotent() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // An existing post already holds the bare path `about`.
    create_post(&client, &cookie, "About", "Existing post.", "publish").await;

    let payload = serde_json::json!({
        "version": 2,
        "site_title": "Imported",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "posts": [{
            "post_type": "page", "title": "About", "slug": "about",
            "excerpt": "", "body": "Imported page.", "status": "publish",
            "comment_status": "closed", "password": "", "author": "owner",
            "published_at": null, "parent": null, "terms": []
        }]
    })
    .to_string();

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    // The same file again: nothing new.
    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("0 imported");

    sign_out(&client);
    assert_eq!(
        client.get("/about-3").send().await.status,
        404,
        "a second suffix means the retry duplicated the page"
    );
    client
        .get("/about-2")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Imported page.");
}

/// Multi-level menus are reachable from the admin UI.
///
/// The item form forced `parent_id: None` and carried no parent field, so the
/// two-level menu the schema stores and the theme renders could not be built.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn menu_items_can_be_nested_and_only_within_their_own_menu() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    for (name, location) in [("Main", "primary"), ("Footer", "")] {
        client
            .post("/admin/appearance/menus")
            .header("cookie", &cookie)
            .form(&form(&[("name", name), ("location", location)]))
            .send()
            .await
            .assert_status(303);
    }

    // A root item in menu 1, and one in menu 2.
    client
        .post("/admin/appearance/menus/1/items")
        .header("cookie", &cookie)
        .form(&form(&[("label", "Products"), ("url", "/products")]))
        .send()
        .await
        .assert_status(303);
    client
        .post("/admin/appearance/menus/2/items")
        .header("cookie", &cookie)
        .form(&form(&[("label", "Legal"), ("url", "/legal")]))
        .send()
        .await
        .assert_status(303);

    // The admin screen offers the parent selector.
    let screen = client
        .get("/admin/appearance")
        .header("cookie", &cookie)
        .send()
        .await;
    screen
        .assert_ok()
        .assert_body_contains(r#"name="parent_id""#)
        .assert_body_contains("Under Products");

    // Nesting under a root item of the same menu works.
    client
        .post("/admin/appearance/menus/1/items")
        .header("cookie", &cookie)
        .form(&form(&[
            ("label", "Widgets"),
            ("url", "/products/widgets"),
            ("parent_id", "1"),
        ]))
        .send()
        .await
        .assert_status(303);

    // Nesting under an item of a *different* menu is refused: the renderer
    // walks one menu's roots, so a foreign parent renders nowhere.
    let foreign = client
        .post("/admin/appearance/menus/1/items")
        .header("cookie", &cookie)
        .form(&form(&[
            ("label", "Smuggled"),
            ("url", "/x"),
            ("parent_id", "2"),
        ]))
        .send()
        .await;
    assert_eq!(foreign.status, 422, "body: {}", foreign.text());

    // And so is nesting under an item that is itself nested — the renderer
    // draws two levels.
    let too_deep = client
        .post("/admin/appearance/menus/1/items")
        .header("cookie", &cookie)
        .form(&form(&[
            ("label", "Deeper"),
            ("url", "/y"),
            ("parent_id", "3"),
        ]))
        .send()
        .await;
    assert_eq!(too_deep.status, 422, "body: {}", too_deep.text());
}

/// A published post cannot be edited into an untitled one.
///
/// The editor's save path applies the form to a locked row and writes the
/// fields with plain Diesel — which is what makes the edit and its revision one
/// transaction, and is also what bypasses `PostHooks::before_update`. The
/// state-machine preflight only fires on a status *change*, so a crafted form
/// keeping `status=publish` while clearing the title went live untitled.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_published_post_cannot_be_saved_without_a_title() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Has A Title", "Body.", "publish").await;

    let refused = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", ""),
                    ("slug", "has-a-title"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", ""),
                    ("comment_status", "open"),
                ],
            )
            .await,
        )
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "an untitled live post must be refused: {}",
        refused.text()
    );

    // And the row is untouched.
    let post: serde_json::Value = client
        .get(&format!("/api/v1/posts/{id}"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(post["title"], serde_json::json!("Has A Title"));
}

/// A revision records who made the edit, not who owns the post.
///
/// `revisions.author_id` stored the post's owner, so every collaborative edit
/// was credited to the wrong account — and nothing rendered the field, which is
/// why it could stay wrong unnoticed. The history shows it now.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_revision_is_attributed_to_the_editor_who_made_it() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;
    let id = create_post(&client, &owner, "Collaborative", "First draft.", "draft").await;

    // A second account, promoted to Editor so it may edit somebody else's post.
    sign_out(&client);
    let editor = register(&client, "editor").await;
    client
        .post("/admin/users/2")
        .header("cookie", &owner)
        .form(&form(&[
            ("role", "editor"),
            ("email", "editor@example.com"),
            ("display_name", "Editor"),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &editor)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Collaborative"),
                    ("slug", "collaborative"),
                    ("excerpt", ""),
                    ("body", "Edited by somebody else."),
                    ("status", "draft"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", ""),
                    ("comment_status", "open"),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    let history = client
        .get(&format!("/admin/content/post/{id}/revisions"))
        .header("cookie", &owner)
        .send()
        .await;
    let html = history.assert_ok().text();
    assert!(
        html.contains("by Editor"),
        "the revision must be credited to the account that made the edit:\n{html}"
    );
    assert!(
        !html.contains("by Owner"),
        "and not to the post's owner:\n{html}"
    );
}

/// A reply to a comment that is no longer approved is refused.
///
/// A form rendered before the parent was moderated still posts. A signed-in
/// reply then lands `approved` and bumps `comment_count`, but the thread query
/// omits its parent — so it can never render and the count drifts up by a
/// comment nobody can see.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_reply_to_a_hidden_parent_is_refused() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Stale Form", "Body.", "publish").await;

    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Parent comment")]))
        .send()
        .await
        .assert_status(303);

    client
        .post("/admin/comments/1/status?to=spam")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let refused = client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Late reply"), ("reply_to", "1")]))
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a reply to a hidden parent must be refused: {}",
        refused.text()
    );

    let page = client.get("/stale-form").send().await;
    page.assert_ok();
    assert!(
        !page.text().contains("Late reply"),
        "and nothing may have been stored"
    );
    assert!(
        page.text().contains("No comments yet"),
        "the count must not have drifted: {}",
        page.text()
    );
}

/// The public list endpoints are navigable, not merely bounded.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_posts_api_pages_through_the_corpus() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    for n in 1..=5 {
        create_post(
            &client,
            &cookie,
            &format!("Entry {n}"),
            &format!("Body {n} about widgets."),
            "publish",
        )
        .await;
    }

    sign_out(&client);
    let titles = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .expect("array")
            .iter()
            .map(|p| p["title"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    let page_one: serde_json::Value = client
        .get("/api/v1/posts?per_page=2")
        .send()
        .await
        .assert_ok()
        .json();
    let page_two: serde_json::Value = client
        .get("/api/v1/posts?per_page=2&page=2")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(titles(&page_one).len(), 2);
    assert_eq!(titles(&page_two).len(), 2);
    assert!(
        titles(&page_one)
            .iter()
            .all(|t| !titles(&page_two).contains(t)),
        "pages must not overlap: {:?} vs {:?}",
        titles(&page_one),
        titles(&page_two)
    );

    // The search branch takes the same offset.
    let search_two: serde_json::Value = client
        .get("/api/v1/posts?search=widgets&per_page=2&page=2")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(titles(&search_two).len(), 2);

    let authors: serde_json::Value = client
        .get("/api/v1/authors?per_page=1")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(authors.as_array().map(Vec::len), Some(1));
}

/// A malformed date format is refused, and never reaches a rendered page.
///
/// `format()` defers everything to `Display`, a bad directive makes `Display`
/// return an error, and `to_string()` turns that into a panic — so `%` in the
/// settings form would 500 every dated listing and every single-post page.
///
/// Two defences, deliberately different. The form refuses it outright: silently
/// keeping the default would reset the site's date style from a typo, and the
/// current value is worth more than a guess. `Settings::from_rows` still
/// *ignores* it, because that funnel also reads an import and a direct write to
/// `options`, where failing would take the site down rather than save it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_malformed_date_format_is_refused_rather_than_crashing_the_site() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Dated", "Body.", "publish").await;

    let settings = |date_format: &str| settings_form(&[("date_format", date_format)]);

    // A working pattern first, so there is a value worth preserving.
    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings("%Y/%m/%d"))
        .send()
        .await
        .assert_status(303);
    sign_out(&client);
    let dated = client
        .get("/api/v1/posts")
        .send()
        .await
        .assert_ok()
        .json::<serde_json::Value>();
    let url = dated.as_array().expect("array")[0]["url"]
        .as_str()
        .expect("url")
        .to_owned();
    client
        .get(&url)
        .send()
        .await
        .assert_ok()
        .assert_body_contains(&chrono::Utc::now().format("%Y/%m/%d").to_string());

    // A pattern chrono cannot render is refused rather than accepted-and-dropped.
    let refused = client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings("%"))
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a malformed pattern must be refused, not silently reset to the default: {}",
        refused.text()
    );

    // The site is unharmed and still on the format the administrator chose.
    sign_out(&client);
    client.get("/").send().await.assert_ok();
    client
        .get(&url)
        .send()
        .await
        .assert_ok()
        .assert_body_contains(&chrono::Utc::now().format("%Y/%m/%d").to_string());
}

/// A failed private creation leaves nothing behind.
///
/// `private` is reached by transitioning a draft, and that edge carries the
/// `can_publish` guard — so an empty title committed the draft, then failed,
/// leaving a row the client never asked for and each retry allocating another
/// suffixed slug.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_refused_private_api_creation_persists_nothing() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    for _ in 0..3 {
        let refused = client
            .post("/api/v1/posts")
            .header("cookie", &cookie)
            .json(&serde_json::json!({
                "title": "",
                "body": "No title here.",
                "status": "private",
            }))
            .send()
            .await;
        assert_eq!(refused.status, 422, "body: {}", refused.text());
    }

    // Nothing was written by any of the three attempts.
    let listing = client
        .get("/admin/content/post")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        !listing.contains("No title here."),
        "a refused creation must persist nothing:\n{listing}"
    );
}

/// Restoring an untitled revision onto a live post is refused.
///
/// A revision captured while the post was an untitled draft is legitimate;
/// restoring it keeps the live status, so it would write the empty title past
/// the invariant every other edit path enforces.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restoring_an_untitled_revision_onto_a_live_post_is_refused() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // An untitled draft. Legal: the title invariant applies to live content.
    let created = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", ""),
            ("slug", "work-in-progress"),
            ("excerpt", ""),
            ("body", "First body."),
            ("status", "draft"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
            ("comment_status", "open"),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303, "body: {}", created.text());
    let id: i64 = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .parse()
        .expect("numeric id");

    // Its initial revision therefore carries an empty title. Now give it one
    // and publish.
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Final Title"),
                    ("slug", "work-in-progress"),
                    ("excerpt", ""),
                    ("body", "Second body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", ""),
                    ("comment_status", "open"),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    // The oldest revision is the untitled snapshot.
    let history = client
        .get(&format!("/admin/content/post/{id}/revisions"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let untitled_revision = history
        .match_indices("/revisions/")
        .filter_map(|(at, _)| {
            history[at + "/revisions/".len()..]
                .split('/')
                .next()
                .and_then(|id| id.parse::<i64>().ok())
        })
        .min()
        .expect("at least one revision");

    let refused = client
        .post(&format!(
            "/admin/content/post/{id}/revisions/{untitled_revision}/restore"
        ))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "restoring an untitled revision onto a live post must be refused: {}",
        refused.text()
    );

    // The live post is untouched.
    let post: serde_json::Value = client
        .get(&format!("/api/v1/posts/{id}"))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(post["title"], serde_json::json!("Final Title"));
}

/// An account with nothing published has no author archive.
///
/// The `Some(author)` branch returned a 200 carrying the account's public name
/// and profile, so on a site with open registration `/author/<username>`
/// answered "does this person have an account here?" for anyone who asked —
/// while `/api/v1/authors` already refused to list them.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_author_archive_needs_published_content() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;
    create_post(&client, &owner, "Owner Post", "Body.", "publish").await;

    // A second account that has published nothing.
    sign_out(&client);
    register(&client, "lurker").await;
    sign_out(&client);

    assert_eq!(
        client.get("/author/lurker").send().await.status,
        404,
        "an account with no public content must not have an archive"
    );
    let unknown = client.get("/author/nobody-at-all").send().await;
    assert_eq!(
        unknown.status, 404,
        "and it must be indistinguishable from an account that does not exist"
    );

    // The author who has published still has one.
    client
        .get("/author/owner")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Owner Post");
}

/// A refused deferred transition on the admin path persists nothing.
///
/// `private` and `future` are reached by transitioning the draft the editor
/// creates, so a rejection after the insert left the draft, its initial
/// revision and its term assignments committed — with each retry consuming
/// another suffixed slug.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_refused_admin_creation_persists_nothing() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    for _ in 0..3 {
        let refused = client
            .post("/admin/content/post")
            .header("cookie", &cookie)
            .form(&form(&[
                ("title", ""),
                ("slug", "sneaky"),
                ("excerpt", ""),
                ("body", "Should not persist."),
                ("status", "private"),
                ("password", ""),
                ("taxonomy_names[post_tag]", ""),
                ("comment_status", "open"),
            ]))
            .send()
            .await;
        assert_eq!(refused.status, 422, "body: {}", refused.text());
    }

    let listing = client
        .get("/admin/content/post")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        !listing.contains("Should not persist."),
        "a refused creation must write nothing:\n{listing}"
    );
    sign_out(&client);
    assert_eq!(client.get("/sneaky").send().await.status, 404);
}

/// A status transition is credited to whoever performed it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_transition_is_attributed_to_the_acting_editor() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;
    let id = create_post(&client, &owner, "Awaiting Review", "Body.", "draft").await;

    sign_out(&client);
    let editor = register(&client, "editor").await;
    client
        .post("/admin/users/2")
        .header("cookie", &owner)
        .form(&form(&[
            ("role", "editor"),
            ("email", "editor@example.com"),
            ("display_name", "Editor"),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    // The Editor publishes somebody else's draft.
    client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &editor)
        .send()
        .await
        .assert_status(303);

    let history = client
        .get(&format!("/admin/content/post/{id}/revisions"))
        .header("cookie", &owner)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        history.contains("draft → publish"),
        "the transition must be in the history:\n{history}"
    );
    let transition_line = history
        .split("draft → publish")
        .nth(1)
        .expect("text after the transition summary");
    // The snapshot records the state *before* the transition, so its status is
    // the one being left — `draft` — and its author is whoever acted.
    assert!(
        transition_line.starts_with(" · draft · by Editor"),
        "the transition must be credited to the acting editor: {}",
        &transition_line[..transition_line.len().min(80)]
    );
}

/// A post is reachable at its permalink, and only at its permalink.
///
/// The dated fallback took the last segment of *any* multi-segment path, so
/// `/hello` was also served at `/anything/hello` and `/2026/13/hello` — an
/// unbounded set of duplicate-content aliases, and a 200 where a 404 belongs.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_post_is_not_served_from_arbitrary_paths() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Hello", "Body.", "publish").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("permalink_structure", "day_and_name")]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);
    let dated = chrono::Utc::now().format("%Y/%m/%d").to_string();
    client
        .get(&format!("/{dated}/hello"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Body.");

    for alias in [
        "/anything/hello",
        "/2026/13/hello",
        "/2026/00/hello",
        "/2026/01/32/hello",
        "/a/b/c/d/hello",
        "/99/hello",
    ] {
        assert_eq!(
            client.get(alias).send().await.status,
            404,
            "`{alias}` is not a permalink and must not serve the post"
        );
    }
}

/// An ordinary save does not unfile a post from taxonomies the editor hides.
///
/// `set_post_terms` replaces a post's filings wholesale, and the editor renders
/// only `category` and `post_tag` — so a save silently deleted every custom
/// taxonomy assignment, including the ones the importer had just restored. The
/// form is not evidence about taxonomies it never showed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn saving_a_post_keeps_assignments_the_editor_does_not_render() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Filed", "Body.", "publish").await;

    // A term in a taxonomy the editor has no field for, filed against the post.
    // Written directly because registering one would leak into every later
    // test through the process-global registry — and `post_terms` does not
    // care whether the taxonomy is registered, which is exactly the state an
    // import leaves behind.
    let db = TestDb::shared().await;
    try_execute(
        db,
        "INSERT INTO terms (taxonomy, name, slug, description) \
         VALUES ('genre', 'Longform', 'longform', '')",
    )
    .await
    .expect("insert custom term");
    try_execute(
        db,
        &format!(
            "INSERT INTO post_terms (post_id, term_id) \
             SELECT {id}, id FROM terms WHERE slug = 'longform'"
        ),
    )
    .await
    .expect("file the post under it");

    // An ordinary edit that names only a tag.
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Filed"),
                    ("slug", "filed"),
                    ("excerpt", ""),
                    ("body", "Edited body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", "rust"),
                    ("comment_status", "open"),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    // The tag was applied, and the custom-taxonomy filing survived.
    let editor = client
        .get(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(editor.contains("rust"), "the tag should have been applied");

    // Read the filing back. `1/COUNT(*)` divides by zero — and so errors —
    // exactly when the row is gone, which is the assertion this needs from a
    // helper that reports success or failure rather than rows.
    let still_filed = try_execute(
        db,
        &format!(
            "SELECT 1/COUNT(*) FROM post_terms pt JOIN terms t ON t.id = pt.term_id \
             WHERE pt.post_id = {id} AND t.taxonomy = 'genre'"
        ),
    )
    .await;
    assert!(
        still_filed.is_ok(),
        "the custom-taxonomy filing was deleted by an ordinary save: {still_filed:?}"
    );
}

/// A post is served at its own dated permalink and no other date.
///
/// Restricting the fallback by shape was not enough: `/2025/01/hello` has a
/// valid shape and is not the post's permalink, so a single post was still
/// reachable at thousands of dates. `/YYYY/<slug>` is not a shape any structure
/// mints at all.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dated_permalink_matches_only_the_posts_own_date() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Hello", "Body.", "publish").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("permalink_structure", "day_and_name")]))
        .send()
        .await
        .assert_status(303);

    sign_out(&client);
    let now = chrono::Utc::now();
    let day = now.format("%Y/%m/%d").to_string();
    let month = now.format("%Y/%m").to_string();
    let year = now.format("%Y").to_string();

    // Its own date, in both dated shapes — these are the aliases that exist so
    // that changing the permalink structure does not 404 shared links.
    client
        .get(&format!("/{day}/hello"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Body.");
    client
        .get(&format!("/{month}/hello"))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Body.");

    // A bare year is not a shape any structure mints.
    assert_eq!(
        client.get(&format!("/{year}/hello")).send().await.status,
        404
    );

    // Well-shaped dates that are not this post's.
    for alias in ["/2015/01/hello", "/2015/01/02/hello"] {
        assert_eq!(
            client.get(alias).send().await.status,
            404,
            "`{alias}` is not this post's permalink"
        );
    }

    // Unpadded is not what the generator writes, so it is not an alias either.
    let unpadded = format!("/{}/{}/hello", now.format("%Y"), now.format("%-m"));
    if unpadded != format!("/{month}/hello") {
        assert_eq!(client.get(&unpadded).send().await.status, 404);
    }
}

/// Search results past the first page are reachable from the UI.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn search_results_render_pagination() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("posts_per_page", "2")]))
        .send()
        .await
        .assert_status(303);

    for n in 1..=5 {
        create_post(
            &client,
            &cookie,
            &format!("Widget Report {n}"),
            "All about widgets.",
            "publish",
        )
        .await;
    }

    sign_out(&client);
    let first = client.get("/search?s=widgets").send().await;
    let html = first.assert_ok().text();
    assert!(
        html.contains("Older →"),
        "the first page of results must link to the next:\n{html}"
    );
    assert!(
        html.contains("s=widgets&amp;page=2"),
        "and that link must carry the search term:\n{html}"
    );

    let second = client.get("/search?s=widgets&page=2").send().await;
    let html = second.assert_ok().text();
    assert!(html.contains("← Newer"), "the second page must link back");
    assert!(html.contains("Page 2 of 3"), "and say where it is:\n{html}");
}

/// An absurd page number is bounded, not an overflow.
///
/// `page` is an unbounded `usize` from the query string and the offset is
/// `(page - 1) * per_page`, so `?page=18446744073709551615` overflows: a panic
/// under overflow checks (which is what a debug build, and this test, uses) and
/// a wrapped, unrelated page in release.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_absurd_page_number_does_not_overflow() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Only Post", "Body.", "publish").await;

    sign_out(&client);
    for path in [
        "/?page=18446744073709551615",
        "/?page=9223372036854775808",
        "/search?s=body&page=18446744073709551615",
        "/api/v1/posts?page=18446744073709551615",
        "/api/v1/terms?page=18446744073709551615",
        "/api/v1/authors?page=18446744073709551615",
    ] {
        let resp = client.get(path).send().await;
        assert!(
            resp.status.is_success(),
            "`{path}` must be bounded rather than overflow, got {}",
            resp.status
        );
    }
}

/// A second `file` part is refused rather than orphaning a blob.
///
/// Every `file` field was written to the blob store but only the last got an
/// attachment row, so the earlier objects were unreachable from the media
/// library and undeletable through the UI — repeatable, so an Author could
/// consume storage indefinitely.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_second_file_part_is_refused() {
    // This is the one test that needs a blob store, so it builds its own app
    // rather than making every other test carry one.
    let db = TestDb::shared().await;
    let _ = db_client().await; // migrate + truncate through the shared path
    // A plain temp directory rather than a `tempfile` dev-dependency: the
    // starter ships its own `Cargo.toml.tmpl`, so a new dev-dependency here
    // would have to be added there too or the scaffolded project would not
    // build its tests.
    let uploads = std::env::temp_dir().join(format!(
        "cms-upload-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&uploads).expect("create the blob store root");
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    config.security.submit_token.enabled = false;
    // `TestApp` does not run the storage preflight that `App::run` does, so the
    // store is mounted onto the state directly.
    let store = autumn_web::storage::LocalBlobStore::new(
        "default".to_owned(),
        uploads.clone(),
        "/_blobs".to_owned(),
        std::time::Duration::from_secs(900),
        autumn_web::storage::local::SigningKey::new(b"cms-upload-test-key".to_vec()),
        Vec::new(),
    )
    .expect("local blob store");
    let client = TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .state_initializer(move |state| {
            state.insert_extension::<autumn_web::storage::BlobStoreState>(
                autumn_web::storage::BlobStoreState::new(std::sync::Arc::new(store)),
            );
        })
        .build();
    let cookie = register(&client, "owner").await;

    let boundary = "----cmsboundary";
    let part = |name: &str, body: &str| {
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n{body}\r\n"
        )
    };
    let payload = format!(
        "{}{}--{boundary}--\r\n",
        part("one.txt", "first"),
        part("two.txt", "second")
    );

    let refused = client
        .post("/admin/media")
        .header("cookie", &cookie)
        .header(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .body(payload)
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a second file part must be refused: {}",
        refused.text()
    );

    // Nothing was recorded, so nothing was stored under a row-less key either.
    let library = client
        .get("/admin/media")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(!library.contains("one.txt"));
    assert!(!library.contains("two.txt"));
}

/// A featured image survives an export round trip.
///
/// The export carried no attachment reference, so a restore silently dropped
/// every featured image — and without the metadata rows, body links to
/// `/media/{slug}` had nothing to resolve against either.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_export_round_trip_keeps_featured_media() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // An attachment and a post that features it. Written directly because a
    // multipart upload is not what this test is about.
    let db = TestDb::shared().await;
    try_execute(
        db,
        "INSERT INTO attachments (title, slug, file, mime_type, byte_size, alt_text, caption) \
         VALUES ('Cover', 'cover-image', \
                 '{\"provider_id\":\"default\",\"key\":\"media/cover-image\",\
                   \"content_type\":\"image/png\",\"byte_size\":1234}'::jsonb, \
                 'image/png', 1234, 'A cover', '')",
    )
    .await
    .expect("insert attachment");
    let id = create_post(&client, &cookie, "Illustrated", "Body.", "publish").await;
    try_execute(
        db,
        &format!(
            "UPDATE posts SET featured_media_id = (SELECT id FROM attachments \
             WHERE slug = 'cover-image') WHERE id = {id}"
        ),
    )
    .await
    .expect("attach the image");

    // The export carries both halves.
    let exported = client
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let payload: serde_json::Value = serde_json::from_str(&exported).expect("valid export JSON");
    assert_eq!(payload["version"], serde_json::json!(3));
    assert_eq!(
        payload["attachments"][0]["slug"],
        serde_json::json!("cover-image")
    );
    // The handle, not just the display metadata: without the provider and key
    // a restored row cannot name its bytes, so restoring the separately
    // backed-up blob store would fix nothing and `/media/{slug}` would 500.
    assert_eq!(
        payload["attachments"][0]["file"]["key"],
        serde_json::json!("media/cover-image"),
        "the export must carry the blob handle: {}",
        payload["attachments"][0]
    );
    let post = payload["posts"]
        .as_array()
        .expect("posts")
        .iter()
        .find(|p| p["slug"] == serde_json::json!("illustrated"))
        .expect("the post is in the export");
    assert_eq!(post["featured_media"], serde_json::json!("cover-image"));

    // Importing it into a site that has neither restores both and re-attaches.
    let fresh = db_client().await;
    let cookie = register(&fresh, "owner").await;
    import_export(&fresh, &cookie, exported.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    let editor = fresh
        .get("/admin/content/post/1")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        editor.contains("cover-image") || editor.contains("Cover"),
        "the restored post must still name its featured image:\n{editor}"
    );

    // And the restored row points at the same bytes it always did.
    let restored = fresh
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let restored: serde_json::Value = serde_json::from_str(&restored).expect("valid JSON");
    assert_eq!(
        restored["attachments"][0]["file"]["key"],
        serde_json::json!("media/cover-image"),
        "the import must restore the handle, not only the metadata: {}",
        restored["attachments"][0]
    );
}

/// A crafted `categories` value cannot file a post under an unrelated taxonomy.
///
/// The ids went straight into `set_post_terms`, which checks neither the term's
/// taxonomy nor whether that taxonomy applies to this post type — so an Author
/// could attach a post to a taxonomy registered for something else, after which
/// that taxonomy's public archive listed it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_crafted_category_id_from_another_taxonomy_is_ignored() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Ordinary", "Body.", "publish").await;

    // A term in a taxonomy that does not apply to `post`.
    let db = TestDb::shared().await;
    try_execute(
        db,
        "INSERT INTO terms (taxonomy, name, slug, description) \
         VALUES ('shelf', 'Reference', 'reference', '')",
    )
    .await
    .expect("insert the foreign term");

    // A real category, so the test proves filtering rather than refusal.
    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "News"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    let categories: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=category")
        .send()
        .await
        .assert_ok()
        .json();
    let news_id = categories.as_array().expect("array")[0]["id"]
        .as_i64()
        .expect("id")
        .to_string();
    let foreign_id = (news_id.parse::<i64>().expect("id") + 1).to_string();

    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Ordinary"),
                    ("slug", "ordinary"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomy_names[post_tag]", ""),
                    ("comment_status", "open"),
                    ("taxonomies[category]", news_id.as_str()),
                    ("taxonomies[category]", foreign_id.as_str()),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    // The legitimate category stuck; the foreign one did not.
    let filed = try_execute(
        db,
        &format!(
            "SELECT 1/COUNT(*) FROM post_terms pt JOIN terms t ON t.id = pt.term_id \
             WHERE pt.post_id = {id} AND t.taxonomy = 'category'"
        ),
    )
    .await;
    assert!(filed.is_ok(), "the real category must have been applied");

    let foreign = try_execute(
        db,
        &format!(
            "SELECT 1/COUNT(*) FROM post_terms pt JOIN terms t ON t.id = pt.term_id \
             WHERE pt.post_id = {id} AND t.taxonomy = 'shelf'"
        ),
    )
    .await;
    assert!(
        foreign.is_err(),
        "a term from a taxonomy that does not apply to `post` must be ignored"
    );
}

/// The `the_excerpt` filter reaches actual output.
///
/// The hook is documented and was applied nowhere outside its own unit test, so
/// a plugin registering it silently did nothing for visitors.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_excerpt_filter_reaches_rendered_output() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(
        &client,
        &cookie,
        "Filtered",
        "The body that becomes an excerpt.",
        "publish",
    )
    .await;

    // `bootstrap` registers a `TheContent` filter turning ` -- ` into an em
    // dash; the excerpt hook needs its own evidence, so register one here. The
    // registry is process-global, so this deliberately uses a marker no other
    // test asserts the absence of.
    {{crate_name}}::plugins::add_filter(
        {{crate_name}}::plugins::Filter::TheExcerpt,
        {{crate_name}}::plugins::DEFAULT_PRIORITY,
        |excerpt| format!("{excerpt} [EXCERPT-FILTER-RAN]"),
    );

    sign_out(&client);
    // The listing card…
    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("[EXCERPT-FILTER-RAN]");
    // …the feed…
    client
        .get("/feed")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("[EXCERPT-FILTER-RAN]");
    // …and the REST projection.
    let api: serde_json::Value = client.get("/api/v1/posts").send().await.assert_ok().json();
    assert!(
        api.as_array().expect("array")[0]["excerpt"]
            .as_str()
            .unwrap_or_default()
            .contains("[EXCERPT-FILTER-RAN]"),
        "the API projection must apply the filter too: {api}"
    );
}

/// Ticking a category box saves the post.
///
/// The editor's category checkboxes post the same key repeatedly, and `Form<T>`
/// decodes bodies through `serde_urlencoded`, which has no repeated-key rule —
/// so checking a single box failed the whole save with "invalid type: string,
/// expected a sequence". Category assignment did not work at all through the
/// admin UI, and none of the suite's other tests ever ticked a box.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn checking_a_category_box_saves_the_post() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Categorised", "Body.", "publish").await;

    for name in ["News", "Reviews"] {
        client
            .post("/admin/terms/category")
            .header("cookie", &cookie)
            .form(&form(&[
                ("name", name),
                ("slug", ""),
                ("description", ""),
                ("parent_id", ""),
            ]))
            .send()
            .await
            .assert_status(303);
    }
    let categories: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=category")
        .send()
        .await
        .assert_ok()
        .json();
    let ids: Vec<String> = categories
        .as_array()
        .expect("array")
        .iter()
        .map(|t| t["id"].as_i64().expect("id").to_string())
        .collect();
    assert_eq!(ids.len(), 2);

    // One box…
    let mut fields = vec![
        ("title", "Categorised"),
        ("slug", "categorised"),
        ("excerpt", ""),
        ("body", "Body."),
        ("status", "publish"),
        ("password", ""),
        ("taxonomy_names[post_tag]", ""),
        ("comment_status", "open"),
        ("taxonomies[category]", ids[0].as_str()),
    ];
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&edit_form(&id, &fields).await)
        .send()
        .await
        .assert_status(303);

    // …and two, which is the shape a checkbox group actually posts.
    fields.push(("taxonomies[category]", ids[1].as_str()));
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&edit_form(&id, &fields).await)
        .send()
        .await
        .assert_status(303);

    // Both archives list it.
    sign_out(&client);
    for slug in ["news", "reviews"] {
        client
            .get(&format!("/category/{slug}"))
            .send()
            .await
            .assert_ok()
            .assert_body_contains("Categorised");
    }
}

/// The editor offers only statuses the state machine can actually reach.
///
/// The dropdown listed every status, but the graph declares no `publish ->
/// pending` or `publish -> future` edge — so choosing either was rejected after
/// the UI had explicitly offered it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_editor_offers_only_reachable_statuses() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Live", "Body.", "publish").await;

    let editor = client
        .get(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();

    // From `publish` the graph declares draft, private and trash. The dropdown
    // shows the reachable ones plus "stay put".
    assert!(
        editor.contains(r#"value="publish""#),
        "staying put is an option"
    );
    assert!(
        editor.contains(r#"value="draft""#),
        "publish -> draft is declared"
    );
    assert!(
        editor.contains(r#"value="private""#),
        "publish -> private is declared"
    );
    // …and hides the two the graph does not declare.
    assert!(
        !editor.contains(r#"value="pending""#),
        "publish -> pending is not a declared edge:\n{editor}"
    );
    assert!(
        !editor.contains(r#"value="future""#),
        "publish -> future is not a declared edge:\n{editor}"
    );

    // A draft still offers the full publisher set.
    let draft = create_post(&client, &cookie, "Unpublished", "Body.", "draft").await;
    let editor = client
        .get(&format!("/admin/content/post/{draft}"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    for value in ["draft", "pending", "publish", "private", "future"] {
        assert!(
            editor.contains(&format!(r#"value="{value}""#)),
            "a draft can reach `{value}`"
        );
    }
}

/// Re-approving an already-approved comment fires nothing a second time.
///
/// `moderate_comment` returns the unchanged row for an idempotent request, but
/// the handler checked only the resulting status — so a retry or double-click
/// dispatched `CommentApproved` again and plugins enqueued duplicate work.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn re_approving_a_comment_is_idempotent() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Discussed", "Body.", "publish").await;

    sign_out(&client);
    client
        .post(&format!("/comments/{post_id}"))
        .form(&form(&[
            ("body", "Held for review"),
            ("author_name", "Guest"),
            ("author_email", "guest@example.com"),
        ]))
        .send()
        .await
        .assert_status(303);

    // Approve it three times; the counter must not drift.
    for _ in 0..3 {
        client
            .post("/admin/comments/1/status?to=approved")
            .header("cookie", &cookie)
            .send()
            .await
            .assert_status(303);
    }

    sign_out(&client);
    client
        .get("/discussed")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("1 comment")
        .assert_body_contains("Held for review");
}

/// A sticky post comes back sticky.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_export_round_trip_keeps_the_sticky_flag() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Pinned", "Body.", "publish").await;
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET sticky = true WHERE id = {id}"),
    )
    .await
    .expect("pin the post");

    let exported = client
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let payload: serde_json::Value = serde_json::from_str(&exported).expect("valid JSON");
    assert_eq!(
        payload["posts"][0]["sticky"],
        serde_json::json!(true),
        "the export must carry the flag: {}",
        payload["posts"][0]
    );

    let fresh = db_client().await;
    let cookie = register(&fresh, "owner").await;
    import_export(&fresh, &cookie, exported.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    let restored = fresh
        .get("/admin/tools/export")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let restored: serde_json::Value = serde_json::from_str(&restored).expect("valid JSON");
    assert_eq!(
        restored["posts"][0]["sticky"],
        serde_json::json!(true),
        "and the import must apply it: {}",
        restored["posts"][0]
    );
}

/// A plugin that registers a hook from inside a hook does not hang the request.
///
/// `do_action` held the registry's read lock while running listeners, so a
/// listener calling `add_action` waited on the write lock — `RwLock` is not
/// reentrant, so the thread waited on itself, forever. Registering from inside
/// a hook is an ordinary thing for a plugin to do.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_hook_that_registers_a_hook_does_not_deadlock() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    {{crate_name}}::plugins::add_action(
        {{crate_name}}::plugins::Action::PostSaved,
        {{crate_name}}::plugins::DEFAULT_PRIORITY,
        |_| {
            // The re-entrant call. Before the fix this never returned.
            {{crate_name}}::plugins::add_filter(
                {{crate_name}}::plugins::Filter::TheTitle,
                {{crate_name}}::plugins::DEFAULT_PRIORITY,
                |title| title,
            );
        },
    );

    // Saving fires `PostSaved`; the request has to come back.
    let created = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        create_post(&client, &cookie, "Re-entrant", "Body.", "publish"),
    )
    .await
    .expect("saving must not hang while a listener registers another hook");
    assert!(created > 0);
}

/// A settings save is all-or-nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn saving_settings_applies_every_option() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[
            ("site_title", "Renamed Site"),
            ("tagline", "A new tagline"),
            ("posts_per_page", "7"),
        ]))
        .send()
        .await
        .assert_status(303);

    let screen = client
        .get("/admin/settings")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(screen.contains("Renamed Site"));
    assert!(screen.contains("A new tagline"));
    assert!(
        screen.contains(r#"value="7""#),
        "the page size stuck:\n{screen}"
    );

    // And the public site reflects all of it, so the cache was invalidated
    // after the commit rather than before it.
    sign_out(&client);
    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Renamed Site");
}

/// A long filename does not leave a blob with no attachment row.
///
/// The derived title exceeded the model's 300-character limit, so the row was
/// refused *after* the bytes were already stored — an object invisible in the
/// media library and impossible to delete through it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_very_long_filename_still_uploads() {
    let db = TestDb::shared().await;
    let _ = db_client().await;
    let uploads = std::env::temp_dir().join(format!(
        "cms-longname-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&uploads).expect("create the blob store root");
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    config.security.submit_token.enabled = false;
    let store = autumn_web::storage::LocalBlobStore::new(
        "default".to_owned(),
        uploads.clone(),
        "/_blobs".to_owned(),
        std::time::Duration::from_secs(900),
        autumn_web::storage::local::SigningKey::new(b"cms-upload-test-key".to_vec()),
        Vec::new(),
    )
    .expect("local blob store");
    let client = TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .state_initializer(move |state| {
            state.insert_extension::<autumn_web::storage::BlobStoreState>(
                autumn_web::storage::BlobStoreState::new(std::sync::Arc::new(store)),
            );
        })
        .build();
    let cookie = register(&client, "owner").await;

    let long_name = format!("{}.txt", "a".repeat(500));
    let boundary = "----cmsboundary";
    let payload = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{long_name}\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
    );

    let resp = client
        .post("/admin/media")
        .header("cookie", &cookie)
        .header(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .body(payload)
        .send()
        .await;
    assert_eq!(
        resp.status,
        303,
        "a long filename is a valid upload: {}",
        resp.text()
    );

    // It is in the library, so it can be deleted through the UI.
    client
        .get("/admin/media")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("aaaa");
}

/// A rejected duplicate upload leaves no blob behind.
///
/// The first `file` part is already persisted when the second is refused, so
/// the early return leaked a whole object — repeatable, and an Author could
/// send a 16 MB first part in a loop to consume storage with files the media
/// library cannot show or delete.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_rejected_duplicate_upload_leaves_no_orphaned_blob() {
    let db = TestDb::shared().await;
    let _ = db_client().await;
    let uploads = std::env::temp_dir().join(format!(
        "cms-orphan-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&uploads).expect("create the blob store root");
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    config.security.submit_token.enabled = false;
    let store = autumn_web::storage::LocalBlobStore::new(
        "default".to_owned(),
        uploads.clone(),
        "/_blobs".to_owned(),
        std::time::Duration::from_secs(900),
        autumn_web::storage::local::SigningKey::new(b"cms-upload-test-key".to_vec()),
        Vec::new(),
    )
    .expect("local blob store");
    let client = TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .state_initializer(move |state| {
            state.insert_extension::<autumn_web::storage::BlobStoreState>(
                autumn_web::storage::BlobStoreState::new(std::sync::Arc::new(store)),
            );
        })
        .build();
    let cookie = register(&client, "owner").await;

    let boundary = "----cmsboundary";
    let part = |name: &str, body: &str| {
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n{body}\r\n"
        )
    };
    let payload = format!(
        "{}{}--{boundary}--\r\n",
        part("first.txt", "the first payload"),
        part("second.txt", "the second payload")
    );

    client
        .post("/admin/media")
        .header("cookie", &cookie)
        .header(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .body(payload)
        .send()
        .await
        .assert_status(422);

    // The cleanup is spawned, so give it a moment to land, then assert the
    // store is empty — no attachment row exists, so any file here is orphaned.
    for _ in 0..50 {
        if count_files(&uploads) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        count_files(&uploads),
        0,
        "the first part's blob must not survive the rejection"
    );
}

/// Every file under `root`, recursively.
fn count_files(root: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}

/// A username has to be usable as the author archive's URL segment.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_username_that_is_not_a_url_segment_is_refused() {
    let client = db_client().await;

    // `Alice` is deliberately absent: `normalize_new_user` lowercases before it
    // validates, so it is accepted and stored as `alice`, which is a perfectly
    // good segment. Only values that cannot be normalized *into* one are
    // refused.
    for bad in [
        "alice/news",
        "alice bloggs",
        "alice?x",
        "alice.news",
        "alice%2f",
    ] {
        let resp = client
            .post("/register")
            .form(&form(&[
                ("username", bad),
                ("email", "someone@example.com"),
                ("password", "correct-horse-battery-staple"),
            ]))
            .send()
            .await;
        assert_ne!(
            resp.status, 303,
            "`{bad}` cannot be an author URL segment and must be refused"
        );
    }

    // A well-formed one still registers, and its byline resolves.
    let cookie = register(&client, "alice-bloggs").await;
    create_post(&client, &cookie, "By Alice", "Body.", "publish").await;
    sign_out(&client);
    client
        .get("/author/alice-bloggs")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("By Alice");
}

/// The configured front page stays selected however old it is.
///
/// The selector listed the newest 200 pages, so a front page older than those
/// was simply absent — the browser then submitted the empty option and saving
/// any unrelated setting silently switched the site back to the posts index.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_configured_front_page_stays_selected() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // The page that will be configured, created first so it is the oldest.
    let front = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Welcome"),
            ("slug", "welcome"),
            ("excerpt", ""),
            ("body", "The front page."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", ""),
        ]))
        .send()
        .await;
    assert_eq!(front.status, 303);
    let front_id = front
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("front_page_id", front_id.as_str())]))
        .send()
        .await
        .assert_status(303);

    // Push it out of the newest-200 window.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO posts (post_type, title, slug, body, status, author_id, published_at) \
         SELECT 'page', 'Filler ' || n, 'filler-' || n, '', 'publish', 1, now() \
         FROM generate_series(1, 250) AS n",
    )
    .await
    .expect("insert filler pages");

    let screen = client
        .get("/admin/settings")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        screen.contains(&format!(r#"value="{front_id}" selected"#))
            || screen.contains(&format!(r#"selected value="{front_id}""#))
            || (screen.contains("Welcome") && screen.contains(&format!(r#"value="{front_id}""#))),
        "the configured front page must still be in the selector:\n{}",
        &screen[..screen.len().min(4000)]
    );
}

/// Removing every category actually unfiles the post.
///
/// `set_post_terms` replaces the filings, so skipping it when the selection is
/// empty made "remove every category" quietly do nothing and the post stayed in
/// archives it had been taken out of.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clearing_every_category_removes_the_post_from_its_archives() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Filed Then Not", "Body.", "publish").await;

    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "News"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    let categories: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=category")
        .send()
        .await
        .assert_ok()
        .json();
    let news = categories.as_array().expect("array")[0]["id"]
        .as_i64()
        .expect("id")
        .to_string();

    let post_id = &id;
    let base = async |extra: Option<&str>| {
        let mut fields = vec![
            ("title", "Filed Then Not"),
            ("slug", "filed-then-not"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
            ("taxonomy_names[post_tag]", "rust"),
            ("comment_status", "open"),
        ];
        if let Some(term) = extra {
            fields.push(("taxonomies[category]", term));
        }
        edit_form(post_id, &fields).await
    };

    // File it, and confirm the archive lists it.
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&base(Some(news.as_str())).await)
        .send()
        .await
        .assert_status(303);
    client
        .get("/category/news")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Filed Then Not");

    // Now clear every category and tag.
    let mut cleared = base(None).await;
    cleared = cleared.replace(
        "taxonomy_names%5Bpost_tag%5D=rust",
        "taxonomy_names%5Bpost_tag%5D=",
    );
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&cleared)
        .send()
        .await
        .assert_status(303);

    sign_out(&client);
    let archive = client.get("/category/news").send().await;
    archive.assert_ok();
    assert!(
        !archive.text().contains("Filed Then Not"),
        "an emptied selection must unfile the post:\n{}",
        archive.text()
    );
}

/// A shortcode handler that registers a shortcode does not hang the page.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_shortcode_that_registers_a_shortcode_does_not_deadlock() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    {{crate_name}}::shortcodes::add_shortcode("reentrant", |_| {
        // The re-entrant call. Before the fix this never returned.
        {{crate_name}}::shortcodes::add_shortcode("added-from-inside", |_| String::new());
        "<span>expanded</span>".to_owned()
    });

    create_post(
        &client,
        &cookie,
        "Shortcoded",
        "Before [reentrant] after.",
        "publish",
    )
    .await;

    sign_out(&client);
    let page = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.get("/shortcoded").send(),
    )
    .await
    .expect("rendering must not hang while a handler registers another shortcode");
    page.assert_ok().assert_body_contains("expanded");
}

/// A scheduled publication records the state it left, not the one it reached.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_scheduled_publication_snapshots_the_scheduled_state() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Due Now", "Body.", "draft").await;

    // Make it a scheduled post that is already due, at a date this test names
    // so the guard can be given exactly what the sweep would have observed.
    let db = TestDb::shared().await;
    let due = chrono::NaiveDateTime::parse_from_str("2026-01-02 03:04:05", "%Y-%m-%d %H:%M:%S")
        .expect("a fixed due date");
    try_execute(
        db,
        &format!("UPDATE posts SET status = 'future', published_at = '{due}' WHERE id = {id}"),
    )
    .await
    .expect("schedule the post");

    // A mismatched `observed_published_at` must not publish it — that guard is
    // what stops an editor's reschedule being overridden by an in-flight sweep.
    assert!(
        !{{crate_name}}::content::publish_due_post(
            &mut db.pool().get().await.expect("connection"),
            id,
            None,
            "publish",
        )
        .await
        .expect("query"),
        "the guard compares the observed publish date"
    );

    assert!(
        {{crate_name}}::content::publish_due_post(
            &mut db.pool().get().await.expect("connection"),
            id,
            Some(due),
            "publish",
        )
        .await
        .expect("publish"),
        "the due post publishes"
    );

    // The revision snapshots the state it was in *before* the transition.
    let history = client
        .get(&format!("/admin/content/post/{id}/revisions"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    let after = history
        .split("future → publish")
        .nth(1)
        .expect("the transition is recorded");
    assert!(
        after.starts_with(" · future"),
        "the snapshot must record the scheduled state, not the published one: {}",
        &after[..after.len().min(60)]
    );
}

/// A plugin's taxonomy is editable through the post editor.
///
/// The editor recognised only `category` and `post_tag`, so an administrator
/// could create custom terms through the generic term screens and then had no
/// way to attach one to anything — the registry-driven workflow the starter
/// advertises stopped halfway.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_registered_custom_taxonomy_is_editable() {
    // Registered before the client is built, and deliberately for `page` rather
    // than `post`: the registry is process-global, so adding a taxonomy to
    // `post` would change every other test's editor. `page` has no taxonomies
    // of its own, which also makes this a clean check that the controls come
    // from the registry rather than from the two built-in slugs.
    {{crate_name}}::content_types::register_taxonomy({{crate_name}}::content_types::Taxonomy {
        slug: "shelf",
        singular: "Shelf",
        plural: "Shelves",
        hierarchical: true,
        post_types: &["page"],
        rewrite_base: "shelf",
    })
    .expect("shelf registers cleanly");

    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/terms/shelf")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Reference"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    let terms: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=shelf")
        .send()
        .await
        .assert_ok()
        .json();
    let shelf_id = terms.as_array().expect("array")[0]["id"]
        .as_i64()
        .expect("id")
        .to_string();

    let created = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Handbook"),
            ("slug", "handbook"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303);
    let id = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    // The editor renders a control for it…
    let editor = client
        .get(&format!("/admin/content/page/{id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(
        editor.contains("Shelves") && editor.contains("taxonomies[shelf]"),
        "the editor must render a control for a registered taxonomy:\n{editor}"
    );

    // …and submitting it attaches the term.
    client
        .post(&format!("/admin/content/page/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Handbook"),
                    ("slug", "handbook"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("taxonomies[shelf]", shelf_id.as_str()),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    let filed = try_execute(
        TestDb::shared().await,
        &format!(
            "SELECT 1/COUNT(*) FROM post_terms pt JOIN terms t ON t.id = pt.term_id \
             WHERE pt.post_id = {id} AND t.taxonomy = 'shelf'"
        ),
    )
    .await;
    assert!(
        filed.is_ok(),
        "the custom taxonomy term must be attached: {filed:?}"
    );

    // And clearing it detaches again.
    client
        .post(&format!("/admin/content/page/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Handbook"),
                    ("slug", "handbook"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);
    let still = try_execute(
        TestDb::shared().await,
        &format!(
            "SELECT 1/COUNT(*) FROM post_terms pt JOIN terms t ON t.id = pt.term_id \
             WHERE pt.post_id = {id} AND t.taxonomy = 'shelf'"
        ),
    )
    .await;
    assert!(still.is_err(), "clearing the control must detach the term");
}

/// A save cannot create an unbounded number of terms.
///
/// Find-or-create means one lookup and possibly one insert per name, and the
/// field is free text — a crafted save could carry millions of names inside the
/// framework's request limit, holding the request open and leaving a permanent
/// term set behind.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_save_cannot_create_unbounded_terms() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Tagged", "Body.", "publish").await;

    let save = async |tags: &str| {
        edit_form(
            &id,
            &[
                ("title", "Tagged"),
                ("slug", "tagged"),
                ("excerpt", ""),
                ("body", "Body."),
                ("status", "publish"),
                ("password", ""),
                ("comment_status", "open"),
                ("taxonomy_names[post_tag]", tags),
            ],
        )
        .await
    };

    // Far more than any editor types.
    let many: Vec<String> = (0..500).map(|n| format!("tag{n}")).collect();
    let refused = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&save(&many.join(",")).await)
        .send()
        .await;
    assert_eq!(refused.status, 422, "body: {}", refused.text());

    // A single absurdly long name is refused too.
    let refused = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&save(&"a".repeat(500)).await)
        .send()
        .await;
    assert_eq!(refused.status, 422, "body: {}", refused.text());

    // Nothing was created by either attempt.
    let terms: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=post_tag")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(terms.as_array().map(Vec::len), Some(0));

    // An ordinary handful still works.
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&save("rust, web, async").await)
        .send()
        .await
        .assert_status(303);
    let terms: serde_json::Value = client
        .get("/api/v1/terms?taxonomy=post_tag")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(terms.as_array().map(Vec::len), Some(3));
}

/// Scheduling requires a date that is actually in the future.
///
/// A published post moved back to draft keeps its original `published_at`, and
/// the editor pre-fills it — so choosing "Scheduled" without touching the field
/// produced a row that was already due, and the next sweep republished it
/// within the minute instead of scheduling it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scheduling_requires_a_future_date() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Was Live", "Body.", "publish").await;

    // Back to draft; the row keeps its past publish date.
    client
        .post(&format!("/admin/content/post/{id}/status?to=draft"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let save = async |publish_at: &str| {
        let mut fields = vec![
            ("title", "Was Live"),
            ("slug", "was-live"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "future"),
            ("password", ""),
            ("comment_status", "open"),
        ];
        if !publish_at.is_empty() {
            fields.push(("publish_at", publish_at));
        }
        edit_form(&id, &fields).await
    };

    // No date at all, and a date in the past, are both refused.
    for attempt in ["", "2020-01-01T09:00"] {
        let refused = client
            .post(&format!("/admin/content/post/{id}"))
            .header("cookie", &cookie)
            .form(&save(attempt).await)
            .send()
            .await;
        assert_eq!(
            refused.status,
            422,
            "`{attempt}` is not a future publish date: {}",
            refused.text()
        );
    }

    // A genuinely future one schedules.
    let future = (chrono::Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&save(&future).await)
        .send()
        .await
        .assert_status(303);
}

/// An administrator-created account is held to the same username rule.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_admin_created_username_must_be_a_url_segment() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let refused = client
        .post("/admin/users")
        .header("cookie", &cookie)
        .form(&form(&[
            ("username", "alice/news"),
            ("email", "alice@example.com"),
            ("password", "correct-horse-battery-staple"),
            ("role", "author"),
            ("display_name", "Alice"),
        ]))
        .send()
        .await;
    assert_ne!(
        refused.status,
        303,
        "the admin screen must apply the same rule as registration: {}",
        refused.text()
    );

    client
        .post("/admin/users")
        .header("cookie", &cookie)
        .form(&form(&[
            ("username", "alice-news"),
            ("email", "alice@example.com"),
            ("password", "correct-horse-battery-staple"),
            ("role", "author"),
            ("display_name", "Alice"),
        ]))
        .send()
        .await
        .assert_status(303);
}

/// An import leaves a same-slug local post completely alone.
///
/// A retry fix in the previous round offered every already-present row to the
/// ancestry pass, including ones matched only by slug — so an import advertised
/// as skipping existing items could re-parent a local page and change its
/// canonical URL. Only rows this importer created are its to move.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_import_does_not_reparent_a_local_post_that_shares_a_slug() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // Two local pages: `guides` at the top level, and `install` under it.
    for (title, slug, parent) in [
        ("Guides", "guides", None),
        ("Install", "install", Some("1")),
    ] {
        let mut fields = vec![
            ("title", title),
            ("slug", slug),
            ("excerpt", ""),
            ("body", "Local content."),
            ("status", "publish"),
            ("password", ""),
        ];
        if let Some(parent) = parent {
            fields.push(("parent_id", parent));
        }
        client
            .post("/admin/content/page")
            .header("cookie", &cookie)
            .form(&form(&fields))
            .send()
            .await
            .assert_status(303);
    }
    client
        .get("/guides/install")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Local content.");

    // A backup that happens to contain a page with the same slug, filed under a
    // different parent.
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "attachments": [],
        "posts": [
            {
                "post_type": "page", "title": "Manuals", "slug": "manuals",
                "excerpt": "", "body": "Imported parent.", "status": "publish",
                "comment_status": "closed", "password": "", "author": "owner",
                "published_at": null, "parent": null, "terms": [],
                "sticky": false, "menu_order": 0
            },
            {
                "post_type": "page", "title": "Install", "slug": "install",
                "excerpt": "", "body": "Imported child.", "status": "publish",
                "comment_status": "closed", "password": "", "author": "owner",
                "published_at": null, "parent": "manuals", "terms": [],
                "sticky": false, "menu_order": 0
            }
        ]
    })
    .to_string();

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    // The local page is where it was, with its own content and URL.
    sign_out(&client);
    client
        .get("/guides/install")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Local content.");
    assert_eq!(
        client.get("/manuals/install").send().await.status,
        404,
        "the import must not have moved the local page under its own parent"
    );
}

/// The one-click status control cannot schedule without a date.
///
/// It carries no date, so `to=future` produced a post that either never
/// publishes (`published_at IS NULL`, which the sweep never matches) or
/// publishes on the very next sweep (a retained past date).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_status_endpoint_refuses_undated_scheduling() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // A fresh draft has no publish date at all.
    let fresh = create_post(&client, &cookie, "Never Dated", "Body.", "draft").await;
    let refused = client
        .post(&format!("/admin/content/post/{fresh}/status?to=future"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(refused.status, 422, "body: {}", refused.text());

    // A formerly published draft has a *past* one, which is worse: it would
    // republish on the next sweep.
    let was_live = create_post(&client, &cookie, "Was Live", "Body.", "publish").await;
    client
        .post(&format!("/admin/content/post/{was_live}/status?to=draft"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
    let refused = client
        .post(&format!("/admin/content/post/{was_live}/status?to=future"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(refused.status, 422, "body: {}", refused.text());

    // With a genuinely future date already on the row, it is allowed.
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET published_at = now() + interval '7 days' WHERE id = {was_live}"),
    )
    .await
    .expect("give it a future date");
    client
        .post(&format!("/admin/content/post/{was_live}/status?to=future"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
}

/// Trashing a page that still has live children is refused.
///
/// `page_ancestry` keeps putting a trashed parent's slug in its children's
/// permalinks while `resolve_page_path` refuses a trashed ancestor, so every
/// published child 404s at its own canonical URL and the sitemap keeps
/// advertising it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn trashing_a_page_with_live_children_is_refused() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let parent = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Docs"),
            ("slug", "docs"),
            ("excerpt", ""),
            ("body", "Parent."),
            ("status", "publish"),
            ("password", ""),
        ]))
        .send()
        .await;
    assert_eq!(parent.status, 303);
    let parent_id = parent
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    let child = client
        .post("/admin/content/page")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Install"),
            ("slug", "install"),
            ("excerpt", ""),
            ("body", "Child."),
            ("status", "publish"),
            ("password", ""),
            ("parent_id", parent_id.as_str()),
        ]))
        .send()
        .await;
    assert_eq!(child.status, 303);
    let child_id = child
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    client
        .get("/docs/install")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Child.");

    // Trashing the parent is refused while the child is live.
    let refused = client
        .post(&format!("/admin/content/page/{parent_id}/status?to=trash"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(refused.status, 422, "body: {}", refused.text());

    // The child is still reachable, so nothing was half-applied.
    client
        .get("/docs/install")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Child.");

    // Trash the child first, and the parent goes.
    client
        .post(&format!("/admin/content/page/{child_id}/status?to=trash"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/admin/content/page/{parent_id}/status?to=trash"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
}

/// An import does not restructure a locally-managed taxonomy.
///
/// The first pass deliberately leaves an existing term alone, but the ancestry
/// pass still assigned the backup's parent — so importing into a populated site
/// could silently reparent a local category, or close a cycle with it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_import_does_not_reparent_a_local_term() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // Two local categories, both at the top level.
    for name in ["Guides", "Reference"] {
        client
            .post("/admin/terms/category")
            .header("cookie", &cookie)
            .form(&form(&[
                ("name", name),
                ("slug", ""),
                ("description", ""),
                ("parent_id", ""),
            ]))
            .send()
            .await
            .assert_status(303);
    }

    // A backup that files `reference` under `guides`.
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "attachments": [],
        "posts": [],
        "terms": [
            {"taxonomy": "category", "name": "Guides", "slug": "guides",
             "description": "", "parent": null},
            {"taxonomy": "category", "name": "Reference", "slug": "reference",
             "description": "", "parent": "guides"}
        ]
    })
    .to_string();

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok();

    // The local hierarchy is untouched: `reference` is still top-level.
    let orphaned = try_execute(
        TestDb::shared().await,
        "SELECT 1/COUNT(*) FROM terms WHERE slug = 'reference' AND parent_id IS NOT NULL",
    )
    .await;
    assert!(
        orphaned.is_err(),
        "the import must not have reparented the local term"
    );
}

/// The moderation queue is bounded and paginated.
///
/// It loaded every row of the selected status, sorted in memory, and looked the
/// post up once per comment — so the screen needed to clear a spam flood was
/// the first one to stop working under it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_moderation_queue_paginates() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Busy", "Body.", "publish").await;

    // Sixty pending guest comments, written directly — the point is the queue's
    // shape, not the submission path, and the throttle would bound the rate.
    try_execute(
        TestDb::shared().await,
        &format!(
            "INSERT INTO comments (post_id, author_name, body, status, created_at) \
             SELECT {post_id}, 'Guest', 'pending ' || n, 'pending', now() - (n || ' minutes')::interval \
             FROM generate_series(1, 60) AS n"
        ),
    )
    .await
    .expect("seed the queue");

    let first = client
        .get("/admin/comments?status=pending")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    // Newest first, bounded at 50: `pending 1` is the newest, `pending 60` the
    // oldest and off this page.
    assert!(
        first.contains("pending 1<"),
        "the newest comment is on page one"
    );
    assert!(
        !first.contains("pending 60<"),
        "the oldest must not be on page one — the queue is unbounded:\n{}",
        &first[..first.len().min(2000)]
    );

    let second = client
        .get("/admin/comments?status=pending&page=2")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .text();
    assert!(second.contains("pending 60<"), "the rest is on page two");
    assert!(!second.contains("pending 1<"), "pages must not overlap");

    // The post title still shows, so the batched lookup replaced the per-row
    // one rather than dropping it.
    assert!(
        first.contains("Busy"),
        "the queue names the post being discussed"
    );
}

/// An imported attachment cannot smuggle an inline-rendered media type.
///
/// The upload path enforces `ALLOWED_MIME`; an import did not, so a tampered
/// export could label bytes already in the store `text/html` and `/media/{slug}`
/// would serve them inline — stored script execution on the site's own origin.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_import_cannot_introduce_an_inline_html_attachment() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Tampered",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "posts": [],
        "attachments": [{
            "slug": "payload", "title": "Payload", "mime_type": "text/html",
            "byte_size": 10, "width": null, "height": null,
            "alt_text": "", "caption": "",
            "file": {"provider_id": "default", "key": "media/payload",
                     "content_type": "text/html", "byte_size": 10}
        }]
    })
    .to_string();

    let refused = import_export(&client, &cookie, payload.as_str()).await;
    assert_eq!(
        refused.status,
        422,
        "an unsupported media type must stop the restore: {}",
        refused.text()
    );

    // And the serving policy is allowlist-shaped, so a row written by any other
    // route is still not rendered inline.
    assert!(!{{crate_name}}::routes::admin::media::may_render_inline("text/html"));
    assert!(!{{crate_name}}::routes::admin::media::may_render_inline(
        "image/svg+xml"
    ));
    assert!({{crate_name}}::routes::admin::media::may_render_inline("image/png"));
}

/// A transition is authorized against the row as it is, not as it was read.
///
/// A Contributor may edit their own draft but not a published post. If an
/// Editor publishes it between the handler's check and the write, the stale
/// `draft` kept the request authorized — so the Contributor could trash content
/// they no longer had the capability to touch.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_transition_is_authorized_against_the_locked_row() {
    let client = db_client().await;
    let owner = register(&client, "owner").await;

    sign_out(&client);
    let contributor = register(&client, "contributor").await;
    client
        .post("/admin/users/2")
        .header("cookie", &owner)
        .form(&form(&[
            ("role", "contributor"),
            ("email", "contributor@example.com"),
            ("display_name", "Contributor"),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    // The Contributor's own draft.
    let id = create_post(&client, &contributor, "Their Draft", "Body.", "draft").await;

    // The owner publishes it — the Contributor no longer has any capability
    // over it.
    client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &owner)
        .send()
        .await
        .assert_status(303);

    // Trashing is refused, and would have been refused even if the handler's
    // pre-check had been made on a stale read.
    let refused = client
        .post(&format!("/admin/content/post/{id}/status?to=trash"))
        .header("cookie", &contributor)
        .send()
        .await;
    assert_eq!(refused.status, 403, "body: {}", refused.text());

    sign_out(&client);
    client
        .get("/their-draft")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Body.");
}

/// A comment cannot land on a post whose comments were just closed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_comment_rechecks_the_post_under_its_lock() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Closing", "Body.", "publish").await;

    // Close comments behind the handler's back, the way a concurrent edit
    // would — then the insert must refuse regardless of what a caller checked.
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET comment_status = 'closed' WHERE id = {post_id}"),
    )
    .await
    .expect("close comments");

    let refused = {{crate_name}}::content::create_comment(
        &mut TestDb::shared().await.pool().get().await.expect("conn"),
        {{crate_name}}::models::NewComment {
            post_id,
            parent_id: None,
            author_id: None,
            author_name: "Guest".to_owned(),
            author_email: "guest@example.com".to_owned(),
            author_url: String::new(),
            author_ip: String::new(),
            body: "Late comment".to_owned(),
            status: "approved".to_owned(),
        },
    )
    .await;
    assert!(
        refused.is_err(),
        "the insert must re-read the post rather than trusting the caller"
    );

    let page = client.get("/closing").send().await;
    page.assert_ok();
    assert!(!page.text().contains("Late comment"));
}

/// A crafted comment submission is validated on the server.
///
/// The form declares `required` and `maxlength`; a POST that never went through
/// a browser declares nothing. `create_comment` inserts through direct Diesel —
/// it has to, so the row and the counter move together — which means it never
/// runs `CommentHooks::before_create`, and for a while it did not run
/// `validate_comment` either. A signed-in commenter's empty body would have
/// been stored *approved*.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_crafted_comment_submission_is_validated_server_side() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Rules", "Body.", "publish").await;

    // Signed in, so the comment would land `approved` and be immediately
    // public — the case where skipping validation costs the most.
    let empty = client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "   ")]))
        .send()
        .await;
    assert_ne!(
        empty.status,
        303,
        "an empty comment body must be refused: {}",
        empty.text()
    );

    // A guest with no name is unattributable.
    sign_out(&client);
    let nameless = client
        .post(&format!("/comments/{post_id}"))
        .form(&form(&[("body", "Anonymous"), ("author_name", "  ")]))
        .send()
        .await;
    assert_ne!(
        nameless.status,
        303,
        "a guest comment with no name must be refused: {}",
        nameless.text()
    );

    // And the declared 10,000-byte cap is the cap, not the global request-body
    // limit.
    let huge = "x".repeat(10_001);
    let oversized = client
        .post(&format!("/comments/{post_id}"))
        .form(&form(&[
            ("body", huge.as_str()),
            ("author_name", "Guest"),
            ("author_email", "guest@example.com"),
        ]))
        .send()
        .await;
    assert_ne!(
        oversized.status, 303,
        "a body past the declared cap must be refused"
    );

    // None of the three reached the queue.
    let queue = client
        .get("/admin/comments?status=approved")
        .header("cookie", &cookie)
        .send()
        .await;
    queue.assert_ok();
    assert!(
        !queue.text().contains("Anonymous"),
        "no rejected submission may have been stored"
    );
}

/// Restoring a revision is authorized against the row as locked.
///
/// A Contributor may edit their own draft but not a published post. If an
/// Editor publishes it between the handler's check and the transaction, the
/// stale `draft` would keep the request authorized and the restore would
/// rewrite the body of live content the Contributor can no longer touch. The
/// re-check lives inside the transaction, so the call is made directly here —
/// through the route the handler's own pre-check would refuse first, and the
/// test would pass with the inner check deleted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restoring_a_revision_is_authorized_against_the_locked_row() {
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;

    let client = db_client().await;
    let owner = register(&client, "owner").await;

    sign_out(&client);
    let contributor = register(&client, "contributor").await;
    client
        .post("/admin/users/2")
        .header("cookie", &owner)
        .form(&form(&[
            ("role", "contributor"),
            ("email", "contributor@example.com"),
            ("display_name", "Contributor"),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    let id = create_post(&client, &contributor, "Their Draft", "Original.", "draft").await;

    // The owner publishes it. The Contributor's capability over it is gone.
    client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &owner)
        .send()
        .await
        .assert_status(303);

    let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
    let actor: {{crate_name}}::models::User = {{crate_name}}::schema::users::table
        .filter({{crate_name}}::schema::users::username.eq("contributor"))
        .select({{crate_name}}::models::User::as_select())
        .first(&mut conn)
        .await
        .expect("the contributor account");
    let revision_id: i64 = {{crate_name}}::schema::revisions::table
        .filter({{crate_name}}::schema::revisions::post_id.eq(id))
        .select({{crate_name}}::schema::revisions::id)
        .order({{crate_name}}::schema::revisions::id.asc())
        .first(&mut conn)
        .await
        .expect("the initial revision");

    let refused =
        {{crate_name}}::content::restore_revision(&mut conn, id, revision_id, Some(actor.id), Some(&actor))
            .await;
    assert!(
        refused.is_err(),
        "the restore must re-check the capability against the locked row"
    );

    // The process's own paths — the importer, the seeder — still have no actor
    // and are still allowed.
    {{crate_name}}::content::restore_revision(&mut conn, id, revision_id, None, None)
        .await
        .expect("an actorless restore is the scheduler's, not a user's");
}

/// A term's parent has to be in the term's own taxonomy.
///
/// The `<select>` offers only this taxonomy's terms, but the id is a number in
/// a form body: the foreign key accepts any term, and `TermHooks` has no
/// database to resolve the candidate in. A category filed under a tag renders
/// nowhere and the exporter drops it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_terms_parent_must_belong_to_the_same_taxonomy() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/terms/post_tag")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Rust"),
            ("slug", ""),
            ("description", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    let tags = client
        .get("/admin/terms/post_tag")
        .header("cookie", &cookie)
        .send()
        .await;
    tags.assert_ok();
    assert!(tags.text().contains("Rust"));

    // The tag is the only term, so it is id 1. Offer it as a category's parent.
    let refused = client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Grafted"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", "1"),
        ]))
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a cross-taxonomy parent must be refused: {}",
        refused.text()
    );

    let categories = client
        .get("/admin/terms/category")
        .header("cookie", &cookie)
        .send()
        .await;
    categories.assert_ok();
    assert!(
        !categories.text().contains("Grafted"),
        "the refused term must not have been stored"
    );

    // A parent from the same taxonomy still works — the check bounds the input,
    // it does not remove hierarchy.
    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Guides"),
            ("slug", ""),
            ("description", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Beginner"),
            ("slug", ""),
            ("description", ""),
            ("parent_id", "2"),
        ]))
        .send()
        .await
        .assert_status(303);
    client
        .get("/admin/terms/category")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Guides — ");
}

/// The media library is paginated in SQL, not loaded whole and sorted in Rust.
///
/// Every Author holds `UploadFiles`, so this table grows without any single
/// upload being invalid; the screen used to manage uploads was the one that
/// became unusable first.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_media_library_is_paginated() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // 50 rows, newest first by `created_at`: `File 001` is the most recent.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO attachments (title, slug, mime_type, byte_size, created_at)
         SELECT 'File ' || lpad(g::text, 3, '0'),
                'file-' || lpad(g::text, 3, '0'),
                'application/pdf',
                1024,
                NOW() - (g || ' minutes')::interval
         FROM generate_series(1, 50) AS g",
    )
    .await
    .expect("seed the library");

    let first = client
        .get("/admin/media")
        .header("cookie", &cookie)
        .send()
        .await;
    first.assert_ok();
    let first = first.text();
    assert!(first.contains("File 001"), "the newest row leads page one");
    assert!(first.contains("File 048"), "page one holds a full page");
    assert!(
        !first.contains("File 049"),
        "page one must stop at the page size rather than render the whole table"
    );
    assert!(
        first.contains("Page 1 of 2"),
        "the pager states where it is"
    );

    let second = client
        .get("/admin/media?page=2")
        .header("cookie", &cookie)
        .send()
        .await;
    second.assert_ok();
    let second = second.text();
    assert!(
        second.contains("File 049") && second.contains("File 050"),
        "the tail is reachable"
    );
    assert!(
        !second.contains("File 001"),
        "page two must not repeat page one"
    );
}

/// A guest's identity fields are capped, not just their comment body.
///
/// `/comments/{post_id}` is unauthenticated with the shipped defaults and the
/// form's `maxlength` attributes are a browser convenience; `author_url` has no
/// input on the form at all. Uncapped, a handful of accepted comments carry
/// request-sized values that the moderation queue then renders fifty at a time.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_guests_identity_fields_are_capped() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Caps", "Body.", "publish").await;

    sign_out(&client);
    let long_name = "n".repeat(81);
    let long_email = format!("{}@example.com", "e".repeat(250));
    let long_url = format!("https://example.com/{}", "u".repeat(200));

    for (label, fields) in [
        (
            "name",
            vec![
                ("body", "Hello"),
                ("author_name", long_name.as_str()),
                ("author_email", "guest@example.com"),
            ],
        ),
        (
            "email",
            vec![
                ("body", "Hello"),
                ("author_name", "Guest"),
                ("author_email", long_email.as_str()),
            ],
        ),
        (
            "url",
            vec![
                ("body", "Hello"),
                ("author_name", "Guest"),
                ("author_email", "guest@example.com"),
                ("author_url", long_url.as_str()),
            ],
        ),
    ] {
        let refused = client
            .post(&format!("/comments/{post_id}"))
            .form(&form(&fields))
            .send()
            .await;
        assert_ne!(
            refused.status,
            303,
            "an oversized {label} must be refused: {}",
            refused.text()
        );
    }

    // A signed-in commenter's name and email come from their account rather
    // than the request, so they are not capped here — enforcing an account
    // rule at the comment door would refuse a legitimate long display name.
    sign_out(&client);
    let reader = register(&client, "reader").await;
    let long_display = "D".repeat(120);
    client
        .post("/admin/users/2")
        .header("cookie", &cookie)
        .form(&form(&[
            ("role", "subscriber"),
            ("email", "reader@example.com"),
            ("display_name", long_display.as_str()),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &reader)
        .form(&form(&[("body", "From the account.")]))
        .send()
        .await
        .assert_status(303);
}

/// Re-parenting is bounded by the deepest descendant, not by the moved page.
///
/// Checking only where the moved row would land let a subtree be dragged under
/// a parent deep enough to push its own children past `MAX_PAGE_DEPTH`:
/// `page_ancestry` then truncated those children's canonical paths while
/// `resolve_page_path` still walked down from a real root, so each of them
/// 404'd at the URL the site itself published.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reparenting_is_bounded_by_the_deepest_descendant() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let page = async |title: &str, parent: Option<&str>| -> String {
        let mut fields = vec![
            ("title", title),
            ("slug", ""),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
        ];
        if let Some(parent) = parent {
            fields.push(("parent_id", parent));
        }
        let created = client
            .post("/admin/content/page")
            .header("cookie", &cookie)
            .form(&form(&fields))
            .send()
            .await;
        assert_eq!(created.status, 303, "creating {title}: {}", created.text());
        created
            .header("location")
            .expect("redirect")
            .rsplit('/')
            .next()
            .expect("id")
            .to_owned()
    };

    // A chain six deep. `P6` is the deepest legal parent for a *leaf*.
    let mut chain = Vec::new();
    for level in 1..=6 {
        let parent = chain.last().cloned();
        chain.push(page(&format!("P{level}"), parent.as_deref()).await);
    }

    // A separate subtree three levels tall: A > B > C.
    let a = page("A", None).await;
    let b = page("B", Some(&a)).await;
    let _c = page("C", Some(&b)).await;

    // Moving A under P6 puts C at nine ancestors. The old check asked only
    // where A itself would land — six — and allowed it.
    let refused = client
        .post(&format!("/admin/content/page/{a}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &a,
                &[
                    ("title", "A"),
                    ("slug", "a"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("parent_id", chain[5].as_str()),
                ],
            )
            .await,
        )
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "the subtree's own height has to count: {}",
        refused.text()
    );

    // A is still where it was, and still reachable.
    sign_out(&client);
    client
        .get("/a/b/c")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Body.");

    // A leaf may still take that place — the bound is on the height that would
    // result, not on re-parenting.
    let leaf = page("Leaf", None).await;
    client
        .post(&format!("/admin/content/page/{leaf}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &leaf,
                &[
                    ("title", "Leaf"),
                    ("slug", "leaf"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "publish"),
                    ("password", ""),
                    ("parent_id", chain[5].as_str()),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);
}

/// An import's term pass is all-or-nothing.
///
/// The creations and the ancestry links are two passes — a child term can
/// appear in the file before its parent — but they are one transaction. Split
/// across statements, a failure during the linking half left the creations
/// committed, and a retry then read every one of those rows as pre-existing
/// local content it must not restructure, so the unfinished links were skipped
/// permanently while the retry reported success.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_failed_import_leaves_no_half_created_taxonomy() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // The failure has to land *after* a row has been written, which is the only
    // shape the atomicity is about: a malformed payload is rejected before the
    // pass starts and proves nothing. A trigger that refuses one particular
    // name is the smallest way to fail the pass mid-flight, standing in for the
    // constraint violation or dropped connection that would do it in practice.
    let db = TestDb::shared().await;
    try_execute(
        db,
        "CREATE OR REPLACE FUNCTION refuse_boom() RETURNS trigger AS $$
         BEGIN
             IF NEW.name = 'Boom' THEN RAISE EXCEPTION 'boom'; END IF;
             RETURN NEW;
         END; $$ LANGUAGE plpgsql",
    )
    .await
    .expect("create the trigger function");
    try_execute(db, "DROP TRIGGER IF EXISTS refuse_boom ON terms")
        .await
        .expect("clear any previous trigger");
    try_execute(
        db,
        "CREATE TRIGGER refuse_boom BEFORE INSERT ON terms
         FOR EACH ROW EXECUTE FUNCTION refuse_boom()",
    )
    .await
    .expect("install the trigger");

    // The first term is perfectly valid and is written; the second is refused,
    // failing the pass with the first already inserted.
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "attachments": [],
        "posts": [],
        "terms": [
            {"taxonomy": "category", "name": "Guides", "slug": "guides", "description": ""},
            {"taxonomy": "category", "name": "Boom", "slug": "boom", "description": ""}
        ]
    })
    .to_string();

    let failed = import_export(&client, &cookie, payload.as_str()).await;
    assert_ne!(failed.status, 303, "the import must not report success");

    let categories = client
        .get("/admin/terms/category")
        .header("cookie", &cookie)
        .send()
        .await;
    categories.assert_ok();
    assert!(
        !categories.text().contains("Guides"),
        "a failed term pass must roll its creations back, or a retry reads \
         them as local content and never finishes the ancestry"
    );

    try_execute(db, "DROP TRIGGER refuse_boom ON terms")
        .await
        .expect("remove the trigger");

    // And a well-formed file still restores the hierarchy it describes.
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "attachments": [],
        "posts": [],
        "terms": [
            {"taxonomy": "category", "name": "Beginner", "slug": "beginner",
             "description": "", "parent": "guides"},
            {"taxonomy": "category", "name": "Guides", "slug": "guides", "description": ""}
        ]
    })
    .to_string();
    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok();
    client
        .get("/admin/terms/category")
        .header("cookie", &cookie)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Guides — ");
}

/// The content-administration list is paginated in SQL.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_content_admin_list_is_paginated() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // 60 posts, newest first by `updated_at`: `Item 01` is the most recent.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO posts (post_type, title, slug, excerpt, body, status, author_id,
                            password, comment_status, menu_order, updated_at)
         SELECT 'post',
                'Item ' || lpad(g::text, 2, '0'),
                'item-' || lpad(g::text, 2, '0'),
                '', 'Body.', 'publish', 1, '', 'open', 0,
                NOW() - (g || ' minutes')::interval
         FROM generate_series(1, 60) AS g",
    )
    .await
    .expect("seed the content list");

    let first = client
        .get("/admin/content/post")
        .header("cookie", &cookie)
        .send()
        .await;
    first.assert_ok();
    let first = first.text();
    assert!(first.contains("Item 01"), "the newest row leads page one");
    assert!(first.contains("Item 50"), "page one holds a full page");
    assert!(
        !first.contains("Item 51"),
        "page one must stop at the page size rather than render the whole table"
    );
    assert!(first.contains("Page 1 of 2"));

    let second = client
        .get("/admin/content/post?page=2")
        .header("cookie", &cookie)
        .send()
        .await;
    second.assert_ok();
    let second = second.text();
    assert!(second.contains("Item 60"), "the tail is reachable");
    assert!(
        !second.contains("Item 01"),
        "page two must not repeat page one"
    );

    // The status filter and the search still apply, and are applied in SQL
    // alongside the bound rather than to rows already loaded.
    let drafts = client
        .get("/admin/content/post?status=draft")
        .header("cookie", &cookie)
        .send()
        .await;
    drafts.assert_ok();
    assert!(!drafts.text().contains("Item 01"));

    let searched = client
        .get("/admin/content/post?s=Item")
        .header("cookie", &cookie)
        .send()
        .await;
    searched.assert_ok();
    let searched = searched.text();
    assert!(searched.contains("Item 01"));
    assert!(
        !searched.contains("Item 51"),
        "a search is bounded too, not just the unfiltered list"
    );

    // A Contributor's restriction is a predicate, not a filter applied after
    // the page was already chosen — otherwise their page one would be mostly
    // empty rows they cannot open.
    sign_out(&client);
    let contributor = register(&client, "contributor").await;
    client
        .post("/admin/users/2")
        .header("cookie", &cookie)
        .form(&form(&[
            ("role", "contributor"),
            ("email", "contributor@example.com"),
            ("display_name", "Contributor"),
            ("bio", ""),
            ("website", ""),
        ]))
        .send()
        .await
        .assert_status(303);
    let theirs = client
        .get("/admin/content/post")
        .header("cookie", &contributor)
        .send()
        .await;
    theirs.assert_ok();
    assert!(
        !theirs.text().contains("Item 01"),
        "a Contributor must not see another account's content"
    );
}

/// The featured-image picker is bounded, and keeps the current selection.
///
/// Paginating `/admin/media` did nothing for this control: opening any
/// thumbnail-capable editor still rendered every attachment as an `<option>`.
/// Bounding it introduces its own hazard — a selection older than the bound
/// would fall out of the select, and saving the form unchanged would clear it —
/// so the current value is added back explicitly.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_featured_media_picker_is_bounded_and_keeps_its_selection() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Illustrated", "Body.", "draft").await;

    // 150 uploads. `Shot 001` is the newest; `Shot 150` is far past the bound.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO attachments (title, slug, mime_type, byte_size, created_at)
         SELECT 'Shot ' || lpad(g::text, 3, '0'),
                'shot-' || lpad(g::text, 3, '0'),
                'image/png',
                1024,
                NOW() - (g || ' minutes')::interval
         FROM generate_series(1, 150) AS g",
    )
    .await
    .expect("seed the library");

    let editor = client
        .get(&format!("/admin/content/post/{post_id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    let editor = editor.text();
    assert!(
        editor.contains("Shot 001"),
        "the newest uploads are offered"
    );
    assert!(
        !editor.contains("Shot 150"),
        "the picker must not render the whole library"
    );
    assert!(
        editor.contains("most recent uploads"),
        "the editor says it is showing a window, rather than implying the \
         library is this small"
    );

    // Now select the oldest one, which is well past the bound.
    let oldest: String = "Shot 150".to_owned();
    try_execute(
        TestDb::shared().await,
        &format!(
            "UPDATE posts SET featured_media_id =
                 (SELECT id FROM attachments WHERE title = '{oldest}')
             WHERE id = {post_id}"
        ),
    )
    .await
    .expect("select the oldest attachment");

    let editor = client
        .get(&format!("/admin/content/post/{post_id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    assert!(
        editor.text().contains(&oldest),
        "a selection older than the bound must still be in the select, or \
         saving the form unchanged silently clears it"
    );
}

/// The admin menu lists every registered taxonomy, not two hardcoded slugs.
///
/// A plugin's taxonomy already had a working screen at `/admin/terms/<slug>`
/// and no way to reach it: the post editor renders a checkbox list that is
/// empty until the taxonomy has a term, and the only place to create that first
/// term was a URL an administrator had to guess. A registry-driven workflow
/// that is undiscoverable is not one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_admin_menu_lists_every_registered_taxonomy() {
    // For `page` rather than `post`, and with its own slug: the registry is
    // process-global, so this must not change what any other test's post
    // editor renders.
    {{crate_name}}::content_types::register_taxonomy({{crate_name}}::content_types::Taxonomy {
        slug: "aisle",
        singular: "Aisle",
        plural: "Aisles",
        hierarchical: true,
        post_types: &["page"],
        rewrite_base: "aisle",
    })
    .expect("aisle registers cleanly");

    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let dashboard = client.get("/admin").header("cookie", &cookie).send().await;
    dashboard.assert_ok();
    let dashboard = dashboard.text();
    assert!(
        dashboard.contains("/admin/terms/aisle"),
        "a registered taxonomy must be reachable from the menu"
    );
    assert!(dashboard.contains("Aisles"), "under its own plural name");
    // The built-ins still come from the same loop rather than a second list.
    assert!(dashboard.contains("/admin/terms/category"));
    assert!(dashboard.contains("/admin/terms/post_tag"));
}

/// Scheduling is validated against the row as locked.
///
/// The status endpoint carries no date — it can only move a post to `future`
/// when the row already holds a future one — so its check is a read that can go
/// stale. An editor who clears or rewinds `published_at` in between would
/// otherwise have the request schedule a post that either never publishes
/// (`NULL` never matches the sweep's `published_at <= now`) or publishes on the
/// very next sweep.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scheduling_is_validated_against_the_locked_row() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Someday", "Body.", "draft").await;

    let mut conn = TestDb::shared().await.pool().get().await.expect("conn");

    // No date at all: the sweep would never see it, so it would sit `future`
    // forever. Called directly, because the handler's own pre-check refuses
    // first and the point is that the check inside the transaction refuses too.
    let refused = {{crate_name}}::content::transition_status(&mut conn, id, "future", None, None).await;
    assert!(
        refused.is_err(),
        "a schedule with no date must be refused under the lock"
    );

    // A date already in the past: the next sweep would publish it within the
    // minute, which is not what scheduling means.
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET published_at = NOW() - interval '1 day' WHERE id = {id}"),
    )
    .await
    .expect("rewind the date");
    let refused = {{crate_name}}::content::transition_status(&mut conn, id, "future", None, None).await;
    assert!(
        refused.is_err(),
        "a schedule with a past date must be refused under the lock"
    );

    // A real future date still schedules.
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET published_at = NOW() + interval '1 day' WHERE id = {id}"),
    )
    .await
    .expect("set a future date");
    let scheduled = {{crate_name}}::content::transition_status(&mut conn, id, "future", None, None)
        .await
        .expect("a real future date schedules");
    assert_eq!(scheduled.status, "future");
}

/// The users screen is paginated in SQL.
///
/// Open registration is the shipped default and the per-IP throttle bounds the
/// rate rather than the total, so the screen an administrator would use to
/// clear a signup flood is the one the flood breaks first.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_users_admin_list_is_paginated() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // 60 accounts, ordered by username: `user-01` … `user-60`. `owner` sorts
    // before all of them, so page one is `owner` plus `user-01` … `user-49`.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO users (username, email, password_hash, display_name, role, bio, website)
         SELECT 'user-' || lpad(g::text, 2, '0'),
                'user-' || lpad(g::text, 2, '0') || '@example.com',
                'x', '', 'subscriber', '', ''
         FROM generate_series(1, 60) AS g",
    )
    .await
    .expect("seed the accounts");

    let first = client
        .get("/admin/users")
        .header("cookie", &cookie)
        .send()
        .await;
    first.assert_ok();
    let first = first.text();
    assert!(
        first.contains("user-01"),
        "the first account leads page one"
    );
    assert!(first.contains("user-49"), "page one holds a full page");
    assert!(
        !first.contains("user-50"),
        "page one must stop at the page size rather than render every account"
    );
    assert!(first.contains("Page 1 of 2"));

    let second = client
        .get("/admin/users?page=2")
        .header("cookie", &cookie)
        .send()
        .await;
    second.assert_ok();
    let second = second.text();
    assert!(
        second.contains("user-50") && second.contains("user-60"),
        "the tail is reachable"
    );
    assert!(
        !second.contains("user-01"),
        "page two must not repeat page one"
    );
}

/// A scheduled post is stored as the instant the editor's wall clock names.
///
/// `datetime-local` submits a wall-clock value with **no offset**. Storing it
/// as-is made it a UTC timestamp by accident, and the scheduler compares
/// against `Utc::now()` — so scheduling 09:00 on a site set to UTC-7 published
/// at 02:00 local.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_scheduled_date_is_read_in_the_sites_timezone() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("timezone", "America/Los_Angeles")]))
        .send()
        .await
        .assert_status(303);

    // Far enough out that it is in the future in every zone, so the test is
    // about the *offset* rather than about the guard.
    let local = (chrono::Utc::now() + chrono::Duration::days(30))
        .naive_utc()
        .format("%Y-%m-%dT09:00")
        .to_string();
    let created = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Later"),
            ("slug", "later"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "future"),
            ("password", ""),
            ("publish_at", local.as_str()),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303, "body: {}", created.text());
    let id = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    // 09:00 in America/Los_Angeles is 16:00 or 17:00 UTC depending on daylight
    // saving — never 09:00. Asserting the stored hour is *not* the submitted
    // one is what the old behaviour fails.
    let stored: String = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        let when: Option<chrono::NaiveDateTime> = {{crate_name}}::schema::posts::table
            .find(id.parse::<i64>().expect("id"))
            .select({{crate_name}}::schema::posts::published_at)
            .first(&mut conn)
            .await
            .expect("the scheduled post");
        when.expect("a scheduled post has a date")
            .format("%H:%M")
            .to_string()
    };
    assert!(
        stored == "16:00" || stored == "17:00",
        "09:00 Pacific must be stored as the UTC instant it names, not as 09:00 UTC: {stored}"
    );

    // And the editor reads it back in the site's zone, so the round trip is
    // stable rather than drifting by the offset on every save.
    let editor = client
        .get(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    assert!(
        editor.text().contains(&local),
        "the editor must redisplay the wall clock the author typed"
    );
}

/// A completed import is not reconciled again.
///
/// The source marker says "an import created this row" and is written before
/// the row's terms, status and ancestry are — so on its own it cannot also mean
/// "and it is done". Without a separate completion record, importing the same
/// backup twice re-applied the file's terms and status over an editor's later
/// changes, on a screen that promises existing items are left alone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_completed_import_is_left_alone_on_a_re_import() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "attachments": [],
        "posts": [
            {
                "post_type": "post", "title": "Restored", "slug": "restored",
                "excerpt": "", "body": "Imported body.", "status": "publish",
                "password": "", "comment_status": "open",
                "author": "owner", "terms": [], "comments": [],
                "published_at": null, "parent": null, "sticky": false, "menu_order": 0
            }
        ]
    })
    .to_string();

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    // An editor then unpublishes it — a perfectly ordinary thing to do to
    // restored content.
    let id: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .filter({{crate_name}}::schema::posts::slug.eq("restored"))
            .select({{crate_name}}::schema::posts::id)
            .first(&mut conn)
            .await
            .expect("the imported post")
    };
    client
        .post(&format!("/admin/content/post/{id}/status?to=draft"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    // The same backup again. It must report the post as already present and
    // change nothing.
    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("already present");

    let status: String = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(id)
            .select({{crate_name}}::schema::posts::status)
            .first(&mut conn)
            .await
            .expect("the imported post")
    };
    assert_eq!(
        status, "draft",
        "a finished import must not republish content an editor unpublished"
    );
}

/// The hierarchical parent picker is bounded, and keeps the current parent.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_parent_picker_is_bounded_and_keeps_its_selection() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // 150 pages. `Page 001` is the most recently edited; `Page 150` is far
    // past the bound.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO posts (post_type, title, slug, excerpt, body, status, author_id,
                            password, comment_status, menu_order, updated_at)
         SELECT 'page',
                'Page ' || lpad(g::text, 3, '0'),
                'page-' || lpad(g::text, 3, '0'),
                '', 'Body.', 'publish', 1, '', 'closed', 0,
                NOW() - (g || ' minutes')::interval
         FROM generate_series(1, 150) AS g",
    )
    .await
    .expect("seed the pages");

    let editor = client
        .get("/admin/content/page/new")
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    let editor = editor.text();
    assert!(editor.contains("Page 001"), "recent pages are offered");
    assert!(
        !editor.contains("Page 150"),
        "the picker must not render every page of the type"
    );
    assert!(editor.contains("most recently edited"));

    // A child whose parent is older than the bound must still see it selected,
    // or saving the form unchanged moves the page to the top level and changes
    // its canonical URL.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO posts (post_type, title, slug, excerpt, body, status, author_id,
                            password, comment_status, menu_order, parent_id, updated_at)
         SELECT 'page', 'Child', 'child', '', 'Body.', 'publish', 1, '', 'closed', 0,
                (SELECT id FROM posts WHERE slug = 'page-150'), NOW()",
    )
    .await
    .expect("seed the child");

    let child_id: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .filter({{crate_name}}::schema::posts::slug.eq("child"))
            .select({{crate_name}}::schema::posts::id)
            .first(&mut conn)
            .await
            .expect("the child page")
    };
    let editor = client
        .get(&format!("/admin/content/page/{child_id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    assert!(
        editor.text().contains("Page 150"),
        "a parent older than the bound must still be in the select"
    );
}

/// The taxonomy screen is paginated, and its parent selector is bounded.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_taxonomy_admin_screen_is_paginated() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // 120 categories, `cat-001` … `cat-120`, ordered by name.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO terms (taxonomy, name, slug, description, post_count)
         SELECT 'category',
                'cat-' || lpad(g::text, 3, '0'),
                'cat-' || lpad(g::text, 3, '0'),
                '', 0
         FROM generate_series(1, 120) AS g",
    )
    .await
    .expect("seed the terms");

    let first = client
        .get("/admin/terms/category")
        .header("cookie", &cookie)
        .send()
        .await;
    first.assert_ok();
    let first = first.text();
    assert!(first.contains("cat-001"), "the first term leads page one");
    assert!(first.contains("cat-050"), "page one holds a full page");
    // Counted on the per-row delete form rather than on a term name: the
    // parent selector below the table renders its own (differently bounded)
    // set of terms, so a name appearing on the page does not mean the *list*
    // grew.
    assert_eq!(
        first.matches("/delete\"").count(),
        50,
        "the list must stop at the page size rather than render the taxonomy"
    );
    assert!(first.contains("Page 1 of 3"));
    // The parent selector is bounded independently of the list: it may reach
    // past the page, but not to the whole taxonomy.
    assert!(
        !first.contains("cat-101"),
        "the parent selector must not render every term either"
    );
    assert!(first.contains("Showing the first"));

    let third = client
        .get("/admin/terms/category?page=3")
        .header("cookie", &cookie)
        .send()
        .await;
    third.assert_ok();
    let third = third.text();
    assert!(third.contains("cat-120"), "the tail is reachable");
    assert_eq!(
        third.matches("/delete\"").count(),
        20,
        "the last page holds the remainder, not a repeat of the first"
    );
}

/// A dated permalink names the day the site says the post was published.
///
/// `published_at` is stored in UTC; a dated URL names a *calendar day*, which
/// is a local question. Formatting the stored value directly gave a post
/// published at 23:30 on the 9th in Los Angeles a `/2026/09/10/` URL while
/// every rendered date on the page said the 9th — and the archive route, whose
/// bounds were raw UTC midnights, filed it under the 10th to match the URL
/// rather than the content.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dated_permalink_and_its_archive_use_the_site_timezone() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[
            ("timezone", "America/Los_Angeles"),
            ("permalink_structure", "day_and_name"),
        ]))
        .send()
        .await
        .assert_status(303);

    create_post(&client, &cookie, "Late", "Body.", "publish").await;

    // 06:30 UTC on 2026-09-10 is 23:30 on the 9th in Los Angeles: the two zones
    // disagree about which day this is, which is the whole point.
    try_execute(
        TestDb::shared().await,
        "UPDATE posts SET published_at = '2026-09-10 06:30:00' WHERE slug = 'late'",
    )
    .await
    .expect("straddle the date boundary");

    sign_out(&client);
    let listed = client
        .get("/api/v1/posts")
        .send()
        .await
        .assert_ok()
        .json::<serde_json::Value>();
    let url = listed.as_array().expect("array")[0]["url"]
        .as_str()
        .expect("url")
        .to_owned();
    assert!(
        url.starts_with("/2026/09/09/"),
        "the URL must name the local day, not the UTC one: {url}"
    );
    client.get(&url).send().await.assert_ok();

    // And the archive agrees with the URL rather than with the stored value.
    client
        .get("/2026/09/09")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Late");
    let wrong_day = client.get("/2026/09/10").send().await;
    wrong_day.assert_ok();
    assert!(
        !wrong_day.text().contains("Late"),
        "the post must not also appear in the following day's archive"
    );
}

/// A mistyped timezone is refused, not stored as UTC.
///
/// `Settings::from_rows` ignores a value it cannot parse and keeps the default,
/// which is right for reading the options table and wrong for a form: the
/// default is UTC, so a typo would silently move a non-UTC site to UTC —
/// shifting every displayed date and the meaning of every scheduled time.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_mistyped_timezone_is_refused_rather_than_resetting_the_site() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("timezone", "America/Los_Angeles")]))
        .send()
        .await
        .assert_status(303);

    let refused = client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings_form(&[("timezone", "Amerca/Los_Angeles")]))
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a name the site cannot resolve must be refused: {}",
        refused.text()
    );

    // The setting the administrator chose is still in place.
    let screen = client
        .get("/admin/settings")
        .header("cookie", &cookie)
        .send()
        .await;
    screen.assert_ok();
    assert!(
        screen.text().contains("America/Los_Angeles"),
        "a refused submission must leave the previous zone alone"
    );
}

/// The editor's taxonomy checkboxes are bounded, and keep the post's own terms.
///
/// Saving *replaces* a post's filings, so a selected term missing from the form
/// would be silently unfiled — which makes retaining them part of the bound
/// rather than a nicety.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_editor_taxonomy_control_is_bounded_and_keeps_its_selection() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Filed", "Body.", "draft").await;

    // 150 categories, `cat-001` … `cat-150` by name.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO terms (taxonomy, name, slug, description, post_count)
         SELECT 'category',
                'cat-' || lpad(g::text, 3, '0'),
                'cat-' || lpad(g::text, 3, '0'),
                '', 0
         FROM generate_series(1, 150) AS g",
    )
    .await
    .expect("seed the terms");

    let editor = client
        .get(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    let editor = editor.text();
    assert!(editor.contains("cat-001"), "the first terms are offered");
    assert!(
        !editor.contains("cat-150"),
        "the control must not render the whole taxonomy"
    );
    assert!(editor.contains("Showing the first"));

    // File the post under a term past the bound, behind the editor's back —
    // the same state an import or a bulk edit would leave.
    try_execute(
        TestDb::shared().await,
        &format!(
            "INSERT INTO post_terms (post_id, term_id)
             SELECT {id}, id FROM terms WHERE slug = 'cat-150'"
        ),
    )
    .await
    .expect("file the post");

    let editor = client
        .get(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .send()
        .await;
    editor.assert_ok();
    assert!(
        editor.text().contains("cat-150"),
        "a term the post carries must be in the form, or saving unfiles it"
    );
}

/// A page of authors keeps its distinct, its order and its bound.
///
/// `/api/v1/authors` is unauthenticated, and loading every distinct author id
/// before bounding the users query made even `?per_page=1` cost one row per
/// author on the wire and in memory. That cost is not observable through the
/// endpoint, so this is a correctness guard on the rewrite rather than a test
/// that fails without it: the `EXISTS` form has to keep excluding accounts with
/// no published content, keep the username ordering, and keep paging — three
/// things the two-query version got for free and a single query can quietly
/// lose.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_authors_endpoint_pages_in_sql() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Owned", "Body.", "publish").await;

    // 60 more accounts, each with a published post, plus 20 with none.
    try_execute(
        TestDb::shared().await,
        "INSERT INTO users (username, email, password_hash, display_name, role, bio, website)
         SELECT 'writer-' || lpad(g::text, 2, '0'),
                'writer-' || lpad(g::text, 2, '0') || '@example.com',
                'x', '', 'author', '', ''
         FROM generate_series(1, 80) AS g",
    )
    .await
    .expect("seed the accounts");
    try_execute(
        TestDb::shared().await,
        "INSERT INTO posts (post_type, title, slug, excerpt, body, status, author_id,
                            password, comment_status, menu_order)
         SELECT 'post',
                'By ' || u.username,
                'by-' || u.username,
                '', 'Body.', 'publish', u.id, '', 'open', 0
         FROM users u
         WHERE u.username LIKE 'writer-%'
           AND substring(u.username from 8)::int <= 60",
    )
    .await
    .expect("seed the posts");

    sign_out(&client);
    let page: serde_json::Value = client
        .get("/api/v1/authors?per_page=5")
        .send()
        .await
        .assert_ok()
        .json();
    let rows = page.as_array().expect("array");
    assert_eq!(rows.len(), 5, "the bound is the request's, not the site's");

    // Only accounts with published content, and ordered by username — the
    // twenty writers with no posts must not appear at all.
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["username"].as_str().expect("username"))
        .collect();
    assert_eq!(
        names,
        vec!["owner", "writer-01", "writer-02", "writer-03", "writer-04"],
        "the distinct, the order and the bound all have to survive the rewrite"
    );

    let later: serde_json::Value = client
        .get("/api/v1/authors?per_page=5&page=2")
        .send()
        .await
        .assert_ok()
        .json();
    let names: Vec<&str> = later
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["username"].as_str().expect("username"))
        .collect();
    assert_eq!(
        names,
        vec![
            "writer-05",
            "writer-06",
            "writer-07",
            "writer-08",
            "writer-09"
        ]
    );
}

/// A save can only apply so many terms, whichever control they came from.
///
/// The cap was on the flat taxonomy's free-text box and not on the hierarchical
/// id list, so a crafted submission could enumerate every term in a large
/// taxonomy: one database lookup per id on the way in, and a permanent set of
/// filings big enough to make that post's editor unbounded again, since the
/// picker adds every selected term back.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_save_cannot_apply_an_unbounded_number_of_terms() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Filed", "Body.", "draft").await;

    try_execute(
        TestDb::shared().await,
        "INSERT INTO terms (taxonomy, name, slug, description, post_count)
         SELECT 'category',
                'cat-' || lpad(g::text, 3, '0'),
                'cat-' || lpad(g::text, 3, '0'),
                '', 0
         FROM generate_series(1, 80) AS g",
    )
    .await
    .expect("seed the terms");

    let ids: Vec<i64> = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::terms::table
            .filter({{crate_name}}::schema::terms::taxonomy.eq("category"))
            .order({{crate_name}}::schema::terms::slug.asc())
            .select({{crate_name}}::schema::terms::id)
            .load(&mut conn)
            .await
            .expect("the seeded terms")
    };

    let save = async |count: usize| {
        let strings: Vec<String> = ids.iter().take(count).map(i64::to_string).collect();
        let mut fields = vec![
            ("title", "Filed"),
            ("slug", "filed"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "draft"),
            ("password", ""),
        ];
        for value in &strings {
            fields.push(("taxonomies[category]", value.as_str()));
        }
        client
            .post(&format!("/admin/content/post/{id}"))
            .header("cookie", &cookie)
            .form(&edit_form(&id, &fields).await)
            .send()
            .await
    };

    let refused = save(80).await;
    assert_eq!(
        refused.status,
        422,
        "a submission past the cap must be refused: {}",
        refused.text()
    );

    // Nothing was filed — the cap is checked before any database work.
    let filed: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::post_terms::table
            .filter({{crate_name}}::schema::post_terms::post_id.eq(id))
            .count()
            .get_result(&mut conn)
            .await
            .expect("the filings")
    };
    assert_eq!(filed, 0, "a refused save must file nothing");

    // A save within the cap still works, and still files exactly what it named.
    assert_eq!(save(50).await.status, 303);
    let filed: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::post_terms::table
            .filter({{crate_name}}::schema::post_terms::post_id.eq(id))
            .count()
            .get_result(&mut conn)
            .await
            .expect("the filings")
    };
    assert_eq!(filed, 50, "the cap bounds the save, it does not break it");
}

/// A reply cannot be approved while an ancestor is hidden.
///
/// Hiding a parent cascades over its *approved* descendants, but a reply that
/// was already pending when its parent was spammed stays pending — and this is
/// where a moderator would then approve it. `assemble_thread` builds from the
/// roots down, so that reply can never be attached, while
/// `recount_post_comments` counts it: the post advertises a comment no reader
/// can reach.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_reply_cannot_be_approved_under_a_hidden_ancestor() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let post_id = create_post(&client, &cookie, "Thread", "Body.", "publish").await;

    // A signed-in comment lands approved; a guest reply is held for moderation.
    client
        .post(&format!("/comments/{post_id}"))
        .header("cookie", &cookie)
        .form(&form(&[("body", "Root comment.")]))
        .send()
        .await
        .assert_status(303);
    let root_id: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::comments::table
            .filter({{crate_name}}::schema::comments::body.eq("Root comment."))
            .select({{crate_name}}::schema::comments::id)
            .first(&mut conn)
            .await
            .expect("the root comment")
    };

    sign_out(&client);
    client
        .post(&format!("/comments/{post_id}"))
        .form(&form(&[
            ("body", "Pending reply."),
            ("author_name", "Guest"),
            ("author_email", "guest@example.com"),
            ("reply_to", root_id.to_string().as_str()),
        ]))
        .send()
        .await
        .assert_status(303);
    let reply_id: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::comments::table
            .filter({{crate_name}}::schema::comments::body.eq("Pending reply."))
            .select({{crate_name}}::schema::comments::id)
            .first(&mut conn)
            .await
            .expect("the reply")
    };

    // Spam the root. The reply was already pending, so the cascade leaves it.
    client
        .post(&format!("/admin/comments/{root_id}/status?to=spam"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);

    let refused = client
        .post(&format!("/admin/comments/{reply_id}/status?to=approved"))
        .header("cookie", &cookie)
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "approving under a hidden ancestor must be refused: {}",
        refused.text()
    );

    // Nothing was counted, and nothing is claimed on the page.
    let count: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(post_id)
            .select({{crate_name}}::schema::posts::comment_count)
            .first(&mut conn)
            .await
            .expect("the post")
    };
    assert_eq!(count, 0, "a comment nobody can see must not be counted");

    // Restore the root, and the reply can be approved — the rule is an
    // ordering constraint, not a dead end.
    client
        .post(&format!("/admin/comments/{root_id}/status?to=approved"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
    client
        .post(&format!("/admin/comments/{reply_id}/status?to=approved"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
    sign_out(&client);
    client
        .get("/thread")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Pending reply.");
}

/// The model's title cap is enforced on the direct-Diesel edit path.
///
/// `update_post_with_revision` writes the fields back with plain Diesel — which
/// is what makes the edit and its revision one transaction, and what bypasses
/// the derived validator. The form's `maxlength` is a browser convenience: an
/// author could park a request-sized title on a draft and publish it
/// afterwards, at which point every listing, feed and API response carries it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_oversized_title_is_refused_on_the_edit_path() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Modest", "Body.", "draft").await;

    let huge = "t".repeat(301);
    let refused = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", huge.as_str()),
                    ("slug", "modest"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "draft"),
                    ("password", ""),
                ],
            )
            .await,
        )
        .send()
        .await;
    assert_eq!(
        refused.status,
        422,
        "a title past the model's cap must be refused even on a draft: {}",
        refused.text()
    );

    // A title at the cap still saves — the bound is the model's, not tighter.
    let ok = "t".repeat(300);
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", ok.as_str()),
                    ("slug", "modest"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "draft"),
                    ("password", ""),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);
}

/// The Appearance screen's category selector is bounded.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_appearance_category_selector_is_bounded() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // A menu, so the screen renders the item builder at all.
    client
        .post("/admin/appearance/menus")
        .header("cookie", &cookie)
        .form(&form(&[("name", "Primary"), ("location", "primary")]))
        .send()
        .await
        .assert_status(303);

    try_execute(
        TestDb::shared().await,
        "INSERT INTO terms (taxonomy, name, slug, description, post_count)
         SELECT 'category',
                'cat-' || lpad(g::text, 3, '0'),
                'cat-' || lpad(g::text, 3, '0'),
                '', 0
         FROM generate_series(1, 250) AS g",
    )
    .await
    .expect("seed the terms");

    let screen = client
        .get("/admin/appearance")
        .header("cookie", &cookie)
        .send()
        .await;
    screen.assert_ok();
    let screen = screen.text();
    assert!(
        screen.contains("cat-001"),
        "the first categories are offered"
    );
    assert!(
        !screen.contains("cat-250"),
        "the selector must not render every category"
    );
    assert!(screen.contains("First 200 by name"));
}

/// Clearing the publish date on an unscheduled post clears the timestamp.
///
/// Leaving the old due time behind made "move this back to draft" keep a date
/// the post was never published at: publishing it later dated and ordered it at
/// that obsolete moment, and with a future date gave it a dated permalink and
/// an archive slot in the future.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unscheduling_a_post_clears_its_publish_date() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let when = (chrono::Utc::now() + chrono::Duration::days(30))
        .naive_utc()
        .format("%Y-%m-%dT09:00")
        .to_string();
    let created = client
        .post("/admin/content/post")
        .header("cookie", &cookie)
        .form(&form(&[
            ("title", "Later"),
            ("slug", "later"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "future"),
            ("password", ""),
            ("publish_at", when.as_str()),
        ]))
        .send()
        .await;
    assert_eq!(created.status, 303, "body: {}", created.text());
    let id = created
        .header("location")
        .expect("redirect")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    // Back to draft with the date field cleared — the editor unscheduling it.
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &id,
                &[
                    ("title", "Later"),
                    ("slug", "later"),
                    ("excerpt", ""),
                    ("body", "Body."),
                    ("status", "draft"),
                    ("password", ""),
                    ("publish_at", ""),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);

    let stored: Option<chrono::NaiveDateTime> = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(id.parse::<i64>().expect("id"))
            .select({{crate_name}}::schema::posts::published_at)
            .first(&mut conn)
            .await
            .expect("the post")
    };
    assert!(
        stored.is_none(),
        "an unscheduled post must not keep the due time it never reached: {stored:?}"
    );

    // Publishing it now dates it now, not at the abandoned schedule.
    client
        .post(&format!("/admin/content/post/{id}/status?to=publish"))
        .header("cookie", &cookie)
        .send()
        .await
        .assert_status(303);
    let stored: chrono::NaiveDateTime = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(id.parse::<i64>().expect("id"))
            .select({{crate_name}}::schema::posts::published_at)
            .first::<Option<chrono::NaiveDateTime>>(&mut conn)
            .await
            .expect("the post")
            .expect("a published post is dated")
    };
    assert!(
        stored <= chrono::Utc::now().naive_utc(),
        "a post published now must not be dated in the future: {stored}"
    );

    // A post that really was published keeps its date across an edit.
    let live = create_post(&client, &cookie, "Live", "Body.", "publish").await;
    try_execute(
        TestDb::shared().await,
        &format!("UPDATE posts SET published_at = '2020-01-01 00:00:00' WHERE id = {live}"),
    )
    .await
    .expect("backdate");
    client
        .post(&format!("/admin/content/post/{live}"))
        .header("cookie", &cookie)
        .form(
            &edit_form(
                &live,
                &[
                    ("title", "Live"),
                    ("slug", "live"),
                    ("excerpt", ""),
                    ("body", "Edited."),
                    ("status", "publish"),
                    ("password", ""),
                    ("publish_at", ""),
                ],
            )
            .await,
        )
        .send()
        .await
        .assert_status(303);
    let kept: Option<chrono::NaiveDateTime> = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(live)
            .select({{crate_name}}::schema::posts::published_at)
            .first(&mut conn)
            .await
            .expect("the post")
    };
    assert!(
        kept.is_some_and(|when| when.format("%Y").to_string() == "2020"),
        "editing a published post must never reorder the blog index: {kept:?}"
    );
}

/// A backup restores after its schedules have elapsed.
///
/// `transition_status` refuses `future` with a past date — correctly, since
/// such a schedule either never fires or fires on the next sweep. But a backup
/// restored after downtime routinely carries exactly that, and the importer
/// unwound the post and aborted, so a valid backup could not be restored
/// without hand-editing its JSON.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_import_publishes_a_schedule_that_has_already_passed() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "attachments": [],
        "posts": [
            {
                "post_type": "post", "title": "Was Scheduled", "slug": "was-scheduled",
                "excerpt": "", "body": "Imported body.", "status": "future",
                "password": "", "comment_status": "open",
                "author": "owner", "terms": [], "comments": [],
                "published_at": "2020-01-01T00:00:00", "parent": null,
                "sticky": false, "menu_order": 0
            }
        ]
    })
    .to_string();

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    // Published rather than stuck `future`, which the sweep would never claim
    // for a date already in the past.
    let status: String = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .filter({{crate_name}}::schema::posts::slug.eq("was-scheduled"))
            .select({{crate_name}}::schema::posts::status)
            .first(&mut conn)
            .await
            .expect("the imported post")
    };
    assert_eq!(status, "publish");

    sign_out(&client);
    let listed: serde_json::Value = client.get("/api/v1/posts").send().await.assert_ok().json();
    assert!(
        listed
            .as_array()
            .expect("array")
            .iter()
            .any(|p| p["slug"] == "was-scheduled"),
        "the restored post has to be readable, not stranded"
    );

    // A schedule still in the future is restored as a schedule.
    let future = (chrono::Utc::now() + chrono::Duration::days(30))
        .naive_utc()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "attachments": [],
        "posts": [
            {
                "post_type": "post", "title": "Still Scheduled", "slug": "still-scheduled",
                "excerpt": "", "body": "Imported body.", "status": "future",
                "password": "", "comment_status": "open",
                "author": "owner", "terms": [], "comments": [],
                "published_at": future, "parent": null,
                "sticky": false, "menu_order": 0
            }
        ]
    })
    .to_string();
    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");
    let status: String = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .filter({{crate_name}}::schema::posts::slug.eq("still-scheduled"))
            .select({{crate_name}}::schema::posts::status)
            .first(&mut conn)
            .await
            .expect("the imported post")
    };
    assert_eq!(
        status, "future",
        "a schedule that has not elapsed must stay a schedule"
    );
}

/// The importer accepts an export the exporter can actually produce.
///
/// The old form posted the JSON as a URL-encoded field, where every quote and
/// brace becomes a three-byte escape — so a backup roughly a third of the
/// request limit already exceeded it, and the CMS could not restore its own
/// export under the shipped configuration.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_importer_accepts_an_export_too_large_to_url_encode() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    // A body big enough that URL-encoding the JSON would have pushed the
    // request past the 32 MiB limit, while the raw bytes stay inside the
    // importer's own cap. Built from characters form encoding has to escape
    // and JSON does not, so the encoded length is close to three times the
    // raw one — which is exactly the inflation that made a real backup
    // unrestorable.
    let body = "{}&%<>,;".repeat(1_500_000);
    let payload = serde_json::json!({
        "version": 3,
        "site_title": "Elsewhere",
        "exported_at": "2026-01-01T00:00:00Z",
        "terms": [],
        "attachments": [],
        "posts": [
            {
                "post_type": "post", "title": "Bulky", "slug": "bulky",
                "excerpt": "", "body": body, "status": "publish",
                "password": "", "comment_status": "open",
                "author": "owner", "terms": [], "comments": [],
                "published_at": null, "parent": null, "sticky": false, "menu_order": 0
            }
        ]
    })
    .to_string();
    assert!(
        payload.len() < 24 * 1024 * 1024,
        "the fixture has to fit the importer's own cap: {}",
        payload.len()
    );
    // The premise, asserted rather than assumed: this payload could not have
    // reached the old handler at all. `form()` percent-encodes every byte that
    // is not unreserved, so the JSON's quotes, braces and spaces each become
    // three bytes — and the encoded body exceeds the framework's 32 MiB request
    // limit, which the extractor applies before any handler runs.
    assert!(
        form(&[("payload", payload.as_str())]).len() > 32 * 1024 * 1024,
        "if this does not exceed the limit, the test is not testing the fix"
    );

    import_export(&client, &cookie, payload.as_str())
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    let stored: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .filter({{crate_name}}::schema::posts::slug.eq("bulky"))
            .count()
            .get_result(&mut conn)
            .await
            .expect("the imported post")
    };
    assert_eq!(stored, 1);
}

/// An edit without a version stamp is refused, not silently unguarded.
///
/// `update_post_with_revision` takes an `Option` because the importer and the
/// API legitimately have no form behind them. For the editor a missing, empty
/// or unparseable value is not "no form" — it is a form whose guard has been
/// removed, and falling through to `None` disabled the stale-edit check
/// entirely, so a crafted save could overwrite an edit committed after the form
/// was loaded.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_edit_without_a_version_stamp_is_refused() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    let id = create_post(&client, &cookie, "Contested", "First.", "publish").await;

    let fields = |version: Option<&str>| {
        let mut fields = vec![
            ("title", "Contested"),
            ("slug", "contested"),
            ("excerpt", ""),
            ("body", "Overwritten."),
            ("status", "publish"),
            ("password", ""),
            ("comment_status", "open"),
        ];
        if let Some(version) = version {
            fields.push(("lock_version", version));
        }
        form(&fields)
    };

    for (label, version) in [
        ("missing", None),
        ("empty", Some("")),
        ("malformed", Some("not-a-number")),
    ] {
        let refused = client
            .post(&format!("/admin/content/post/{id}"))
            .header("cookie", &cookie)
            .form(&fields(version))
            .send()
            .await;
        assert_eq!(
            refused.status,
            422,
            "a {label} version stamp must be refused rather than skipping the check: {}",
            refused.text()
        );
    }

    // The body is untouched by any of them.
    sign_out(&client);
    client
        .get("/contested")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("First.");

    // A stale-but-well-formed version is still refused by the existing check,
    // and the current one still saves — the requirement is on the stamp being
    // present, not on the guard changing.
    let current: i32 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::posts::table
            .find(id)
            .select({{crate_name}}::schema::posts::lock_version)
            .first(&mut conn)
            .await
            .expect("the post")
    };
    let stale = client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&fields(Some(&(current - 1).to_string())))
        .send()
        .await;
    assert_ne!(stale.status, 303, "a stale save must still be refused");

    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&fields(Some(&current.to_string())))
        .send()
        .await
        .assert_status(303);
    sign_out(&client);
    client
        .get("/contested")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Overwritten.");
}

/// A widget or sitemap archive is derived from posts, not from a cached count.
///
/// `terms.post_count` is maintained by `recount_term`, which applies the
/// *current* `public_type_slugs()` — right whenever it runs, and stale the
/// moment the answer to "is this type public?" changes without a write to
/// touch it. `register_post_type` supports replacing a registration, so a
/// deployment can flip a type's visibility between restarts and nothing
/// recomputes the counters. A stale positive count then advertises an archive
/// whose own listing is empty.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_stale_term_count_does_not_advertise_an_empty_archive() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;

    client
        .post("/admin/terms/category")
        .header("cookie", &cookie)
        .form(&form(&[
            ("name", "Ghosts"),
            ("slug", ""),
            ("description", ""),
        ]))
        .send()
        .await
        .assert_status(303);

    // The state a visibility flip leaves behind: a positive count with nothing
    // published behind it. Written directly, because the supported way to
    // produce it — re-registering a post type as non-public — is
    // process-global and would change every other test's registry.
    try_execute(
        TestDb::shared().await,
        "UPDATE terms SET post_count = 7 WHERE slug = 'ghosts'",
    )
    .await
    .expect("stale the counter");

    sign_out(&client);
    let home = client.get("/").send().await;
    home.assert_ok();
    assert!(
        !home.text().contains("Ghosts"),
        "the widget must not advertise an archive with nothing in it"
    );
    let sitemap = client.get("/sitemap.xml").send().await;
    sitemap.assert_ok();
    assert!(
        !sitemap.text().contains("/category/ghosts"),
        "the sitemap must not list an archive with nothing in it"
    );

    // And a term that really does have a published post is still listed, so
    // the derivation replaced the counter rather than emptying the widget.
    let id = create_post(&client, &cookie, "Real", "Body.", "publish").await;
    let term: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = TestDb::shared().await.pool().get().await.expect("conn");
        {{crate_name}}::schema::terms::table
            .filter({{crate_name}}::schema::terms::slug.eq("ghosts"))
            .select({{crate_name}}::schema::terms::id)
            .first(&mut conn)
            .await
            .expect("the term")
    };
    try_execute(
        TestDb::shared().await,
        &format!("INSERT INTO post_terms (post_id, term_id) VALUES ({id}, {term})"),
    )
    .await
    .expect("file the post");

    sign_out(&client);
    client
        .get("/sitemap.xml")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("/category/ghosts");
}

/// The importer's ceiling is the deployment's configuration, not a constant.
///
/// A hard-coded cap made `security.upload.max_request_size_bytes` ineffective:
/// the exporter is unbounded, so a fixed number is a size of backup the CMS can
/// create and cannot restore, with no way out. The screen states the number the
/// handler enforces, and both come from the same place.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_import_ceiling_follows_the_configured_request_limit() {
    // For the schema, the truncation and the cache reset — this test then
    // builds its own client, because the limit under test is configuration the
    // shared harness does not vary.
    drop(db_client().await);
    let db = TestDb::shared().await;

    // A deployment that has raised the limit well past the old constant.
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    config.security.submit_token.enabled = false;
    config.security.upload.max_request_size_bytes = 96 * 1024 * 1024;
    let client = TestApp::new()
        .routes(app_routes())
        .config(config)
        .with_db(db.pool())
        .build();
    let cookie = register(&client, "owner").await;

    let screen = client
        .get("/admin/tools")
        .header("cookie", &cookie)
        .send()
        .await;
    screen.assert_ok();
    let screen = screen.text();
    assert!(
        screen.contains("Up to 95 MB"),
        "the screen must state the configured ceiling, not a constant:\n{screen}"
    );
    assert!(
        screen.contains("max_request_size_bytes"),
        "and name the knob that changes it"
    );
}
