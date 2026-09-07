//! Regression test for #2544: concurrent identical `/submit`s must never let
//! two posts in the same subreddit share a slug.
//!
//! `unique_slug()`'s SELECT-then-INSERT was a classic TOCTOU — two racing
//! submits could both observe "not taken" for the same slug and both commit
//! it, after which `show()`'s unordered `.first()` serves an arbitrary one of
//! the two forever at the other's own permalink. The fix
//! (`posts_subreddit_id_slug_key`, a composite `UNIQUE (subreddit_id, slug)`
//! constraint, plus a retry loop in `submit`/`update`) is only proven by
//! actually racing real concurrent requests against it — so this drives the
//! real, unmodified compiled `reddit-clone` binary over plain HTTP, spawned
//! the same way `tests/system/smoke.rs` does, and reproduces the issue's own
//! `curl ... & curl ... & wait` harness with `reqwest` instead of a shell
//! script. It exercises the exact `Db` extractor / `db.tx` / `unique_slug`
//! code path a production request takes — no in-process shortcuts.
//!
//! Before the fix this test fails close to every run (the issue reported
//! 15/15 reproductions); after it, every concurrent submit lands its own
//! distinct slug and the database itself has no duplicate `(subreddit_id,
//! slug)` pair to find.
//!
//! Run (requires Docker):
//!   cargo test -p reddit-clone --test post_slug_race_e2e -- --ignored

use std::sync::Arc;

use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use example_e2e::{
    DEFAULT_READY_TIMEOUT, ExampleProcess, PgTopology, provision_postgres, spawn_example,
};

/// How many identical, fully concurrent submits to fire. The issue's own
/// harness used 2 (a plain double-click) and 10 (a `threading.Barrier` stress
/// case); 10 here exercises the retry loop's suffix-hunting path (`-2`
/// through `-10`), not just the minimal two-submit case.
const CONCURRENT_SUBMITS: usize = 10;

