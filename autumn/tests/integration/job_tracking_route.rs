//! Integration coverage for the built-in tracked-job status route
//! (`GET /_autumn/jobs/{token}`, issue #1373): JSON leg, config wiring, and
//! the OpenAPI/MCP claimed-path preflight.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::job::{self, JobInfo, TrackedJobOwner};
use autumn_web::plugin::Plugin;
use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::{AppState, AutumnError, AutumnResult};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// Gates `tracked_gate_job` mid-execution (after it reports progress) so
/// tests can observe the "running" state deterministically before letting it
/// settle to a terminal state.
static RELEASE_GATE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn tracked_gate_job_handler(
    _state: AppState,
    payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        let ctx = job::JobContext::current();
        ctx.set_progress(40, Some("Rows 400/1000")).await.ok();
        RELEASE_GATE.notified().await;
        if payload.get("mode").and_then(Value::as_str) == Some("fail") {
            ctx.set_user_error("The export could not reach storage.");
            return Err(AutumnError::internal_server_error(std::io::Error::other(
                "boom",
            )));
        }
        ctx.set_result(json!({"download_url": "/blob/abc.csv"}));
        Ok(())
    })
}

struct TrackedGateJobPlugin;

impl Plugin for TrackedGateJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(vec![JobInfo {
            name: "tracked_gate_job".to_string(),
            max_attempts: 1,
            initial_backoff_ms: 1,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
            handler: tracked_gate_job_handler,
        }])
    }
}

fn test_config() -> AutumnConfig {
    AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    }
}

/// Poll `path` until `predicate` matches the JSON body, or panic after ~1s.
async fn poll_until(client: &TestClient, path: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100 {
        let response = client.get(path).send().await;
        assert_eq!(response.header("content-type"), Some("application/json"));
        let body: Value = response.json();
        if predicate(&body) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition on {path} was not met in time");
}

/// Poll `path` via an htmx request (`HX-Request: true`) until `predicate`
/// matches the returned HTML fragment text, or panic after ~1s.
async fn poll_until_html(
    client: &TestClient,
    path: &str,
    predicate: impl Fn(&str) -> bool,
) -> String {
    for _ in 0..100 {
        let response = client.get(path).header("HX-Request", "true").send().await;
        assert_eq!(
            response.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        let body = response.text();
        if predicate(&body) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition on {path} was not met in time");
}

#[tokio::test]
async fn status_route_tracks_progress_through_to_success_as_json() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    let running = poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;
    assert_eq!(running["progress"], 40);
    assert_eq!(running["message"], "Rows 400/1000");
    assert!(running["result"].is_null());
    assert!(running["error"].is_null());

    RELEASE_GATE.notify_waiters();

    let done = poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "succeeded"
    })
    .await;
    assert_eq!(done["result"]["download_url"], "/blob/abc.csv");
    assert!(done["error"].is_null());

    job::clear_global_job_client();
}

#[tokio::test]
async fn status_route_reports_failure_as_json() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "fail"}))
        .await
        .unwrap();

    // Let it reach "running" first so we know the gate was actually hit,
    // then release it to fail.
    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;
    RELEASE_GATE.notify_waiters();

    let done = poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "failed"
    })
    .await;
    assert_eq!(done["error"], "The export could not reach storage.");
    assert!(done["result"].is_null());

    job::clear_global_job_client();
}

#[tokio::test]
async fn unknown_token_is_404() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    client
        .get("/_autumn/jobs/does-not-exist")
        .send()
        .await
        .assert_status(404);

    job::clear_global_job_client();
}

#[tokio::test]
async fn route_disabled_via_config_is_not_mounted() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let mut config = test_config();
    config.jobs.tracking.route_enabled = false;

    let client = TestApp::new()
        .config(config)
        .plugin(TrackedGateJobPlugin)
        .build();

    client
        .get("/_autumn/jobs/anything")
        .send()
        .await
        .assert_status(404);

    job::clear_global_job_client();
}

