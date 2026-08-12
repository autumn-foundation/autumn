//! Integration tests for failure-capsule capture (issue #1598), Docker-free
//! slice: request/clock/outcome recording, persistence, and the
//! `ErrorEvent` → capsule link.
//!
//! Verifies that:
//!   * capture is inert until `[failure_capture] enabled = true`;
//!   * a failing request (5xx or caught panic) leaves exactly one capsule,
//!     and a successful or 4xx request leaves none;
//!   * the capsule records the clock reads the handler took, in order;
//!   * the capsule file already exists on disk when a reporter is invoked
//!     (the ordering guarantee that makes `ErrorEvent::capsule` useful);
//!   * request bodies are bounded and capsule files are owner-only;
//!   * the retention cap prunes oldest-first.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::TimeZone as _;

use autumn_web::AppState;
use autumn_web::capsule::{Capsule, CapsuleBody, CapsuleOutcome};
use autumn_web::config::AutumnConfig;
use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};
use autumn_web::test::TestApp;
use autumn_web::{get, post, routes};
use axum::extract::State;

// ── Handlers ────────────────────────────────────────────────────────────────

#[get("/ok")]
async fn ok() -> &'static str {
    "ok"
}

#[get("/fail")]
async fn fail() -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(
        "database on fire",
    ))
}

#[get("/boom")]
async fn boom() -> &'static str {
    panic!("kaboom in handler");
}

#[get("/missing")]
async fn missing() -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::not_found_msg("no such thing"))
}

/// Reads the framework clock three times before failing. The `Clock` extractor
/// snapshots a single instant, so this goes through the state's clock source
/// directly — the same seam the framework's own internals read.
#[get("/clock-fail")]
async fn clock_fail(
    State(state): State<AppState>,
) -> Result<&'static str, autumn_web::AutumnError> {
    let first = state.clock().now();
    let second = state.clock().now();
    let third = state.clock().now();
    Err(autumn_web::AutumnError::internal_server_error_msg(format!(
        "{first} {second} {third}"
    )))
}

#[post("/echo-fail")]
async fn echo_fail(body: String) -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(format!(
        "got {} bytes",
        body.len()
    )))
}

/// Fails without ever extracting the body — the common shape of a handler that
/// rejects a request before reading it.
#[post("/ignore-body-fail")]
async fn ignore_body_fail() -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(
        "rejected before reading the body",
    ))
}

/// Fails with the submitted payload quoted back in the error message — the way
/// a real "could not store X" error usually reads.
#[post("/store-secret")]
async fn store_secret(body: String) -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(format!(
        "could not store {body}"
    )))
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// A test config with capture pointed at `dir`.
fn capture_config(dir: &Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = true;
    config.failure_capture.dir = dir.to_string_lossy().into_owned();
    config
}

/// Every capsule file currently in `dir`, oldest first.
fn capsule_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

/// Wait for a capsule to appear; capsules are written on a detached task.
async fn await_capsules(dir: &Path, expected: usize) -> Vec<PathBuf> {
    for _ in 0..100 {
        let paths = capsule_paths(dir);
        if paths.len() >= expected {
            return paths;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    capsule_paths(dir)
}

/// Drop a placeholder capsule file into `dir` whose name carries a timestamp
/// `age_secs` in the past.
///
/// Pruning reads a file's age from the timestamp prefix the writer puts in the
/// name, so a seeded name is all it takes to make a file look old — no clock
/// mocking, no `filetime`.
fn write_aged_capsule(dir: &Path, age_secs: i64, sequence: u32) -> PathBuf {
    let stamp =
        (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).format("%Y%m%dT%H%M%S%.6f");
    let path = dir.join(format!("{stamp}-{sequence:06}-seeded.json"));
    std::fs::write(&path, b"{}").expect("seeded capsule writes");
    path
}

/// Poll until `settled` holds; capsules are written (and pruned) on a detached
/// task, so a directory's final shape only arrives after the response does.
async fn await_settled(settled: impl Fn() -> bool) {
    for _ in 0..100 {
        if settled() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read_capsule(path: &Path) -> Capsule {
    let json = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("capsule at {} must be readable: {error}", path.display()));
    Capsule::from_json(&json)
        .unwrap_or_else(|error| panic!("capsule at {} must parse: {error}", path.display()))
}

/// A reporter that records, for each event, whether the referenced capsule
/// file already existed at the moment `report` ran.
/// One observation: the capsule path the event referenced (if any), and
/// whether that file already existed when the reporter ran.
type Observation = (Option<PathBuf>, bool);

#[derive(Clone, Default)]
struct CapsuleWitness {
    seen: Arc<Mutex<Vec<Observation>>>,
}

impl ErrorReporter for CapsuleWitness {
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
        let seen = Arc::clone(&self.seen);
        let observed = event
            .capsule
            .as_ref()
            .map(|reference| (reference.path.clone(), reference.path.exists()));
        Box::pin(async move {
            if let Ok(mut seen) = seen.lock() {
                seen.push(observed.map_or((None, false), |(path, exists)| (Some(path), exists)));
            }
        })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn capture_disabled_by_default_writes_no_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Default config: capture off, but point `dir` at the tempdir anyway so a
    // regression that ignores `enabled` is caught here rather than polluting
    // the repo's `tmp/`.
    let mut config = capture_config(dir.path());
    config.failure_capture.enabled = false;

    let client = TestApp::new().config(config).routes(routes![fail]).build();
    client.get("/fail").send().await.assert_status(500);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        capsule_paths(dir.path()).is_empty(),
        "capture is opt-in; a disabled app must never write a capsule"
    );
}

#[tokio::test]
async fn enabled_capture_persists_capsule_on_500() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![fail])
        .build();

    client.get("/fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(paths.len(), 1, "a 500 must leave exactly one capsule");
    let capsule = read_capsule(&paths[0]);

    assert_eq!(capsule.request.method, "GET");
    assert_eq!(capsule.request.uri, "/fail");
    assert_eq!(capsule.request.route.as_deref(), Some("/fail"));
    match capsule.outcome {
        CapsuleOutcome::Status { code, message, .. } => {
            assert_eq!(code, 500);
            assert!(
                message.contains("database on fire"),
                "the capsule must carry the real error message, got {message}"
            );
        }
        outcome @ CapsuleOutcome::Panic { .. } => {
            panic!("a 500 must record a Status outcome, got {outcome:?}")
        }
    }
    assert!(!capsule.truncated);
}

#[tokio::test]
async fn enabled_capture_persists_capsule_on_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![boom])
        .build();

    client.get("/boom").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(paths.len(), 1, "a caught panic must leave one capsule");
    let capsule = read_capsule(&paths[0]);

    match capsule.outcome {
        CapsuleOutcome::Panic {
            status, payload, ..
        } => {
            assert_eq!(status, 500);
            assert!(
                payload.contains("kaboom in handler"),
                "the capsule must carry the panic payload, got {payload}"
            );
        }
        outcome @ CapsuleOutcome::Status { .. } => {
            panic!("a panic must record a Panic outcome, got {outcome:?}")
        }
    }
}

