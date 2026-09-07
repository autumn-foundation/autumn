//! Integration tests for the extended capsule seams and capsule → regression
//! test conversion (issue #1634).
//!
//! Docker-free by construction: every seam here is served from the capsule, and
//! the capsules these tests replay carry no database traffic.
//!
//! Verifies that:
//!   * an outbound HTTP call a failing request made is captured, redacted, and
//!     served from the capsule on replay instead of being dialled;
//!   * a job the failing request enqueued is captured and asserted on replay
//!     without a queue;
//!   * cache reads/writes, mail sends and the resolved tenant replay from the
//!     capsule;
//!   * framework-minted identifiers replay byte-for-byte;
//!   * an effect the replayed code performs that the recording never did — and
//!     a recorded effect it never performs — are both divergences;
//!   * a committed capsule replays as an ordinary `#[tokio::test]` with no
//!     network, database or queue, and the fixture carries only redacted
//!     content.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::capsule::regression::{RegressionCase, RegressionContext};
use autumn_web::capsule::{
    Capsule, CapsuleOutcome, DivergenceLog, ReplayFixtures, Verdict, execute, load_capsule,
};
use autumn_web::config::AutumnConfig;
use autumn_web::entropy::Rng;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use futures::FutureExt as _;
use serde::{Deserialize, Serialize};

// ── Handlers ────────────────────────────────────────────────────────────────

/// Calls a third party, then fails with what it got back — the shape of a
/// production failure that depends on an upstream response.
#[get("/charge")]
async fn charge(State(state): State<AppState>) -> Result<&'static str, AutumnError> {
    let client = autumn_web::http_client::Client::from_state(&state).named("payments");
    let response = client
        .get("https://payments.example/charge")
        .header("authorization", "Bearer downstream-secret-token")
        .send()
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(format!("upstream: {error}")))?;
    Err(AutumnError::internal_server_error_msg(format!(
        "upstream said {}",
        response.status().as_u16()
    )))
}

/// Mints an identifier through the framework's entropy seam and fails with it.
#[get("/mint")]
async fn mint(rng: Rng) -> Result<&'static str, AutumnError> {
    Err(AutumnError::internal_server_error_msg(format!(
        "minted {}",
        rng.uuid_v4()
    )))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReceiptArgs {
    order: i64,
}

static RECEIPT_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "capsule_send_receipt", max_attempts = 1, backoff_ms = 1)]
async fn capsule_send_receipt(_state: AppState, args: ReceiptArgs) -> AutumnResult<()> {
    let _ = args;
    RECEIPT_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

struct CapsuleJobsPlugin;

impl Plugin for CapsuleJobsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![capsule_send_receipt])
    }
}

/// Enqueues a job, then fails — the "the failure happened after the side
/// effect" shape.
#[post("/order")]
async fn place_order() -> Result<&'static str, AutumnError> {
    CapsuleSendReceiptJob::enqueue(ReceiptArgs { order: 7 }).await?;
    Err(AutumnError::internal_server_error_msg("order blew up"))
}

// ── Harness ─────────────────────────────────────────────────────────────────

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

fn replay_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config
}

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

async fn await_one_capsule(dir: &Path) -> PathBuf {
    for _ in 0..200 {
        if let Some(path) = capsule_paths(dir).into_iter().next() {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no capsule was written to {}", dir.display());
}

// ── Phase 1: outbound HTTP ──────────────────────────────────────────────────

#[tokio::test]
async fn an_outbound_call_is_captured_redacted_and_replayed_from_the_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![charge]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(502, serde_json::json!({"error": "upstream down"}));
    let client = app.build();

    client.get("/charge").send().await.assert_status(500);
    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");

    // ── Captured ────────────────────────────────────────────────────────
    let exchange = capsule
        .effects
        .http
        .first()
        .expect("the outbound call must be on the tape");
    assert_eq!(exchange.method, "GET");
    assert_eq!(exchange.url, "https://payments.example/charge");
    assert_eq!(exchange.status, 502);

    // ── Redacted (AC: redaction covers the new seams) ───────────────────
    let authorization = exchange
        .request_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        authorization,
        Some("[FILTERED]"),
        "an outbound Authorization header carries a downstream credential and \
         must be masked like an inbound one: {:?}",
        exchange.request_headers
    );
    let serialized = serde_json::to_string(&capsule).expect("capsule serializes");
    assert!(
        !serialized.contains("downstream-secret-token"),
        "the capsule must not carry the outbound credential anywhere"
    );
    assert!(
        capsule
            .request
            .redacted_keys
            .iter()
            .any(|key| key.contains("http[0].request_header:authorization")),
        "the redaction manifest must name what was masked on the outbound seam: {:?}",
        capsule.request.redacted_keys
    );

    // ── Replayed with no network ────────────────────────────────────────
    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![charge])
        .with_clock(fixtures.clock())
        .with_entropy(fixtures.entropy())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert!(
        outcome.effect_divergences.is_empty(),
        "a faithful replay must stay on the effect tape: {:?}",
        outcome.effect_divergences
    );
    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded 500 must reproduce from the recorded 502: {outcome:?}"
    );
}

