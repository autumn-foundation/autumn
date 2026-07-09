//! Integration tests for scoped service tokens (issue #1158).
//!
//! Exercises the falsifiable success metric end-to-end with the in-memory
//! token store: a token granted scopes `S` gets `200` on endpoints requiring a
//! subset of `S` and `403` on endpoints requiring a scope outside `S`, plus the
//! lifecycle rules (expired → 401, revoked → 401) and the non-user-principal
//! path through a policy.

use std::sync::Arc;

use autumn_web::auth::{
    ApiTokenScopes, ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec, RequireApiToken,
};
use autumn_web::authorization::{BoxFuture, Policy, PolicyContext, authorize_with_scopes};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::Extension;
use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::FixedClock;
use chrono::{Duration as ChronoDuration, TimeZone as _, Utc};
use http::StatusCode;

// ── Handlers gated purely on token scopes (no session) ────────────────────────

#[autumn_web::get("/read")]
#[autumn_web::secured(scopes = ["posts:read"])]
async fn read_posts() -> &'static str {
    "read-ok"
}

#[autumn_web::post("/write")]
#[autumn_web::secured(scopes = ["posts:write"])]
async fn write_posts() -> &'static str {
    "write-ok"
}

// ── Handler gated on BOTH a session role AND a token scope (AND semantics) ─────

#[autumn_web::post("/admin-write")]
#[autumn_web::secured("admin", scopes = ["posts:write"])]
async fn admin_write_posts() -> &'static str {
    "admin-write-ok"
}

// ── Resource + policy that authorizes a non-user principal on scopes ───────────

#[derive(Clone)]
struct Doc;

#[derive(Default, Clone)]
struct ScopePolicy;

impl Policy<Doc> for ScopePolicy {
    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, _doc: &'a Doc) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_scope("posts:write") })
    }
}

#[autumn_web::post("/policy-write")]
async fn policy_write(
    State(state): State<AppState>,
    session: Session,
    scopes: Option<Extension<ApiTokenScopes>>,
) -> AutumnResult<&'static str> {
    authorize_with_scopes::<Doc>(
        &state,
        &session,
        scopes.as_ref().map(|ext| &ext.0),
        "update",
        &Doc,
    )
    .await?;
    Ok("policy-ok")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn token_app(store: Arc<InMemoryApiTokenStore>) -> TestClient {
    TestApp::new()
        .routes(routes![read_posts, write_posts])
        .layer(RequireApiToken::new(store))
        .build()
}

async fn get_with_bearer(client: &TestClient, path: &str, raw: &str) -> StatusCode {
    client
        .get(path)
        .header("Authorization", &format!("Bearer {raw}"))
        .send()
        .await
        .status
}

async fn post_with_bearer(client: &TestClient, path: &str, raw: &str) -> StatusCode {
    client
        .post(path)
        .header("Authorization", &format!("Bearer {raw}"))
        .send()
        .await
        .status
}

