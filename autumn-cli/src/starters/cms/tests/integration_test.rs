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
    ] {
        assert!(
            paths.contains(&reserved),
            "`{reserved}` has no literal route, so the front controller would try to resolve \
             it as content; mounted paths: {paths:?}"
        );
    }
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
    let sources = [
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
    ];

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
        .form(&form(&[
            ("site_title", "Autumn CMS"),
            ("tagline", ""),
            ("posts_per_page", "10"),
            ("permalink_structure", "day_and_name"),
            ("default_comment_status", "open"),
            ("active_theme", "default"),
            ("date_format", "%B %-d, %Y"),
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
