//! TDD integration tests for the inline broadcast pre-fetch fix (issue #1475).
//!
//! The inline broadcast path (`broadcasts = true`, no `commit_hooks`) previously
//! had three limitations that these tests pin down:
//!
//! 1. **AC1** — Delete with a dynamic topic fell back to the raw table name instead
//!    of the interpolated topic (e.g. `"live_pf_cat:rust"`) because the record was
//!    already gone from the DB.
//!
//! 2. **AC2** — Delete with `broadcast_render` used an approximated dom id
//!    (`"{model_prefix}-{id}"`) instead of the real id produced by the render fn.
//!
//! 3. **AC3** — Update that mutates a topic-keyed field only broadcast on the new
//!    topic; the old-topic subscribers received no delete and were left stale.
//!
//! Run with:
//!
//!     cargo test --test inline_broadcast_prefetch \
//!         --features "ws,maud,htmx,db,test-support" -- --ignored

#![cfg(all(
    feature = "ws",
    feature = "maud",
    feature = "htmx",
    feature = "db",
    feature = "test-support"
))]
#![allow(
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::too_many_lines
)]

use autumn_web::__private::CURRENT_CHANNELS;
use autumn_web::channels::Channels;
use autumn_web::hooks::Patch;
use autumn_web::live::LiveFragment;
use autumn_web::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Schema ────────────────────────────────────────────────────────────────────

diesel::table! {
    live_pf_posts (id) {
        id -> Int8,
        title -> Text,
        category -> Text,
    }
}

// ── Model with dynamic topic (category field) ─────────────────────────────────

#[autumn_web::model(table = "live_pf_posts")]
#[derive(PartialEq, Eq)]
pub struct LivePfPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub category: String,
}

impl LiveFragment for LivePfPost {
    fn dom_id_for(id: i64) -> String {
        format!("live-pf-post-{id}")
    }

    fn dom_id(&self) -> String {
        Self::dom_id_for(self.id)
    }

    fn render_fragment(&self) -> maud::Markup {
        html! {
            li id=(self.dom_id()) class="pf-post" {
                (self.title) " [" (self.category) "]"
            }
        }
    }

    fn insert_swap() -> autumn_web::htmx::OobSwap {
        autumn_web::htmx::OobSwap::Target(
            autumn_web::htmx::OobMethod::BeforeEnd,
            "#live-pf-posts-list".to_string(),
        )
    }
}

// Repository with dynamic topic (no commit_hooks — inline broadcast path)
#[autumn_web::repository(
    LivePfPost,
    table = "live_pf_posts",
    broadcasts = true,
    topic = "live_pf_cat:{category}",
    container = "live-pf-posts-list"
)]
pub trait LivePfPostRepository {}

// ── Custom render function ────────────────────────────────────────────────────
//
// Produces an id of the form `custom-pf-{id}` — intentionally different from the
// `LiveFragment::dom_id_for` default of `live-pf-post-{id}` so we can verify that
// the delete uses the render-fn id, not the approximated one.

fn render_custom_pf(post: &LivePfPost) -> maud::Markup {
    maud::html! {
        div id=(format!("custom-pf-{}", post.id)) class="custom-pf" {
            span { (post.title) }
        }
    }
}

// Repository using broadcast_render (static topic, custom render)
#[autumn_web::repository(
    LivePfPost,
    table = "live_pf_posts",
    broadcasts = true,
    render = render_custom_pf,
    container = "custom-pf-list"
)]
pub trait CustomPfPostRepository {}

// ── Static-topic repository (simple / unaffected path) ───────────────────────

diesel::table! {
    live_pf_simple (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "live_pf_simple")]
pub struct LivePfSimple {
    #[id]
    pub id: i64,
    pub name: String,
}

impl LiveFragment for LivePfSimple {
    fn dom_id_for(id: i64) -> String {
        format!("pf-simple-{id}")
    }

    fn dom_id(&self) -> String {
        Self::dom_id_for(self.id)
    }

    fn render_fragment(&self) -> maud::Markup {
        html! { li id=(self.dom_id()) { (self.name) } }
    }

    fn insert_swap() -> autumn_web::htmx::OobSwap {
        autumn_web::htmx::OobSwap::Target(
            autumn_web::htmx::OobMethod::BeforeEnd,
            "#pf-simple-list".to_string(),
        )
    }
}

// Simple repo: static topic, no render override — should NOT add a pre-fetch.
#[autumn_web::repository(LivePfSimple, table = "live_pf_simple", broadcasts = true)]
pub trait LivePfSimpleRepository {}

// ── DB setup ──────────────────────────────────────────────────────────────────

async fn setup_db() -> (
    testcontainers::ContainerAsync<Postgres>,
    Pool<AsyncPgConnection>,
) {
    let container = Postgres::default().start().await.expect("postgres start");
    let url = format!(
        "postgres://postgres:postgres@{}:{}/postgres",
        container.get_host().await.unwrap(),
        container.get_host_port_ipv4(5432).await.unwrap(),
    );
    let manager = AsyncDieselConnectionManager::new(url);
    let pool = Pool::builder(manager).build().expect("pool build");
    let mut conn = pool.get().await.unwrap();

    let _: diesel::QueryResult<usize> = diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS live_pf_posts (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL,
            category TEXT NOT NULL
        )",
    )
    .execute(&mut *conn)
    .await;

    let _: diesel::QueryResult<usize> = diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS live_pf_simple (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL
        )",
    )
    .execute(&mut *conn)
    .await;

    drop(conn);
    (container, pool)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC1: Delete on a dynamic-topic repo must publish the delete on the
