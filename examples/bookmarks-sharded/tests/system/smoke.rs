//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `bookmarks-sharded` binary against **three** ephemeral
//! testcontainer Postgres instances (control + shard0 + shard1), drives a
//! headless-Chromium browser against the cross-shard `/api/stats`
//! fan-out endpoint, and asserts both shards answer with no uncaught
//! console errors — the direct regression guard for the framework's
//! `[[database.shards]]` shard-routing feature. A second test proves
//! read-your-own-writes shard consistency: a tenant's create and their very
//! next list read must always land on the same shard.
//!
//! `bookmarks-sharded` uses the framework's own `AutumnConfig`, so unlike
//! `bookmarks-distributed`'s bespoke loader, standard
//! `AUTUMN_DATABASE__*` env overrides work directly — no scratch config
//! file needed. The app auto-migrates the control database *and* every
//! configured shard on boot (`AUTUMN_ENV=development`, set by
//! `spawn_example`), so `/api/stats`'s per-shard `bookmarks: 0` (rather
//! than an `error` field) is itself proof the shard pools connected and
//! the schema landed on both.
//!
//! Out of scope (per issue #1192): the real docker-compose topology
//! (nginx + multiple web replicas + a shard's own read replica). This
//! proves the *shard routing/fan-out feature* in a single process.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p bookmarks-sharded --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

/// Spawn `bookmarks-sharded` against a fresh 3-container topology (control +
/// shard0 + shard1), using the same fixed slot split every test in this file
/// relies on.
///
/// Returns the [`PgTopology`](example_e2e::PgTopology) alongside the spawned
/// process: dropping it tears the containers down, so callers must keep it
/// bound for as long as `ExampleProcess` is in use.
async fn spawn() -> (example_e2e::ExampleProcess, example_e2e::PgTopology) {
    let db = example_e2e::provision_postgres(3).await;
    let control_url = &db.urls()[0];
    let shard0_url = &db.urls()[1];
    let shard1_url = &db.urls()[2];

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_bookmarks-sharded"),
        env!("CARGO_MANIFEST_DIR"),
        &[
            ("AUTUMN_DATABASE__PRIMARY_URL", control_url.as_str()),
            ("AUTUMN_DATABASE__SHARDS__0__NAME", "shard0"),
            (
                "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
                shard0_url.as_str(),
            ),
            ("AUTUMN_DATABASE__SHARDS__0__SLOTS", "0-8191"),
            ("AUTUMN_DATABASE__SHARDS__1__NAME", "shard1"),
            (
                "AUTUMN_DATABASE__SHARDS__1__PRIMARY_URL",
                shard1_url.as_str(),
            ),
            ("AUTUMN_DATABASE__SHARDS__1__SLOTS", "8192-16383"),
        ],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn bookmarks-sharded example — is it built?");

    (app, db)
}

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn bookmarks_sharded_boots_and_fans_out_across_shards() {
    let (app, _db) = spawn().await;

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    // `/api/stats` sits behind the app's global header-based tenancy
    // middleware (every route requires `X-Tenant-Id`, not just the
    // tenant-scoped ones), and a plain navigation can't set a custom
    // request header. `/health` is exempt from that middleware (public
    // probe endpoint) and always 200s, so load it first purely to land in
    // the app's origin, then `evaluate()` can `fetch()` `/api/stats` with
    // the header from there and write the response into the DOM for
    // `expect_text` to poll.
    page.visit("/health").await.expect("visit /health");
    page.evaluate(
        "fetch('/api/stats', { headers: { 'X-Tenant-Id': 'acme' } })\
         .then(r => r.text())\
         .then(t => { document.body.textContent = t; })",
    )
    .await
    .expect("evaluate authenticated fetch");

    page.expect_text("\"shard0\"")
        .await
        .expect("cross-shard fan-out reaches shard0");
    page.expect_text("\"shard1\"")
        .await
        .expect("cross-shard fan-out reaches shard1");
    page.expect_text("\"bookmarks\":0")
        .await
        .expect("both shards report a bookmark count (not an error) — schema landed on both");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /api/stats");
}

/// Regression guard for `[[database.shards]]` routing consistency: a tenant
/// who just created a bookmark must see it on the very next list read.
///
/// `ShardedDb` hashes the tenant id onto a shard the same way for every
/// request — no cache, no staleness — so the create and the list below
/// should always land on the same shard by construction (see
/// `autumn/src/sharding.rs`'s `HashShardRouter`). This is a deterministic
/// pass/fail, not a timing race: shard0 and shard1 here are two wholly
/// independent testcontainer databases, so if routing were ever
/// inconsistent for the same tenant, the created bookmark would never
/// appear — it would be sitting in the other shard's database instead.
#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn bookmarks_sharded_read_your_own_write_after_create() {
    let (app, _db) = spawn().await;

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    // See the fan-out test above for why /health is visited first: every
    // route requires X-Tenant-Id, and a plain navigation can't set headers.
    page.visit("/health").await.expect("visit /health");
    page.evaluate(
        "fetch('/api/bookmarks', { \
           method: 'POST', \
           headers: { 'X-Tenant-Id': 'acme', 'Content-Type': 'application/json' }, \
           body: JSON.stringify({ \
             url: 'https://example.com/ryow', \
             title: 'RYOW shard bookmark', \
             tag: 'ryow' \
           }) \
         }) \
         .then(r => r.text()) \
         .then(() => fetch('/api/bookmarks', { headers: { 'X-Tenant-Id': 'acme' } })) \
         .then(r => r.text()) \
         .then(t => { document.body.textContent = t; })",
    )
    .await
    .expect("evaluate create-then-list fetch chain");

    page.expect_text("RYOW shard bookmark").await.expect(
        "bookmark created for tenant \"acme\" must appear on the very next list read for \
         the same tenant — the create and the list must have routed to the same shard",
    );
    page.expect_no_console_errors()
        .await
        .expect("no console errors across the create -> list flow");
}