#[tokio::test]
async fn successful_request_persists_no_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![ok])
        .build();

    client.get("/ok").send().await.assert_status(200);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        capsule_paths(dir.path()).is_empty(),
        "capsules are for failures; a 200 must leave nothing behind"
    );
}

#[tokio::test]
async fn four_xx_persists_no_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![missing])
        .build();

    client.get("/missing").send().await.assert_status(404);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        capsule_paths(dir.path()).is_empty(),
        "client errors are not reported, so they must not be captured either"
    );
}

#[tokio::test]
async fn capsule_records_clock_reads_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let start = chrono::Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid timestamp");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .with_clock(autumn_web::time::TickingClock::starting_at(start))
        .routes(routes![clock_fail])
        .build();

    client.get("/clock-fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    assert!(
        capsule.clock.len() >= 3,
        "the handler read the clock three times; the capsule must record them, got {:?}",
        capsule.clock
    );
    let mut sorted = capsule.clock.clone();
    sorted.sort_unstable();
    assert_eq!(
        capsule.clock, sorted,
        "clock readings must be recorded in the order they were taken"
    );
}

#[tokio::test]
async fn error_event_references_the_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let witness = CapsuleWitness::default();
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .with_error_reporter(witness.clone())
        .routes(routes![fail])
        .build();

    client.get("/fail").send().await.assert_status(500);

    for _ in 0..100 {
        if witness.seen.lock().is_ok_and(|seen| !seen.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let seen = witness.seen.lock().expect("witness lock").clone();
    assert_eq!(seen.len(), 1, "exactly one event must reach the reporter");
    let (path, existed) = &seen[0];
    let path = path
        .as_ref()
        .expect("ErrorEvent::capsule must reference the written capsule");
    assert!(
        *existed,
        "the capsule at {} must already exist when the reporter runs — a reporter \
         that follows the reference must not race the writer",
        path.display()
    );
}

#[tokio::test]
async fn capsule_body_is_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    config.failure_capture.max_body_bytes = 16;
    let client = TestApp::new()
        .config(config)
        .routes(routes![echo_fail])
        .build();

    let big = "x".repeat(4096);
    client
        .post("/echo-fail")
        .body(big.clone())
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    match capsule.request.body {
        CapsuleBody::Skipped { declared_len } => {
            assert_eq!(declared_len, Some(4096));
        }
        other => panic!(
            "a body over `max_body_bytes` must be recorded as skipped, never copied, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn handler_that_never_reads_the_body_still_gets_a_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![ignore_body_fail])
        .build();

    client
        .post("/ignore-body-fail")
        .body("payload the handler never looks at")
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(paths.len(), 1, "an unread body must not cost the capsule");
    let capsule = read_capsule(&paths[0]);

    assert_eq!(capsule.request.method, "POST");
    assert_eq!(capsule.request.uri, "/ignore-body-fail");
    match capsule.outcome {
        CapsuleOutcome::Status { code, .. } => assert_eq!(code, 500),
        outcome @ CapsuleOutcome::Panic { .. } => panic!("expected a 500, got {outcome:?}"),
    }
    // Body capture is a tee off the handler's own read, so a handler that never
    // reads gives the capsule nothing — which the capsule has to say out loud
    // rather than pass off as "the request had no body".
    assert!(
        matches!(
            capsule.request.body,
            CapsuleBody::Absent | CapsuleBody::Text(_)
        ),
        "got {:?}",
        capsule.request.body
    );
    assert!(
        capsule.notes.iter().any(|note| note.contains("body")),
        "an incomplete body must be flagged in the capsule notes, got {:?}",
        capsule.notes
    );
}

#[tokio::test]
async fn outcome_does_not_echo_a_redacted_request_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![store_secret])
        .build();

    client
        .post("/store-secret")
        .form("email=ada&password=hunter2secret")
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    let CapsuleOutcome::Status { message, .. } = &capsule.outcome else {
        panic!("expected a 500, got {:?}", capsule.outcome);
    };
    assert!(
        !message.contains("hunter2secret"),
        "the outcome must not smuggle back a value redaction removed from the \
         request body, got {message}"
    );
    assert!(
        message.contains("[FILTERED]"),
        "the echoed value must be replaced by the placeholder, got {message}"
    );

    let whole = std::fs::read_to_string(&paths[0]).expect("capsule readable");
    assert!(
        !whole.contains("hunter2secret"),
        "no part of the capsule may contain the redacted value"
    );
}