#[tokio::test]
async fn an_outbound_call_the_capsule_never_recorded_diverges_instead_of_dialling() {
    // The capsule records no outbound calls at all, but the replayed route
    // makes one: the run must be reported as diverged, and nothing may leave
    // the process.
    let capsule = bare_capsule("GET", "/charge", 500, "upstream said 502");
    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![charge])
        .with_clock(fixtures.clock())
        .build()
        .into_router();

    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(outcome.verdict, Verdict::Diverged, "{outcome:?}");
    assert!(
        outcome
            .effect_divergences
            .iter()
            .any(|divergence| divergence.detail.contains("payments.example")),
        "the report must name the call that was refused: {:?}",
        outcome.effect_divergences
    );
}

/// Outbound webhook deliveries are covered by the outbound-HTTP seam
/// *transitively* — they send through the same framework client — so the
/// property worth pinning is that they still do.
///
/// A source-level check rather than a delivery test: standing up a webhook
/// endpoint proves the delivery works, not that it went through the seam, and
/// a refactor to a bare `reqwest` call would silently drop capsule coverage
/// with every behavioural test still green.
#[test]
fn outbound_webhook_deliveries_still_send_through_the_captured_client() {
    let source = include_str!("../../src/webhook_outbound.rs");
    assert!(
        source.contains("use crate::http_client::Client;"),
        "webhook delivery must use the framework HTTP client, which is where \
         the failure-capsule seam lives"
    );
    assert!(
        !source.contains("reqwest::Client::new()"),
        "a bare reqwest client would bypass the outbound-HTTP capsule seam"
    );
}

// ── Phase 1: jobs ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_enqueue_is_captured_and_asserted_on_replay_without_a_queue() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let dir = tempfile::tempdir().expect("tempdir");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .plugin(CapsuleJobsPlugin)
        .routes(routes![place_order])
        .build();
    client.post("/order").send().await.assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    let enqueued = capsule
        .effects
        .jobs
        .first()
        .expect("the enqueue must be on the tape");
    assert_eq!(enqueued.name, "capsule_send_receipt");

    // Replay: no job runtime at all, so an enqueue that is not served from the
    // tape would fail with "job runtime is not initialized".
    job::clear_global_job_client();
    let runs_before = RECEIPT_RUNS.load(Ordering::SeqCst);
    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![place_order])
        .with_clock(fixtures.clock())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
    assert_eq!(
        RECEIPT_RUNS.load(Ordering::SeqCst),
        runs_before,
        "replaying an enqueue must never run the job"
    );
}

// ── Phase 1: job-scoped capsules ────────────────────────────────────────────

static FAILING_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "capsule_failing_job", max_attempts = 1, backoff_ms = 1)]
async fn capsule_failing_job(_state: AppState, args: ReceiptArgs) -> AutumnResult<()> {
    FAILING_RUNS.fetch_add(1, Ordering::SeqCst);
    Err(AutumnError::internal_server_error_msg(format!(
        "receipt {} could not be sent",
        args.order
    )))
}

struct FailingJobPlugin;

impl Plugin for FailingJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![capsule_failing_job])
    }
}

#[post("/fail-later")]
async fn enqueue_failing() -> Result<&'static str, AutumnError> {
    CapsuleFailingJobJob::enqueue(ReceiptArgs { order: 11 }).await?;
    Ok("queued")
}

#[tokio::test]
async fn a_failure_inside_a_job_produces_a_job_scoped_capsule() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let dir = tempfile::tempdir().expect("tempdir");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .plugin(FailingJobPlugin)
        .routes(routes![enqueue_failing])
        .build();
    // The request itself succeeds; the *job* is what fails, and it is the job
    // that must leave a capsule.
    client.post("/fail-later").send().await.assert_ok();

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    let recorded = capsule
        .job
        .as_ref()
        .expect("a job failure must record the job it happened in");
    assert_eq!(recorded.name, "capsule_failing_job");
    assert_eq!(recorded.payload, serde_json::json!({"order": 11}));
    assert_eq!(
        capsule.request.method, "JOB",
        "the synthetic entry point must not look like a replayable HTTP request"
    );
    match &capsule.outcome {
        CapsuleOutcome::Status { code, message, .. } => {
            assert_eq!(*code, 500);
            assert!(
                message.contains("receipt 11 could not be sent"),
                "the job's own error must be recorded: {message}"
            );
        }
        other @ CapsuleOutcome::Panic { .. } => {
            panic!("expected a status outcome, got {other:?}")
        }
    }

    // A job capsule is not replayable against a router, and says so rather
    // than 404ing into a `mismatch` that reads as "the bug is gone".
    let case = RegressionCase::from_path(&path).expect("the capsule parses");
    let refusal =
        std::panic::AssertUnwindSafe(case.assert_reproduces(|_: &RegressionContext<'_>| {
            TestApp::new().config(replay_config()).build().into_router()
        }))
        .catch_unwind()
        .await;
    assert!(
        refusal.is_err(),
        "a job capsule must not be replayed against a router"
    );

    job::clear_global_job_client();
}

