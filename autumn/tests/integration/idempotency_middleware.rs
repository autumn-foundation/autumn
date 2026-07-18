//! Integration tests for HTTP idempotency-key middleware (issue #677).
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use autumn_web::idempotency::{
    IdempotencyCacheCommittedErrorResponse, IdempotencyLayer, IdempotencyReplayLayer,
    IdempotencyStore, IdempotencyStoreError, MemoryIdempotencyStore,
};
use autumn_web::session::{
    MemoryStore, Session, SessionConfig, SessionLayer, SessionStore, SessionStoreError,
};
use autumn_web::test::TestApp;
use autumn_web::{AppState, Route, RouteIdempotency, get, post, put, routes};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tower::{Layer, Service, ServiceExt};

// ── helpers ───────────────────────────────────────────────────────────────────

async fn ok_handler() -> &'static str {
    "pong"
}

async fn tenant_echo_handler(headers: axum::http::HeaderMap) -> String {
    TENANT_HEADER_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    headers
        .get("x-tenant-id")
        .or_else(|| headers.get("tenant"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing")
        .to_owned()
}

async fn tenant_scope_extension_handler(
    axum::extract::Extension(tenant): axum::extract::Extension<String>,
) -> String {
    TENANT_EXTENSION_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    tenant
}

#[post("/scoped-generated-tenant")]
async fn scoped_generated_tenant_handler(
    axum::extract::Extension(tenant): axum::extract::Extension<String>,
) -> String {
    SCOPED_GENERATED_TENANT_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    tenant
}

async fn committed_error_handler() -> Response {
    COMMITTED_ERROR_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let mut response = (
        StatusCode::INTERNAL_SERVER_ERROR,
        "repository commit hook finalization failed",
    )
        .into_response();
    response
        .extensions_mut()
        .insert(IdempotencyCacheCommittedErrorResponse);
    response
}

fn make_store(ttl: Duration) -> Arc<dyn autumn_web::idempotency::IdempotencyStore> {
    Arc::new(MemoryIdempotencyStore::new(ttl))
}

/// Replicates the storage-key format used by `IdempotencyLayer` so tests can
/// pre-lock the exact slot the middleware will look up.
fn principal_digest() -> String {
    use sha2::Digest as _;

    let mut hasher = sha2::Sha256::new();
    hasher.update(b"authorization:");
    hasher.update(b"\nsession:");
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn storage_key(method: &str, path: &str, idempotency_key: &str) -> String {
    use sha2::Digest as _;

    fn push_component(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
        hasher.update(label.as_bytes());
        hasher.update(b":");
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(b":");
        hasher.update(value);
        hasher.update(b";");
    }

    let principal = principal_digest();

    let mut storage = sha2::Sha256::new();
    push_component(&mut storage, "method", method.as_bytes());
    push_component(&mut storage, "target", path.as_bytes());
    push_component(&mut storage, "scope-header-count", b"0");
    push_component(&mut storage, "principal", principal.as_bytes());
    push_component(&mut storage, "idempotency-key", idempotency_key.as_bytes());
    format!(
        "v2:{}",
        storage
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            })
    )
}

/// Axum middleware that injects a 5-byte `UploadConfig` limit into extensions.
/// Used by `test_request_body_too_large_returns_413` to trigger the 413 path
/// in the idempotency middleware without needing a 32 MiB request body.
async fn inject_tiny_upload_limit(
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use autumn_web::security::UploadConfig;
    req.extensions_mut().insert(UploadConfig {
        max_request_size_bytes: 5,
        ..Default::default()
    });
    next.run(req).await
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Identical POST with the same idempotency key replays the first response.
#[tokio::test]
async fn test_deduplication() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let store = make_store(Duration::from_secs(3600));
    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .post("/ping")
        .header("idempotency-key", "dedup-key-1")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/ping")
        .header("idempotency-key", "dedup-key-1")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(r1.text(), r2.text());

    let _ = store; // keep store alive
}

static INTERCEPT_CALLS: AtomicUsize = AtomicUsize::new(0);
static INTERCEPTED_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static ANONYMOUS_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static STALE_COOKIE_ANONYMOUS_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static RAW_MERGE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static RAW_NEST_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static RAW_LAYERED_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SESSION_LOGIN_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SESSION_ROTATION_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SESSION_MARKER_ROTATION_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static BLOCKING_SESSION_ROTATION_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SESSION_ALIAS_FAILURE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static LOOKUP_FAILURE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static REPLAY_METADATA_POLICY_CALLS: AtomicUsize = AtomicUsize::new(0);
static REPLAY_METADATA_MUTATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_ROUTE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_OPENAPI_ROUTE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_SCOPED_ROUTE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_LAYERED_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_LAYERED_AUTH_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_LAYERED_ALLOWED: AtomicBool = AtomicBool::new(true);
static MANUAL_DIRECT_LAYERED_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_DIRECT_LAYERED_AUTH_CALLS: AtomicUsize = AtomicUsize::new(0);
static MANUAL_DIRECT_LAYERED_ALLOWED: AtomicBool = AtomicBool::new(true);
static RAW_LAYERED_AUTH_CALLS: AtomicUsize = AtomicUsize::new(0);
static RAW_LAYERED_ALLOWED: AtomicBool = AtomicBool::new(true);
static SESSION_SAVE_FAILURE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static TENANT_HEADER_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static TENANT_EXTENSION_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SCOPED_GENERATED_TENANT_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static APP_WIDE_GENERATED_TENANT_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static APP_WIDE_TENANT_LAYER_CALLS: AtomicUsize = AtomicUsize::new(0);
static VOLATILE_HEADER_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static COMMITTED_ERROR_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SECURED_SESSION_ROTATION_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static SECURED_SESSION_TOUCH_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Route-local interceptor used to prove generated routes with opaque
/// `#[intercept(...)]` layers do not replay cached responses across scopes
/// the idempotency storage key cannot see.
#[derive(Clone)]
struct CountInterceptLayer;

#[derive(Clone)]
struct CountInterceptService<S> {
    inner: S,
}

impl<S> Layer<S> for CountInterceptLayer {
    type Service = CountInterceptService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CountInterceptService { inner }
    }
}

impl<S> Service<axum::extract::Request> for CountInterceptService<S>
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        INTERCEPT_CALLS.fetch_add(1, Ordering::SeqCst);
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

#[derive(Clone)]
struct ManualLayeredAuthLayer;

#[derive(Clone)]
struct ManualLayeredAuthService<S> {
    inner: S,
}

impl<S> Layer<S> for ManualLayeredAuthLayer {
    type Service = ManualLayeredAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ManualLayeredAuthService { inner }
    }
}

impl<S> Service<axum::extract::Request> for ManualLayeredAuthService<S>
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        MANUAL_LAYERED_AUTH_CALLS.fetch_add(1, Ordering::SeqCst);
        if !MANUAL_LAYERED_ALLOWED.load(Ordering::SeqCst) {
            return Box::pin(async move {
                Ok((StatusCode::FORBIDDEN, "manual route layer denied").into_response())
            });
        }
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

#[derive(Clone)]
struct ManualDirectLayeredAuthLayer;

#[derive(Clone)]
struct ManualDirectLayeredAuthService<S> {
    inner: S,
}

impl<S> Layer<S> for ManualDirectLayeredAuthLayer {
    type Service = ManualDirectLayeredAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ManualDirectLayeredAuthService { inner }
    }
}

impl<S> Service<axum::extract::Request> for ManualDirectLayeredAuthService<S>
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        MANUAL_DIRECT_LAYERED_AUTH_CALLS.fetch_add(1, Ordering::SeqCst);
        if !MANUAL_DIRECT_LAYERED_ALLOWED.load(Ordering::SeqCst) {
            return Box::pin(async move {
                Ok((StatusCode::FORBIDDEN, "manual direct route layer denied").into_response())
            });
        }
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

#[derive(Clone)]
struct RawLayeredAuthLayer;

#[derive(Clone)]
struct RawLayeredAuthService<S> {
    inner: S,
}

impl<S> Layer<S> for RawLayeredAuthLayer {
    type Service = RawLayeredAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RawLayeredAuthService { inner }
    }
}

impl<S> Service<axum::extract::Request> for RawLayeredAuthService<S>
where
    S: Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        RAW_LAYERED_AUTH_CALLS.fetch_add(1, Ordering::SeqCst);
        if !RAW_LAYERED_ALLOWED.load(Ordering::SeqCst) {
            return Box::pin(async move {
                Ok((StatusCode::FORBIDDEN, "raw route layer denied").into_response())
            });
        }
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

#[post("/public-create")]
async fn anonymous_create_handler() -> &'static str {
    ANONYMOUS_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "created"
}

#[post("/stale-cookie-public-create")]
async fn stale_cookie_anonymous_create_handler() -> &'static str {
    STALE_COOKIE_ANONYMOUS_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "created"
}

async fn raw_merge_create_handler() -> &'static str {
    RAW_MERGE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "raw-merged"
}

async fn raw_nest_create_handler() -> &'static str {
    RAW_NEST_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "raw-nested"
}

async fn raw_layered_create_handler() -> &'static str {
    RAW_LAYERED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "raw-layered"
}

