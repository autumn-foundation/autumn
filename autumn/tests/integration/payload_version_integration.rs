//! Integration + macro-level tests for versioned job payloads (issue #1205).
//!
//! Covers the opt-in schema-version envelope (`__autumn_schema_version`), the
//! `#[job(version = N, upgrade = ...)]` macro surface, the precise
//! dead-letter error on a version/shape mismatch, and the CI golden-fixture
//! guard.
//!
//! These tests exercise the process-global job client (like
//! `job_recorder_integration.rs`), so they serialize on
//! `job::global_job_runtime_test_lock()` and clear the global client.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::payload_version::{self, JobPayloadVersionError};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ── Pure module round-trip tests ─────────────────────────────────────────────

#[test]
fn wrap_read_strip_round_trip() {
    let args = json!({ "user_id": 7 });
    let wrapped = payload_version::wrap(3, args.clone());
    assert_eq!(wrapped["__autumn_schema_version"], json!(3));
    assert_eq!(wrapped["args"], args);

    assert_eq!(payload_version::read_version(&wrapped), 3);

    let (version, inner) = payload_version::strip_version(wrapped);
    assert_eq!(version, 3);
    assert_eq!(inner, args);
}

#[test]
fn missing_marker_reads_as_version_1() {
    let raw = json!({ "user_id": 7 });
    assert_eq!(payload_version::read_version(&raw), 1);

    let (version, inner) = payload_version::split_version(&raw);
    assert_eq!(version, 1);
    assert_eq!(inner, &raw, "un-versioned payloads pass through unchanged");

    // A non-object payload also reads as v1 and passes through.
    assert_eq!(payload_version::read_version(&Value::Null), 1);
}

#[test]
fn decode_versioned_upgrades_chain_v1_to_v3() {
    // v1 -> v2 adds `greeting`; v2 -> v3 adds `locale`.
    fn upgrade(
        from: u32,
        mut value: Value,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let obj = value.as_object_mut().ok_or("expected object args")?;
        match from {
            1 => {
                obj.insert("greeting".into(), json!("hello"));
            }
            2 => {
                obj.insert("locale".into(), json!("en"));
            }
            other => return Err(format!("no upgrade from version {other}").into()),
        }
        Ok(value)
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct ArgsV3 {
        user_id: i64,
        greeting: String,
        locale: String,
    }

    let stored = payload_version::wrap(1, json!({ "user_id": 7 }));
    let decoded: ArgsV3 = payload_version::decode_versioned("welcome", 3, Some(upgrade), stored)
        .expect("upgrade v1->v3");
    assert_eq!(
        decoded,
        ArgsV3 {
            user_id: 7,
            greeting: "hello".into(),
            locale: "en".into()
        }
    );
}

#[test]
fn decode_versioned_without_upgrade_hook_yields_precise_error() {
    #[derive(Debug, Deserialize)]
    struct ArgsV3 {
        #[allow(dead_code)]
        user_id: i64,
    }

    let stored = payload_version::wrap(1, json!({ "user_id": 7 }));
    let err: JobPayloadVersionError =
        payload_version::decode_versioned::<ArgsV3>("send_welcome", 3, None, stored)
            .expect_err("no upgrade path must fail");

    assert_eq!(err.job_type(), "send_welcome");
    assert_eq!(err.stored_version(), 1);
    assert_eq!(err.expected_version(), 3);

    let msg = err.to_string();
    assert!(msg.contains("send_welcome"), "names job: {msg}");
    assert!(msg.contains("version 1"), "names stored version: {msg}");
    assert!(msg.contains("version 3"), "names expected version: {msg}");
    assert!(msg.contains("no upgrade path"), "explains why: {msg}");
    assert!(
        payload_version::is_payload_version_error(&msg),
        "detectable as a payload-version error, not a generic serde error"
    );
}

#[test]
fn golden_fixture_round_trips_against_current_struct() {
    // The falsifiable case from the issue: with the upgrade hook a v1 fixture
    // decodes into the *current* v2 struct; without it, the guard fails (see
    // `golden_fixture_without_upgrade_fails`).
    let fixture = include_str!("../fixtures/versioned_welcome_v1.json");

    payload_version::assert_golden_payload::<VersionedWelcomeArgs>(
        "versioned_welcome",
        2,
        Some(upgrade_versioned_welcome),
        fixture,
    );
}

#[test]
#[should_panic(expected = "versioned_welcome")]
fn golden_fixture_without_upgrade_fails() {
    // Removing the upgrade hook is exactly the breaking change the CI guard must
    // catch: a v1 fixture can no longer become the current v2 struct.
    let fixture = include_str!("../fixtures/versioned_welcome_v1.json");
    payload_version::assert_golden_payload::<VersionedWelcomeArgs>(
        "versioned_welcome",
        2,
        None,
        fixture,
    );
}

// ── Macro-level job definitions ──────────────────────────────────────────────

// Current (v2) shape of the welcome args: `greeting` was added in v2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VersionedWelcomeArgs {
    user_id: i64,
    greeting: String,
}