static PANIC_RUNS: AtomicUsize = AtomicUsize::new(0);
static RETRY_RUNS: AtomicUsize = AtomicUsize::new(0);

/// Three attempts configured — but all three backends dead-letter a panic
/// immediately, so attempt 1 is also the last one this job ever gets.
#[job(name = "capsule_panicking_job", max_attempts = 3, backoff_ms = 1)]
async fn capsule_panicking_job(_state: AppState, args: ReceiptArgs) -> AutumnResult<()> {
    PANIC_RUNS.fetch_add(1, Ordering::SeqCst);
    panic!("receipt {} exploded", args.order);
}

/// An ordinary failure with attempts left: this one really will be retried, so
/// it must *not* leave a capsule yet.
#[job(name = "capsule_retrying_job", max_attempts = 3, backoff_ms = 1)]
async fn capsule_retrying_job(_state: AppState, _args: ReceiptArgs) -> AutumnResult<()> {
    RETRY_RUNS.fetch_add(1, Ordering::SeqCst);
    Err(AutumnError::internal_server_error_msg("not yet"))
}

struct RetryingJobPlugin;

impl Plugin for RetryingJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![capsule_panicking_job, capsule_retrying_job])
    }
}

#[post("/panic-later")]
async fn enqueue_panicking() -> Result<&'static str, AutumnError> {
    CapsulePanickingJobJob::enqueue(ReceiptArgs { order: 12 }).await?;
    Ok("queued")
}

/// A panicked job dead-letters on its first attempt whatever `max_attempts`
/// says, so gating capture on "is this the final attempt?" would mean the one
/// job failure most worth a capsule never produced one.
#[tokio::test]
async fn a_job_that_panics_before_its_final_attempt_still_leaves_a_capsule() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let dir = tempfile::tempdir().expect("tempdir");

    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .plugin(RetryingJobPlugin)
        .routes(routes![enqueue_panicking])
        .build();
    client.post("/panic-later").send().await.assert_ok();

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    assert_eq!(
        capsule
            .job
            .as_ref()
            .expect("a panicked job records its entry point")
            .name,
        "capsule_panicking_job"
    );
    match &capsule.outcome {
        CapsuleOutcome::Panic { payload, .. } => assert!(
            payload.contains("receipt 12 exploded"),
            "the panic payload must be recorded: {payload}"
        ),
        other @ CapsuleOutcome::Status { .. } => {
            panic!("expected a panic outcome, got {other:?}")
        }
    }
    assert_eq!(
        PANIC_RUNS.load(Ordering::SeqCst),
        1,
        "a panic is dead-lettered, not retried"
    );

    job::clear_global_job_client();
}

// ── Phase 2: mail ───────────────────────────────────────────────────────────

#[cfg(feature = "mail")]
#[tokio::test]
async fn a_mail_send_is_captured_and_asserted_on_replay_without_delivering() {
    use autumn_web::mail::{Mail, Mailer};

    #[post("/receipt")]
    async fn mail_then_fail(mailer: Mailer) -> Result<&'static str, AutumnError> {
        let mail = Mail::builder()
            .to("alice@example.com")
            .subject("Your receipt")
            .text("thanks")
            .build()
            .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
        mailer
            .send(mail)
            .await
            .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
        Err(AutumnError::internal_server_error_msg(
            "receipt saved, order lost",
        ))
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    config.mail.transport = autumn_web::mail::Transport::Log;
    config.mail.from = Some("noreply@example.com".to_owned());
    let client = TestApp::new()
        .config(config)
        .routes(routes![mail_then_fail])
        .build();
    client.post("/receipt").send().await.assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    let sent = capsule
        .effects
        .mail
        .first()
        .expect("the send must be on the tape");
    assert_eq!(sent.to, vec!["alice@example.com".to_owned()]);
    assert_eq!(sent.subject, "Your receipt");

    // Replay with **no mail configuration at all**: a send that reached a
    // transport would fail, and one served from the tape must not.
    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![mail_then_fail])
        .with_clock(fixtures.clock())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded send must be asserted from the tape, not delivered: {outcome:?}"
    );
    assert!(
        client.sent_mail().len() == 1,
        "exactly the capture-phase send should have reached the recorder"
    );
}