async fn manual_route_create_handler() -> &'static str {
    MANUAL_ROUTE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "manual-route"
}

async fn manual_openapi_route_create_handler() -> &'static str {
    MANUAL_OPENAPI_ROUTE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "manual-openapi-route"
}

async fn manual_scoped_route_create_handler() -> &'static str {
    MANUAL_SCOPED_ROUTE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "manual-scoped-route"
}

async fn manual_layered_route_create_handler() -> &'static str {
    MANUAL_LAYERED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "manual-layered-route"
}

async fn manual_direct_layered_route_create_handler() -> &'static str {
    MANUAL_DIRECT_LAYERED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "manual-direct-layered-route"
}

async fn volatile_header_create_handler() -> &'static str {
    VOLATILE_HEADER_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "volatile-created"
}

#[post("/app-wide-generated-tenant")]
async fn app_wide_generated_tenant_handler(
    axum::extract::Extension(tenant): axum::extract::Extension<String>,
) -> String {
    APP_WIDE_GENERATED_TENANT_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    tenant
}

async fn session_save_failure_handler(session: Session) -> &'static str {
    SESSION_SAVE_FAILURE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    "stored-side-effect"
}

#[derive(Clone)]
struct FailingSaveSessionStore;

impl SessionStore for FailingSaveSessionStore {
    async fn load(&self, _id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        Ok(None)
    }

    async fn save(
        &self,
        _id: &str,
        _data: HashMap<String, String>,
    ) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::backend("save", "boom"))
    }

    async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
        Ok(())
    }
}

#[derive(Clone)]
struct BlockingSaveSessionStore {
    sessions: Arc<tokio::sync::RwLock<HashMap<String, HashMap<String, String>>>>,
    old_session_id: String,
    old_destroyed: Arc<tokio::sync::Notify>,
    release_save: Arc<tokio::sync::Notify>,
    block_next_save: Arc<AtomicBool>,
}

impl BlockingSaveSessionStore {
    fn new(old_session_id: &str) -> Self {
        Self {
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            old_session_id: old_session_id.to_owned(),
            old_destroyed: Arc::new(tokio::sync::Notify::new()),
            release_save: Arc::new(tokio::sync::Notify::new()),
            block_next_save: Arc::new(AtomicBool::new(true)),
        }
    }

    async fn seed(&self, id: &str, data: HashMap<String, String>) {
        self.sessions.write().await.insert(id.to_owned(), data);
    }
}

impl SessionStore for BlockingSaveSessionStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        Ok(self.sessions.read().await.get(id).cloned())
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        if self.block_next_save.swap(false, Ordering::SeqCst) {
            self.release_save.notified().await;
        }
        self.sessions.write().await.insert(id.to_owned(), data);
        Ok(())
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        self.sessions.write().await.remove(id);
        if id == self.old_session_id {
            self.old_destroyed.notify_waiters();
        }
        Ok(())
    }
}

#[post("/session-login")]
async fn session_login_handler(session: Session) -> &'static str {
    SESSION_LOGIN_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    "logged-in"
}

async fn session_rotation_handler(session: Session) -> &'static str {
    SESSION_ROTATION_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    "logged-in"
}

async fn session_marker_rotation_handler(session: Session) -> String {
    SESSION_MARKER_ROTATION_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    let marker = session
        .get("marker")
        .await
        .unwrap_or_else(|| "anonymous".to_owned());
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    marker
}

async fn blocking_session_rotation_handler(session: Session) -> &'static str {
    BLOCKING_SESSION_ROTATION_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    "rotated"
}

#[post("/secured-session-rotation")]
#[autumn_web::secured]
async fn secured_session_rotation_handler(session: Session) -> &'static str {
    SECURED_SESSION_ROTATION_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    "secured-rotated"
}

#[post("/secured-session-touch")]
#[autumn_web::secured]
async fn secured_session_touch_handler(session: Session) -> &'static str {
    SECURED_SESSION_TOUCH_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("flash", "saved").await;
    "secured-touched"
}

async fn session_alias_failure_handler(session: Session) -> &'static str {
    SESSION_ALIAS_FAILURE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    session.insert("user_id", "42").await;
    session.rotate_id().await;
    "logged-in"
}

async fn lookup_failure_handler() -> &'static str {
    LOOKUP_FAILURE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "should-not-run"
}

async fn replay_metadata_handler(
    replay: Option<axum::extract::Extension<autumn_web::idempotency::IdempotencyReplayResponse>>,
) -> autumn_web::idempotency::IdempotencyReplayOr<&'static str> {
    if let Some(bytes) = autumn_web::idempotency::__replay_metadata(&replay, "policy.record") {
        REPLAY_METADATA_POLICY_CALLS.fetch_add(1, Ordering::SeqCst);
        assert_eq!(bytes, b"deleted-record");
        let response = autumn_web::idempotency::__replay_response(&replay)
            .expect("cached replay response should accompany replay metadata");
        return autumn_web::idempotency::IdempotencyReplayOr::Replay(response);
    }

    REPLAY_METADATA_MUTATION_CALLS.fetch_add(1, Ordering::SeqCst);
    autumn_web::idempotency::IdempotencyReplayOr::InnerWithReplayMetadata(
        "deleted",
        vec![("policy.record".to_owned(), b"deleted-record".to_vec())],
    )
}

#[post("/intercepted")]
#[intercept(CountInterceptLayer)]
async fn intercepted_create_handler() -> &'static str {
    INTERCEPTED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    "created"
}

#[tokio::test]
async fn test_merged_raw_router_fails_closed_on_replay_without_replay_stop() {
    RAW_MERGE_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let raw = axum::Router::<AppState>::new()
        .route("/raw-create", axum::routing::post(raw_merge_create_handler));

    let client = TestApp::new().merge(raw).idempotent().build();

    let r1 = client
        .post("/raw-create")
        .header("idempotency-key", "raw-merge-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/raw-create")
        .header("idempotency-key", "raw-merge-key")
        .send()
        .await;
    r2.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(r2.header("x-idempotent-replayed"), None);
    assert_eq!(
        RAW_MERGE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "raw merged routers must not rerun mutating handlers for a cached idempotency key"
    );
}

#[tokio::test]
async fn test_nested_raw_router_fails_closed_on_replay_without_replay_stop() {
    RAW_NEST_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let raw = axum::Router::<AppState>::new()
        .route("/raw-create", axum::routing::post(raw_nest_create_handler));

    let client = TestApp::new().nest("/api", raw).idempotent().build();

    let r1 = client
        .post("/api/raw-create")
        .header("idempotency-key", "raw-nest-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/api/raw-create")
        .header("idempotency-key", "raw-nest-key")
        .send()
        .await;
    r2.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(r2.header("x-idempotent-replayed"), None);
    assert_eq!(
        RAW_NEST_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "raw nested routers must not rerun mutating handlers for a cached idempotency key"
    );
}

#[tokio::test]
async fn test_merged_raw_router_layers_do_not_receive_stale_success_on_retry() {
    RAW_LAYERED_HANDLER_CALLS.store(0, Ordering::SeqCst);
    RAW_LAYERED_AUTH_CALLS.store(0, Ordering::SeqCst);
    RAW_LAYERED_ALLOWED.store(true, Ordering::SeqCst);
    let raw = axum::Router::<AppState>::new().route(
        "/raw-layered-create",
        axum::routing::post(raw_layered_create_handler).route_layer(RawLayeredAuthLayer),
    );

    let client = TestApp::new().merge(raw).idempotent().build();

    client
        .post("/raw-layered-create")
        .header("idempotency-key", "raw-layered-key")
        .send()
        .await
        .assert_ok();

    RAW_LAYERED_ALLOWED.store(false, Ordering::SeqCst);
    let retry = client
        .post("/raw-layered-create")
        .header("idempotency-key", "raw-layered-key")
        .send()
        .await;
    retry.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        RAW_LAYERED_AUTH_CALLS.load(Ordering::SeqCst),
        1,
        "opaque raw router-local layers cannot safely run before app-level fail-closed replay"
    );
    assert_eq!(
        RAW_LAYERED_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "the retry must not replay stale success or re-enter the mutating raw handler"
    );
}

#[tokio::test]
async fn test_manual_route_registered_through_routes_fails_closed_without_replay_stop() {
    MANUAL_ROUTE_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let route = Route {
        method: axum::http::Method::POST,
        path: "/manual-create",
        handler: axum::routing::post(manual_route_create_handler),
        name: "manual_route_create_handler",
        api_doc: autumn_web::openapi::ApiDoc::default(),
        repository: None,
        idempotency: RouteIdempotency::Direct,
        timeout: autumn_web::RouteTimeout::Inherit,
        api_version: None,
        sunset_opt_out: false,
    };

    let client = TestApp::new().routes(vec![route]).idempotent().build();

    let r1 = client
        .post("/manual-create")
        .header("idempotency-key", "manual-route-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/manual-create")
        .header("idempotency-key", "manual-route-key")
        .send()
        .await;
    r2.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(r2.header("x-idempotent-replayed"), None);
    assert_eq!(
        MANUAL_ROUTE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "manual Route values without an inner replay stop must fail closed instead of re-running"
    );
}

