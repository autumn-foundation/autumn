use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer};
use axum::{Router, body::Body, http::Request, routing::get};
use tower::ServiceExt;

#[tokio::test]
async fn eris_session_exhaustion() {
    let store = MemoryStore::new();
    let config = SessionConfig::default();

    let app = Router::new()
        .route("/", get(|| async { "hello" }))
        .layer(SessionLayer::new(store.clone(), config))
        .with_state(autumn_web::AppState::detached());

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();

    // An empty session shouldn't issue a cookie
    let cookie = resp.headers().get(http::header::SET_COOKIE);
    assert!(
        cookie.is_none(),
        "Threat: Session exhaustion DoS. App issues an empty session cookie for every unauthenticated request."
    );
}
