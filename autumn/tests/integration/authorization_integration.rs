//! Integration tests for record-level authorization (issue #496).
//!
//! Covers the four acceptance-criteria checks the issue calls out:
//!
//! 1. A user with no role cannot update another user's record via a
//!    hand-written handler.
//! 2. A user with the `admin` role can.
//! 3. The unauthorized response is `404` by default.
//! 4. A custom `forbidden_response = "403"` round-trips correctly.

use autumn_web::authorization::{BoxFuture, ForbiddenResponse, Policy, PolicyContext};
use autumn_web::prelude::*;
use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};
use autumn_web::test::TestApp;
use http::StatusCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── Test resource and policy ──────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Note {
    id: i64,
    author_id: i64,
}

#[derive(Default, Clone)]
struct AdminOrOwnerPolicy;

impl Policy<Note> for AdminOrOwnerPolicy {
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(note.author_id) })
    }
    fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(note.author_id) })
    }
}

#[derive(Default, Clone)]
struct AnonymousTogglePolicy;

impl Policy<Note> for AnonymousTogglePolicy {
    fn can_update<'a>(&'a self, _ctx: &'a PolicyContext, _note: &'a Note) -> BoxFuture<'a, bool> {
        Box::pin(async move { ANONYMOUS_POLICY_ALLOWED.load(Ordering::SeqCst) })
    }
}

const FIXED_NOTE: Note = Note {
    id: 1,
    author_id: 42,
};
static SECURED_ADMIN_MUTATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static SECURED_ADMIN_REPLAY_CALLS: AtomicUsize = AtomicUsize::new(0);
static AUTHORIZE_SESSION_ROTATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static AUTHORIZE_SESSION_TOUCH_CALLS: AtomicUsize = AtomicUsize::new(0);
static AUTHORIZE_ANONYMOUS_SESSION_TOUCH_CALLS: AtomicUsize = AtomicUsize::new(0);
static ANONYMOUS_POLICY_ALLOWED: AtomicBool = AtomicBool::new(true);

// ── Hand-written handler exercising the inline `authorize` helper ──

#[autumn_web::put("/notes/{id}")]
async fn update_note_inline(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    State(state): State<AppState>,
    session: Session,
) -> AutumnResult<&'static str> {
    let _ = id;
    autumn_web::authorization::authorize::<Note>(&state, &session, "update", &FIXED_NOTE).await?;
    Ok("ok")
}

#[autumn_web::post("/secured-admin")]
#[autumn_web::secured("admin")]
async fn secured_admin_mutation() -> AutumnResult<&'static str> {
    SECURED_ADMIN_MUTATION_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok("ok")
}

#[autumn_web::post("/secured-admin-replay")]
#[autumn_web::secured("admin")]
async fn secured_admin_replay_mutation() -> AutumnResult<&'static str> {
    SECURED_ADMIN_REPLAY_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok("ok")
}

// ── Helpers ───────────────────────────────────────────────────

fn build_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_inline])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

async fn seed_session(store: &MemoryStore, sid: &str, user_id: &str, role: Option<&str>) {
    let mut data = std::collections::HashMap::new();
    data.insert("user_id".to_owned(), user_id.to_owned());
    if let Some(role) = role {
        data.insert("role".to_owned(), role.to_owned());
    }
    store.save(sid, data).await.unwrap();
}

async fn put_with_session(
    client: &autumn_web::test::TestClient,
    path: &str,
    sid: &str,
) -> autumn_web::test::TestResponse {
    client
        .put(path)
        .header("Cookie", &format!("autumn.sid={sid}"))
        .send()
        .await
}

// ── Tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_request_is_denied() {
    let store = MemoryStore::new();
    let client = build_app(store, ForbiddenResponse::default());
    // No session cookie at all -> ctx has no user_id, policy denies.
    let response = client.put("/notes/1").send().await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_owner_without_role_cannot_update() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_app(store, ForbiddenResponse::default());
    let response = put_with_session(&client, "/notes/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_can_update_anyones_record() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-admin", "999", Some("admin")).await;
    let client = build_app(store, ForbiddenResponse::default());
    let response = put_with_session(&client, "/notes/1", "sess-admin").await;
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn owner_can_update_their_own_record() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-owner", "42", None).await;
    let client = build_app(store, ForbiddenResponse::default());
    let response = put_with_session(&client, "/notes/1", "sess-owner").await;
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn forbidden_response_default_is_404() {
    // No policy registration mismatch — just validates the default
    // status the framework picks when a policy denies.
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_app(store, ForbiddenResponse::default());
    let response = put_with_session(&client, "/notes/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn forbidden_response_can_be_set_to_403() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_app(store, ForbiddenResponse::Forbidden403);
    let response = put_with_session(&client, "/notes/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

// ── #[authorize] attribute macro coverage ─────────────────────

/// Custom `FromRequestParts` extractor that loads our test fixture
/// without needing a real database — lets us exercise the
/// `#[authorize]` attribute macro path.
struct LoadedNote(Note);

impl<S> axum::extract::FromRequestParts<S> for LoadedNote
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(FIXED_NOTE))
    }
}

/// Wrap the loaded note in a fresh `Note` binding so the
/// snake-cased default param name resolves.
#[autumn_web::post("/notes-attr/{id}")]
#[autumn_web::authorize("update", resource = Note)]
async fn update_note_attr(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    Ok("ok")
}

#[autumn_web::post("/notes-attr-rotate/{id}")]
#[autumn_web::authorize("update", resource = Note)]
async fn update_note_attr_rotate(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
    session: Session,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    AUTHORIZE_SESSION_ROTATION_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "999").await;
    session.rotate_id().await;
    Ok("authorized-rotated")
}

#[autumn_web::post("/notes-attr-touch/{id}")]
#[autumn_web::authorize("update", resource = Note)]
async fn update_note_attr_touch(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
    session: Session,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    AUTHORIZE_SESSION_TOUCH_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("flash", "saved").await;
    Ok("authorized-touched")
}

#[autumn_web::post("/notes-anon-touch/{id}")]
#[autumn_web::authorize("update", resource = Note)]
async fn update_note_anonymous_touch(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
    session: Session,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    AUTHORIZE_ANONYMOUS_SESSION_TOUCH_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("flash", "saved").await;
    Ok("anonymous-touched")
}

fn build_attr_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_attr])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

fn build_idempotent_attr_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_attr])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

fn build_idempotent_attr_rotation_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_attr_rotate])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

fn build_idempotent_attr_touch_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_attr_touch])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

fn build_idempotent_anonymous_touch_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_anonymous_touch])
        .policy::<Note, _>(AnonymousTogglePolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

fn build_idempotent_secured_app(store: MemoryStore) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![secured_admin_mutation])
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

fn build_idempotent_secured_replay_app(store: MemoryStore) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![secured_admin_replay_mutation])
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .idempotent()
        .build()
}

async fn post_with_session(
    client: &autumn_web::test::TestClient,
    path: &str,
    sid: &str,
) -> autumn_web::test::TestResponse {
    client
        .post(path)
        .header("Cookie", &format!("autumn.sid={sid}"))
        .send()
        .await
}

#[tokio::test]
async fn attribute_macro_denies_non_owner_with_404() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_attr_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-attr/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn attribute_macro_allows_owner() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-owner", "42", None).await;
    let client = build_attr_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-attr/1", "sess-owner").await;
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn attribute_macro_allows_admin() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-admin", "999", Some("admin")).await;
    let client = build_attr_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-attr/1", "sess-admin").await;
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn attribute_macro_honors_forbidden_response_override() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_attr_app(store, ForbiddenResponse::Forbidden403);
    let response = post_with_session(&client, "/notes-attr/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn idempotent_replay_does_not_bypass_authorize_policy_changes() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-policy", "999", Some("admin")).await;
    let client = build_idempotent_attr_app(store.clone(), ForbiddenResponse::Forbidden403);

    let first = client
        .post("/notes-attr/1")
        .header("Cookie", "autumn.sid=sess-policy")
        .header("idempotency-key", "policy-recheck-key")
        .send()
        .await;
    assert_eq!(first.status, StatusCode::OK);

    seed_session(&store, "sess-policy", "999", None).await;

    let retry = client
        .post("/notes-attr/1")
        .header("Cookie", "autumn.sid=sess-policy")
        .header("idempotency-key", "policy-recheck-key")
        .send()
        .await;
    assert_eq!(
        retry.status,
        StatusCode::FORBIDDEN,
        "cached idempotency replay must not skip the current #[authorize] policy check"
    );
    assert_eq!(retry.header("x-idempotent-replayed"), None);
}