/// **interpolated** topic (`live_pf_cat:rust`), not the raw table name.
///
/// Before the fix: the inline path fell back to `live_pf_posts` as the topic
/// because the record was already deleted. Subscribers on `live_pf_cat:rust`
/// timed out.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_dynamic_topic_publishes_on_interpolated_topic() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_pf_cat:rust");
    let repo = PgLivePfPostRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLivePfPost {
                title: "Rust post".to_owned(),
                category: "rust".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;

    // Drain the create broadcast (published on live_pf_cat:rust too)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels, async {
            repo.delete_by_id(saved.id).await.expect("delete");
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("delete broadcast timed out — wrong topic used (table-name fallback?)")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(
        html.contains("delete"),
        "must be a delete swap, got: {html}"
    );
    assert!(
        html.contains(&format!("live-pf-post-{}", saved.id)),
        "delete must target correct dom-id live-pf-post-{}, got: {html}",
        saved.id
    );
}

/// AC2: Delete with `broadcast_render` must use the id from the render function
/// (`custom-pf-{id}`), not the approximated `live_pf_post-{id}`.
///
/// Before the fix: the inline path used `"{model_prefix}-{id}"` (i.e.
/// `live_pf_post-{id}`) because it couldn't call the render fn without the record.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_with_render_uses_real_dom_id() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    // Custom render repo uses a static topic: table name "live_pf_posts"
    let mut rx = channels.subscribe("live_pf_posts");
    let repo = PgCustomPfPostRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLivePfPost {
                title: "Custom".to_owned(),
                category: "any".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;

    // Drain the create broadcast
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels, async {
            repo.delete_by_id(saved.id).await.expect("delete");
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("delete broadcast timed out")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(
        html.contains("delete"),
        "must be a delete swap, got: {html}"
    );
    // The render function produces id="custom-pf-{id}"
    assert!(
        html.contains(&format!("custom-pf-{}", saved.id)),
        "delete must use render-fn id custom-pf-{}, not approximated id, got: {html}",
        saved.id
    );
    // The old approximated id must NOT appear
    assert!(
        !html.contains(&format!("live_pf_post-{}", saved.id)),
        "must not use approximated model_prefix id, got: {html}"
    );
}

/// AC3: Update that changes a topic-keyed field must:
///   (a) publish a **delete** on the **old** topic, and
///   (b) publish an **insert** (beforeend) on the **new** topic.
///
/// Before the fix: the inline path only published an outerHTML update on the new
/// topic. Old-topic subscribers timed out.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn update_topic_change_deletes_old_inserts_new() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let repo = PgLivePfPostRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLivePfPost {
                title: "Topic changer".to_owned(),
                category: "rust".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;

    let mut old_rx = channels.subscribe("live_pf_cat:rust");
    let mut new_rx = channels.subscribe("live_pf_cat:go");

    // Drain any leftover create broadcast on old topic
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), old_rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.update(
                saved.id,
                &UpdateLivePfPost {
                    title: Patch::Unchanged,
                    category: Patch::Set("go".to_owned()),
                },
            )
            .await
            .expect("update");
        })
        .await;

    // (a) Old topic must receive a delete broadcast
    let del_msg = tokio::time::timeout(std::time::Duration::from_millis(500), old_rx.recv())
        .await
        .expect("no delete on old topic live_pf_cat:rust — pre-fetch not implemented?")
        .expect("channel closed");
    let del_html = del_msg.as_str();
    assert!(
        del_html.contains("delete"),
        "old-topic broadcast must be a delete, got: {del_html}"
    );
    assert!(
        del_html.contains(&format!("live-pf-post-{}", saved.id)),
        "delete on old topic must target correct id, got: {del_html}"
    );

    // (b) New topic must receive an insert/beforeend broadcast
    let ins_msg = tokio::time::timeout(std::time::Duration::from_millis(500), new_rx.recv())
        .await
        .expect("no insert on new topic live_pf_cat:go")
        .expect("channel closed");
    let ins_html = ins_msg.as_str();
    assert!(
        ins_html.contains("beforeend"),
        "new-topic broadcast must be a beforeend insert, got: {ins_html}"
    );
    assert!(
        ins_html.contains("live-pf-posts-list"),
        "insert must target container live-pf-posts-list, got: {ins_html}"
    );
    assert!(
        ins_html.contains(&format!("live-pf-post-{}", saved.id)),
        "insert must contain the updated record, got: {ins_html}"
    );
}

/// Regression guard: the simple static-topic path must still work after
/// the prefetch feature is added. No extra `find_by_id` call should occur
/// for repos with a static topic and no custom render function.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn simple_static_topic_delete_still_works() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_pf_simple");
    let repo = PgLivePfSimpleRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLivePfSimple {
                name: "simple item".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;

    // Drain the create broadcast
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels, async {
            repo.delete_by_id(saved.id).await.expect("delete");
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("delete broadcast timed out on simple static-topic repo")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(
        html.contains("delete"),
        "must be a delete swap, got: {html}"
    );
    assert!(
        html.contains(&format!("pf-simple-{}", saved.id)),
        "simple delete must target dom id pf-simple-{}, got: {html}",
        saved.id
    );
}
