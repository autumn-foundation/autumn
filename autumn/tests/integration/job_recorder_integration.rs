//! Integration tests for the built-in, always-on background-job recorder and
//! the `assert_job_enqueued*` / `enqueued_jobs` / `perform_enqueued_jobs`
//! helpers on `TestClient` (issue #1380).
//!
//! Mirrors the mail recorder tests (issue #1034) but for the job enqueue
//! interceptor seam. The recorder is on by default for every `TestApp::build`
//! client — no `.with_job_interceptor()` opt-in is required.
//!
//! The routes below enqueue through the free/`#[job]` enqueue helpers, which
//! use the process-global job client. Like the other global-job-runtime tests
//! (`job_tracking_route.rs`) these therefore serialize on
//! `job::global_job_runtime_test_lock()` and clear the global client, so the
//! consolidated binary's other tests never observe a foreign client.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ── Jobs under test ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WelcomeArgs {
    user_id: i64,
}

// Distinct side-effect counters per job so concurrent tests never collide (the
// global-runtime lock serializes them, but per-job statics keep intent clear).
static WELCOME_RUNS: AtomicUsize = AtomicUsize::new(0);
static PERFORM_RUNS: AtomicUsize = AtomicUsize::new(0);
static AFTER_COMMIT_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "send_welcome", max_attempts = 1, backoff_ms = 1)]
async fn send_welcome(_state: AppState, args: WelcomeArgs) -> AutumnResult<()> {
    let _ = args;
    WELCOME_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

// A job whose handler increments a counter, used to prove `perform_enqueued_jobs`
// actually dispatches the registered handler.
#[job(name = "perform_probe", max_attempts = 1, backoff_ms = 1)]
async fn perform_probe(_state: AppState, args: WelcomeArgs) -> AutumnResult<()> {
    let _ = args;
    PERFORM_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

// A job registered via after-commit enqueue path.
#[job(name = "after_commit_job", max_attempts = 1, backoff_ms = 1)]
async fn after_commit_job(_state: AppState, args: WelcomeArgs) -> AutumnResult<()> {
    let _ = args;
    AFTER_COMMIT_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

// A job whose handler always fails, to prove per-job errors surface.
#[job(name = "always_fails", max_attempts = 1, backoff_ms = 1)]
async fn always_fails(_state: AppState, args: WelcomeArgs) -> AutumnResult<()> {
    let _ = args;
    Err(AutumnError::internal_server_error(std::io::Error::other(
        "job handler intentionally failed",
    )))
}

/// Registers every test job. Jobs registered but never enqueued are inert.
struct TestJobsPlugin;

impl Plugin for TestJobsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![
            send_welcome,
            perform_probe,
            after_commit_job,
            always_fails
        ])
    }
}

// ── Routes ──────────────────────────────────────────────────────────────────

#[post("/welcome/{id}")]
async fn enqueue_welcome(Path(id): Path<i64>) -> &'static str {
    SendWelcomeJob::enqueue(WelcomeArgs { user_id: id })
        .await
        .unwrap();
    "ok"
}

#[post("/perform/{id}")]
async fn enqueue_perform(Path(id): Path<i64>) -> &'static str {
    PerformProbeJob::enqueue(WelcomeArgs { user_id: id })
        .await
        .unwrap();
    "ok"
}

#[post("/after-commit/{id}")]
async fn enqueue_after_commit_route(Path(id): Path<i64>) -> &'static str {
    // Outside a `db.tx` scope this enqueues immediately, still funneling through
    // the same enqueue interceptor the recorder installs.
    job::enqueue_after_commit("after_commit_job", WelcomeArgs { user_id: id })
        .await
        .unwrap();
    "ok"
}

#[post("/noop")]
async fn noop() -> &'static str {
    "quiet"
}

/// Poll until `f` returns true or ~2s elapse, yielding to let the in-process
/// worker drain.
async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not met in time");
}

// ── AC1 / AC3: capture enqueues with name + payload, enqueued_jobs() accessor ─