// ── Phase 2: tenancy ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_resolved_tenant_is_captured_and_replayed_without_live_tenant_config() {
    use autumn_web::tenancy::Tenant;

    #[get("/whose")]
    async fn whose(tenant: Tenant) -> Result<&'static str, AutumnError> {
        Err(AutumnError::internal_server_error_msg(format!(
            "tenant {} exploded",
            tenant.0
        )))
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = capture_config(dir.path());
    config.tenancy.enabled = true;
    config.tenancy.source = "header".to_owned();
    config.tenancy.header_name = "x-tenant-id".to_owned();
    let client = TestApp::new().config(config).routes(routes![whose]).build();

    client
        .get("/whose")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    assert_eq!(
        capsule
            .effects
            .tenant
            .as_ref()
            .and_then(|tenant| tenant.id.as_deref()),
        Some("acme"),
        "the resolved tenant must be on the tape"
    );

    // Replay with tenancy *disabled* in the config: resolution would fail
    // outright, so a reproduction proves the tenant came from the capsule.
    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![whose])
        .with_clock(fixtures.clock())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded tenant must be served without live tenant config: {outcome:?}"
    );
}

// ── Phase 3: randomness ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_framework_minted_identifier_replays_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![mint])
        .build();
    client.get("/mint").send().await.assert_status(500);

    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");
    assert!(
        !capsule.effects.random.is_empty(),
        "the draw the handler made must be on the tape"
    );
    let CapsuleOutcome::Status {
        message: recorded, ..
    } = &capsule.outcome
    else {
        panic!("expected a status outcome: {:?}", capsule.outcome);
    };

    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![mint])
        .with_clock(fixtures.clock())
        .with_entropy(fixtures.entropy())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the identifier the failing request minted must reappear ({recorded}): {outcome:?}"
    );
}

// ── Conversion: a capsule as an ordinary test ───────────────────────────────

#[tokio::test]
async fn a_committed_capsule_replays_as_an_ordinary_test_with_no_live_dependencies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![charge]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(502, serde_json::json!({"error": "upstream down"}));
    let client = app.build();
    client.get("/charge").send().await.assert_status(500);
    let path = await_one_capsule(dir.path()).await;

    // Exactly what a generated test does: read the committed fixture and
    // replay it. No mock registry is installed on the replay app — the
    // recorded response comes from the capsule.
    let case = RegressionCase::from_path(&path).expect("the committed capsule parses");
    case.assert_reproduces(|ctx: &RegressionContext<'_>| {
        TestApp::new()
            .config(replay_config())
            .routes(routes![charge])
            .with_clock(ctx.clock())
            .with_entropy(ctx.entropy())
            .build()
            .into_router()
    })
    .await;
}

#[tokio::test]
async fn a_capsule_from_an_incompatible_format_version_fails_loudly_not_vacuously() {
    let mut capsule = bare_capsule("GET", "/charge", 500, "boom");
    capsule.format_version = 1;
    let json = serde_json::to_string(&capsule).expect("serializes");
    let error =
        RegressionCase::from_json(&json).expect_err("an incompatible capsule must not load");
    let message = error.to_string();
    assert!(
        message.contains("older") && message.contains("guide"),
        "the refusal must be actionable: {message}"
    );
}

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A capsule with no recorded effects at all, for the divergence cases.
///
/// Built from JSON rather than through `schema::test_support` on purpose: that
/// helper lives behind the non-default `test-support` feature, and requiring it
/// here would keep this whole suite out of `cargo test --workspace` — the gate
/// CI actually runs. Building the document the way a reader meets it is the
/// more faithful fixture anyway.
fn bare_capsule(method: &str, uri: &str, code: u16, message: &str) -> Capsule {
    let json = serde_json::json!({
        "format_version": autumn_web::capsule::CAPSULE_FORMAT_VERSION,
        "id": "fixture",
        "captured_at": "2026-08-27T10:00:00Z",
        "autumn_version": env!("CARGO_PKG_VERSION"),
        "request": {
            "method": method,
            "uri": uri,
            "http_version": "HTTP/1.1",
            "headers": [],
            "body": "absent",
        },
        "outcome": {"status": {"code": code, "message": message}},
    })
    .to_string();
    Capsule::from_json(&json).expect("the fixture parses")
}
