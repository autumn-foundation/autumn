use autumn_web::session::{MemoryStore, Session, SessionConfig, SessionLayer, SessionStore};
use axum::{Router, body::Body, http::Request, routing::get};
use tower::ServiceExt;

#[tokio::test]
async fn eris_session_cookie_tossing() {
    let store = MemoryStore::new();
    let config = SessionConfig::default();

    let app = Router::new()
        .route(
            "/session",
            get(|session: Session| async move {
                session
                    .get("user")
                    .await
                    .unwrap_or_else(|| "none".to_string())
            }),
        )
        .layer(SessionLayer::new(store.clone(), config))
        .with_state(autumn_web::AppState::for_test());

    // Attacker pre-creates an evil session
    let mut evil_data = std::collections::HashMap::new();
    evil_data.insert("user".to_string(), "attacker".to_string());
    store.save("evil-id", evil_data).await.unwrap();

    // Victim has a legitimate session
    let mut legit_data = std::collections::HashMap::new();
    legit_data.insert("user".to_string(), "victim".to_string());
    store.save("legit-id", legit_data).await.unwrap();

    // Attacker does Cookie Tossing
    let req = Request::builder()
        .method("GET")
        .uri("/session")
        .header(
            "Cookie",
            "autumn.sid=evil-id; autumn.sid=legit-id".to_string(),
        )
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let user = String::from_utf8(body.to_vec()).unwrap();

    // If it returns "attacker", it's vulnerable to Cookie Tossing for Session IDs.
    // If it returns "none" or some error, it's protected.
    // I bet it returns "attacker" because `find_map` returns the first!
    assert_ne!(user, "attacker", "Session Cookie Tossing vulnerability!");
}