fn scopes(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn token_with_required_scope_is_allowed_and_missing_scope_is_forbidden() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    // Grant read+write.
    let full = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read", "posts:write"]),
            expires_at: None,
        })
        .await
        .unwrap();
    // Grant read only.
    let read_only = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:reader",
            name: "reader",
            scopes: &scopes(&["posts:read"]),
            expires_at: None,
        })
        .await
        .unwrap();

    let client = token_app(Arc::clone(&store));

    // Subset of granted scopes → 200 on both endpoints for the full token.
    assert_eq!(
        get_with_bearer(&client, "/read", &full).await,
        StatusCode::OK
    );
    assert_eq!(
        post_with_bearer(&client, "/write", &full).await,
        StatusCode::OK
    );

    // Read-only token: 200 on read, 403 on write (scope outside its grant).
    assert_eq!(
        get_with_bearer(&client, "/read", &read_only).await,
        StatusCode::OK
    );
    assert_eq!(
        post_with_bearer(&client, "/write", &read_only).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn expired_token_is_rejected_401() {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let store =
        Arc::new(InMemoryApiTokenStore::default().with_clock(Arc::new(FixedClock::at(now))));
    let raw = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read"]),
            expires_at: Some(now - ChronoDuration::seconds(1)),
        })
        .await
        .unwrap();

    let client = token_app(Arc::clone(&store));
    // Expired tokens never reach the scope gate — RequireApiToken rejects 401.
    assert_eq!(
        get_with_bearer(&client, "/read", &raw).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn revoked_token_is_rejected_401() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    let raw = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read"]),
            expires_at: None,
        })
        .await
        .unwrap();
    store.revoke(&raw).await.unwrap();

    let client = token_app(Arc::clone(&store));
    assert_eq!(
        get_with_bearer(&client, "/read", &raw).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn missing_token_is_rejected_401() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    let client = token_app(store);
    assert_eq!(
        client.get("/read").send().await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn scope_only_gate_authorizes_pure_service_token_with_no_session() {
    // AC5: a service token with no user/role authorizes purely on scopes.
    let store = Arc::new(InMemoryApiTokenStore::default());
    let raw = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:write"]),
            expires_at: None,
        })
        .await
        .unwrap();
    let client = token_app(Arc::clone(&store));
    assert_eq!(
        post_with_bearer(&client, "/write", &raw).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn roles_and_scopes_require_both_session_role_and_token_scope() {
    // App requires a valid token (app-wide) AND has a session layer so the
    // `#[secured("admin", scopes = [...])]` route can read the role.
    let session_store = MemoryStore::new();
    let mut admin_session = std::collections::HashMap::new();
    admin_session.insert("user_id".to_owned(), "1".to_owned());
    admin_session.insert("role".to_owned(), "admin".to_owned());
    session_store
        .save("sid-admin", admin_session)
        .await
        .unwrap();

    let token_store = Arc::new(InMemoryApiTokenStore::default());
    let scoped = token_store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:write"]),
            expires_at: None,
        })
        .await
        .unwrap();
    let unscoped = token_store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read"]),
            expires_at: None,
        })
        .await
        .unwrap();

    let client = TestApp::new()
        .routes(routes![admin_write_posts])
        .layer(RequireApiToken::new(Arc::clone(&token_store)))
        .layer(SessionLayer::new(session_store, SessionConfig::default()))
        .build();

    let send = |raw: String, sid: Option<&'static str>| {
        let mut req = client
            .post("/admin-write")
            .header("Authorization", &format!("Bearer {raw}"));
        if let Some(sid) = sid {
            req = req.header("Cookie", &format!("autumn.sid={sid}"));
        }
        async move { req.send().await.status }
    };

    // Admin session + scoped token → both checks pass → 200.
    assert_eq!(
        send(scoped.clone(), Some("sid-admin")).await,
        StatusCode::OK
    );
    // Scoped token but no admin session → role check fails → 401 (no auth user).
    assert_eq!(send(scoped.clone(), None).await, StatusCode::UNAUTHORIZED);
    // Admin session but token lacks the scope → scope check fails → 403.
    assert_eq!(
        send(unscoped, Some("sid-admin")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn token_scopes_flow_into_policy_context() {
    // AC3/AC5: a policy decides on ctx.has_scope for a non-user principal.
    let store = Arc::new(InMemoryApiTokenStore::default());
    let writer = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:write"]),
            expires_at: None,
        })
        .await
        .unwrap();
    let reader = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:reader",
            name: "reader",
            scopes: &scopes(&["posts:read"]),
            expires_at: None,
        })
        .await
        .unwrap();

    let client = TestApp::new()
        .routes(routes![policy_write])
        .policy::<Doc, _>(ScopePolicy)
        .layer(RequireApiToken::new(Arc::clone(&store)))
        .layer(SessionLayer::new(
            MemoryStore::new(),
            SessionConfig::default(),
        ))
        .build();

    assert_eq!(
        post_with_bearer(&client, "/policy-write", &writer).await,
        StatusCode::OK
    );
    // Reader token: policy denies → default forbidden response (404).
    assert_eq!(
        post_with_bearer(&client, "/policy-write", &reader).await,
        StatusCode::NOT_FOUND
    );
}
