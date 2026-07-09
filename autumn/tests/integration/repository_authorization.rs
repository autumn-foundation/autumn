//! End-to-end integration test for the `#[repository(policy = ...)]`
//! auto-generated CRUD endpoints (issue #496, AC #9).
//!
//! Spins up a real Postgres instance via testcontainers, mounts the
//! `#[repository(api = "/api/notes", policy = NotePolicy, scope = NoteScope)]`
//! handlers behind `TestApp`, and exercises the four AC-listed
//! permission outcomes over real HTTP:
//!
//! 1. A user with no role cannot update another user's record.
//! 2. A user with the `admin` role can.
//! 3. The unauthorized response is `404` by default.
//! 4. A custom `forbidden_response = "403"` round-trips correctly.
//!
//! Plus a `Scope` test confirming the `GET /api/notes` index endpoint
//! filters records to the current user (and that
//! `Note::scope(&ctx).load(&mut db).await?` does the same thing in
//! a hand-written list handler).
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]

use autumn_web::authorization::{
    BoxFuture, ForbiddenResponse, Policy, PolicyContext, Scope, Scoped,
};
use autumn_web::prelude::*;
use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};
use autumn_web::test::TestApp;
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use http::StatusCode;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Schema + model ────────────────────────────────────────────

diesel::table! {
    test_notes (id) {
        id -> Int8,
        title -> Text,
        author_id -> Int8,
    }
}

#[autumn_web::model(table = "test_notes")]
pub struct Note {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

#[autumn_web::repository(
    Note,
    table = "test_notes",
    api = "/api/notes",
    policy = NotePolicy,
    scope = NoteScope,
)]
pub trait NoteRepository {}

// ── Policy + Scope ────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct NotePolicy;

impl Policy<Note> for NotePolicy {
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn can_create<'a>(&'a self, ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.is_authenticated() })
    }
    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(note.author_id) })
    }
    fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(note.author_id) })
    }
}

#[derive(Default, Clone)]
pub struct NoteScope;

impl Scope<Note> for NoteScope {
    fn list<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        conn: &'a mut AsyncPgConnection,
    ) -> BoxFuture<'a, AutumnResult<Vec<Note>>> {
        Box::pin(async move {
            // Admins see all notes; everyone else sees only their own.
            let user_id = ctx.user_id_i64();
            if ctx.has_role("admin") {
                test_notes::table
                    .order(test_notes::id.asc())
                    .load::<Note>(conn)
                    .await
                    .map_err(AutumnError::from)
            } else if let Some(uid) = user_id {
                test_notes::table
                    .filter(test_notes::author_id.eq(uid))
                    .order(test_notes::id.asc())
                    .load::<Note>(conn)
                    .await
                    .map_err(AutumnError::from)
            } else {
                Ok(Vec::new()) // anon -> empty
            }
        })
    }
}

// ── Hand-written list handler exercising `Note::scope(&ctx).load(&mut db)` ─

#[autumn_web::get("/notes/mine")]
async fn list_my_notes(
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
) -> AutumnResult<Json<Vec<Note>>> {
    let ctx = PolicyContext::from_request(&state, &session).await;
    let notes = Note::scope(&ctx).load(&mut db).await?;
    Ok(Json(notes))
}

// ── Test helpers ──────────────────────────────────────────────

const CREATE_TABLE_SQL: &str = r"
    CREATE TABLE IF NOT EXISTS test_notes (
        id BIGSERIAL PRIMARY KEY,
        title TEXT NOT NULL,
        author_id BIGINT NOT NULL
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
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");

    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("DROP TABLE IF EXISTS test_notes")
        .execute(&mut conn)
        .await
        .expect("drop");
    diesel::sql_query(CREATE_TABLE_SQL)
        .execute(&mut conn)
        .await
        .expect("create");
    diesel::sql_query("TRUNCATE test_notes RESTART IDENTITY")
        .execute(&mut conn)
        .await
        .expect("truncate");

    (pool, container)
}

async fn seed_note(pool: &Pool<AsyncPgConnection>, title: &str, author_id: i64) -> i64 {
    let mut conn = pool.get().await.unwrap();
    diesel::insert_into(test_notes::table)
        .values((
            test_notes::title.eq(title),
            test_notes::author_id.eq(author_id),
        ))
        .returning(test_notes::id)
        .get_result(&mut conn)
        .await
        .unwrap()
}