#[tokio::test]
async fn captures_enqueues_in_order_with_name_and_payload() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome, enqueue_after_commit_route])
        .build();

    // Two distinct enqueue paths: `enqueue` (via `#[job]::enqueue`) and
    // `enqueue_after_commit` (immediate, outside any tx). Both are captured.
    client.post("/welcome/7").send().await.assert_ok();
    client.post("/after-commit/9").send().await.assert_ok();

    let jobs = client.enqueued_jobs();
    assert_eq!(jobs.len(), 2, "both enqueues captured");
    assert_eq!(jobs[0].name, "send_welcome");
    assert_eq!(jobs[0].payload, json!({ "user_id": 7 }));
    assert_eq!(jobs[1].name, "after_commit_job");
    assert_eq!(jobs[1].payload, json!({ "user_id": 9 }));

    job::clear_global_job_client();
}

// ── AC2: assert_job_enqueued pass + fail-lists-what-was-enqueued ─────────────

#[tokio::test]
async fn assert_job_enqueued_passes_when_present() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/1").send().await.assert_ok();

    client.assert_job_enqueued("send_welcome");

    job::clear_global_job_client();
}

#[tokio::test]
#[should_panic(expected = "Enqueued jobs:")]
async fn assert_job_enqueued_fails_and_lists_enqueued() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/1").send().await.assert_ok();

    // "send_welcome" was enqueued, "never_enqueued" was not — the panic lists
    // send_welcome so the failure is self-diagnosing.
    client.assert_job_enqueued("never_enqueued");

    job::clear_global_job_client();
}

// ── AC3: assert_job_enqueued_with pass + mismatch panic ─────────────────────

#[tokio::test]
async fn assert_job_enqueued_with_matches_name_and_payload() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/7").send().await.assert_ok();

    client.assert_job_enqueued_with("send_welcome", json!({ "user_id": 7 }));

    job::clear_global_job_client();
}

#[tokio::test]
#[should_panic(expected = "no match was found")]
async fn assert_job_enqueued_with_fails_on_payload_mismatch() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/7").send().await.assert_ok();

    // Right name, wrong payload.
    client.assert_job_enqueued_with("send_welcome", json!({ "user_id": 999 }));

    job::clear_global_job_client();
}

// ── AC3: assert_no_jobs_enqueued pass (fresh) + panic when something enqueued ─

#[tokio::test]
async fn assert_no_jobs_enqueued_passes_on_fresh_app() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![noop])
        .build();

    client.post("/noop").send().await.assert_ok();

    client.assert_no_jobs_enqueued();

    job::clear_global_job_client();
}

#[tokio::test]
#[should_panic(expected = "expected no jobs to have been enqueued")]
async fn assert_no_jobs_enqueued_fails_when_present() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/1").send().await.assert_ok();

    client.assert_no_jobs_enqueued();

    job::clear_global_job_client();
}

// ── AC4: perform_enqueued_jobs drains, runs the handler, empties the queue ───

#[tokio::test]
async fn perform_enqueued_jobs_runs_handler_and_drains() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    PERFORM_RUNS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_perform])
        .build();

    client.post("/perform/3").send().await.assert_ok();
    client.assert_job_enqueued("perform_probe");

    // The in-process worker also drains this job. Wait for that single run to
    // settle first so the counter is stable, then attribute the *next*
    // increment unambiguously to `perform_enqueued_jobs`.
    wait_until(|| PERFORM_RUNS.load(Ordering::SeqCst) >= 1).await;
    let baseline = PERFORM_RUNS.load(Ordering::SeqCst);

    let report = client.perform_enqueued_jobs().await;
    report.assert_all_succeeded();
    assert_eq!(report.len(), 1, "exactly one captured job performed");

    // perform_enqueued_jobs ran the handler exactly once more.
    assert_eq!(
        PERFORM_RUNS.load(Ordering::SeqCst),
        baseline + 1,
        "perform_enqueued_jobs dispatched the registered handler"
    );

    // Queue drained: a second perform does nothing and enqueued_jobs is empty.
    assert!(client.enqueued_jobs().is_empty(), "queue drained");
    assert!(
        client.perform_enqueued_jobs().await.is_empty(),
        "second perform performs nothing"
    );

    job::clear_global_job_client();
}

// ── AC4: per-job handler errors are surfaced, not swallowed ──────────────────

#[tokio::test]
async fn perform_enqueued_jobs_surfaces_handler_error() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(TestJobsPlugin).build();

    // Enqueue a failing job directly through the global client (no route needed).
    job::enqueue("always_fails", json!({ "user_id": 1 }))
        .await
        .unwrap();
    client.assert_job_enqueued("always_fails");

    let report = client.perform_enqueued_jobs().await;
    let failures = report.failures();
    assert_eq!(failures.len(), 1, "one failing job surfaced");
    assert_eq!(failures[0].0, "always_fails");

    job::clear_global_job_client();
}

