// RED phase: tests that describe the expected behaviour of the inspector.
// These fail until the implementation lands.

use autumn_web::inspector::{
    InspectorBuffer, InspectorLayer, QueryRecord, RequestInspector, RequestRecord,
    detect_n_plus_one,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// ── InspectorBuffer ───────────────────────────────────────────────────────────

#[test]
fn ring_buffer_starts_empty() {
    let buf = InspectorBuffer::new(10);
    assert_eq!(buf.snapshot().len(), 0);
}

#[test]
fn ring_buffer_stores_records() {
    let buf = InspectorBuffer::new(10);
    buf.push(make_record("GET", "/a", 200));
    buf.push(make_record("POST", "/b", 201));
    let snapshot = buf.snapshot();
    assert_eq!(snapshot.len(), 2);
}

#[test]
fn ring_buffer_returns_newest_first() {
    let buf = InspectorBuffer::new(10);
    buf.push(make_record("GET", "/first", 200));
    buf.push(make_record("GET", "/second", 200));
    let snapshot = buf.snapshot();
    assert_eq!(snapshot[0].path, "/second");
    assert_eq!(snapshot[1].path, "/first");
}

#[test]
fn ring_buffer_respects_capacity() {
    let buf = InspectorBuffer::new(2);
    buf.push(make_record("GET", "/oldest", 200));
    buf.push(make_record("GET", "/middle", 200));
    buf.push(make_record("GET", "/newest", 200));
    let snapshot = buf.snapshot();
    assert_eq!(snapshot.len(), 2, "buffer should hold exactly 2 records");
    assert_eq!(snapshot[0].path, "/newest");
    assert_eq!(snapshot[1].path, "/middle");
}

#[test]
fn ring_buffer_get_by_id() {
    let buf = InspectorBuffer::new(10);
    buf.push(make_record("GET", "/a", 200));
    buf.push(make_record("GET", "/b", 200));
    let snapshot = buf.snapshot();
    let id = snapshot[0].id;
    let found = buf.get(id).expect("record should be findable by id");
    assert_eq!(found.path, "/b");
}

#[test]
fn ring_buffer_capacity_zero_stores_nothing() {
    let buf = InspectorBuffer::new(0);
    buf.push(make_record("GET", "/a", 200));
    assert_eq!(buf.snapshot().len(), 0);
}

// ── N+1 detector ─────────────────────────────────────────────────────────────

#[test]
fn n_plus_one_fires_at_threshold() {
    let queries = make_queries("SELECT * FROM users WHERE id = $1", 5);
    let warning = detect_n_plus_one(&queries, 5);
    assert!(warning.is_some(), "should fire at threshold");
    let w = warning.unwrap();
    assert_eq!(w.count, 5);
}

#[test]
fn n_plus_one_does_not_fire_below_threshold() {
    let queries = make_queries("SELECT * FROM users WHERE id = $1", 4);
    let warning = detect_n_plus_one(&queries, 5);
    assert!(warning.is_none(), "should not fire below threshold");
}

#[test]
fn n_plus_one_fires_above_threshold() {
    let queries = make_queries("SELECT * FROM posts WHERE author_id = $1", 10);
    let warning = detect_n_plus_one(&queries, 5);
    assert!(warning.is_some());
    let w = warning.unwrap();
    assert_eq!(w.count, 10);
}

#[test]
fn n_plus_one_picks_worst_offender() {
    let mut queries = make_queries("SELECT * FROM users WHERE id = $1", 3);
    queries.extend(make_queries("SELECT * FROM posts WHERE id = $1", 7));
    let warning = detect_n_plus_one(&queries, 3);
    let w = warning.expect("should detect N+1");
    // should pick the query with more repetitions
    assert_eq!(w.count, 7);
}

#[test]
fn n_plus_one_normalizes_whitespace() {
    let queries = vec![
        make_query("SELECT   *   FROM users  WHERE id = $1"),
        make_query("SELECT * FROM users WHERE id = $1"),
        make_query("SELECT *  FROM users WHERE  id = $1"),
        make_query("SELECT * FROM users WHERE id = $1"),
        make_query("SELECT * FROM users WHERE id = $1"),
    ];
    let warning = detect_n_plus_one(&queries, 5);
    assert!(
        warning.is_some(),
        "whitespace-normalized queries should match"
    );
}

#[test]
fn n_plus_one_zero_threshold_never_fires() {
    let queries = make_queries("SELECT * FROM users", 100);
    assert!(detect_n_plus_one(&queries, 0).is_none());
}

#[test]
fn n_plus_one_empty_queries_never_fires() {
    assert!(detect_n_plus_one(&[], 1).is_none());
}

// ── Inspector middleware (HTTP-level recording) ───────────────────────────────

#[tokio::test]
async fn inspector_middleware_records_request() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route(
            "/hello",
            axum::routing::get(|| async { axum::response::Response::new(Body::from("hi")) }),
        )
        .layer(layer);

    let req = Request::builder()
        .uri("/hello")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let snapshot = buf.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].method, "GET");
    assert_eq!(snapshot[0].path, "/hello");
    assert_eq!(snapshot[0].status, 200);
}