/// Extract `name="{name}" value="{value}"` from a rendered page — the same
/// thing the issue's repro script's `grep -o` did. Good enough for this
/// app's own markup (attributes always render in `type, name, value` order);
/// not a general HTML parser.
fn extract_hidden_value(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` found in response body:\n{html}"))
        + marker.len();
    let end = html[start..]
        .find('"')
        .unwrap_or_else(|| panic!("unterminated `{name}` attribute value"));
    html[start..start + end].to_string()
}

async fn hidden_value_from(client: &reqwest::Client, url: &str, name: &str) -> String {
    let html = client
        .get(url)
        .send()
        .await
        .unwrap_or_else(|err| panic!("GET {url}: {err}"))
        .text()
        .await
        .unwrap_or_else(|err| panic!("read body of GET {url}: {err}"));
    extract_hidden_value(&html, name)
}

#[derive(diesel::QueryableByName)]
struct SlugCount {
    #[diesel(sql_type = Text)]
    slug: String,
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct Slug {
    #[diesel(sql_type = Text)]
    slug: String,
}

/// No duplicate `(subreddit_id, slug)` pair exists anywhere in the table —
/// the invariant `posts_subreddit_id_slug_key` (and the retry loop that backs
/// off it) exists to guarantee, whatever raced to produce the current rows.
async fn assert_no_duplicate_slugs(conn: &mut AsyncPgConnection, context: &str) {
    let duplicates: Vec<SlugCount> = diesel::sql_query(
        "SELECT slug, COUNT(*) AS count FROM posts \
         GROUP BY subreddit_id, slug HAVING COUNT(*) > 1",
    )
    .load(conn)
    .await
    .expect("duplicate-slug verification query");
    assert!(
        duplicates.is_empty(),
        "duplicate (subreddit_id, slug) pairs survived {context}: {}",
        duplicates
            .iter()
            .map(|d| format!("{} (x{})", d.slug, d.count))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// Build a pool against the testcontainer's own URL — the verification path
/// every test below uses to ask the database, not the application, whether
/// the invariant held.
fn pg_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url.to_string());
    Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("build pool")
}

/// Boot the real, unmodified compiled binary against a fresh testcontainer
/// Postgres, register one user (auto-logged-in, mirroring the issue's own
/// repro), and create one subreddit for it to post/edit into. Returns
/// everything the caller needs kept alive for the test's duration — dropping
/// [`PgTopology`]/[`ExampleProcess`] tears down the container/process.
async fn boot_with_one_subreddit(
    subreddit_name: &str,
) -> (PgTopology, ExampleProcess, Arc<reqwest::Client>, String) {
    let db = provision_postgres(1).await;
    let app = spawn_example(
        env!("CARGO_BIN_EXE_reddit-clone"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn reddit-clone example — is it built?");
    let base_url = app.base_url().to_string();

    // One client, one cookie jar — the same session the issue's `-c jar.txt
    // -b jar.txt` curl invocations shared across every request. Redirects are
    // disabled: every assertion below checks the app's own `303 See Other`
    // (Codex review on this PR — reqwest follows redirects by default, which
    // would silently replace that status with the destination page's `200`
    // and mask a real failure).
    let client = Arc::new(
        reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build reqwest client"),
    );

    // Register (auto-logs in, mirroring the issue's repro exactly).
    let csrf = hidden_value_from(&client, &format!("{base_url}/register"), "_csrf").await;
    let register = client
        .post(format!("{base_url}/register"))
        .form(&[
            ("_csrf", csrf.as_str()),
            ("username", "racer1"),
            ("email", "racer1@example.com"),
            ("password", "RacerPass123!"),
        ])
        .send()
        .await
        .expect("POST /register");
    assert!(
        register.status().is_redirection(),
        "register should redirect (auto-login); got {}",
        register.status()
    );

    // Create a subreddit to post/edit into.
    let csrf = hidden_value_from(&client, &format!("{base_url}/r/create"), "_csrf").await;
    let create = client
        .post(format!("{base_url}/r/create"))
        .form(&[
            ("_csrf", csrf.as_str()),
            ("name", subreddit_name),
            ("description", ""),
        ])
        .send()
        .await
        .expect("POST /r/create");
    assert!(
        create.status().is_redirection(),
        "create subreddit should redirect; got {}",
        create.status()
    );

    (db, app, client, base_url)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_identical_submits_never_share_a_slug() {
    let (db, _app, client, base_url) = boot_with_one_subreddit("raceclub").await;

    // `/r/{slug}/submit` pre-fills `subreddit_id` as a hidden input, so the
    // numeric id never needs to be hardcoded (the issue's own script assumed
    // `subreddit_id=1`; this doesn't).
    let submit_form_html = client
        .get(format!("{base_url}/r/raceclub/submit"))
        .send()
        .await
        .expect("GET /r/raceclub/submit")
        .text()
        .await
        .expect("response body");
    let csrf = extract_hidden_value(&submit_form_html, "_csrf");
    let subreddit_id = extract_hidden_value(&submit_form_html, "subreddit_id");

    // Fire every submit at once from the same session — exactly what a
    // double-click, or a flaky-network auto-retry, sends. `join_all` over
    // freshly spawned tasks starts every request before any of them can
    // complete, matching the issue's backgrounded-curl-then-`wait` harness
    // rather than serializing them.
    let submits = (0..CONCURRENT_SUBMITS).map(|_| {
        let client = client.clone();
        let base_url = base_url.clone();
        let csrf = csrf.clone();
        let subreddit_id = subreddit_id.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base_url}/submit"))
                .form(&[
                    ("_csrf", csrf.as_str()),
                    ("subreddit_id", subreddit_id.as_str()),
                    ("title", "Race condition test post"),
                    ("url", ""),
                    ("body", "x"),
                ])
                .send()
                .await
        })
    });
    let results = futures::future::join_all(submits).await;

    for result in results {
        let response = result
            .expect("submit task panicked")
            .expect("POST /submit request failed");
        assert!(
            response.status().is_redirection(),
            "every concurrent submit must still succeed (303 See Other, per the issue's own \
             repro) — got {}",
            response.status()
        );
    }

    // The database is the oracle, exactly as the issue's own verification
    // query was: no two posts in the same subreddit may share a slug.
    let pool = pg_pool(&db.urls()[0]);
    let mut conn = pool.get().await.expect("connection");
    assert_no_duplicate_slugs(
        &mut conn,
        &format!("{CONCURRENT_SUBMITS} concurrent identical submits"),
    )
    .await;

    let distinct_slugs: Count =
        diesel::sql_query("SELECT COUNT(DISTINCT slug) AS count FROM posts")
            .get_result(&mut conn)
            .await
            .expect("distinct-slug count");
    assert_eq!(
        distinct_slugs.count, CONCURRENT_SUBMITS as i64,
        "every concurrent submit must land its own distinct slug, not merely avoid an exact \
         duplicate (e.g. two racers must not both fall back to the same suffix)"
    );
}