async fn seed_session(store: &MemoryStore, sid: &str, user_id: &str, role: Option<&str>) {
    let mut data = std::collections::HashMap::new();
    data.insert("user_id".to_owned(), user_id.to_owned());
    if let Some(r) = role {
        data.insert("role".to_owned(), r.to_owned());
    }
    store.save(sid, data).await.unwrap();
}

fn build_app(
    pool: Pool<AsyncPgConnection>,
    store: MemoryStore,
    forbidden: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .with_db(pool)
        .routes(vec![
            __autumn_route_info_note_api_list(),
            __autumn_route_info_note_api_get(),
            __autumn_route_info_note_api_create(),
            __autumn_route_info_note_api_update(),
            __autumn_route_info_note_api_delete(),
            __autumn_route_info_list_my_notes(),
        ])
        .policy::<Note, _>(NotePolicy)
        .scope::<Note, _>(NoteScope)
        .forbidden_response(forbidden)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

// ── AC #9: integration tests against the auto-generated handlers ──

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac_9a_non_owner_cannot_update_via_repository_endpoint() {
    let (pool, _container) = setup_pool().await;
    let note_id = seed_note(&pool, "Owner's note", 42).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .put(&format!("/api/notes/{note_id}"))
        .header("Cookie", "autumn.sid=sess-stranger")
        .json(&serde_json::json!({"title": "Hacked"}))
        .send()
        .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac_9b_admin_can_update_via_repository_endpoint() {
    let (pool, _container) = setup_pool().await;
    let note_id = seed_note(&pool, "Owner's note", 42).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-admin", "999", Some("admin")).await;
    let client = build_app(pool.clone(), store, ForbiddenResponse::default());

    let response = client
        .put(&format!("/api/notes/{note_id}"))
        .header("Cookie", "autumn.sid=sess-admin")
        .json(&serde_json::json!({"title": "Edited by admin"}))
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);

    // Confirm DB write happened.
    let mut conn = pool.get().await.unwrap();
    let updated: Note = test_notes::table
        .find(note_id)
        .first(&mut conn)
        .await
        .unwrap();
    assert_eq!(updated.title, "Edited by admin");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac_9c_unauthorized_response_is_404_by_default() {
    let (pool, _container) = setup_pool().await;
    let note_id = seed_note(&pool, "Note", 42).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-other", "5", None).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .delete(&format!("/api/notes/{note_id}"))
        .header("Cookie", "autumn.sid=sess-other")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac_9d_custom_forbidden_response_403_round_trips() {
    let (pool, _container) = setup_pool().await;
    let note_id = seed_note(&pool, "Note", 42).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-other", "5", None).await;
    let client = build_app(pool, store, ForbiddenResponse::Forbidden403);

    let response = client
        .delete(&format!("/api/notes/{note_id}"))
        .header("Cookie", "autumn.sid=sess-other")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn owner_can_update_their_own_note_via_repository_endpoint() {
    let (pool, _container) = setup_pool().await;
    let note_id = seed_note(&pool, "Mine", 42).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-owner", "42", None).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .put(&format!("/api/notes/{note_id}"))
        .header("Cookie", "autumn.sid=sess-owner")
        .json(&serde_json::json!({"title": "Renamed"}))
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
}

// ── Scope tests ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scope_filters_repository_index_endpoint_to_owners_records() {
    let (pool, _container) = setup_pool().await;
    seed_note(&pool, "alice's", 1).await;
    seed_note(&pool, "bob's #1", 2).await;
    seed_note(&pool, "bob's #2", 2).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-bob", "2", None).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .get("/api/notes")
        .header("Cookie", "autumn.sid=sess-bob")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.len(), 2);
    for note in &body {
        assert_eq!(note["author_id"], 2);
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn admin_scope_returns_all_records() {
    let (pool, _container) = setup_pool().await;
    seed_note(&pool, "alice's", 1).await;
    seed_note(&pool, "bob's", 2).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-admin", "999", Some("admin")).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .get("/api/notes")
        .header("Cookie", "autumn.sid=sess-admin")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn handwritten_list_handler_uses_scope_via_blanket_trait() {
    // Exercises `Note::scope(&ctx).load(&mut db).await?` — the
    // blanket `Scoped` trait that the authorization guide
    // documents.
    let (pool, _container) = setup_pool().await;
    seed_note(&pool, "alice's", 1).await;
    seed_note(&pool, "bob's", 2).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-alice", "1", None).await;
    let client = build_app(pool, store, ForbiddenResponse::default());

    let response = client
        .get("/notes/mine")
        .header("Cookie", "autumn.sid=sess-alice")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["author_id"], 1);
    assert_eq!(body[0]["title"], "alice's");
}

// ── Policy-without-scope: list applies `can_show` per record ─

diesel::table! {
    test_secret_notes (id) {
        id -> Int8,
        title -> Text,
        author_id -> Int8,
    }
}

#[autumn_web::model(table = "test_secret_notes")]
pub struct SecretNote {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

// Mounted with `policy = ...` but *no* `scope = ...`. Used by
// `policy_without_scope_filters_index_via_can_show` to confirm the
// list endpoint falls back to per-record `can_show` filtering and
// does not just return everything (the data-exposure path the codex
// review caught).
#[autumn_web::repository(
    SecretNote,
    table = "test_secret_notes",
    api = "/api/secret-notes",
    policy = SecretNotePolicy,
)]
pub trait SecretNoteRepository {}

#[derive(Default, Clone)]
pub struct SecretNotePolicy;

impl Policy<SecretNote> for SecretNotePolicy {
    fn can_show<'a>(&'a self, ctx: &'a PolicyContext, note: &'a SecretNote) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.user_id_i64() == Some(note.author_id) })
    }
    fn can_update<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        note: &'a SecretNote,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.user_id_i64() == Some(note.author_id) })
    }
    fn can_delete<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        note: &'a SecretNote,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.user_id_i64() == Some(note.author_id) })
    }
}

