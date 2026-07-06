//! Database-level integration tests for mutation hook lifecycle.
//!
//! These tests spin up a real Postgres instance via testcontainers and validate
//! that before hooks integrate correctly with actual database operations.
//!
//! **Requires Docker** to be running.

use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, MutationOp, UpdateDraft};
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Schema & model definitions ─────────────────────────────────────────────────

diesel::table! {
    test_articles (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        status -> Text,
        published_at -> Nullable<Timestamp>,
    }
}

#[derive(Debug, Clone, Queryable, Selectable, AsChangeset, PartialEq)]
#[diesel(table_name = test_articles)]
struct Article {
    pub id: i64,
    pub title: String,
    pub slug: String,
    pub status: String,
    pub published_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = test_articles)]
struct NewArticle {
    pub title: String,
    pub slug: String,
    pub status: String,
}

// ── Hook implementations ─────────────────────────────────────────────────────

/// Rewrites the `slug` field from the `title` when title changes.
#[derive(Clone, Default)]
struct SlugRewriteHooks;

impl MutationHooks for SlugRewriteHooks {
    type Model = Article;
    type NewModel = NewArticle;
    type UpdateModel = ();

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Article>,
    ) -> AutumnResult<()> {
        if draft.before.title != draft.after.title {
            let new_slug = draft.after.title.to_lowercase().replace(' ', "-");
            draft.after.slug = new_slug;
        }
        Ok(())
    }
}

/// Rejects creation when the title is empty.
#[derive(Clone, Default)]
struct RejectEmptyTitleHooks;

impl MutationHooks for RejectEmptyTitleHooks {
    type Model = Article;
    type NewModel = NewArticle;
    type UpdateModel = ();

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewArticle,
    ) -> AutumnResult<()> {
        if new.title.trim().is_empty() {
            return Err(autumn_web::AutumnError::bad_request_msg(
                "title must not be empty",
            ));
        }
        Ok(())
    }
}

// ── Test helpers ────────────────────────────────────────────────────────────

const CREATE_TABLE_SQL: &str = r"
    CREATE TABLE IF NOT EXISTS test_articles (
        id BIGSERIAL PRIMARY KEY,
        title TEXT NOT NULL,
        slug TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'draft',
        published_at TIMESTAMP
    )