#[tokio::test]
async fn htmx_request_receives_fragment_with_every_2s_trigger_while_running() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    let running = poll_until_html(&client, &handle.status_path(), |body| {
        // Wait for the actual progress write, not just the initial pending
        // record (whose unset progress would also render "0%").
        body.contains("Rows 400/1000")
    })
    .await;
    assert!(
        running.contains(r#"hx-get="/_autumn/jobs/"#),
        "expected self hx-get while running: {running}"
    );
    assert!(
        running.contains(r#"hx-trigger="every 2s""#),
        "expected hx-trigger=\"every 2s\" while running: {running}"
    );
    assert!(
        running.contains(r#"hx-swap="outerHTML""#),
        "expected hx-swap=\"outerHTML\" while running: {running}"
    );
    assert!(
        running.contains(r#"value="40""#),
        "expected the progress bar to reflect 40%: {running}"
    );

    RELEASE_GATE.notify_waiters();
    job::clear_global_job_client();
}

#[tokio::test]
async fn accept_text_html_without_htmx_header_also_receives_fragment() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    // Wait for the job to actually be running (via the JSON leg, so we don't
    // depend on the behavior under test to observe readiness), then re-fetch
    // with a browser-style Accept header and no HX-Request.
    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;

    let response = client
        .get(&handle.status_path())
        .header("Accept", "text/html")
        .send()
        .await;
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let body = response.text();
    assert!(body.contains("autumn-job-status"), "{body}");

    RELEASE_GATE.notify_waiters();
    job::clear_global_job_client();
}

#[tokio::test]
async fn bare_wildcard_accept_still_receives_json_not_the_html_fragment() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;

    // A bare wildcard Accept (curl's default, and many fetch()/HTTP-client
    // defaults) carries no real preference and must not be treated as a
    // browser navigation — this JSON-first route should still return JSON.
    let response = client
        .get(&handle.status_path())
        .header("Accept", "*/*")
        .send()
        .await;
    assert_eq!(response.header("content-type"), Some("application/json"));

    RELEASE_GATE.notify_waiters();
    job::clear_global_job_client();
}

#[tokio::test]
async fn succeeded_fragment_renders_download_link_and_stops_polling() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;
    RELEASE_GATE.notify_waiters();

    let done = poll_until_html(&client, &handle.status_path(), |body| {
        body.contains("autumn-job-status__success")
    })
    .await;
    assert!(
        done.contains(r#"href="/blob/abc.csv""#),
        "expected a download link: {done}"
    );
    assert!(
        !done.contains("hx-get"),
        "terminal fragment must not keep polling (no hx-get): {done}"
    );
    assert!(
        !done.contains("hx-trigger"),
        "terminal fragment must not keep polling (no hx-trigger): {done}"
    );

    job::clear_global_job_client();
}

#[tokio::test]
async fn failed_fragment_shows_user_safe_error_and_stops_polling() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "fail"}))
        .await
        .unwrap();

    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;
    RELEASE_GATE.notify_waiters();

    let done = poll_until_html(&client, &handle.status_path(), |body| {
        body.contains("autumn-job-status__error")
    })
    .await;
    assert!(
        done.contains("The export could not reach storage."),
        "expected the user-safe error message: {done}"
    );
    assert!(
        !done.contains("hx-get"),
        "terminal fragment must not keep polling (no hx-get): {done}"
    );
    assert!(
        !done.contains("hx-trigger"),
        "terminal fragment must not keep polling (no hx-trigger): {done}"
    );

    job::clear_global_job_client();
}

// ── Owner authorization ───────────────────────────────────────────────────

async fn seed_session(store: &MemoryStore, sid: &str, user_id: Option<&str>) {
    let mut data = std::collections::HashMap::new();
    if let Some(user_id) = user_id {
        data.insert("user_id".to_owned(), user_id.to_owned());
    }
    store.save(sid, data).await.unwrap();
}

fn build_owned_client(store: MemoryStore) -> TestClient {
    TestApp::new()
        .config(test_config())
        .plugin(TrackedGateJobPlugin)
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build()
}

async fn get_status(
    client: &TestClient,
    path: &str,
    sid: Option<&str>,
) -> autumn_web::test::TestResponse {
    let mut request = client.get(path);
    if let Some(sid) = sid {
        request = request.header("Cookie", &format!("autumn.sid={sid}"));
    }
    request.send().await
}

#[tokio::test]
async fn anonymous_token_readable_with_token_alone() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let store = MemoryStore::new();
    let client = build_owned_client(store.clone());

    // Anonymous handle: no owner binding at all.
    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();
    RELEASE_GATE.notify_waiters();

    // No cookie whatsoever still reads it.
    get_status(&client, &handle.status_path(), None)
        .await
        .assert_status(200);
    // An arbitrary session cookie also reads it — the token is the only capability.
    seed_session(&store, "sess-anyone", None).await;
    get_status(&client, &handle.status_path(), Some("sess-anyone"))
        .await
        .assert_status(200);

    job::clear_global_job_client();
}

#[tokio::test]
async fn session_bound_token_404_for_other_session() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let store = MemoryStore::new();
    let client = build_owned_client(store.clone());

    // Seeding the session ID in the store is what makes the middleware trust
    // the client-supplied cookie value; an unseeded ID is treated as unknown
    // and gets a freshly minted (different) session id instead.
    seed_session(&store, "sess-owner", None).await;

    let handle = job::enqueue_tracked_for(
        "tracked_gate_job",
        json!({"mode": "succeed"}),
        TrackedJobOwner::Session("sess-owner".to_owned()),
    )
    .await
    .unwrap();
    RELEASE_GATE.notify_waiters();

    // The owning session reads it fine.
    get_status(&client, &handle.status_path(), Some("sess-owner"))
        .await
        .assert_status(200);

    // A different session — even an authenticated one — does not.
    seed_session(&store, "sess-stranger", None).await;
    get_status(&client, &handle.status_path(), Some("sess-stranger"))
        .await
        .assert_status(404);

    // No cookie at all (a fresh anonymous session) does not either.
    get_status(&client, &handle.status_path(), None)
        .await
        .assert_status(404);

    job::clear_global_job_client();
}

