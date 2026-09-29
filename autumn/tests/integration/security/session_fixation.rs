use autumn_web::session::{MemoryStore, Session, SessionConfig, SessionLayer, SessionStore};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use tower::ServiceExt;

#[tokio::test]
async fn test_session_fixation() {
    async fn login_handler(session: Session) -> &'static str {
        session.rotate_id().await;
        session.insert("user_id", "123").await;
        "logged in"
    }

    let store = MemoryStore::new();
    let config = SessionConfig::default();

    let state = autumn_web::AppState::for_test();

    let app = Router::new()
        .route("/login", get(login_handler))
        .layer(SessionLayer::new(store.clone(), config))
        .with_state(state);

    let attacker_session_id = "attacker-provided-id";

    // To make it a true Session Fixation attack, the attacker might pre-create the session
    // in the server so it's considered valid.
    let mut initial_data = std::collections::HashMap::new();
    initial_data.insert("some_data".to_string(), "attacker_set".to_string());
    store.save(attacker_session_id, initial_data).await.unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .header("Cookie", format!("autumn.sid={attacker_session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let set_cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();

    assert!(
        !set_cookie.contains(attacker_session_id),
        "Cookie should have a new ID, but the attacker's ID was fixed!"
    );

    let new_id = set_cookie
        .split('=')
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();

    // The attacker's ID should have been destroyed in the store
    assert!(
        store.load(attacker_session_id).await.unwrap().is_none(),
        "Old session ID was not destroyed"
    );

    // The new ID should have the new data
    let new_data = store
        .load(new_id)
        .await
        .unwrap()
        .expect("New session ID should be in store");
    assert_eq!(new_data.get("user_id").unwrap(), "123");
}

#[tokio::test]
async fn test_rotate_id() {
    // A handler that explicitly rotates the session ID
    async fn rotate_handler(session: Session) -> &'static str {
        session.insert("user", "alice").await;
        session.rotate_id().await;
        "rotated"
    }

    let store = MemoryStore::new();
    let config = SessionConfig::default();

    let state = autumn_web::AppState::for_test();

    let app = Router::new()
        .route("/rotate", get(rotate_handler))
        .layer(SessionLayer::new(store.clone(), config))
        .with_state(state);

    // Initial session setup
    let session_id = "initial-id-123";
    let mut initial_data = std::collections::HashMap::new();
    initial_data.insert("pre_existing".to_string(), "data".to_string());
    store.save(session_id, initial_data).await.unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rotate")
                .header("Cookie", format!("autumn.sid={session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let set_cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();

    // The new cookie must NOT match the initial ID
    assert!(
        !set_cookie.contains(session_id),
        "Cookie should have a new ID"
    );

    let new_id = set_cookie
        .split('=')
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();

    // The old ID should be deleted from the store
    assert!(
        store.load(session_id).await.unwrap().is_none(),
        "Old session ID was not destroyed"
    );

    // The new ID should have both the old data and the new data
    let new_data = store
        .load(new_id)
        .await
        .unwrap()
        .expect("New session ID should be in store");
    assert_eq!(new_data.get("pre_existing").unwrap(), "data");
    assert_eq!(new_data.get("user").unwrap(), "alice");
}