#[tokio::test]
async fn test_manual_route_with_openapi_method_fails_closed_without_replay_stop() {
    MANUAL_OPENAPI_ROUTE_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let route = Route {
        method: axum::http::Method::POST,
        path: "/manual-openapi-create",
        handler: axum::routing::post(manual_openapi_route_create_handler),
        name: "manual_openapi_route_create_handler",
        api_doc: autumn_web::openapi::ApiDoc {
            method: "POST",
            path: "/manual-openapi-create",
            operation_id: "manual_openapi_route_create_handler",
            success_status: 200,
            ..Default::default()
        },
        repository: None,
        idempotency: RouteIdempotency::Direct,
        timeout: autumn_web::RouteTimeout::Inherit,
        api_version: None,
        sunset_opt_out: false,
    };

    let client = TestApp::new().routes(vec![route]).idempotent().build();

    client
        .post("/manual-openapi-create")
        .header("idempotency-key", "manual-openapi-route-key")
        .send()
        .await
        .assert_ok();

    let replay = client
        .post("/manual-openapi-create")
        .header("idempotency-key", "manual-openapi-route-key")
        .send()
        .await;
    replay.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(replay.header("x-idempotent-replayed"), None);
    assert_eq!(
        MANUAL_OPENAPI_ROUTE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "OpenAPI metadata must not imply a manual Route can safely replay directly"
    );
}

#[tokio::test]
async fn test_manual_scoped_route_registered_through_routes_fails_closed_without_replay_stop() {
    MANUAL_SCOPED_ROUTE_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let route = Route {
        method: axum::http::Method::POST,
        path: "/manual-scoped-create",
        handler: axum::routing::post(manual_scoped_route_create_handler),
        name: "manual_scoped_route_create_handler",
        api_doc: autumn_web::openapi::ApiDoc::default(),
        repository: None,
        idempotency: RouteIdempotency::Direct,
        timeout: autumn_web::RouteTimeout::Inherit,
        api_version: None,
        sunset_opt_out: false,
    };

    let client = TestApp::new()
        .scoped("/api", tower::layer::util::Identity::new(), vec![route])
        .idempotent()
        .build();

    client
        .post("/api/manual-scoped-create")
        .header("idempotency-key", "manual-scoped-route-key")
        .send()
        .await
        .assert_ok();

    let replay = client
        .post("/api/manual-scoped-create")
        .header("idempotency-key", "manual-scoped-route-key")
        .send()
        .await;
    replay.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(replay.header("x-idempotent-replayed"), None);
    assert_eq!(
        MANUAL_SCOPED_ROUTE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "manual scoped Route values without an inner replay stop must fail closed instead of re-running"
    );
}

#[tokio::test]
async fn test_session_mutating_response_replays_final_cookie_without_rerunning_handler() {
    SESSION_LOGIN_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = TestApp::new()
        .routes(routes![session_login_handler])
        .idempotent()
        .build();

    let r1 = client
        .post("/session-login")
        .header("idempotency-key", "session-login-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);
    assert!(
        r1.header("set-cookie").is_some(),
        "fresh session mutation must append the session cookie"
    );

    let r2 = client
        .post("/session-login")
        .header("idempotency-key", "session-login-key")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(
        r2.header("x-idempotent-replayed"),
        Some("true"),
        "session-mutating responses must replay the finalized response, not re-enter the handler"
    );
    assert!(
        r2.header("set-cookie").is_some(),
        "retry after a lost login response must receive the cached session cookie"
    );
    assert_eq!(
        SESSION_LOGIN_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "session-mutating retries must not duplicate non-session side effects"
    );
}

#[tokio::test]
async fn test_session_rotation_replays_for_old_and_new_cookie_scopes() {
    SESSION_ROTATION_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = MemoryStore::new();
    let mut existing = HashMap::new();
    existing.insert("guest".to_owned(), "1".to_owned());
    session_store.save("guest-session", existing).await.unwrap();

    let idempotency_store: Arc<dyn IdempotencyStore> =
        Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/session-login",
            axum::routing::post(session_rotation_handler),
        )
        .layer(IdempotencyLayer::new(idempotency_store))
        .layer(SessionLayer::new(session_store, SessionConfig::default()));

    let first = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-login")
                .header("idempotency-key", "rotating-session-key")
                .header("cookie", "autumn.sid=guest-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let set_cookie = first
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("rotating session response should set a new cookie")
        .to_owned();
    let new_cookie = set_cookie
        .split(';')
        .next()
        .expect("set-cookie should start with a cookie pair")
        .to_owned();

    let old_cookie_retry = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-login")
                .header("idempotency-key", "rotating-session-key")
                .header("cookie", "autumn.sid=guest-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old_cookie_retry.status(), StatusCode::OK);
    assert_eq!(
        old_cookie_retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "retry with the pre-rotation cookie must hit the original idempotency record"
    );

    let new_cookie_retry = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-login")
                .header("idempotency-key", "rotating-session-key")
                .header("cookie", new_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(new_cookie_retry.status(), StatusCode::OK);
    assert_eq!(
        new_cookie_retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "retry after accepting the rotated cookie must hit the alias idempotency record"
    );
    assert_eq!(
        SESSION_ROTATION_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "session rotation must not move retries into a fresh idempotency scope"
    );
}

#[tokio::test]
async fn test_stale_cookie_replay_prefers_old_session_record_over_anonymous_primary() {
    SESSION_MARKER_ROTATION_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = MemoryStore::new();
    let mut existing = HashMap::new();
    existing.insert("marker".to_owned(), "old-session".to_owned());
    session_store
        .save("preferred-old-session", existing)
        .await
        .unwrap();

    let idempotency_store: Arc<dyn IdempotencyStore> =
        Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/session-marker-login",
            axum::routing::post(session_marker_rotation_handler),
        )
        .layer(IdempotencyLayer::new(idempotency_store))
        .layer(SessionLayer::new(session_store, SessionConfig::default()));

    let anonymous = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-marker-login")
                .header("idempotency-key", "stale-cookie-precedence-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let anonymous_body = axum::body::to_bytes(anonymous.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&anonymous_body[..], b"anonymous");

    let old_session = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-marker-login")
                .header("idempotency-key", "stale-cookie-precedence-key")
                .header("cookie", "autumn.sid=preferred-old-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let old_session_body = axum::body::to_bytes(old_session.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&old_session_body[..], b"old-session");

    let retry = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-marker-login")
                .header("idempotency-key", "stale-cookie-precedence-key")
                .header("cookie", "autumn.sid=preferred-old-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let retry_body = axum::body::to_bytes(retry.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &retry_body[..],
        b"old-session",
        "stale-cookie retries must prefer the old-session fallback record over an anonymous primary record"
    );
    assert_eq!(
        SESSION_MARKER_ROTATION_HANDLER_CALLS.load(Ordering::SeqCst),
        2,
        "stale-cookie replay must not re-enter the mutating handler"
    );
}

#[tokio::test]
async fn test_stale_cookie_retry_conflicts_with_old_session_in_flight_lock() {
    BLOCKING_SESSION_ROTATION_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = BlockingSaveSessionStore::new("in-flight-old-session");
    let mut existing = HashMap::new();
    existing.insert("guest".to_owned(), "1".to_owned());
    session_store.seed("in-flight-old-session", existing).await;

    let old_destroyed = session_store.old_destroyed.clone();
    let old_destroyed_wait = old_destroyed.notified();
    let release_save = session_store.release_save.clone();
    let idempotency_store: Arc<dyn IdempotencyStore> =
        Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/blocking-session-login",
            axum::routing::post(blocking_session_rotation_handler),
        )
        .layer(IdempotencyLayer::new(idempotency_store))
        .layer(SessionLayer::new(session_store, SessionConfig::default()));

    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/blocking-session-login")
                    .header("idempotency-key", "stale-cookie-in-flight-key")
                    .header("cookie", "autumn.sid=in-flight-old-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    old_destroyed_wait.await;

    let retry = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/blocking-session-login")
                .header("idempotency-key", "stale-cookie-in-flight-key")
                .header("cookie", "autumn.sid=in-flight-old-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        retry.status(),
        StatusCode::CONFLICT,
        "old-cookie retries must observe the old-session in-flight lock"
    );
    assert_eq!(
        BLOCKING_SESSION_ROTATION_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "old-cookie retry must not re-enter while the first mutation is finalizing"
    );

    release_save.notify_waiters();
    let first = first.await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_secured_session_rotation_replays_final_cookie_for_old_cookie_scope() {
    SECURED_SESSION_ROTATION_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = MemoryStore::new();
    let mut existing = HashMap::new();
    existing.insert("user_id".to_owned(), "42".to_owned());
    session_store
        .save("secured-guest-session", existing)
        .await
        .unwrap();

    let client = TestApp::new()
        .routes(routes![secured_session_rotation_handler])
        .idempotent()
        .layer(SessionLayer::new(
            session_store.clone(),
            SessionConfig::default(),
        ))
        .build();

    let first = client
        .post("/secured-session-rotation")
        .header("cookie", "autumn.sid=secured-guest-session")
        .header("idempotency-key", "secured-rotation-key")
        .send()
        .await;
    first.assert_ok();
    let set_cookie = first
        .header("set-cookie")
        .expect("rotating secured session response should set a new cookie")
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
        .post("/secured-session-rotation")
        .header("cookie", "autumn.sid=secured-guest-session")
        .header("idempotency-key", "secured-rotation-key")
        .send()
        .await;
    retry.assert_ok();
    assert_eq!(
        retry.header("x-idempotent-replayed"),
        Some("true"),
        "a retry with the destroyed pre-rotation session must replay the finalized session response"
    );
    assert!(
        retry.header("set-cookie").is_some(),
        "the replay must deliver the rotated session cookie"
    );
    assert_eq!(
        SECURED_SESSION_ROTATION_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "secured session-rotating retries must not re-enter the handler"
    );

    session_store.destroy(&new_session_id).await.unwrap();
    let accepted_cookie_retry = client
        .post("/secured-session-rotation")
        .header("cookie", &new_cookie)
        .header("idempotency-key", "secured-rotation-key")
        .send()
        .await;
    accepted_cookie_retry.assert_status(StatusCode::UNAUTHORIZED.as_u16());
    assert_eq!(accepted_cookie_retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        SECURED_SESSION_ROTATION_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "a retry after accepting a now-revoked rotated cookie must not replay the prior success"
    );
}

#[tokio::test]
async fn test_secured_same_session_mutation_denial_does_not_replay_cached_success() {
    SECURED_SESSION_TOUCH_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = MemoryStore::new();
    let mut existing = HashMap::new();
    existing.insert("user_id".to_owned(), "42".to_owned());
    session_store
        .save("secured-touch-session", existing)
        .await
        .unwrap();

    let client = TestApp::new()
        .routes(routes![secured_session_touch_handler])
        .idempotent()
        .layer(SessionLayer::new(
            session_store.clone(),
            SessionConfig::default(),
        ))
        .build();

    let first = client
        .post("/secured-session-touch")
        .header("cookie", "autumn.sid=secured-touch-session")
        .header("idempotency-key", "secured-touch-key")
        .send()
        .await;
    first.assert_ok();
    assert!(first.header("set-cookie").is_some());

    session_store
        .save("secured-touch-session", HashMap::new())
        .await
        .unwrap();
    let retry = client
        .post("/secured-session-touch")
        .header("cookie", "autumn.sid=secured-touch-session")
        .header("idempotency-key", "secured-touch-key")
        .send()
        .await;
    retry.assert_status(StatusCode::UNAUTHORIZED.as_u16());
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        SECURED_SESSION_TOUCH_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "dirty same-session retries must run current secured checks instead of replaying cached success"
    );
}

struct SecondSetFailsStore {
    inner: MemoryIdempotencyStore,
    sets: AtomicUsize,
}

impl SecondSetFailsStore {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: MemoryIdempotencyStore::new(ttl),
            sets: AtomicUsize::new(0),
        }
    }
}

impl IdempotencyStore for SecondSetFailsStore {
    fn get(&self, key: &str) -> Option<autumn_web::idempotency::IdempotencyEntry> {
        self.inner.get(key)
    }

    fn set(
        &self,
        key: &str,
        record: autumn_web::idempotency::IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) {
        self.inner.set(key, record, body_hash, ttl);
    }

    fn try_set(
        &self,
        key: &str,
        record: autumn_web::idempotency::IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> Result<(), IdempotencyStoreError> {
        if self.sets.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(IdempotencyStoreError::backend("forced alias write failure"));
        }
        self.inner.try_set(key, record, body_hash, ttl)
    }

    fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
        self.inner.try_lock(key, lock_ttl)
    }

    fn unlock(&self, key: &str) {
        self.inner.unlock(key);
    }

    fn default_ttl(&self) -> Duration {
        self.inner.default_ttl()
    }
}

#[tokio::test]
async fn test_session_rotation_alias_write_failure_leaves_old_cookie_replay_cached() {
    SESSION_ALIAS_FAILURE_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let session_store = MemoryStore::new();
    let mut existing = HashMap::new();
    existing.insert("guest".to_owned(), "1".to_owned());
    session_store
        .save("guest-session-alias-fail", existing)
        .await
        .unwrap();

    let idempotency_store: Arc<dyn IdempotencyStore> =
        Arc::new(SecondSetFailsStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/session-login",
            axum::routing::post(session_alias_failure_handler),
        )
        .layer(IdempotencyLayer::new(idempotency_store))
        .layer(SessionLayer::new(session_store, SessionConfig::default()));

    let first = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-login")
                .header("idempotency-key", "rotating-session-alias-fails")
                .header("cookie", "autumn.sid=guest-session-alias-fail")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        first.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "alias persistence failure should replace the successful mutation response"
    );
    assert!(
        first.headers().get("set-cookie").is_none(),
        "failed finalized idempotency persistence must not hand out a new session cookie"
    );

    let retry = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-login")
                .header("idempotency-key", "rotating-session-alias-fails")
                .header("cookie", "autumn.sid=guest-session-alias-fail")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "old-cookie retries must still hit the original cached record after alias persistence fails"
    );
    assert_eq!(
        SESSION_ALIAS_FAILURE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "partial alias persistence failure must not allow the old-cookie retry to rerun"
    );
}