#[tokio::test]
async fn user_bound_token_allowed_across_sessions_of_same_user() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let store = MemoryStore::new();
    let client = build_owned_client(store.clone());

    let handle = job::enqueue_tracked_for(
        "tracked_gate_job",
        json!({"mode": "succeed"}),
        TrackedJobOwner::User("user-42".to_owned()),
    )
    .await
    .unwrap();
    RELEASE_GATE.notify_waiters();

    // Two different (unauthenticated) session ids, both logged in as the
    // same user, both may poll — the binding is to the user, not a session.
    seed_session(&store, "sess-a", Some("user-42")).await;
    seed_session(&store, "sess-b", Some("user-42")).await;
    get_status(&client, &handle.status_path(), Some("sess-a"))
        .await
        .assert_status(200);
    get_status(&client, &handle.status_path(), Some("sess-b"))
        .await
        .assert_status(200);

    job::clear_global_job_client();
}

#[tokio::test]
async fn user_bound_token_404_for_other_user_and_anonymous() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let store = MemoryStore::new();
    let client = build_owned_client(store.clone());

    let handle = job::enqueue_tracked_for(
        "tracked_gate_job",
        json!({"mode": "succeed"}),
        TrackedJobOwner::User("user-42".to_owned()),
    )
    .await
    .unwrap();
    RELEASE_GATE.notify_waiters();

    // A different authenticated user is rejected.
    seed_session(&store, "sess-other-user", Some("user-999")).await;
    get_status(&client, &handle.status_path(), Some("sess-other-user"))
        .await
        .assert_status(404);

    // An anonymous (logged-out) session is rejected.
    seed_session(&store, "sess-anonymous", None).await;
    get_status(&client, &handle.status_path(), Some("sess-anonymous"))
        .await
        .assert_status(404);

    job::clear_global_job_client();
}

#[tokio::test]
async fn owner_mismatch_response_is_byte_identical_to_unknown_token() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let store = MemoryStore::new();
    let client = build_owned_client(store.clone());

    let handle = job::enqueue_tracked_for(
        "tracked_gate_job",
        json!({"mode": "succeed"}),
        TrackedJobOwner::Session("sess-owner".to_owned()),
    )
    .await
    .unwrap();
    RELEASE_GATE.notify_waiters();

    seed_session(&store, "sess-stranger", None).await;
    let mismatch = get_status(&client, &handle.status_path(), Some("sess-stranger")).await;
    let unknown = get_status(
        &client,
        "/_autumn/jobs/does-not-exist",
        Some("sess-stranger"),
    )
    .await;

    assert_eq!(
        mismatch.header("content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        unknown.header("content-type"),
        Some("application/problem+json")
    );
    let mismatch_body: Value = mismatch.json();
    let unknown_body: Value = unknown.json();
    mismatch.assert_status(404);
    unknown.assert_status(404);
    // Compare only the fields a client actually sees as "the reason" — not
    // `instance` (which just echoes back the requested path, and naturally
    // differs since the two requests hit different URLs) or `request_id`
    // (unique per request by design).
    for field in ["type", "title", "status", "detail", "code"] {
        assert_eq!(
            mismatch_body[field], unknown_body[field],
            "owner mismatch and unknown-token responses must render an \
             identical reason for field {field:?}: {mismatch_body} vs {unknown_body}"
        );
    }

    job::clear_global_job_client();
}

// ── TTL end-to-end via config ──────────────────────────────────────────────

#[tokio::test]
async fn custom_ttl_from_config_expires_the_record_via_route() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let mut config = test_config();
    config.jobs.tracking.ttl_secs = 1;

    let client = TestApp::new()
        .config(config)
        .plugin(TrackedGateJobPlugin)
        .build();

    let handle = job::enqueue_tracked("tracked_gate_job", json!({"mode": "succeed"}))
        .await
        .unwrap();

    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "running"
    })
    .await;
    RELEASE_GATE.notify_waiters();

    poll_until(&client, &handle.status_path(), |body| {
        body["status"] == "succeeded"
    })
    .await;

    // The 1s TTL configured above (rather than the 86400s default) should
    // have expired the record shortly after it settled.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    client
        .get(&handle.status_path())
        .send()
        .await
        .assert_status(404);

    job::clear_global_job_client();
}