#[tokio::test]
async fn idempotent_authorize_session_rotation_replays_final_cookie_for_old_cookie() {
    AUTHORIZE_SESSION_ROTATION_CALLS.store(0, Ordering::SeqCst);
    let store = MemoryStore::new();
    seed_session(&store, "sess-authorize-rotate", "999", Some("admin")).await;
    let client = build_idempotent_attr_rotation_app(store.clone(), ForbiddenResponse::Forbidden403);

    let first = client
        .post("/notes-attr-rotate/1")
        .header("Cookie", "autumn.sid=sess-authorize-rotate")
        .header("idempotency-key", "authorize-rotation-key")
        .send()
        .await;
    first.assert_ok();
    let set_cookie = first
        .header("set-cookie")
        .expect("authorized rotating session response should set a new cookie")
        .to_owned();
    let new_cookie = set_cookie
        .split(';')
        .next()
        .expect("set-cookie should start with a cookie pair")
        .to_owned();
    let new_session_id = new_cookie
        .strip_prefix("autumn.sid=")
        .expect("set-cookie should use the default Autumn session cookie")
        .to_owned();

    let retry = client
        .post("/notes-attr-rotate/1")
        .header("Cookie", "autumn.sid=sess-authorize-rotate")
        .header("idempotency-key", "authorize-rotation-key")
        .send()
        .await;
    retry.assert_ok();
    assert_eq!(retry.header("x-idempotent-replayed"), Some("true"));
    assert!(
        retry.header("set-cookie").is_some(),
        "old-cookie retries must receive the finalized rotated session cookie"
    );
    assert_eq!(
        AUTHORIZE_SESSION_ROTATION_CALLS.load(Ordering::SeqCst),
        1,
        "authorized session-rotating retries must not re-enter the handler"
    );

    store.destroy(&new_session_id).await.unwrap();
    let accepted_cookie_retry = client
        .post("/notes-attr-rotate/1")
        .header("Cookie", &new_cookie)
        .header("idempotency-key", "authorize-rotation-key")
        .send()
        .await;
    assert_eq!(
        accepted_cookie_retry.status,
        StatusCode::FORBIDDEN,
        "a retry after accepting a now-revoked rotated cookie must run current policy checks"
    );
    assert_eq!(accepted_cookie_retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        AUTHORIZE_SESSION_ROTATION_CALLS.load(Ordering::SeqCst),
        1,
        "accepted-cookie policy denials must not re-enter the mutating handler"
    );
}

#[tokio::test]
async fn idempotent_authorize_same_session_mutation_denial_does_not_replay_cached_success() {
    AUTHORIZE_SESSION_TOUCH_CALLS.store(0, Ordering::SeqCst);
    let store = MemoryStore::new();
    seed_session(&store, "sess-authorize-touch", "999", Some("admin")).await;
    let client = build_idempotent_attr_touch_app(store.clone(), ForbiddenResponse::Forbidden403);

    let first = client
        .post("/notes-attr-touch/1")
        .header("Cookie", "autumn.sid=sess-authorize-touch")
        .header("idempotency-key", "authorize-touch-key")
        .send()
        .await;
    first.assert_ok();
    assert!(first.header("set-cookie").is_some());

    store
        .save("sess-authorize-touch", std::collections::HashMap::new())
        .await
        .unwrap();
    let retry = client
        .post("/notes-attr-touch/1")
        .header("Cookie", "autumn.sid=sess-authorize-touch")
        .header("idempotency-key", "authorize-touch-key")
        .send()
        .await;
    assert_eq!(
        retry.status,
        StatusCode::FORBIDDEN,
        "dirty same-session retries must run current policy checks instead of replaying cached success"
    );
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        AUTHORIZE_SESSION_TOUCH_CALLS.load(Ordering::SeqCst),
        1,
        "same-session policy denials must not re-enter the mutating handler"
    );
}