";

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("failed to get port");

    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager)
        .max_size(5)
        .build()
        .expect("failed to build pool");

    // Create the test table.
    let mut conn = pool.get().await.expect("failed to get connection");
    diesel::sql_query(CREATE_TABLE_SQL)
        .execute(&mut conn)
        .await
        .expect("failed to create table");

    (pool, container)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn before_update_rewrites_derived_field() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.unwrap();

    // Insert an article.
    let inserted: Article = diesel::insert_into(test_articles::table)
        .values(&NewArticle {
            title: "Hello World".into(),
            slug: "hello-world".into(),
            status: "draft".into(),
        })
        .get_result(&mut conn)
        .await
        .unwrap();

    assert_eq!(inserted.slug, "hello-world");

    // Build an UpdateDraft with a title change.
    let mut draft = UpdateDraft::new(inserted.clone());
    draft.after.title = "Goodbye World".into();

    // Run before_update hook -- should rewrite slug.
    let hooks = SlugRewriteHooks;
    let mut ctx = MutationContext::new(MutationOp::Update);
    hooks.before_update(&mut ctx, &mut draft).await.unwrap();

    assert_eq!(draft.after.slug, "goodbye-world");
    assert_eq!(draft.before.slug, "hello-world");

    // Persist the change using AsChangeset.
    diesel::update(test_articles::table.find(inserted.id))
        .set(&draft.after)
        .execute(&mut conn)
        .await
        .unwrap();

    // Verify DB state.
    let updated: Article = test_articles::table
        .find(inserted.id)
        .first(&mut conn)
        .await
        .unwrap();

    assert_eq!(updated.title, "Goodbye World");
    assert_eq!(updated.slug, "goodbye-world");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn before_create_rejection_prevents_insert() {
    let (pool, _container) = setup_pool().await;
    let conn = pool.get().await.unwrap();

    let hooks = RejectEmptyTitleHooks;
    let mut ctx = MutationContext::new(MutationOp::Create);
    let mut new_article = NewArticle {
        title: "   ".into(),
        slug: "empty".into(),
        status: "draft".into(),
    };

    // Hook should reject.
    let result = hooks.before_create(&mut ctx, &mut new_article).await;
    assert!(result.is_err());

    // No row should exist in the DB (we never inserted).
    let count: i64 = test_articles::table
        .count()
        .get_result(&mut &*conn)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn draft_field_accessors_match_persisted_diff() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.unwrap();

    // Insert original article.
    let original: Article = diesel::insert_into(test_articles::table)
        .values(&NewArticle {
            title: "Original Title".into(),
            slug: "original-title".into(),
            status: "draft".into(),
        })
        .get_result(&mut conn)
        .await
        .unwrap();

    // Build a draft with specific changes.
    let mut draft = UpdateDraft::new(original.clone());
    draft.after.title = "Updated Title".into();
    draft.after.status = "published".into();
    // slug and published_at remain unchanged.

    // Verify accessors on the draft.
    assert_ne!(draft.before().title, draft.after().title);
    assert_ne!(draft.before().status, draft.after().status);
    assert_eq!(draft.before().slug, draft.after().slug);
    assert_eq!(draft.before().published_at, draft.after().published_at);

    // Persist via AsChangeset.
    diesel::update(test_articles::table.find(original.id))
        .set(&draft.after)
        .execute(&mut conn)
        .await
        .unwrap();

    // Verify DB matches the draft's after state.
    let persisted: Article = test_articles::table
        .find(original.id)
        .first(&mut conn)
        .await
        .unwrap();

    assert_eq!(persisted.title, "Updated Title");
    assert_eq!(persisted.status, "published");
    assert_eq!(persisted.slug, "original-title"); // unchanged
    assert_eq!(persisted.published_at, None); // unchanged
}

// ── DbApiTokenStore tests ────────────────────────────────────────────────────────────────────────

const CREATE_API_TOKENS_SQL: &str = "
    CREATE TABLE api_tokens (
        id BIGSERIAL PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        principal_id TEXT NOT NULL,
        created_at TIMESTAMP NOT NULL DEFAULT NOW(),
        revoked_at TIMESTAMP,
        name TEXT NOT NULL DEFAULT '',
        scopes JSONB NOT NULL DEFAULT '[]'::jsonb,
        expires_at TIMESTAMP,
        last_used_at TIMESTAMP
    )
";