#[tokio::test]
async fn inspector_middleware_self_excludes() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route(
            "/_autumn/inspect",
            axum::routing::get(|| async { "inspector ui" }),
        )
        .layer(layer);

    let req = Request::builder()
        .uri("/_autumn/inspect")
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    assert_eq!(
        buf.snapshot().len(),
        0,
        "inspector's own requests should not be recorded"
    );
}

#[tokio::test]
async fn inspector_middleware_records_elapsed_and_status() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route(
            "/gone",
            axum::routing::get(|| async { (StatusCode::NOT_FOUND, "gone") }),
        )
        .layer(layer);

    let req = Request::builder().uri("/gone").body(Body::empty()).unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(record.status, 404);
    assert_eq!(record.path, "/gone");
}

// ── RequestInspector extractor ────────────────────────────────────────────────

#[tokio::test]
async fn request_inspector_records_queries() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route(
            "/handler",
            axum::routing::get(|inspector: RequestInspector| async move {
                inspector.record_query(QueryRecord {
                    sql: "SELECT * FROM users".to_owned(),
                    params: vec![],
                    elapsed_ms: 2,
                    location: "src/users.rs:42".to_owned(),
                });
                inspector.record_query(QueryRecord {
                    sql: "SELECT * FROM posts".to_owned(),
                    params: vec![],
                    elapsed_ms: 1,
                    location: "src/posts.rs:10".to_owned(),
                });
                "ok"
            }),
        )
        .layer(layer);

    let req = Request::builder()
        .uri("/handler")
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(record.query_count(), 2);
    assert_eq!(record.queries[0].sql, "SELECT * FROM users");
    assert_eq!(record.queries[1].sql, "SELECT * FROM posts");
}