/// v1 -> v2 upgrade: default the newly-added `greeting` field.
///
/// Always `Ok` here, but the signature must stay `Result<_, _>` to match
/// `payload_version::UpgradeFn`, so `unnecessary_wraps` is allowed.
#[allow(clippy::unnecessary_wraps)]
fn upgrade_versioned_welcome(
    from: u32,
    mut value: Value,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    if from == 1
        && let Some(obj) = value.as_object_mut()
    {
        obj.entry("greeting".to_string())
            .or_insert_with(|| json!("hi"));
    }
    Ok(value)
}

static VERSIONED_USER: AtomicI64 = AtomicI64::new(0);
static VERSIONED_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "versioned_welcome", version = 2, upgrade = upgrade_versioned_welcome, max_attempts = 1, backoff_ms = 1)]
async fn versioned_welcome(_state: AppState, args: VersionedWelcomeArgs) -> AutumnResult<()> {
    VERSIONED_USER.store(args.user_id, Ordering::SeqCst);
    VERSIONED_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

// A default (un-versioned) job: proves opt-in-only wrapping stores raw args.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PlainArgs {
    user_id: i64,
}

#[job(name = "plain_job", max_attempts = 1, backoff_ms = 1)]
async fn plain_job(_state: AppState, args: PlainArgs) -> AutumnResult<()> {
    let _ = args;
    Ok(())
}

// A versioned job WITHOUT an upgrade hook, to prove a stale-version payload
// dead-letters with the precise error rather than a generic serde miss.
#[job(
    name = "strict_versioned",
    version = 3,
    max_attempts = 1,
    backoff_ms = 1
)]
async fn strict_versioned(_state: AppState, args: PlainArgs) -> AutumnResult<()> {
    let _ = args;
    Ok(())
}

struct VersionedJobsPlugin;

impl Plugin for VersionedJobsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![versioned_welcome, plain_job, strict_versioned])
    }
}

// ── Macro: opt-in wrapping + recorder strips the version marker ──────────────

#[tokio::test]
async fn opt_in_job_wraps_and_recorder_sees_clean_args() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    VersionedWelcomeJob::enqueue(VersionedWelcomeArgs {
        user_id: 5,
        greeting: "yo".into(),
    })
    .await
    .unwrap();

    // The recorder captures the raw (version-wrapped) payload, but the
    // assertion helper strips the marker so tests compare on clean args.
    client.assert_job_enqueued_with(
        "versioned_welcome",
        json!({ "user_id": 5, "greeting": "yo" }),
    );

    // The stored payload really is wrapped (opt-in job, version > 1).
    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(
        raw["__autumn_schema_version"],
        json!(2),
        "opt-in job is wrapped: {raw}"
    );

    job::clear_global_job_client();
}

#[tokio::test]
async fn default_job_stores_raw_args_no_envelope() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    PlainJobJob::enqueue(PlainArgs { user_id: 9 })
        .await
        .unwrap();

    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(
        *raw,
        json!({ "user_id": 9 }),
        "default job stores raw args: {raw}"
    );
    assert!(
        raw.get("__autumn_schema_version").is_none(),
        "default job must not be wrapped: {raw}"
    );

    job::clear_global_job_client();
}

// ── Macro: a v1 payload upgrades and the handler runs ────────────────────────

#[tokio::test]
async fn stored_v1_payload_upgrades_and_handler_runs() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    VERSIONED_RUNS.store(0, Ordering::SeqCst);
    VERSIONED_USER.store(0, Ordering::SeqCst);

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    // Simulate a payload enqueued by the *old* v1 code: wrapped at version 1
    // with only `user_id`. The current v2 handler must upgrade it.
    job::enqueue(
        "versioned_welcome",
        json!({ "__autumn_schema_version": 1, "args": { "user_id": 11 } }),
    )
    .await
    .unwrap();

    let report = client.perform_enqueued_jobs().await;
    report.assert_all_succeeded();
    assert_eq!(
        VERSIONED_USER.load(Ordering::SeqCst),
        11,
        "v1 payload upgraded and ran"
    );

    job::clear_global_job_client();
}