const CREATE_SECRET_NOTES_SQL: &str = r"
    CREATE TABLE IF NOT EXISTS test_secret_notes (
        id BIGSERIAL PRIMARY KEY,
        title TEXT NOT NULL,
        author_id BIGINT NOT NULL
    )
";

async fn setup_secret_notes_pool() -> (
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
    diesel::sql_query("DROP TABLE IF EXISTS test_secret_notes")
        .execute(&mut conn)
        .await
        .expect("drop");
    diesel::sql_query(CREATE_SECRET_NOTES_SQL)
        .execute(&mut conn)
        .await
        .expect("create");

    (pool, container)
}

async fn seed_secret(pool: &Pool<AsyncPgConnection>, title: &str, author_id: i64) -> i64 {
    let mut conn = pool.get().await.unwrap();
    diesel::insert_into(test_secret_notes::table)
        .values((
            test_secret_notes::title.eq(title),
            test_secret_notes::author_id.eq(author_id),
        ))
        .returning(test_secret_notes::id)
        .get_result(&mut conn)
        .await
        .unwrap()
}

fn build_secret_app(
    pool: Pool<AsyncPgConnection>,
    store: MemoryStore,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .with_db(pool)
        .routes(vec![
            __autumn_route_info_secret_note_api_list(),
            __autumn_route_info_secret_note_api_get(),
        ])
        .policy::<SecretNote, _>(SecretNotePolicy)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn policy_without_scope_filters_index_via_can_show() {
    let (pool, _container) = setup_secret_notes_pool().await;
    seed_secret(&pool, "alice's secret", 1).await;
    seed_secret(&pool, "bob's secret #1", 2).await;
    seed_secret(&pool, "bob's secret #2", 2).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-bob", "2", None).await;
    let client = build_secret_app(pool, store);

    let response = client
        .get("/api/secret-notes")
        .header("Cookie", "autumn.sid=sess-bob")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body.len(), 2, "should only see bob's secrets");
    for note in &body {
        assert_eq!(note["author_id"], 2);
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn policy_without_scope_returns_empty_when_no_records_pass_can_show() {
    let (pool, _container) = setup_secret_notes_pool().await;
    seed_secret(&pool, "alice's", 1).await;

    let store = MemoryStore::new();
    seed_session(&store, "sess-bob", "999", None).await;
    let client = build_secret_app(pool, store);

    let response = client
        .get("/api/secret-notes")
        .header("Cookie", "autumn.sid=sess-bob")
        .send()
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body: Vec<serde_json::Value> = serde_json::from_slice(&response.body).unwrap();
    assert!(body.is_empty());
}