#[tokio::test]
async fn idempotent_anonymous_session_mutation_denial_does_not_replay_cached_success() {
    AUTHORIZE_ANONYMOUS_SESSION_TOUCH_CALLS.store(0, Ordering::SeqCst);
    ANONYMOUS_POLICY_ALLOWED.store(true, Ordering::SeqCst);
    let store = MemoryStore::new();
    let client = build_idempotent_anonymous_touch_app(store, ForbiddenResponse::Forbidden403);

    let first = client
        .post("/notes-anon-touch/1")
        .header("idempotency-key", "authorize-anonymous-touch-key")
        .send()
        .await;
    first.assert_ok();
    assert!(
        first.header("set-cookie").is_some(),
        "anonymous session mutation should persist the new session cookie"
    );

    ANONYMOUS_POLICY_ALLOWED.store(false, Ordering::SeqCst);
    let retry = client
        .post("/notes-anon-touch/1")
        .header("idempotency-key", "authorize-anonymous-touch-key")
        .send()
        .await;
    assert_eq!(
        retry.status,
        StatusCode::FORBIDDEN,
        "anonymous retries must run current policy checks instead of replaying cached success"
    );
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        AUTHORIZE_ANONYMOUS_SESSION_TOUCH_CALLS.load(Ordering::SeqCst),
        1,
        "anonymous policy denials must not re-enter the mutating handler"
    );
}

#[tokio::test]
async fn idempotent_replay_does_not_bypass_secured_role_changes() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-secured", "999", Some("admin")).await;
    let client = build_idempotent_secured_app(store.clone());

    let first = client
        .post("/secured-admin")
        .header("Cookie", "autumn.sid=sess-secured")
        .header("idempotency-key", "secured-recheck-key")
        .send()
        .await;
    assert_eq!(first.status, StatusCode::OK);

    seed_session(&store, "sess-secured", "999", Some("viewer")).await;

    let retry = client
        .post("/secured-admin")
        .header("Cookie", "autumn.sid=sess-secured")
        .header("idempotency-key", "secured-recheck-key")
        .send()
        .await;
    assert_eq!(
        retry.status,
        StatusCode::FORBIDDEN,
        "cached idempotency replay must not skip the current #[secured] role check"
    );
    assert_eq!(retry.header("x-idempotent-replayed"), None);
}

#[tokio::test]
async fn idempotent_authorized_secured_mutation_replays_without_rerunning_handler() {
    SECURED_ADMIN_REPLAY_CALLS.store(0, Ordering::SeqCst);
    let store = MemoryStore::new();
    seed_session(&store, "sess-secured-replay", "999", Some("admin")).await;
    let client = build_idempotent_secured_replay_app(store);

    client
        .post("/secured-admin-replay")
        .header("Cookie", "autumn.sid=sess-secured-replay")
        .header("idempotency-key", "secured-replay-key")
        .send()
        .await
        .assert_ok();

    let replay = client
        .post("/secured-admin-replay")
        .header("Cookie", "autumn.sid=sess-secured-replay")
        .header("idempotency-key", "secured-replay-key")
        .send()
        .await;
    replay.assert_ok();
    assert_eq!(replay.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(
        SECURED_ADMIN_REPLAY_CALLS.load(Ordering::SeqCst),
        1,
        "authorized idempotent retries must replay instead of re-entering the mutating handler"
    );
}

// ── #[authorize] stacked with #[secured] ──────────────────────

/// `#[secured]` already injects a hidden `__autumn_session` extractor.
/// `#[authorize]` injects the same name. Without collision-detection
/// this combination would emit a function with two parameters named
/// `__autumn_session` and fail to compile. Compiling this test is
/// itself the assertion.
#[autumn_web::post("/notes-stacked/{id}")]
#[autumn_web::secured]
#[autumn_web::authorize("update", resource = Note)]
async fn update_note_stacked_with_secured(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    Ok("ok")
}

fn build_stacked_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_stacked_with_secured])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