// ── Macro: a stale-version payload dead-letters with the precise error ───────

#[tokio::test]
async fn stale_version_without_upgrade_dead_letters_with_precise_error() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    // strict_versioned is at version 3 with no upgrade hook; a v1 payload cannot
    // be decoded and must surface the precise error.
    job::enqueue(
        "strict_versioned",
        json!({ "__autumn_schema_version": 1, "args": { "user_id": 1 } }),
    )
    .await
    .unwrap();

    let report = client.perform_enqueued_jobs().await;
    let failures = report.failures();
    assert_eq!(failures.len(), 1, "the stale payload surfaced as a failure");
    assert_eq!(failures[0].0, "strict_versioned");

    let msg = format!("{:?}", failures[0].1);
    assert!(msg.contains("strict_versioned"), "names job type: {msg}");
    assert!(msg.contains("version 1"), "names stored version: {msg}");
    assert!(msg.contains("version 3"), "names expected version: {msg}");

    job::clear_global_job_client();
}

// ── Transactional enqueue paths must wrap too (Codex tx-enqueue P2, #1205) ────
//
// The version envelope was originally applied only in the 5 generated enqueue
// methods; the transactional free functions serialized typed args raw. These
// tests drive `enqueue_after_commit` (which needs no DB connection — outside a
// tx it enqueues eagerly through the same chokepoint the connection-based
// paths use). The `*_on_conn` / `enqueue_in_tx` variants share the
// `enqueue_on_conn_due` chokepoint and are compile-verified; a Docker-gated DB
// test is out of scope here.

#[tokio::test]
async fn after_commit_enqueue_wraps_versioned_job() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    job::enqueue_after_commit(
        "versioned_welcome",
        VersionedWelcomeArgs {
            user_id: 3,
            greeting: "yo".into(),
        },
    )
    .await
    .unwrap();

    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(
        raw["__autumn_schema_version"],
        json!(2),
        "transactional enqueue must wrap an opt-in job: {raw}"
    );
    client.assert_job_enqueued_with(
        "versioned_welcome",
        json!({ "user_id": 3, "greeting": "yo" }),
    );

    job::clear_global_job_client();
}

#[tokio::test]
async fn after_commit_default_job_stores_raw() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    job::enqueue_after_commit("plain_job", PlainArgs { user_id: 1 })
        .await
        .unwrap();

    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(
        *raw,
        json!({ "user_id": 1 }),
        "default job stays raw via tx: {raw}"
    );
    assert!(
        raw.get("__autumn_schema_version").is_none(),
        "default job must not be wrapped: {raw}"
    );

    job::clear_global_job_client();
}

#[tokio::test]
async fn after_commit_versioned_no_upgrade_decodes_at_current_version() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    // strict_versioned is version 3 with no upgrade hook. Enqueued transactionally
    // with current args, the marker must be stored so decode lands at v3 — not
    // read as a missing-marker v1 and dead-lettered with "no upgrade path".
    job::enqueue_after_commit("strict_versioned", PlainArgs { user_id: 9 })
        .await
        .unwrap();

    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(
        raw["__autumn_schema_version"],
        json!(3),
        "wrapped at v3: {raw}"
    );

    let report = client.perform_enqueued_jobs().await;
    report.assert_all_succeeded();

    job::clear_global_job_client();
}

#[tokio::test]
async fn generated_method_wraps_exactly_once_no_double_wrap() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let client = TestApp::new().plugin(VersionedJobsPlugin).build();

    VersionedWelcomeJob::enqueue(VersionedWelcomeArgs {
        user_id: 4,
        greeting: "hi".into(),
    })
    .await
    .unwrap();

    let raw = &client.enqueued_jobs()[0].payload;
    assert_eq!(raw["__autumn_schema_version"], json!(2));
    assert_eq!(
        raw.as_object().map(serde_json::Map::len),
        Some(2),
        "exactly a 2-key envelope, not nested: {raw}"
    );
    assert!(
        raw["args"].get("__autumn_schema_version").is_none(),
        "inner args must not itself be an envelope (no double wrap): {raw}"
    );

    job::clear_global_job_client();
}