async fn setup_token_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(CREATE_API_TOKENS_SQL)
        .execute(&mut conn)
        .await
        .expect("create api_tokens table");
    (pool, container)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_api_token_store_issue_and_verify() {
    use autumn_web::auth::{ApiTokenStore as _, DbApiTokenStore};
    let (pool, _c) = setup_token_pool().await;
    let store = DbApiTokenStore::new(pool);

    let token = store.issue("user:42").await.unwrap();
    assert!(!token.is_empty(), "issued token must be non-empty");

    let principal = store.verify(&token).await.unwrap();
    assert_eq!(principal, Some("user:42".to_owned()));

    let unknown = store.verify("not_a_real_token").await.unwrap();
    assert_eq!(unknown, None, "unknown token must return None");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_api_token_store_revoke_invalidates_token() {
    use autumn_web::auth::{ApiTokenStore as _, DbApiTokenStore};
    let (pool, _c) = setup_token_pool().await;
    let store = DbApiTokenStore::new(pool);

    let token = store.issue("user:7").await.unwrap();
    assert!(store.verify(&token).await.unwrap().is_some());

    store.revoke(&token).await.unwrap();
    assert_eq!(store.verify(&token).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_api_token_store_two_tokens_same_principal() {
    use autumn_web::auth::{ApiTokenStore as _, DbApiTokenStore};
    let (pool, _c) = setup_token_pool().await;
    let store = DbApiTokenStore::new(pool);

    let t1 = store.issue("user:1").await.unwrap();
    let t2 = store.issue("user:1").await.unwrap();
    assert_ne!(t1, t2, "each issued token must be unique");

    assert_eq!(store.verify(&t1).await.unwrap(), Some("user:1".to_owned()));
    assert_eq!(store.verify(&t2).await.unwrap(), Some("user:1".to_owned()));

    // Revoking t1 must not affect t2.
    store.revoke(&t1).await.unwrap();
    assert_eq!(store.verify(&t1).await.unwrap(), None);
    assert_eq!(store.verify(&t2).await.unwrap(), Some("user:1".to_owned()));
}

// ── PostgresIsrCoordinator tests ─────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_isr_coordinator_backend_name() {
    use autumn_web::static_gen::PostgresIsrCoordinator;
    use autumn_web::static_gen::isr_coordinator::IsrCoordinator as _;

    let (pool, _container) = setup_pool().await;
    let coord = PostgresIsrCoordinator::new(pool);
    assert_eq!(coord.backend(), "postgres");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_isr_coordinator_acquire_grants_and_release_unlocks() {
    use autumn_web::static_gen::PostgresIsrCoordinator;
    use autumn_web::static_gen::isr_coordinator::IsrCoordinator as _;

    let (pool, _container) = setup_pool().await;
    let coord = PostgresIsrCoordinator::new(pool);

    assert!(
        coord.try_acquire("/about", "window-1").await,
        "first acquire must succeed"
    );
    // release must not panic
    coord.release("/about", "window-1").await;

    // after release, the same window is acquirable again
    assert!(
        coord.try_acquire("/about", "window-1").await,
        "acquire after release must succeed"
    );
    coord.release("/about", "window-1").await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_isr_coordinator_blocks_duplicate_window() {
    use autumn_web::static_gen::PostgresIsrCoordinator;
    use autumn_web::static_gen::isr_coordinator::IsrCoordinator as _;

    let (pool, _container) = setup_pool().await;
    let coord = PostgresIsrCoordinator::new(pool);

    // First acquisition holds the advisory lock on its connection.
    assert!(
        coord.try_acquire("/about", "window-1").await,
        "first acquire must succeed"
    );

    // Second acquisition for the same (route, window) must fail: the advisory
    // lock is session-scoped, and the first connection still holds it.
    assert!(
        !coord.try_acquire("/about", "window-1").await,
        "duplicate acquire for the same window must fail"
    );

    coord.release("/about", "window-1").await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_isr_coordinator_release_without_acquire_is_noop() {
    use autumn_web::static_gen::PostgresIsrCoordinator;
    use autumn_web::static_gen::isr_coordinator::IsrCoordinator as _;

    let (pool, _container) = setup_pool().await;
    let coord = PostgresIsrCoordinator::new(pool);

    // release without a preceding acquire must not panic (emits a warning).
    coord.release("/about", "window-1").await;

    // Subsequent acquire must still succeed.
    assert!(
        coord.try_acquire("/about", "window-1").await,
        "acquire after orphaned release must succeed"
    );
    coord.release("/about", "window-1").await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_isr_coordinator_independent_routes_and_windows() {
    use autumn_web::static_gen::PostgresIsrCoordinator;
    use autumn_web::static_gen::isr_coordinator::IsrCoordinator as _;

    let (pool, _container) = setup_pool().await;
    let coord = PostgresIsrCoordinator::new(pool);

    // Different routes in the same window get different lock keys.
    assert!(coord.try_acquire("/", "window-1").await);
    assert!(coord.try_acquire("/about", "window-1").await);

    coord.release("/", "window-1").await;
    coord.release("/about", "window-1").await;

    // Same route in different windows also gets different lock keys.
    assert!(coord.try_acquire("/about", "window-1").await);
    assert!(coord.try_acquire("/about", "window-2").await);

    coord.release("/about", "window-1").await;
    coord.release("/about", "window-2").await;
}