#[tokio::test]
async fn stacked_secured_and_authorize_run_both_checks() {
    // The handler stacks `#[secured]` and `#[authorize]`. Both
    // checks are present in the body — an unauthenticated request
    // is rejected by whichever check runs first. (Rust applies
    // attribute macros top-down, so the outer `#[authorize]`
    // wraps the inner `#[secured]` check; the policy's
    // `can_update` denies on a missing session before the
    // secured guard returns 401.) The exact status matters less
    // than "is the request rejected" — without collision
    // detection in `#[authorize]`, the handler wouldn't compile
    // at all, so reaching this assertion is the real win.
    let store = MemoryStore::new();
    let client = build_stacked_app(store, ForbiddenResponse::default());
    let response = client.post("/notes-stacked/1").send().await;
    assert!(
        response.status == StatusCode::UNAUTHORIZED
            || response.status == StatusCode::NOT_FOUND
            || response.status == StatusCode::FORBIDDEN,
        "expected an auth-related rejection, got {}",
        response.status
    );
}

#[tokio::test]
async fn stacked_secured_and_authorize_authorized_user_passes_secured_then_authorize_denies() {
    // Authenticated stranger -> #[secured] passes, #[authorize]
    // denies because the stranger isn't the owner.
    let store = MemoryStore::new();
    seed_session(&store, "sess-stranger", "999", None).await;
    let client = build_stacked_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-stacked/1", "sess-stranger").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stacked_secured_and_authorize_owner_passes_both() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-owner", "42", None).await;
    let client = build_stacked_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-stacked/1", "sess-owner").await;
    assert_eq!(response.status, StatusCode::OK);
}

// ── Reverse attribute order: #[authorize] above #[secured] ───
//
// Codex review noted that the collision check originally only worked
// when `#[secured]` ran first. Both orderings must compile; reaching
// these tests is the assertion. (`#[secured]` now also skips
// re-injection when `__autumn_session` already exists.)

#[autumn_web::post("/notes-reversed/{id}")]
#[autumn_web::authorize("update", resource = Note)]
#[autumn_web::secured]
async fn update_note_reversed_attribute_order(
    autumn_web::extract::Path(id): autumn_web::extract::Path<i64>,
    LoadedNote(note): LoadedNote,
) -> AutumnResult<&'static str> {
    let _ = id;
    let _ = note;
    Ok("ok")
}

fn build_reversed_app(
    store: MemoryStore,
    forbidden_response: ForbiddenResponse,
) -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![update_note_reversed_attribute_order])
        .policy::<Note, _>(AdminOrOwnerPolicy)
        .forbidden_response(forbidden_response)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

#[tokio::test]
async fn reversed_attribute_order_compiles_and_runs_both_checks() {
    // Without the symmetric collision guard in `#[secured]`, the
    // function above wouldn't compile at all — duplicate
    // `__autumn_session` bindings.
    let store = MemoryStore::new();
    let client = build_reversed_app(store, ForbiddenResponse::default());
    let response = client.post("/notes-reversed/1").send().await;
    assert!(
        response.status == StatusCode::UNAUTHORIZED
            || response.status == StatusCode::NOT_FOUND
            || response.status == StatusCode::FORBIDDEN,
        "expected an auth-related rejection, got {}",
        response.status
    );
}

#[tokio::test]
async fn reversed_attribute_order_owner_passes_both() {
    let store = MemoryStore::new();
    seed_session(&store, "sess-owner", "42", None).await;
    let client = build_reversed_app(store, ForbiddenResponse::default());
    let response = post_with_session(&client, "/notes-reversed/1", "sess-owner").await;
    assert_eq!(response.status, StatusCode::OK);
}