// ── AC5: serialization round-trip failure is observable, not a silent miss ───

#[tokio::test]
async fn perform_enqueued_jobs_surfaces_deserialization_failure() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(TestJobsPlugin).build();

    // Enqueue a payload that does not match `WelcomeArgs` (missing `user_id`).
    // The enqueue itself succeeds (the name is registered), so the miss is only
    // observable when the real handler deserializes the captured payload.
    job::enqueue("send_welcome", json!({ "wrong_field": true }))
        .await
        .unwrap();
    client.assert_job_enqueued("send_welcome");

    let report = client.perform_enqueued_jobs().await;
    let failures = report.failures();
    assert_eq!(
        failures.len(),
        1,
        "the malformed payload surfaced as a failure"
    );
    assert_eq!(failures[0].0, "send_welcome");
    assert!(
        format!("{:?}", failures[0].1).contains("deserialization failed"),
        "error identifies the deserialization miss: {:?}",
        failures[0].1
    );

    job::clear_global_job_client();
}

// ── AC6: recorder state is per-TestApp — no global static leakage ────────────

#[tokio::test]
async fn recorders_are_isolated_between_apps() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // App A enqueues one job and captures it in its own recorder.
    let client_a = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();
    client_a.post("/welcome/1").send().await.assert_ok();
    assert_eq!(
        client_a.enqueued_jobs().len(),
        1,
        "app A captured its enqueue"
    );

    // App B is a fresh instance; building it makes B the global client but must
    // not leak A's captured state into B's recorder.
    let client_b = TestApp::new().plugin(TestJobsPlugin).build();
    client_b.assert_no_jobs_enqueued();

    // App A still owns its capture — proving each recorder is a per-instance
    // Arc, not a shared static.
    assert_eq!(
        client_a.enqueued_jobs().len(),
        1,
        "app A retains its capture"
    );

    job::clear_global_job_client();
}

// ── AC1: user-supplied interceptor still composes with the recorder ──────────

#[tokio::test]
async fn user_job_interceptor_composes_with_recorder() {
    use std::pin::Pin;

    static USER_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct CountingJobInterceptor;
    impl autumn_web::interceptor::JobInterceptor for CountingJobInterceptor {
        fn intercept_enqueue<'a>(
            &'a self,
            _name: &'a str,
            _payload: &'a serde_json::Value,
            next: Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>>,
        ) -> Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>> {
            Box::pin(async move {
                USER_CALLS.fetch_add(1, Ordering::SeqCst);
                next.await
            })
        }

        fn intercept_execute<'a>(
            &'a self,
            _name: &'a str,
            _payload: &'a serde_json::Value,
            next: Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>>,
        ) -> Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>> {
            next
        }
    }

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    USER_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .with_job_interceptor(CountingJobInterceptor)
        .routes(routes![enqueue_welcome])
        .build();

    client.post("/welcome/1").send().await.assert_ok();

    // Built-in recorder captured it...
    client.assert_job_enqueued("send_welcome");
    // ...and the user interceptor also ran.
    assert_eq!(USER_CALLS.load(Ordering::SeqCst), 1);

    job::clear_global_job_client();
}

// ── AC8: full flow — route → assert_job_enqueued_with → perform → effect ─────

#[tokio::test]
async fn full_flow_enqueue_assert_perform_effect() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    WELCOME_RUNS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .plugin(TestJobsPlugin)
        .routes(routes![enqueue_welcome])
        .build();

    // 1. Trigger a route that enqueues a job.
    client.post("/welcome/42").send().await.assert_ok();

    // 2. Assert the enqueue with the exact payload.
    client.assert_job_enqueued_with("send_welcome", json!({ "user_id": 42 }));

    // 3. Let the worker's own run settle, snapshot, then perform.
    wait_until(|| WELCOME_RUNS.load(Ordering::SeqCst) >= 1).await;
    let baseline = WELCOME_RUNS.load(Ordering::SeqCst);
    let report = client.perform_enqueued_jobs().await;

    // 4. Assert the resulting effect: the handler's side effect fired once more,
    // and the report records the successful run.
    report.assert_all_succeeded();
    assert_eq!(
        WELCOME_RUNS.load(Ordering::SeqCst),
        baseline + 1,
        "perform_enqueued_jobs produced the handler's side effect"
    );

    job::clear_global_job_client();
}