#[tokio::test]
async fn malformed_json_body_never_reaches_the_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![echo_fail])
        .build();

    // Declares JSON, is not JSON — the shape a truncated or hand-rolled
    // payload arrives in. There are no keys to redact on, so none of it may be
    // copied.
    client
        .post("/echo-fail")
        .header("content-type", "application/json")
        .body(r#"{"password":"hunter2secret", "#)
        .send()
        .await
        .assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let capsule = read_capsule(&paths[0]);
    assert!(
        matches!(capsule.request.body, CapsuleBody::Skipped { .. }),
        "a body that declared JSON but did not parse must be masked, got {:?}",
        capsule.request.body
    );
    assert!(
        capsule
            .notes
            .iter()
            .any(|note| note.contains("did not parse")),
        "the capsule must say why the body is missing, got {:?}",
        capsule.notes
    );
    let whole = std::fs::read_to_string(&paths[0]).expect("capsule readable");
    assert!(
        !whole.contains("hunter2secret"),
        "no part of the capsule may contain an unredactable body's contents"
    );
}

#[tokio::test]
async fn max_capsules_prunes_oldest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    config.failure_capture.max_capsules = 2;
    let client = TestApp::new().config(config).routes(routes![fail]).build();

    // Seeded with names old enough to be outside the prune grace window, so
    // "oldest" is unambiguous and pruning is free to act on them.
    let oldest = write_aged_capsule(dir.path(), 3 * 3600, 0);
    let middle = write_aged_capsule(dir.path(), 2 * 3600, 1);
    let newest_seed = write_aged_capsule(dir.path(), 3600, 2);

    client.get("/fail").send().await.assert_status(500);
    await_settled(|| !middle.exists()).await;
    let paths = capsule_paths(dir.path());

    assert!(!oldest.exists(), "the oldest capsule must be pruned first");
    assert!(
        !middle.exists(),
        "retention must prune down to max_capsules"
    );
    assert!(
        newest_seed.exists(),
        "pruning stops at the cap; the newest retained capsule must survive"
    );
    assert_eq!(
        paths.len(),
        2,
        "retention must leave exactly max_capsules, found {paths:?}"
    );
}

#[tokio::test]
async fn max_capsules_zero_still_keeps_the_new_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    // Nonsensical, but configuration is user input: a zero cap must not mean
    // "record the failure and immediately delete it".
    config.failure_capture.max_capsules = 0;
    let client = TestApp::new().config(config).routes(routes![fail]).build();

    client.get("/fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(
        paths.len(),
        1,
        "max_capsules is clamped to at least one, found {paths:?}"
    );
    assert_eq!(read_capsule(&paths[0]).request.uri, "/fail");
}

#[tokio::test]
async fn prune_spares_recent_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    config.failure_capture.max_capsules = 1;
    let client = TestApp::new().config(config).routes(routes![fail]).build();

    let ancient = write_aged_capsule(dir.path(), 24 * 3600, 0);

    client.get("/fail").send().await.assert_status(500);
    await_settled(|| !ancient.exists() && capsule_paths(dir.path()).len() == 1).await;
    client.get("/fail").send().await.assert_status(500);
    await_settled(|| capsule_paths(dir.path()).len() == 2).await;
    let paths = capsule_paths(dir.path());

    assert!(
        !ancient.exists(),
        "an aged capsule beyond the cap must be pruned"
    );
    assert_eq!(
        paths.len(),
        2,
        "a capsule written seconds ago may still be on its way to a reporter, \
         so retention overshoots rather than deleting it, found {paths:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn capsule_file_is_owner_readable_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![fail])
        .build();

    client.get("/fail").send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    let metadata = std::fs::metadata(&paths[0]).expect("capsule metadata");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o600,
        "a capsule holds production request data; it must not be group- or world-readable"
    );
}
