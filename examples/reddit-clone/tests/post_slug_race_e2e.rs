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
use example_e2e::{DEFAULT_READY_TIMEOUT, provision_postgres, spawn_example};

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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_identical_submits_never_share_a_slug() {
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
    // -b jar.txt` curl invocations shared across every request.
    let client = Arc::new(
        reqwest::Client::builder()
            .cookie_store(true)
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

    // Create a subreddit to submit into.
    let csrf = hidden_value_from(&client, &format!("{base_url}/r/create"), "_csrf").await;
    let create = client
        .post(format!("{base_url}/r/create"))
        .form(&[
            ("_csrf", csrf.as_str()),
            ("name", "raceclub"),
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
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(db.urls()[0].clone());
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    let mut conn = pool.get().await.expect("connection");

    let duplicates: Vec<SlugCount> = diesel::sql_query(
        "SELECT slug, COUNT(*) AS count FROM posts \
         GROUP BY subreddit_id, slug HAVING COUNT(*) > 1",
    )
    .load(&mut conn)
    .await
    .expect("duplicate-slug verification query");
    assert!(
        duplicates.is_empty(),
        "duplicate (subreddit_id, slug) pairs survived {CONCURRENT_SUBMITS} concurrent \
         identical submits: {}",
        duplicates
            .iter()
            .map(|d| format!("{} (x{})", d.slug, d.count))
            .collect::<Vec<_>>()
            .join(", ")
    );

    #[derive(diesel::QueryableByName, Debug)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }
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