#[tokio::test]
async fn test_session_save_failure_keeps_idempotency_key_closed() {
    SESSION_SAVE_FAILURE_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let idempotency_store: Arc<dyn IdempotencyStore> =
        Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/session-save-fails",
            axum::routing::post(session_save_failure_handler),
        )
        .layer(IdempotencyLayer::new(idempotency_store))
        .layer(SessionLayer::new(
            FailingSaveSessionStore,
            SessionConfig::default(),
        ));

    let first = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-save-fails")
                .header("idempotency-key", "session-save-fails")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);

    let retry = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/session-save-fails")
                .header("idempotency-key", "session-save-fails")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::CONFLICT);
    assert_eq!(
        SESSION_SAVE_FAILURE_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "session persistence failure after handler success must fail closed for retries"
    );
}

#[tokio::test]
async fn test_manual_layered_route_can_check_access_before_replay_stop() {
    MANUAL_LAYERED_HANDLER_CALLS.store(0, Ordering::SeqCst);
    MANUAL_LAYERED_AUTH_CALLS.store(0, Ordering::SeqCst);
    MANUAL_LAYERED_ALLOWED.store(true, Ordering::SeqCst);

    let route = Route {
        method: axum::http::Method::POST,
        path: "/manual-layered-create",
        handler: axum::routing::post(manual_layered_route_create_handler)
            .layer(IdempotencyReplayLayer)
            .layer(ManualLayeredAuthLayer),
        name: "manual_layered_route_create_handler",
        api_doc: autumn_web::openapi::ApiDoc::default(),
        repository: None,
        idempotency: RouteIdempotency::ReplayThroughInner,
        timeout: autumn_web::RouteTimeout::Inherit,
        api_version: None,
        sunset_opt_out: false,
    };

    let client = TestApp::new().routes(vec![route]).idempotent().build();

    client
        .post("/manual-layered-create")
        .header("idempotency-key", "manual-layered-route-key")
        .send()
        .await
        .assert_ok();

    MANUAL_LAYERED_ALLOWED.store(false, Ordering::SeqCst);
    let replay = client
        .post("/manual-layered-create")
        .header("idempotency-key", "manual-layered-route-key")
        .send()
        .await;

    replay.assert_status(StatusCode::FORBIDDEN.as_u16());
    assert_eq!(replay.header("x-idempotent-replayed"), None);
    assert_eq!(
        MANUAL_LAYERED_AUTH_CALLS.load(Ordering::SeqCst),
        2,
        "manual route-local layers must run again before a cached replay is released"
    );
    assert_eq!(
        MANUAL_LAYERED_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "the explicit replay stop must prevent the mutating handler from rerunning"
    );
}

#[tokio::test]
async fn test_manual_layered_direct_route_fails_closed_instead_of_stale_replay() {
    MANUAL_DIRECT_LAYERED_HANDLER_CALLS.store(0, Ordering::SeqCst);
    MANUAL_DIRECT_LAYERED_AUTH_CALLS.store(0, Ordering::SeqCst);
    MANUAL_DIRECT_LAYERED_ALLOWED.store(true, Ordering::SeqCst);

    let route = Route {
        method: axum::http::Method::POST,
        path: "/manual-layered-direct-create",
        handler: axum::routing::post(manual_direct_layered_route_create_handler)
            .layer(ManualDirectLayeredAuthLayer),
        name: "manual_direct_layered_route_create_handler",
        api_doc: autumn_web::openapi::ApiDoc::default(),
        repository: None,
        idempotency: RouteIdempotency::Direct,
        timeout: autumn_web::RouteTimeout::Inherit,
        api_version: None,
        sunset_opt_out: false,
    };

    let client = TestApp::new().routes(vec![route]).idempotent().build();

    client
        .post("/manual-layered-direct-create")
        .header("idempotency-key", "manual-layered-direct-route-key")
        .send()
        .await
        .assert_ok();

    MANUAL_DIRECT_LAYERED_ALLOWED.store(false, Ordering::SeqCst);
    let replay = client
        .post("/manual-layered-direct-create")
        .header("idempotency-key", "manual-layered-direct-route-key")
        .send()
        .await;

    replay.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(replay.header("x-idempotent-replayed"), None);
    assert_eq!(
        MANUAL_DIRECT_LAYERED_AUTH_CALLS.load(Ordering::SeqCst),
        1,
        "direct manual replay must not release a cached success around route-local layers"
    );
    assert_eq!(
        MANUAL_DIRECT_LAYERED_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "direct manual replay must not re-enter the mutating handler"
    );
}

