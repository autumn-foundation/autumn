//! Integration tests for the CMS starter.
//!
//! ```text
//! cargo test -p cms                                    # smoke tests (no Docker)
//! cargo test -p cms -- --include-ignored --test-threads=1   # full flow (needs Docker)
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
    cms::all_routes()
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
        cms::repositories::PgSiteOptionRepository::invalidate_declared_caches(),
        "the test cache backend cannot invalidate by namespace, so settings would leak \
         between tests"
    );

    // Registrations are process-global; the real `main` calls this too, so the
    // test app and the shipped app see the same post types and shortcodes.
    cms::bootstrap();

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
        cms::repositories::PgSiteOptionRepository::invalidate_declared_caches(),
        "the test cache backend cannot invalidate by namespace, so settings would leak \
         between tests"
    );
    cms::bootstrap();

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
    ];
    for (key, value) in overrides {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => fields.push((key, value)),
        }
    }
    form(&fields)
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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

    let result = client
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", payload.as_str())]))
        .send()
        .await;
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
            ("tags", ""),
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
            ("tags", ""),
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
        .form(&form(&[
            ("title", "Versioned"),
            ("slug", "versioned"),
            ("excerpt", ""),
            ("body", "Second draft."),
            ("status", "publish"),
            ("password", ""),
            ("tags", ""),
        ]))
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
            ("tags", "Rust, Web Frameworks"),
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
            ("tags", ""),
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
    let reimport = client
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", payload.as_str())]))
        .send()
        .await;
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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
            ("tags", ""),
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

    let result = client
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", payload.as_str())]))
        .send()
        .await;
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
            ("tags", ""),
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

    client
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", payload.as_str())]))
        .send()
        .await
        .assert_ok()
        .assert_body_contains("1 imported");

    // The same file again: nothing new.
    client
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", payload.as_str())]))
        .send()
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
        .form(&form(&[
            ("title", ""),
            ("slug", "has-a-title"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
            ("tags", ""),
            ("comment_status", "open"),
        ]))
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
        .form(&form(&[
            ("title", "Collaborative"),
            ("slug", "collaborative"),
            ("excerpt", ""),
            ("body", "Edited by somebody else."),
            ("status", "draft"),
            ("password", ""),
            ("tags", ""),
            ("comment_status", "open"),
        ]))
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

/// A malformed date format must not take the site down.
///
/// `format()` defers everything to `Display`, a bad directive makes `Display`
/// return an error, and `to_string()` turns that into a panic — so `%` in the
/// settings form would 500 every dated listing and every single-post page.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_malformed_date_format_is_refused_rather_than_crashing_the_site() {
    let client = db_client().await;
    let cookie = register(&client, "owner").await;
    create_post(&client, &cookie, "Dated", "Body.", "publish").await;

    let settings = |date_format: &str| settings_form(&[("date_format", date_format)]);

    // A pattern chrono cannot render.
    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings("%"))
        .send()
        .await
        .assert_status(303);

    // Every dated surface still renders.
    sign_out(&client);
    client.get("/").send().await.assert_ok();
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
    client.get(&url).send().await.assert_ok();

    // A valid pattern is still accepted and applied.
    client
        .post("/admin/settings")
        .header("cookie", &cookie)
        .form(&settings("%Y/%m/%d"))
        .send()
        .await
        .assert_status(303);
    sign_out(&client);
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
            ("tags", ""),
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
        .form(&form(&[
            ("title", "Final Title"),
            ("slug", "work-in-progress"),
            ("excerpt", ""),
            ("body", "Second body."),
            ("status", "publish"),
            ("password", ""),
            ("tags", ""),
            ("comment_status", "open"),
        ]))
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
                ("tags", ""),
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
        .form(&form(&[
            ("title", "Filed"),
            ("slug", "filed"),
            ("excerpt", ""),
            ("body", "Edited body."),
            ("status", "publish"),
            ("password", ""),
            ("tags", "rust"),
            ("comment_status", "open"),
        ]))
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
    fresh
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", exported.as_str())]))
        .send()
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
        .form(&form(&[
            ("title", "Ordinary"),
            ("slug", "ordinary"),
            ("excerpt", ""),
            ("body", "Body."),
            ("status", "publish"),
            ("password", ""),
            ("tags", ""),
            ("comment_status", "open"),
            ("categories", news_id.as_str()),
            ("categories", foreign_id.as_str()),
        ]))
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
    cms::plugins::add_filter(
        cms::plugins::Filter::TheExcerpt,
        cms::plugins::DEFAULT_PRIORITY,
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
        ("tags", ""),
        ("comment_status", "open"),
        ("categories", ids[0].as_str()),
    ];
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&form(&fields))
        .send()
        .await
        .assert_status(303);

    // …and two, which is the shape a checkbox group actually posts.
    fields.push(("categories", ids[1].as_str()));
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", &cookie)
        .form(&form(&fields))
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
    fresh
        .post("/admin/tools/import")
        .header("cookie", &cookie)
        .form(&form(&[("payload", exported.as_str())]))
        .send()
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

    cms::plugins::add_action(
        cms::plugins::Action::PostSaved,
        cms::plugins::DEFAULT_PRIORITY,
        |_| {
            // The re-entrant call. Before the fix this never returned.
            cms::plugins::add_filter(
                cms::plugins::Filter::TheTitle,
                cms::plugins::DEFAULT_PRIORITY,
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
            ("tags", ""),
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
