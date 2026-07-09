use autumn_web::security::CsrfConfig;
use autumn_web::security::CsrfLayer;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;

#[tokio::test]
async fn eris_csrf_cookie_tossing_bypass() {
    let config = CsrfConfig {
        enabled: true,
        ..Default::default()
    };

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config));

    // The legitimate token set by the server
    let legit_token = "valid_server_token".to_string();

    // The malicious token injected by the attacker via Cookie Tossing
    let malicious_token = "malicious_attacker_token".to_string();

    // Attacker sends a request containing their forged X-CSRF-Token header
    // and a Cookie header that contains BOTH the malicious cookie (tossed)
    // AND the legitimate cookie. If the malicious cookie comes first,
    // the extract_cookie_token might pick it and match it against the header.
    let malicious_req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header(
            "Cookie",
            format!("autumn-csrf={malicious_token}; autumn-csrf={legit_token}"),
        )
        .header("X-CSRF-Token", &malicious_token)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(malicious_req).await.unwrap();

    // The test MUST return FORBIDDEN to be secure. If it returns OK, it's vulnerable.
    // If this test fails (response is OK), it means the application is vulnerable
    // to Cookie Tossing and needs to reject requests with multiple CSRF cookies.
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "VULNERABILITY: CSRF bypass via Cookie Tossing! Multiple cookies allowed the malicious token to match."
    );
}