/// Regression test for the *edit* path's identical TOCTOU (#2544):
/// `unique_slug_excluding` has the same SELECT-then-write shape as
/// `unique_slug`, so two different posts edited to the same new title at the
/// same time can race for the same base slug just as two `/submit`s can.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_identical_edits_never_share_a_slug() {
    let (db, _app, client, base_url) = boot_with_one_subreddit("editrace").await;
    let pool = pg_pool(&db.urls()[0]);

    // Seed two distinct posts sequentially — no race here, the race under
    // test is in the concurrent edit that follows.
    for title in ["Seed Post Alpha", "Seed Post Beta"] {
        let submit_form_url = format!("{base_url}/r/editrace/submit");
        let csrf = hidden_value_from(&client, &submit_form_url, "_csrf").await;
        let subreddit_id = hidden_value_from(&client, &submit_form_url, "subreddit_id").await;
        let submit = client
            .post(format!("{base_url}/submit"))
            .form(&[
                ("_csrf", csrf.as_str()),
                ("subreddit_id", subreddit_id.as_str()),
                ("title", title),
                ("url", ""),
                ("body", "seed"),
            ])
            .send()
            .await
            .expect("POST /submit (seed)");
        assert!(
            submit.status().is_redirection(),
            "seed submit for {title:?} should redirect; got {}",
            submit.status()
        );
    }

    let mut conn = pool.get().await.expect("connection");
    let slug_a = diesel::sql_query("SELECT slug FROM posts WHERE title = $1")
        .bind::<Text, _>("Seed Post Alpha")
        .get_result::<Slug>(&mut conn)
        .await
        .expect("look up seeded post Alpha")
        .slug;
    let slug_b = diesel::sql_query("SELECT slug FROM posts WHERE title = $1")
        .bind::<Text, _>("Seed Post Beta")
        .get_result::<Slug>(&mut conn)
        .await
        .expect("look up seeded post Beta")
        .slug;
    drop(conn);

    // Edit BOTH distinct posts to the exact same new title at once — the
    // `update` analogue of the issue's double-submit: two different posts'
    // edits race for the identical `unique_slug_excluding` base slug.
    const NEW_TITLE: &str = "Renamed Race Post";
    let edits = [slug_a, slug_b].into_iter().map(|slug| {
        let client = client.clone();
        let base_url = base_url.clone();
        tokio::spawn(async move {
            let edit_url = format!("{base_url}/r/editrace/posts/{slug}/edit");
            let csrf = hidden_value_from(&client, &edit_url, "_csrf").await;
            client
                .post(format!("{base_url}/r/editrace/posts/{slug}"))
                .form(&[
                    ("_csrf", csrf.as_str()),
                    ("title", NEW_TITLE),
                    ("body", "renamed"),
                ])
                .send()
                .await
        })
    });
    let results = futures::future::join_all(edits).await;
    for result in results {
        let response = result
            .expect("edit task panicked")
            .expect("POST update request failed");
        assert!(
            response.status().is_redirection(),
            "every concurrent edit must still succeed (303 See Other) — got {}",
            response.status()
        );
    }

    let mut conn = pool.get().await.expect("connection");
    assert_no_duplicate_slugs(&mut conn, "2 concurrent identical edits").await;

    let distinct_slugs: Count =
        diesel::sql_query("SELECT COUNT(DISTINCT slug) AS count FROM posts")
            .get_result(&mut conn)
            .await
            .expect("distinct-slug count");
    assert_eq!(
        distinct_slugs.count, 2,
        "both concurrently edited posts must land their own distinct slug"
    );
}