#[tokio::test]
async fn test_replay_metadata_is_available_before_cached_response_is_released() {
    REPLAY_METADATA_POLICY_CALLS.store(0, Ordering::SeqCst);
    REPLAY_METADATA_MUTATION_CALLS.store(0, Ordering::SeqCst);

    let store: Arc<dyn IdempotencyStore> =
        Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
    let app = axum::Router::new()
        .route(
            "/metadata-delete",
            axum::routing::delete(replay_metadata_handler),
        )
        .layer(IdempotencyLayer::new(store).replay_through_inner());

    let first = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::DELETE)
                .uri("/metadata-delete")
                .header("idempotency-key", "metadata-delete-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let replay = app
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::DELETE)
                .uri("/metadata-delete")
                .header("idempotency-key", "metadata-delete-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        replay
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        REPLAY_METADATA_POLICY_CALLS.load(Ordering::SeqCst),
        1,
        "replay-through-inner handlers must be able to inspect cached metadata before replaying"
    );
    assert_eq!(
        REPLAY_METADATA_MUTATION_CALLS.load(Ordering::SeqCst),
        1,
        "metadata-backed replay must not re-enter the mutating branch"
    );
}

/// Anonymous requests without an existing session cookie still share a stable
/// anonymous idempotency scope. The global `SessionLayer` creates a fresh
/// request-local `Session` for each request, but that generated ID must not split
/// the idempotency cache when no cookie was persisted by the client.
#[tokio::test]
async fn test_anonymous_requests_without_cookie_replay() {
    ANONYMOUS_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .routes(routes![anonymous_create_handler])
        .idempotent()
        .build();

    let r1 = client
        .post("/public-create")
        .header("idempotency-key", "anonymous-create")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("set-cookie"), None);

    let r2 = client
        .post("/public-create")
        .header("idempotency-key", "anonymous-create")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(
        ANONYMOUS_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "anonymous retry without a session cookie must not re-enter the handler"
    );
}

#[tokio::test]
async fn test_stale_session_cookies_do_not_split_anonymous_idempotency_scope() {
    STALE_COOKIE_ANONYMOUS_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .routes(routes![stale_cookie_anonymous_create_handler])
        .idempotent()
        .build();

    let first = client
        .post("/stale-cookie-public-create")
        .header("cookie", "autumn.sid=stale-a")
        .header("idempotency-key", "stale-cookie-anonymous-key")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(first.header("x-idempotent-replayed"), None);

    let retry = client
        .post("/stale-cookie-public-create")
        .header("cookie", "autumn.sid=stale-b")
        .header("idempotency-key", "stale-cookie-anonymous-key")
        .send()
        .await;
    retry.assert_ok();
    assert_eq!(retry.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(
        STALE_COOKIE_ANONYMOUS_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "unloaded session cookies must not create attacker-controlled anonymous idempotency scopes"
    );
}

/// Generated routes with opaque route-local interceptors fail closed on replay
/// unless their scopes are explicit in the idempotency key.
#[tokio::test]
async fn test_intercepted_route_fails_closed_on_replay_without_visible_scope() {
    INTERCEPT_CALLS.store(0, Ordering::SeqCst);
    INTERCEPTED_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .routes(routes![intercepted_create_handler])
        .idempotent()
        .build();

    let r1 = client
        .post("/intercepted")
        .header("idempotency-key", "intercepted-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/intercepted")
        .header("idempotency-key", "intercepted-key")
        .send()
        .await;
    r2.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(r2.header("x-idempotent-replayed"), None);
    assert_eq!(
        INTERCEPT_CALLS.load(Ordering::SeqCst),
        1,
        "opaque route interceptors must not release cached successes across unkeyed scopes"
    );
    assert_eq!(
        INTERCEPTED_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "cached replay must not re-enter the mutating handler"
    );
}

/// A different payload with the same key returns 422.
#[tokio::test]
async fn test_payload_mismatch_returns_422() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route("/echo", axum::routing::post(ok_handler))
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/echo")
        .header("idempotency-key", "mismatch-key")
        .body(axum::body::Body::from("body-one"))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/echo")
        .header("idempotency-key", "mismatch-key")
        .body(axum::body::Body::from("body-two"))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// POST without an idempotency key is passed through on every call.
#[tokio::test]
async fn test_no_key_passthrough() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client.post("/ping").send().await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client.post("/ping").send().await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), None);
}

/// GET requests with an idempotency key are not deduplicated (not mutating).
#[tokio::test]
async fn test_get_passthrough() {
    #[get("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .get("/ping")
        .header("idempotency-key", "get-key")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .get("/ping")
        .header("idempotency-key", "get-key")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), None);
}

/// PUT with an idempotency key is also deduplicated.
#[tokio::test]
async fn test_put_deduplication() {
    #[put("/item")]
    async fn handler() -> &'static str {
        "updated"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .put("/item")
        .header("idempotency-key", "put-key-1")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .put("/item")
        .header("idempotency-key", "put-key-1")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
}

/// Different idempotency keys are stored independently.
#[tokio::test]
async fn test_distinct_keys_are_independent() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    // First request with key-a — should be fresh.
    let ra1 = client
        .post("/ping")
        .header("idempotency-key", "distinct-key-a")
        .send()
        .await;
    assert_eq!(ra1.header("x-idempotent-replayed"), None);

    // First request with key-b — should also be fresh.
    let rb1 = client
        .post("/ping")
        .header("idempotency-key", "distinct-key-b")
        .send()
        .await;
    assert_eq!(rb1.header("x-idempotent-replayed"), None);

    // Second request with key-a — replayed.
    let ra2 = client
        .post("/ping")
        .header("idempotency-key", "distinct-key-a")
        .send()
        .await;
    assert_eq!(ra2.header("x-idempotent-replayed"), Some("true"));

    // Second request with key-b — replayed.
    let rb2 = client
        .post("/ping")
        .header("idempotency-key", "distinct-key-b")
        .send()
        .await;
    assert_eq!(rb2.header("x-idempotent-replayed"), Some("true"));
}

/// The `X-Idempotent-Replayed` header is present only on replayed responses.
#[tokio::test]
async fn test_x_idempotent_replayed_header_semantics() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let fresh = client
        .post("/ping")
        .header("idempotency-key", "replay-header-key")
        .send()
        .await;
    assert_eq!(
        fresh.header("x-idempotent-replayed"),
        None,
        "fresh response must NOT have x-idempotent-replayed"
    );

    let replayed = client
        .post("/ping")
        .header("idempotency-key", "replay-header-key")
        .send()
        .await;
    assert_eq!(
        replayed.header("x-idempotent-replayed"),
        Some("true"),
        "replayed response must have x-idempotent-replayed: true"
    );
}

/// `TestApp::idempotent()` builder wires the middleware correctly.
#[tokio::test]
async fn test_idempotent_builder_method() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .post("/ping")
        .header("idempotency-key", "builder-key")
        .send()
        .await;
    r1.assert_ok();

    let r2 = client
        .post("/ping")
        .header("idempotency-key", "builder-key")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
}

/// The default TTL for idempotency config is 86400 seconds (24 hours).
#[test]
fn test_config_default_ttl_is_24h() {
    let config = autumn_web::config::IdempotencyConfig::default();
    assert_eq!(
        config.ttl_secs, 86_400,
        "default TTL should be 86400 seconds"
    );
}

/// Entries past their TTL are not replayed.
#[test]
fn test_ttl_eviction() {
    use autumn_web::idempotency::{IdempotencyRecord, IdempotencyStore};

    let store = MemoryIdempotencyStore::new(Duration::from_millis(1));
    let record = IdempotencyRecord {
        status: 200,
        headers: vec![],
        body: b"ok".to_vec(),
        metadata: vec![],
    };
    store.set("evict-key", record, vec![0u8; 8], Duration::from_millis(1));

    // Sleep long enough for the entry to expire.
    std::thread::sleep(Duration::from_millis(20));

    assert!(
        store.get("evict-key").is_none(),
        "expired entry should not be returned"
    );
}

/// A concurrent duplicate request (same key, first still in flight) receives
/// 409 Conflict with a Retry-After header.
#[tokio::test]
async fn test_concurrent_duplicate_returns_409() {
    use autumn_web::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
    use tower::ServiceExt;

    let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(3600)));
    let layer = IdempotencyLayer::new(store.clone() as Arc<dyn IdempotencyStore>);

    let app = axum::Router::new()
        .route("/ping", axum::routing::post(ok_handler))
        .layer(layer);

    // Pre-lock the scoped storage key to simulate an in-flight request.
    // The middleware namespaces by method and path, so we replicate the same
    // format here.
    store.try_lock(
        &storage_key("POST", "/ping", "inflight-key"),
        Duration::from_secs(3600),
    );

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/ping")
        .header("idempotency-key", "inflight-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "concurrent duplicate should return 409 Conflict"
    );
    assert!(
        resp.headers().contains_key("retry-after"),
        "409 response must include Retry-After header"
    );
}

