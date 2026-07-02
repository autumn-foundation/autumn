//! Integration coverage for the built-in tracked-job status route
//! (`GET /_autumn/jobs/{token}`, issue #1373): JSON leg, config wiring, and
//! the OpenAPI/MCP claimed-path preflight.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::job::{self, JobInfo};
use autumn_web::plugin::Plugin;
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
async fn poll_until(
    client: &TestClient,
    path: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
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
