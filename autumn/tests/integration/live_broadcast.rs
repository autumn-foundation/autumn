//! Integration tests for declarative live broadcast via `#[repository(Model, broadcasts = true)]`.
//!
//! These tests verify that:
//! - `save` publishes an OOB insert fragment to the configured topic
//! - `update` publishes an OOB outerHTML swap fragment
//! - `delete_by_id` publishes an OOB delete fragment
//! - Repositories declared without `broadcasts` emit nothing
//! - Repositories constructed without a `CURRENT_CHANNELS` context skip broadcast silently
//!
//! **Requires Docker** (Postgres testcontainer) for DB-backed tests.

#![cfg(all(feature = "ws", feature = "maud", feature = "htmx", feature = "db"))]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

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
    live_items (id) {
        id -> Int8,
        name -> Text,
    }
}

// ── Model ─────────────────────────────────────────────────────────────────────

#[autumn_web::model(table = "live_items")]
#[derive(PartialEq, Eq)]
pub struct LiveItem {
    #[id]
    pub id: i64,
    pub name: String,
}

// ── LiveFragment impl (compile check — trait is user-facing) ──────────────────

impl LiveFragment for LiveItem {
    fn dom_id_for(id: i64) -> String {
        format!("live-item-{id}")
    }

    fn dom_id(&self) -> String {
        Self::dom_id_for(self.id)
    }

    fn render_fragment(&self) -> maud::Markup {
        html! {
            li id=(self.dom_id()) { (self.name) }
        }
    }

    fn insert_swap() -> autumn_web::htmx::OobSwap {
        autumn_web::htmx::OobSwap::Target(
            autumn_web::htmx::OobMethod::BeforeEnd,
            "#live_items-list".to_string(),
        )
    }
}

// ── Repository with broadcasts ────────────────────────────────────────────────

#[autumn_web::repository(LiveItem, table = "live_items", broadcasts = true)]
pub trait LiveItemRepository {}

// ── Repository WITHOUT broadcasts (control group) ────────────────────────────

#[autumn_web::model(table = "live_items")]
#[derive(PartialEq, Eq)]
pub struct SilentItem {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(SilentItem, table = "live_items")]
pub trait SilentItemRepository {}

// ── Test helpers ──────────────────────────────────────────────────────────────

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
        "CREATE TABLE IF NOT EXISTS live_items (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    )
    .execute(&mut *conn)
    .await;
    drop(conn);
    (container, pool)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_broadcasts_oob_fragment() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_items");
    let repo = PgLiveItemRepository::with_pool_untracked(pool);
    let new_item = NewLiveItem {
        name: "hello".to_owned(),
    };

    let _saved = CURRENT_CHANNELS
        .scope(channels, async {
            repo.save(&new_item).await.expect("save")
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("broadcast timed out")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(
        html.contains("hx-swap-oob"),
        "must contain hx-swap-oob, got: {html}"
    );
    // Trunk-dev default container: "{table}-list" = "live_items-list"
    assert!(
        html.contains("live_items-list"),
        "create must target container live_items-list, got: {html}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn update_broadcasts_true_swap() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_items");
    let repo = PgLiveItemRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLiveItem {
                name: "first".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;
    // drain the insert broadcast
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels, async {
            repo.update(
                saved.id,
                &UpdateLiveItem {
                    name: Patch::Set("updated".to_owned()),
                },
            )
            .await
            .expect("update")
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("update broadcast timed out")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(
        html.contains("hx-swap-oob"),
        "must contain hx-swap-oob: {html}"
    );
    // Update uses OobSwap::True (outerHTML) — fragment carries the attribute directly
    assert!(
        html.contains("live_item"),
        "update fragment must reference item element, got: {html}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_broadcasts_oob_delete() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_items");
    let repo = PgLiveItemRepository::with_pool_untracked(pool);

    let saved = CURRENT_CHANNELS
        .scope(channels.clone(), async {
            repo.save(&NewLiveItem {
                name: "to_delete".to_owned(),
            })
            .await
            .expect("save")
        })
        .await;
    // drain the insert broadcast
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;

    CURRENT_CHANNELS
        .scope(channels, async {
            repo.delete_by_id(saved.id).await.expect("delete");
        })
        .await;

    let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("delete broadcast timed out")
        .expect("channel closed");

    let html = msg.as_str();
    assert!(html.contains("delete"), "must contain delete swap: {html}");
    // Trunk-dev delete tombstone: <div id="live_item-{id}" hx-swap-oob="delete">
    assert!(
        html.contains(&format!("live_item-{}", saved.id)),
        "must contain element id live_item-{}, got: {html}",
        saved.id
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn no_broadcasts_attr_emits_nothing() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_items");

    let repo = PgSilentItemRepository::with_pool_untracked(pool);
    let new_item = NewSilentItem {
        name: "quiet".to_owned(),
    };
    // Even inside a CURRENT_CHANNELS scope, SilentItemRepository has no broadcasts attr
    CURRENT_CHANNELS
        .scope(channels, async {
            repo.save(&new_item).await.expect("save");
        })
        .await;

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
    assert!(result.is_err(), "no broadcasts should have been emitted");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn with_pool_untracked_skips_broadcast_silently() {
    let (_container, pool) = setup_db().await;
    let channels = Channels::new(16);
    let mut rx = channels.subscribe("live_items");

    // No CURRENT_CHANNELS scope — get_global_channels() returns None
    let repo = PgLiveItemRepository::with_pool_untracked(pool);
    let new_item = NewLiveItem {
        name: "no_state".to_owned(),
    };
    let _saved = repo
        .save(&new_item)
        .await
        .expect("save succeeds even without broadcast context");

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
    assert!(
        result.is_err(),
        "without CURRENT_CHANNELS scope, no broadcast should fire"
    );
    drop(channels);
}

// ── LiveFragment compile/unit tests (no DB needed) ───────────────────────────

#[test]
fn live_fragment_dom_id_for_matches_dom_id() {
    let item = LiveItem {
        id: 42,
        name: "test".to_owned(),
    };
    assert_eq!(item.dom_id(), LiveItem::dom_id_for(42));
    assert_eq!(item.dom_id(), "live-item-42");
}

#[test]
fn live_fragment_render_contains_dom_id() {
    let item = LiveItem {
        id: 7,
        name: "hello".to_owned(),
    };
    let markup = item.render_fragment().into_string();
    assert!(
        markup.contains("live-item-7"),
        "render must use dom_id: {markup}"
    );
    assert!(
        markup.contains("hello"),
        "render must include item name: {markup}"
    );
}