/// After processing completes the in-flight lock is released so a subsequent
/// sequential request can be served normally.
#[tokio::test]
async fn test_in_flight_lock_released_after_response() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    // First request acquires and releases lock, stores response.
    let r1 = client
        .post("/ping")
        .header("idempotency-key", "lock-release-key")
        .send()
        .await;
    r1.assert_ok();

    // Second request should replay (not conflict), proving the lock was released.
    let r2 = client
        .post("/ping")
        .header("idempotency-key", "lock-release-key")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(
        r2.header("x-idempotent-replayed"),
        Some("true"),
        "second request should replay, not conflict"
    );
}

#[tokio::test]
async fn test_in_flight_lock_held_fail_closed_when_request_future_is_cancelled() {
    use tokio::sync::Notify;
    use tower::ServiceExt;

    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    CALL_COUNT.store(0, Ordering::SeqCst);

    let started = Arc::new(Notify::new());
    let never_finish = Arc::new(Notify::new());
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store).with_in_flight_ttl(Duration::from_secs(60));
    let app = axum::Router::new()
        .route(
            "/slow",
            axum::routing::post({
                let started = Arc::clone(&started);
                let never_finish = Arc::clone(&never_finish);
                move || {
                    let started = Arc::clone(&started);
                    let never_finish = Arc::clone(&never_finish);
                    async move {
                        CALL_COUNT.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        never_finish.notified().await;
                        "finished"
                    }
                }
            }),
        )
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/slow")
        .header("idempotency-key", "cancelled-key")
        .body(Body::empty())
        .unwrap();
    let pending = tokio::spawn(app.clone().oneshot(req1));
    started.notified().await;
    // Cancel the in-flight request (as the request-timeout layer would when its
    // deadline fires) before the handler returns and stores its response.
    pending.abort();
    let _ = pending.await;
    never_finish.notify_one();

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/slow")
        .header("idempotency-key", "cancelled-key")
        .body(Body::empty())
        .unwrap();
    let resp2 = tokio::time::timeout(Duration::from_millis(200), app.clone().oneshot(req2))
        .await
        .expect("retry should not hang: the held lock returns an immediate 409")
        .expect("retry request should complete");

    // Fail closed: the cancelled handler may have committed its side effect, so
    // the in-flight lock must stay held until its TTL. A retry carrying the same
    // Idempotency-Key gets an in-flight conflict instead of re-running the
    // mutation.
    assert_eq!(
        resp2.status(),
        StatusCode::CONFLICT,
        "a cancelled request must keep its in-flight lock fail-closed until TTL"
    );
    assert_eq!(
        CALL_COUNT.load(Ordering::SeqCst),
        1,
        "the retry must not re-execute the handler while the key is locked"
    );
}

/// Metrics counters are incremented correctly for hits and misses.
#[tokio::test]
async fn test_metrics_recorded() {
    #[post("/ping")]
    async fn handler() -> &'static str {
        "pong"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    // Miss: first request.
    client
        .post("/ping")
        .header("idempotency-key", "metrics-key")
        .send()
        .await
        .assert_ok();

    // Hit: second request with same key.
    let replayed = client
        .post("/ping")
        .header("idempotency-key", "metrics-key")
        .send()
        .await;
    replayed.assert_ok();
    assert_eq!(replayed.header("x-idempotent-replayed"), Some("true"));
    // Metrics are recorded in the background — the test verifies behaviour, not
    // the counter value, since the MetricsCollector is private to the router.
}

/// `IdempotencyConfig::default()` reflects documented defaults.
#[test]
fn test_config_fields() {
    let config = autumn_web::config::IdempotencyConfig::default();
    assert!(
        config.enabled.is_none(),
        "middleware must be absent (not enabled) by default"
    );
    assert_eq!(config.ttl_secs, 86_400, "default TTL is 24 hours");
    assert_eq!(
        config.in_flight_ttl_secs, 86_400,
        "default in-flight lock TTL is long enough for supported request durations"
    );
    assert!(
        !config.allow_memory_in_production,
        "memory backend is rejected in production by default"
    );
    assert_eq!(
        config.redis.key_prefix, "autumn:idempotency",
        "default Redis key prefix"
    );
}

/// `MemoryIdempotencyStore::new(ttl)` stores the TTL and exposes it via
/// `default_ttl()`, and `IdempotencyLayer::new(store)` picks it up.
#[test]
fn test_store_ttl_propagates_to_layer() {
    use autumn_web::idempotency::IdempotencyStore;

    let ttl = Duration::from_secs(300);
    let store = MemoryIdempotencyStore::new(ttl);
    assert_eq!(
        store.default_ttl(),
        ttl,
        "store must return the TTL passed to new()"
    );
}

#[test]
fn test_memory_in_flight_lock_expires_after_lock_ttl() {
    use autumn_web::idempotency::IdempotencyStore;

    let store = MemoryIdempotencyStore::new(Duration::from_secs(3600));
    assert!(
        store.try_lock("stale-lock", Duration::from_millis(10)),
        "first acquisition should succeed"
    );

    std::thread::sleep(Duration::from_millis(30));

    assert!(
        store.try_lock("stale-lock", Duration::from_millis(10)),
        "memory in-flight locks must honor lock_ttl instead of leaking forever"
    );
}

/// Non-2xx responses are not cached; a second request with the same key
/// re-executes the handler rather than replaying the error.
#[tokio::test]
async fn test_error_response_not_cached() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[post("/fail")]
    async fn handler() -> (StatusCode, &'static str) {
        CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        (StatusCode::INTERNAL_SERVER_ERROR, "boom")
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .post("/fail")
        .header("idempotency-key", "error-key")
        .send()
        .await;
    r1.assert_status(500);
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/fail")
        .header("idempotency-key", "error-key")
        .send()
        .await;
    r2.assert_status(500);
    assert_eq!(
        r2.header("x-idempotent-replayed"),
        None,
        "error responses must not be replayed"
    );
    assert_eq!(
        CALL_COUNT.load(Ordering::SeqCst),
        2,
        "handler should execute twice since error was not cached"
    );
}

/// Redirect-after-post is a successful mutation outcome and must be cached,
/// otherwise a client retry can duplicate the side effect before following the
/// redirect.
#[tokio::test]
async fn test_successful_redirect_response_is_cached() {
    use tower::ServiceExt;

    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    CALL_COUNT.store(0, Ordering::SeqCst);

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/create-then-redirect",
            axum::routing::post(|| async {
                CALL_COUNT.fetch_add(1, Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .header("location", "/created")
                    .body(axum::body::Body::empty())
                    .unwrap()
            }),
        )
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/create-then-redirect")
        .header("idempotency-key", "redirect-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::SEE_OTHER);

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/create-then-redirect")
        .header("idempotency-key", "redirect-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp2
            .headers()
            .get("x-idempotent-replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true")
    );
    assert_eq!(
        CALL_COUNT.load(Ordering::SeqCst),
        1,
        "successful redirect retry must replay instead of re-entering the handler"
    );
}

/// `set-cookie` headers are delivered on the first (non-replayed) response but
/// excluded from the cached replay to prevent session fixation.
#[tokio::test]
async fn test_set_cookie_on_first_response_absent_on_replay() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/login",
            axum::routing::post(|| async {
                axum::response::Response::builder()
                    .status(200)
                    .header("set-cookie", "session=abc; HttpOnly; SameSite=Strict")
                    .body(axum::body::Body::from("ok"))
                    .unwrap()
            }),
        )
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/login")
        .header("idempotency-key", "login-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    assert!(
        resp1.headers().contains_key("set-cookie"),
        "first response must include set-cookie"
    );
    assert!(
        !resp1.headers().contains_key("x-idempotent-replayed"),
        "first response must not have x-idempotent-replayed"
    );

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/login")
        .header("idempotency-key", "login-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert!(
        !resp2.headers().contains_key("set-cookie"),
        "replayed response must NOT include set-cookie"
    );
    assert_eq!(
        resp2
            .headers()
            .get("x-idempotent-replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true"),
        "replayed response must have x-idempotent-replayed: true"
    );
}

/// An empty `Idempotency-Key` header value is treated as absent — the request
/// is passed through without caching.
#[tokio::test]
async fn test_empty_idempotency_key_is_passthrough() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route("/ping", axum::routing::post(ok_handler))
        .layer(layer);

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/ping")
        .header("idempotency-key", "")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-idempotent-replayed").is_none(),
        "empty key should not be cached or replayed"
    );
}

/// When the request body exceeds the `UploadConfig` size limit the idempotency
/// middleware returns 413 Payload Too Large before forwarding to the handler.
#[tokio::test]
async fn test_request_body_too_large_returns_413() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    // inject_tiny_upload_limit must be OUTER (applied last) so it runs before
    // idempotency reads the UploadConfig extension.
    let app = axum::Router::new()
        .route("/upload", axum::routing::post(ok_handler))
        .layer(layer)
        .layer(axum::middleware::from_fn(inject_tiny_upload_limit));

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/upload")
        .header("idempotency-key", "big-body-key")
        .body(axum::body::Body::from("more than five bytes"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body larger than UploadConfig limit must return 413"
    );
}

