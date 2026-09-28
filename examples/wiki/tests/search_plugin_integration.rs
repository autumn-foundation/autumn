//! `autumn-search` mounted on `wiki::models::Page` (`lib.rs::search_plugin`,
//! docs/guide/search.md) — proves the wiring end to end through the real app:
//!
//! 1. `POST /api/v1/pages` creates a page. `PageRepository`'s
//!    `commit_hooks = true` (src/repositories.rs) durably stages `PageHooks`'s
//!    `after_create_commit` (src/hooks.rs), which calls
//!    `autumn_search::enqueue_reindex_for` — composed alongside the existing
//!    slug/state-machine hooks, not replacing them.
//! 2. Running the enqueued job is what actually writes the plugin's index —
//!    proving the durable-queue path, not just a direct client call.
//! 3. `GET /api/v1/search` (routes/pages.rs) — the plugin-backed sibling of
//!    the hand-rolled `/search` route — finds the page, hydrated back into a
//!    real `Page` row.
//! 4. Deleting the page removes it from the index too
//!    (`after_delete_commit` → `enqueue_unindex_for`).
//!
//! Issue #2320's T3 Gap 6 asked for exactly this pairing: mount `autumn-search`
//! on `wiki`, the example that already carries `#[searchable]` FTS.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p wiki --test search_plugin_integration -- --ignored --test-threads=1
//! ```

use autumn_web::test::{TestApp, TestClient, drain_ready_repository_commit_hooks};
use diesel::Connection;
use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_FILES: &[&str] = &[
    include_str!("../migrations/00000000000000_create_wiki/up.sql"),
    include_str!("../migrations/20260506000000_add_lock_version_to_pages/up.sql"),
    include_str!("../migrations/20260524000000_add_search_to_pages/up.sql"),
    include_str!("../migrations/20260601000000_add_api_credentials/up.sql"),
    include_str!("../migrations/20260721000000_add_collections/up.sql"),
    // Framework migration (autumn/repository_commit_hook_migrations): the
    // durable queue `commit_hooks = true` (src/repositories.rs) stages
    // `PageHooks`'s `after_*_commit` intents into. Normally applied
    // automatically alongside `FRAMEWORK_MIGRATIONS` when an app boots via
    // `.run()`; `TestApp::with_db` does not run it, so — same convention as
    // `autumn/tests/integration/sharding_commit_hooks.rs` — it is applied
    // here by hand.
    include_str!(
        "../../../autumn/repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql"
    ),
];

fn apply_migrations(conn: &mut PgConnection) {
    for sql in MIGRATION_FILES {
        conn.batch_execute(sql).expect("apply wiki migration");
    }
}

/// Start a fresh Postgres container, migrate it, and boot the real `wiki`
/// app (`wiki::all_routes()`) with `autumn-search` mounted
/// (`wiki::search_plugin()`).
///
/// Returns the pool and the container alongside the client: the pool is
/// needed to drive `drain_ready_repository_commit_hooks` directly (see
/// `run_commit_hooks` below), and the container must outlive every request
/// the test makes, or the database it holds disappears with it (same
/// convention as `tests/collection_links_batch_profile.rs`).
async fn boot() -> (
    TestClient,
    Pool<AsyncPgConnection>,
    ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    apply_migrations(&mut conn);

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(config).build().expect("pool");

    let client = TestApp::new()
        .routes(wiki::all_routes())
        .with_db(pool.clone())
        .plugin(wiki::search_plugin())
        .build();

    (client, pool, container)
}

/// Run every ready durable commit hook to completion.
///
/// `commit_hooks = true` stages `PageHooks::after_*_commit`'s intent into
/// `autumn_repository_commit_hooks` inside the mutation's own transaction; a
/// served app drains that queue on a timer, which `TestApp` deliberately does
/// not start (see `drain_ready_repository_commit_hooks`'s own rustdoc). This
/// runs the actual hook body — the call to `autumn_search::enqueue_reindex_for`
/// / `enqueue_unindex_for` — which is itself just an enqueue: the reindex job
/// still needs `TestClient::perform_enqueued_jobs` to actually run.
async fn run_commit_hooks(pool: &Pool<AsyncPgConnection>) {
    drain_ready_repository_commit_hooks(pool, 16).await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn creating_a_page_makes_it_searchable_through_the_plugin() {
    let (client, pool, _container) = boot().await;

    // The index starts empty: no page exists yet.
    client
        .get("/api/v1/search?q=marmot")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|body| {
            assert_eq!(body["content"].as_array().expect("content array").len(), 0);
        });

    // Create a page through the real JSON API. `slug` is required by
    // `NewPage`'s shape but `PageHooks::before_create` (src/hooks.rs)
    // overwrites it from the title regardless of what is sent here.
    let created = client
        .post("/api/v1/pages")
        .json(&serde_json::json!({
            "title": "Marmot Husbandry",
            "slug": "placeholder",
            "body": "A field guide to keeping marmots happy and well fed.",
            "status": "published",
        }))
        .send()
        .await;
    created.assert_status(201);
    let page_id = created.json::<serde_json::Value>()["id"]
        .as_i64()
        .expect("created page has an id");

    // The reindex was staged durably by `after_create_commit`, not written
    // synchronously — the index must still be empty until the commit hook
    // itself has run and its job has been performed.
    client
        .get("/api/v1/search?q=marmot")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|body| {
            assert_eq!(
                body["content"].as_array().expect("content array").len(),
                0,
                "nothing is indexed before the commit hook and reindex job run"
            );
        });

    run_commit_hooks(&pool).await;
    let performed = client.perform_enqueued_jobs().await;
    performed.assert_all_succeeded();

    // Now the plugin-backed `/api/v1/search` finds it, hydrated into a real
    // `Page` row — not just a bare `{index, id, score}` hit.
    client
        .get("/api/v1/search?q=marmot")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|body| {
            let content = body["content"].as_array().expect("content array");
            assert_eq!(content.len(), 1);
            assert_eq!(content[0]["id"].as_i64(), Some(page_id));
            assert_eq!(content[0]["title"], "Marmot Husbandry");
        });

    // Deleting the page removes it from the index too, once its own
    // durably-enqueued `after_delete_commit` job runs.
    client
        .delete(&format!("/api/v1/pages/{page_id}"))
        .send()
        .await
        .assert_status(204);
    run_commit_hooks(&pool).await;
    client.perform_enqueued_jobs().await.assert_all_succeeded();

    client
        .get("/api/v1/search?q=marmot")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|body| {
            assert_eq!(
                body["content"].as_array().expect("content array").len(),
                0,
                "a deleted page must be removed from the index"
            );
        });
}