#[tokio::test]
async fn request_inspector_triggers_n_plus_one_detection() {
    let buf = InspectorBuffer::new(10);
    // threshold = 3
    let layer = InspectorLayer::new(buf.clone(), 3, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route(
            "/loop",
            axum::routing::get(|inspector: RequestInspector| async move {
                for _ in 0..3 {
                    inspector.record_query(QueryRecord {
                        sql: "SELECT * FROM users WHERE id = $1".to_owned(),
                        params: vec!["1".to_owned()],
                        elapsed_ms: 1,
                        location: "src/loop.rs:5".to_owned(),
                    });
                }
                "ok"
            }),
        )
        .layer(layer);

    let req = Request::builder().uri("/loop").body(Body::empty()).unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert!(
        record.n_plus_one.is_some(),
        "should have detected N+1 at threshold=3"
    );
    assert_eq!(record.n_plus_one.as_ref().unwrap().count, 3);
}

// ── Inspector UI routes ───────────────────────────────────────────────────────

#[tokio::test]
async fn inspector_index_returns_html() {
    let buf = InspectorBuffer::new(10);
    buf.push(make_record("GET", "/posts", 200));

    let router = autumn_web::inspector::inspector_router(buf, "/_autumn/inspect");

    let req = Request::builder()
        .uri("/_autumn/inspect")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("inspector index request");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(
        html.contains("/_autumn/inspect"),
        "index page should reference inspector path"
    );
    assert!(
        html.contains("/posts"),
        "index should list the recorded request"
    );
}

#[tokio::test]
async fn inspector_detail_returns_html() {
    let buf = InspectorBuffer::new(10);
    buf.push(make_record("GET", "/detail-test", 200));
    let snapshot = buf.snapshot();
    let id = snapshot[0].id;

    let router = autumn_web::inspector::inspector_router(buf, "/_autumn/inspect");

    let req = Request::builder()
        .uri(format!("/_autumn/inspect/requests/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("inspector detail request");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(
        html.contains("/detail-test"),
        "detail page should show the request path"
    );
}

#[tokio::test]
async fn inspector_detail_returns_404_for_unknown_id() {
    let buf = InspectorBuffer::new(10);
    let router = autumn_web::inspector::inspector_router(buf, "/_autumn/inspect");

    let req = Request::builder()
        .uri("/_autumn/inspect/requests/9999")
        .body(Body::empty())
        .unwrap();
    let resp = router
        .oneshot(req)
        .await
        .expect("inspector detail 404 request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Route pattern (MatchedPath) and session ID ────────────────────────────────

#[tokio::test]
async fn inspector_records_matched_route_pattern() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route("/posts/{id}", axum::routing::get(|| async { "ok" }))
        .layer(layer);

    let req = Request::builder()
        .uri("/posts/42")
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(record.path, "/posts/42", "path should be the concrete URI");
    assert_eq!(
        record.route.as_deref(),
        Some("/posts/{id}"),
        "route should be the Axum route pattern"
    );
}

#[tokio::test]
async fn inspector_records_session_id_from_cookie() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned())
        .with_session_cookie_name("my_session");

    let app = axum::Router::new()
        .route("/page", axum::routing::get(|| async { "ok" }))
        .layer(layer);

    let req = Request::builder()
        .uri("/page")
        .header(
            axum::http::header::COOKIE,
            "my_session=abc123def456; other=x",
        )
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(
        record.session_id.as_deref(),
        Some("abc123def456"),
        "session_id should be extracted from the named cookie"
    );
}

#[tokio::test]
async fn inspector_strips_hmac_from_signed_session_cookie() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned())
        .with_session_cookie_name("sess");

    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "ok" }))
        .layer(layer);

    // Simulate a signed cookie: session_id.hmac_hex
    let req = Request::builder()
        .uri("/")
        .header(axum::http::header::COOKIE, "sess=sessionid123.hmacdata")
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(
        record.session_id.as_deref(),
        Some("sessionid123"),
        "HMAC suffix should be stripped from the session ID"
    );
}

#[tokio::test]
async fn inspector_session_id_none_when_no_session_cookie() {
    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());

    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "ok" }))
        .layer(layer);

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let _ = app.oneshot(req).await.unwrap();

    assert!(
        buf.snapshot()[0].session_id.is_none(),
        "session_id should be None when no cookie is present"
    );
}

// ── Config defaults ────────────────────────────────────────────────────────────

#[test]
fn dev_config_defaults() {
    let cfg = autumn_web::config::DevConfig::default();
    assert_eq!(cfg.inspector_path, "/_autumn/inspect");
    assert_eq!(cfg.inspector_capacity, 100);
    assert_eq!(cfg.inspector_n_plus_one_threshold, 5);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_record(method: &str, path: &str, status: u16) -> RequestRecord {
    RequestRecord {
        id: 0,
        method: method.to_owned(),
        path: path.to_owned(),
        route: None,
        status,
        elapsed_ms: 10,
        content_type: None,
        content_length: None,
        session_id: None,
        queries: vec![],
        n_plus_one: None,
        recorded_at: 0,
    }
}

fn make_queries(sql: &str, count: usize) -> Vec<QueryRecord> {
    (0..count).map(|_| make_query(sql)).collect()
}

fn make_query(sql: &str) -> QueryRecord {
    QueryRecord {
        sql: sql.to_owned(),
        params: vec![],
        elapsed_ms: 1,
        location: "test:1".to_owned(),
    }
}