/// A response body larger than the 10 MiB cache cap is streamed through to the
/// client without being stored. A second request with the same key re-runs the
/// handler rather than replaying a cached response.
#[tokio::test]
async fn test_large_response_not_cached_and_streamed_through() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    // One byte over the 10 MiB cache cap.
    const OVER_CAP: usize = 10 * 1024 * 1024 + 1;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/big",
            axum::routing::post(|| async {
                CALL_COUNT.fetch_add(1, Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::from(vec![b'x'; OVER_CAP]))
                    .unwrap()
            }),
        )
        .layer(layer);

    // First request — handler runs; response is streamed through uncached.
    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/big")
        .header("idempotency-key", "large-resp-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let bytes1 = axum::body::to_bytes(resp1.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes1.len(), OVER_CAP, "full body must be delivered");

    // Second request — not cached; handler runs again.
    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/big")
        .header("idempotency-key", "large-resp-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert!(
        resp2.headers().get("x-idempotent-replayed").is_none(),
        "oversized response must not be replayed from cache"
    );
    assert_eq!(
        CALL_COUNT.load(Ordering::SeqCst),
        2,
        "handler must execute twice when response body exceeds cache cap"
    );
}

#[derive(Default)]
struct FailingLookupStore;

impl autumn_web::idempotency::IdempotencyStore for FailingLookupStore {
    fn get(&self, _key: &str) -> Option<autumn_web::idempotency::IdempotencyEntry> {
        None
    }

    fn try_get(
        &self,
        _key: &str,
    ) -> Result<Option<autumn_web::idempotency::IdempotencyEntry>, IdempotencyStoreError> {
        Err(IdempotencyStoreError::backend("forced lookup failure"))
    }

    fn set(
        &self,
        _key: &str,
        _record: autumn_web::idempotency::IdempotencyRecord,
        _body_hash: Vec<u8>,
        _ttl: Duration,
    ) {
    }

    fn try_lock(&self, _key: &str, _lock_ttl: Duration) -> bool {
        true
    }

    fn unlock(&self, _key: &str) {}
}

#[tokio::test]
async fn test_lookup_failure_fails_closed_before_handler_runs() {
    LOOKUP_FAILURE_HANDLER_CALLS.store(0, Ordering::SeqCst);

    let app = axum::Router::new()
        .route("/lookup-fails", axum::routing::post(lookup_failure_handler))
        .layer(IdempotencyLayer::new(Arc::new(FailingLookupStore)));

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/lookup-fails")
                .header("idempotency-key", "redis-read-failed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        LOOKUP_FAILURE_HANDLER_CALLS.load(Ordering::SeqCst),
        0,
        "idempotency lookup failures must not be treated as cache misses"
    );
}

#[derive(Default)]
struct FailingPersistenceStore {
    unlocks: AtomicUsize,
}

impl autumn_web::idempotency::IdempotencyStore for FailingPersistenceStore {
    fn get(&self, _key: &str) -> Option<autumn_web::idempotency::IdempotencyEntry> {
        None
    }

    fn set(
        &self,
        _key: &str,
        _record: autumn_web::idempotency::IdempotencyRecord,
        _body_hash: Vec<u8>,
        _ttl: Duration,
    ) {
    }

    fn try_set(
        &self,
        _key: &str,
        _record: autumn_web::idempotency::IdempotencyRecord,
        _body_hash: Vec<u8>,
        _ttl: Duration,
    ) -> Result<(), IdempotencyStoreError> {
        Err(IdempotencyStoreError::backend("forced persistence failure"))
    }

    fn try_lock(&self, _key: &str, _lock_ttl: Duration) -> bool {
        true
    }

    fn unlock(&self, _key: &str) {
        self.unlocks.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn test_persistence_failure_surfaces_and_keeps_lock_closed() {
    use tower::ServiceExt;

    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    CALL_COUNT.store(0, Ordering::SeqCst);

    let store = Arc::new(FailingPersistenceStore::default());
    let layer = IdempotencyLayer::new(store.clone());
    let app = axum::Router::new()
        .route(
            "/charge",
            axum::routing::post(|| async {
                CALL_COUNT.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(Response::new(Body::from("charged")))
            }),
        )
        .layer(layer);

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/charge")
        .header("idempotency-key", "redis-write-failed")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a successful mutation must not be reported as cacheable success when persistence fails"
    );
    assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.unlocks.load(Ordering::SeqCst),
        0,
        "persistence failure should fail closed by leaving the in-flight lock to expire"
    );
}

/// The default `IdempotencyStore::default_ttl()` implementation returns 24 h.
/// Custom stores that do not override the method get this default.
#[test]
fn test_default_store_ttl_trait_impl() {
    use autumn_web::idempotency::{IdempotencyEntry, IdempotencyRecord, IdempotencyStore};

    struct BareStore;
    impl IdempotencyStore for BareStore {
        fn get(&self, _: &str) -> Option<IdempotencyEntry> {
            None
        }
        fn set(&self, _: &str, _: IdempotencyRecord, _: Vec<u8>, _: Duration) {}
        fn try_lock(&self, _: &str, _: Duration) -> bool {
            true
        }
        fn unlock(&self, _: &str) {}
        // default_ttl() deliberately not overridden — tests the trait default.
    }

    assert_eq!(
        BareStore.default_ttl(),
        Duration::from_secs(86_400),
        "IdempotencyStore::default_ttl() must return 24 hours"
    );
}

/// Requests to different paths sharing the same `Idempotency-Key` are stored
/// independently — no cross-endpoint replay occurs (P2: request-target scope).
#[tokio::test]
async fn test_different_paths_same_key_are_independent() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route("/a", axum::routing::post(|| async { "a" }))
        .route("/b", axum::routing::post(|| async { "b" }))
        .layer(layer);

    let req_a = axum::http::Request::builder()
        .method("POST")
        .uri("/a")
        .header("idempotency-key", "shared-path-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    assert!(resp_a.headers().get("x-idempotent-replayed").is_none());

    let req_b = axum::http::Request::builder()
        .method("POST")
        .uri("/b")
        .header("idempotency-key", "shared-path-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::OK);
    assert!(
        resp_b.headers().get("x-idempotent-replayed").is_none(),
        "different path with same key must not replay another endpoint's response"
    );
}

/// Raw delimiters in request paths and idempotency keys must not let one
/// endpoint synthesize another endpoint's storage key.
#[tokio::test]
async fn test_storage_key_delimits_path_principal_and_client_key() {
    use tower::ServiceExt;

    let principal = principal_digest();
    let colliding_path = format!("/a:{principal}:b");
    let colliding_key = format!("b:{principal}:k");
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            &colliding_path,
            axum::routing::post(|| async { "path-with-delimiters" }),
        )
        .route("/a", axum::routing::post(|| async { "plain-path" }))
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri(&colliding_path)
        .header("idempotency-key", "k")
        .body(Body::from("same"))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1 = axum::body::to_bytes(resp1.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body1, "path-with-delimiters");

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/a")
        .header("idempotency-key", colliding_key)
        .body(Body::from("same"))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert!(
        resp2.headers().get("x-idempotent-replayed").is_none(),
        "delimiter-bearing path/key components must not collide"
    );
    let body2 = axum::body::to_bytes(resp2.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body2, "plain-path");
}

/// Raw `Authorization` is a request header, not a trusted principal scope.
/// Changing or removing it on retry must not turn a completed mutation into a
/// fresh miss.
#[tokio::test]
async fn test_authorization_header_does_not_split_idempotency_scope() {
    use tower::ServiceExt;

    VOLATILE_HEADER_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/auth-header-action",
            axum::routing::post(volatile_header_create_handler),
        )
        .layer(layer);

    let req_a = axum::http::Request::builder()
        .method("POST")
        .uri("/auth-header-action")
        .header("idempotency-key", "shared-auth-key")
        .header("authorization", "Bearer token-user-a")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    assert!(resp_a.headers().get("x-idempotent-replayed").is_none());

    let req_b = axum::http::Request::builder()
        .method("POST")
        .uri("/auth-header-action")
        .header("idempotency-key", "shared-auth-key")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::OK);
    assert_eq!(
        resp_b
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "raw Authorization must not be able to force a fresh miss for the same completed mutation"
    );
    assert_eq!(
        VOLATILE_HEADER_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "changing raw Authorization must not rerun the mutating handler"
    );
}

#[tokio::test]
async fn test_client_tenant_headers_do_not_split_fail_closed_idempotency_scope() {
    use tower::ServiceExt;

    TENANT_HEADER_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store).fail_closed_on_replay();

    let app = axum::Router::new()
        .route("/tenant-action", axum::routing::post(tenant_echo_handler))
        .layer(layer);

    let req_a = axum::http::Request::builder()
        .method("POST")
        .uri("/tenant-action")
        .header("idempotency-key", "shared-tenant-key")
        .header("tenant", "tenant-a")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    let body_a = axum::body::to_bytes(resp_a.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body_a, "tenant-a");

    let req_b = axum::http::Request::builder()
        .method("POST")
        .uri("/tenant-action")
        .header("idempotency-key", "shared-tenant-key")
        .header("tenant", "tenant-b")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::CONFLICT);
    assert!(resp_b.headers().get("x-idempotent-replayed").is_none());
    assert_eq!(
        TENANT_HEADER_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "client-controlled tenant headers must not split the storage key or rerun the mutation"
    );
}

#[tokio::test]
async fn test_volatile_x_headers_do_not_split_idempotency_scope() {
    use tower::ServiceExt;

    VOLATILE_HEADER_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/volatile-action",
            axum::routing::post(volatile_header_create_handler),
        )
        .layer(layer);

    let req_a = axum::http::Request::builder()
        .method("POST")
        .uri("/volatile-action")
        .header("idempotency-key", "volatile-header-key")
        .header("x-workspace-id", "workspace-a")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    assert!(resp_a.headers().get("x-idempotent-replayed").is_none());

    let req_b = axum::http::Request::builder()
        .method("POST")
        .uri("/volatile-action")
        .header("idempotency-key", "volatile-header-key")
        .header("x-workspace-id", "workspace-b")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::OK);
    assert_eq!(
        resp_b
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "client-controlled volatile x-* headers must not turn a retry into a fresh miss"
    );
    assert_eq!(
        VOLATILE_HEADER_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "a changed volatile x-* header must not rerun a cached mutation"
    );
}

#[tokio::test]
async fn test_scoped_generated_route_fails_closed_for_opaque_tenant_scope() {
    async fn scoped_customer_scope(
        mut req: axum::http::Request<Body>,
        next: axum::middleware::Next,
    ) -> Response {
        let tenant = req
            .headers()
            .get("customer")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing")
            .to_owned();
        req.extensions_mut().insert(tenant);
        next.run(req).await
    }

    SCOPED_GENERATED_TENANT_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = TestApp::new()
        .scoped(
            "/api",
            axum::middleware::from_fn(scoped_customer_scope),
            routes![scoped_generated_tenant_handler],
        )
        .idempotent()
        .build();

    let first = client
        .post("/api/scoped-generated-tenant")
        .header("idempotency-key", "scoped-generated-tenant-key")
        .header("customer", "tenant-a")
        .body("same")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(first.text(), "tenant-a");

    let retry = client
        .post("/api/scoped-generated-tenant")
        .header("idempotency-key", "scoped-generated-tenant-key")
        .header("customer", "tenant-b")
        .body("same")
        .send()
        .await;
    retry.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        SCOPED_GENERATED_TENANT_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "scoped generated routes must not replay cached mutations across opaque tenant layers"
    );
}

#[tokio::test]
async fn test_app_wide_generated_route_fails_closed_for_opaque_tenant_scope() {
    async fn app_wide_customer_scope(
        mut req: axum::http::Request<Body>,
        next: axum::middleware::Next,
    ) -> Response {
        APP_WIDE_TENANT_LAYER_CALLS.fetch_add(1, Ordering::SeqCst);
        let tenant = req
            .headers()
            .get("customer")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing")
            .to_owned();
        req.extensions_mut().insert(tenant);
        next.run(req).await
    }

    APP_WIDE_GENERATED_TENANT_HANDLER_CALLS.store(0, Ordering::SeqCst);
    APP_WIDE_TENANT_LAYER_CALLS.store(0, Ordering::SeqCst);
    let client = TestApp::new()
        .routes(routes![app_wide_generated_tenant_handler])
        .layer(axum::middleware::from_fn(app_wide_customer_scope))
        .idempotent()
        .build();

    let first = client
        .post("/app-wide-generated-tenant")
        .header("idempotency-key", "app-wide-generated-tenant-key")
        .header("customer", "tenant-a")
        .body("same")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(first.text(), "tenant-a");

    let retry = client
        .post("/app-wide-generated-tenant")
        .header("idempotency-key", "app-wide-generated-tenant-key")
        .header("customer", "tenant-b")
        .body("same")
        .send()
        .await;
    retry.assert_status(StatusCode::CONFLICT.as_u16());
    assert_eq!(retry.header("x-idempotent-replayed"), None);
    assert_eq!(
        APP_WIDE_TENANT_LAYER_CALLS.load(Ordering::SeqCst),
        2,
        "app-wide tenant middleware must still run before an opaque cached mutation is denied"
    );
    assert_eq!(
        APP_WIDE_GENERATED_TENANT_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "app-wide generated routes must not replay cached mutations across opaque tenant layers"
    );
}

#[tokio::test]
async fn test_route_local_tenant_header_scope_uses_fail_closed_replay() {
    use tower::ServiceExt;

    async fn tenant_scope(
        mut req: axum::http::Request<Body>,
        next: axum::middleware::Next,
    ) -> Response {
        let tenant = req
            .headers()
            .get("customer-scope")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing")
            .to_owned();
        req.extensions_mut().insert(tenant);
        next.run(req).await
    }

    TENANT_EXTENSION_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store).fail_closed_on_replay();

    let app = axum::Router::new()
        .route(
            "/tenant-extension-action",
            axum::routing::post(tenant_scope_extension_handler),
        )
        .layer(layer)
        .layer(axum::middleware::from_fn(tenant_scope));

    let req_a = axum::http::Request::builder()
        .method("POST")
        .uri("/tenant-extension-action")
        .header("idempotency-key", "shared-tenant-header-key")
        .header("customer-scope", "tenant-a")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    let body_a = axum::body::to_bytes(resp_a.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body_a, "tenant-a");

    let req_b = axum::http::Request::builder()
        .method("POST")
        .uri("/tenant-extension-action")
        .header("idempotency-key", "shared-tenant-header-key")
        .header("customer-scope", "tenant-b")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::CONFLICT);
    assert!(resp_b.headers().get("x-idempotent-replayed").is_none());
    assert_eq!(
        TENANT_EXTENSION_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "middleware-resolved tenant headers must fail closed instead of splitting the storage key or rerunning the mutation"
    );
}

#[tokio::test]
async fn test_committed_error_response_is_cached() {
    use tower::ServiceExt;

    COMMITTED_ERROR_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route(
            "/committed-error",
            axum::routing::post(committed_error_handler),
        )
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/committed-error")
        .header("idempotency-key", "committed-error-key")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(resp1.headers().get("x-idempotent-replayed").is_none());

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/committed-error")
        .header("idempotency-key", "committed-error-key")
        .body(axum::body::Body::from("same"))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        resp2.headers().get("x-idempotent-replayed"),
        Some(&axum::http::HeaderValue::from_static("true")),
        "committed mutation errors must replay instead of reopening the key"
    );
    assert_eq!(
        COMMITTED_ERROR_HANDLER_CALLS.load(Ordering::SeqCst),
        1,
        "cached committed errors must not re-enter the mutating handler"
    );
}

/// The same body bytes with a different `Content-Type` are treated as a
/// distinct payload; the middleware returns 422 to signal the key is being
/// reused for a representationally different request (P2: representation scope).
#[tokio::test]
async fn test_different_content_type_same_body_returns_422() {
    use tower::ServiceExt;

    let store = make_store(Duration::from_secs(3600));
    let layer = IdempotencyLayer::new(store);

    let app = axum::Router::new()
        .route("/items", axum::routing::post(ok_handler))
        .layer(layer);

    let req1 = axum::http::Request::builder()
        .method("POST")
        .uri("/items")
        .header("idempotency-key", "ct-scope-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"x":1}"#))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    let req2 = axum::http::Request::builder()
        .method("POST")
        .uri("/items")
        .header("idempotency-key", "ct-scope-key")
        .header("content-type", "application/xml")
        .body(axum::body::Body::from(r#"{"x":1}"#))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "same bytes with different Content-Type must return 422 (payload mismatch)"
    );
}

/// DELETE requests with an idempotency key are deduplicated (DELETE is mutating).
#[tokio::test]
async fn test_delete_deduplication() {
    use autumn_web::delete;

    #[delete("/item")]
    async fn handler() -> &'static str {
        "deleted"
    }

    let client = TestApp::new().routes(routes![handler]).idempotent().build();

    let r1 = client
        .delete("/item")
        .header("idempotency-key", "delete-key-1")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .delete("/item")
        .header("idempotency-key", "delete-key-1")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
}

static STACKED_FLAG_CALLS: AtomicUsize = AtomicUsize::new(0);

#[tokio::test]
async fn test_stacked_feature_flag_idempotency() {
    use autumn_web::feature_flags::{FlagStore, InMemoryFlagStore};

    #[autumn_web::feature_flag("beta")]
    #[post("/stacked-flag")]
    async fn stacked_flag_handler() -> u64 {
        STACKED_FLAG_CALLS.fetch_add(1, Ordering::SeqCst);
        42
    }

    let store = InMemoryFlagStore::new();
    store.enable("beta", None).unwrap();

    let client = TestApp::new()
        .routes(routes![stacked_flag_handler])
        .with_flag_store(store)
        .idempotent()
        .build();

    let r1 = client
        .post("/stacked-flag")
        .header("idempotency-key", "stacked-key-1")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.text(), "42");
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/stacked-flag")
        .header("idempotency-key", "stacked-key-1")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.text(), "42");
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(STACKED_FLAG_CALLS.load(Ordering::SeqCst), 1);
}
