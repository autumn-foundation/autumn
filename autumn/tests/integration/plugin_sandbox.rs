//! End-to-end evidence for the capability-sandboxed plugin lane (issue #1609).
//!
//! The unit tests in `plugin_sandbox::host` prove each containment in
//! isolation. This suite proves the thing an operator actually cares about: a
//! packaged artifact, installed into a real app, serving real requests — and an
//! adversarial corpus that cannot get out of it, cannot take the process down,
//! and cannot stop any other route from serving.

use std::sync::Arc;

use autumn_web::plugin_sandbox::test_guests as guests;
use autumn_web::plugin_sandbox::{
    DeniedCapability, ResourceLimits, SandboxArtifact, SandboxHost, SandboxManifest,
    SandboxPluginError, SandboxedPlugin,
};
use axum::body::Body;
use http::{Request, StatusCode};
use tower::ServiceExt as _;

const PLUGIN_NAME: &str = "autumn-plugin-hello";
const PREFIX: &str = "/hello";

fn manifest_toml(limits: &ResourceLimits) -> String {
    format!(
        r#"
name = "{PLUGIN_NAME}"
version = "0.1.0"
wire_version = 1
prefix = "{PREFIX}"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"

[limits]
fuel = {fuel}
memory_bytes = {memory}
max_request_body_bytes = {body}
max_response_bytes = {response}
max_concurrency = {concurrency}
"#,
        digest = "a".repeat(64),
        fuel = limits.fuel,
        memory = limits.memory_bytes,
        body = limits.max_request_body_bytes,
        response = limits.max_response_bytes,
        concurrency = limits.max_concurrency,
    )
}

/// Package a WAT guest exactly the way `autumn plugin package` does.
fn pack(wat: &str, limits: ResourceLimits) -> SandboxArtifact {
    let manifest = SandboxManifest::parse(&manifest_toml(&limits)).expect("valid manifest");
    let module = wat::parse_str(wat).expect("valid WAT");
    SandboxArtifact::seal(manifest, module).expect("seals")
}

fn plugin(wat: &str, limits: ResourceLimits) -> SandboxedPlugin {
    SandboxedPlugin::from_artifact(&pack(wat, limits)).expect("loads")
}

/// A sandboxed plugin mounted beside an ordinary route.
fn app(plugin: &SandboxedPlugin) -> axum::Router {
    axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .nest(PREFIX, plugin.router())
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ── AC-1 and AC-2: package it, install it, serve from it ─────────────────

#[tokio::test]
async fn a_packaged_artifact_installs_from_disk_and_serves_under_its_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hello.autumn-plugin");
    pack(guests::HELLO, ResourceLimits::default())
        .write_file(&path)
        .expect("writes");

    // This is the whole install: one file, read and verified, then mounted.
    let plugin = SandboxedPlugin::from_file(&path).expect("installs");
    assert_eq!(plugin.manifest().name, PLUGIN_NAME);

    let (status, body) = get(app(&plugin), "/hello/greet").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "hello from the sandbox");
}

#[tokio::test]
async fn an_artifact_modified_after_review_is_refused_at_install() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hello.autumn-plugin");
    let mut bytes = pack(guests::HELLO, ResourceLimits::default())
        .to_bytes()
        .expect("packs");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("writes");

    let err = SandboxedPlugin::from_file(&path).expect_err("tampering must be refused");
    assert!(
        matches!(err, SandboxPluginError::Artifact(_)),
        "{err}"
    );
    // The refusal has to say what happened, or an operator cannot act on it.
    assert!(err.to_string().contains("digest mismatch"), "{err}");
}

// ── AC-6: the grant is surfaced, and conformance still passes ────────────

#[test]
fn the_capability_grant_is_reviewable_before_the_plugin_serves() {
    let artifact = pack(guests::HELLO, ResourceLimits::default());
    let summary = artifact.manifest.consent_summary();
    for expected in [
        PLUGIN_NAME,
        PREFIX,
        "http-request",
        "GET /hello/greet",
        &artifact.manifest.sha256,
        "filesystem",
        "network",
        "environment",
        "database",
    ] {
        assert!(summary.contains(expected), "missing {expected}:\n{summary}");
    }
}

#[test]
fn a_sandboxed_plugin_passes_the_existing_conformance_checks() {
    let builder = autumn_web::app().plugin(plugin(guests::HELLO, ResourceLimits::default()));
    let routes = builder.plugin_route_infos().expect("route manifest");
    let report = autumn_web::plugin_conformance::run_conformance(
        &autumn_web::plugin_conformance::ConformanceConfig::new(PLUGIN_NAME).prefix(PREFIX),
        &routes,
    );
    assert!(report.passed(), "{}", report.to_text_report());
}

// ── AC-3, AC-4, AC-5: the adversarial corpus ─────────────────────────────

/// What containment a given escape attempt is expected to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Containment {
    /// Refused before the artifact ever runs.
    RefusedAtLoad,
    /// Ran, was denied the authority it reached for, and still answered.
    DeniedAndServed(DeniedCapability),
    /// Ran and was stopped without an answer; the prefix serves a 5xx.
    StoppedWithFivehundred,
}

/// Every distinct escape or denial-of-service attempt, and the containment each
/// one must produce. The success metric for #1609 is that this table is
/// exhaustive over the corpus and that every row holds.
const CORPUS: &[(&str, &str, Containment)] = &[
    (
        "read a file",
        guests::READ_FILE,
        Containment::DeniedAndServed(DeniedCapability::Filesystem),
    ),
    (
        "discover pre-opened directories",
        guests::DISCOVER_PREOPENS,
        Containment::DeniedAndServed(DeniedCapability::Filesystem),
    ),
    (
        "read a descriptor it was never given",
        guests::READ_STRAY_FD,
        Containment::DeniedAndServed(DeniedCapability::Filesystem),
    ),
    (
        "send on a socket",
        guests::NETWORK,
        Containment::DeniedAndServed(DeniedCapability::Network),
    ),
    (
        "read the host environment",
        guests::ENVIRONMENT,
        Containment::DeniedAndServed(DeniedCapability::Environment),
    ),
    (
        "read the host argv",
        guests::ARGUMENTS,
        Containment::DeniedAndServed(DeniedCapability::Environment),
    ),
    (
        "block the host on a poll",
        guests::POLL,
        Containment::DeniedAndServed(DeniedCapability::ProcessControl),
    ),
    (
        "forge a session cookie",
        guests::FORGE_COOKIE,
        Containment::DeniedAndServed(DeniedCapability::ResponseHeader),
    ),
    ("call a database seam", guests::DATABASE, Containment::RefusedAtLoad),
    (
        "call an invented host escape",
        guests::HOST_COMMAND,
        Containment::RefusedAtLoad,
    ),
    (
        "import a WASI function the shim does not implement",
        guests::UNDEFINED_WASI,
        Containment::RefusedAtLoad,
    ),
    ("spin the CPU forever", guests::CPU_SPIN, Containment::StoppedWithFivehundred),
    (
        "allocate without bound",
        guests::MEMORY_BOMB,
        Containment::StoppedWithFivehundred,
    ),
    ("trap", guests::TRAP, Containment::StoppedWithFivehundred),
    (
        "exit the process",
        guests::EXIT,
        Containment::StoppedWithFivehundred,
    ),
    (
        "never answer",
        guests::SILENT,
        Containment::StoppedWithFivehundred,
    ),
    (
        "flood stdout without ending a frame",
        guests::OUTPUT_FLOOD,
        Containment::StoppedWithFivehundred,
    ),
    (
        "split the response with CRLF",
        guests::SPLIT_RESPONSE,
        Containment::StoppedWithFivehundred,
    ),
    (
        "answer with an impossible status",
        guests::IMPOSSIBLE_STATUS,
        Containment::StoppedWithFivehundred,
    ),
    (
        "answer with something that is not a frame",
        guests::MALFORMED_FRAME,
        Containment::StoppedWithFivehundred,
    ),
    (
        "answer with an op the wire does not define",
        guests::UNKNOWN_OP,
        Containment::StoppedWithFivehundred,
    ),
];

/// Limits tight enough that a runaway guest is stopped in milliseconds, so the
/// whole corpus runs in a normal test budget.
fn adversarial_limits() -> ResourceLimits {
    ResourceLimits {
        fuel: 5_000_000,
        memory_bytes: 1024 * 1024,
        max_response_bytes: 4096,
        ..ResourceLimits::default()
    }
}

#[tokio::test]
async fn the_adversarial_corpus_is_contained_in_full() {
    assert!(
        CORPUS.len() >= 10,
        "the containment claim needs at least ten distinct attempts"
    );

    for (what, wat, expected) in CORPUS {
        let artifact = pack(wat, adversarial_limits());
        let loaded = SandboxedPlugin::from_artifact(&artifact);

        match expected {
            Containment::RefusedAtLoad => {
                let err = loaded
                    .err()
                    .unwrap_or_else(|| panic!("{what}: must be refused at load"));
                assert!(
                    matches!(err, SandboxPluginError::Load(_)),
                    "{what}: {err}"
                );
                continue;
            }
            _ => {}
        }

        let plugin = loaded.unwrap_or_else(|err| panic!("{what}: should load — {err}"));
        let (status, _) = get(app(&plugin), "/hello/greet").await;

        match expected {
            Containment::DeniedAndServed(capability) => {
                assert_eq!(status, StatusCode::OK, "{what}: should still answer");
                // The denial itself is asserted at the host level, where the
                // ledger is visible; here we prove it did not cost the caller
                // its answer.
                let outcome = SandboxHost::load(&artifact)
                    .unwrap_or_else(|err| panic!("{what}: {err}"))
                    .run(&sandbox_request());
                assert!(
                    outcome
                        .denials
                        .iter()
                        .any(|denial| denial.capability == *capability),
                    "{what}: expected a {capability} denial, got {:?}",
                    outcome.denials
                );
            }
            Containment::StoppedWithFivehundred => {
                assert!(status.is_server_error(), "{what}: expected 5xx, got {status}");
            }
            Containment::RefusedAtLoad => unreachable!(),
        }

        // Whatever it did, the rest of the application is untouched — which is
        // the claim that makes a sandbox worth having.
        let (status, body) = get(app(&plugin), "/healthz").await;
        assert_eq!(status, StatusCode::OK, "{what}: the app must keep serving");
        assert_eq!(body, "ok");
    }
}

fn sandbox_request() -> autumn_web::plugin_sandbox::SandboxRequest {
    autumn_web::plugin_sandbox::SandboxRequest {
        method: "GET".to_owned(),
        route: "/hello/greet".to_owned(),
        path: "/hello/greet".to_owned(),
        query: String::new(),
        path_params: vec![],
        headers: vec![],
        body: vec![],
    }
}

#[tokio::test]
async fn a_runaway_plugin_does_not_stop_a_concurrent_request_to_another_route() {
    // The interpreter is synchronous; if it ran on the async runtime a single
    // spinning guest would stall every other in-flight request on that worker.
    let plugin = Arc::new(plugin(guests::CPU_SPIN, ResourceLimits {
        fuel: 200_000_000,
        ..adversarial_limits()
    }));
    let spinning = tokio::spawn({
        let plugin = Arc::clone(&plugin);
        async move { get(app(&plugin), "/hello/greet").await }
    });
    let (status, body) = get(app(&plugin), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let (status, _) = spinning.await.expect("the spinning request completes");
    assert!(status.is_server_error(), "{status}");
}

// ── the success metric's latency half ────────────────────────────────────

/// Sandboxed hello-world overhead versus an equivalent native route.
///
/// Ignored by default: it is a timing measurement, and a debug-profile
/// interpreter measures the profile, not the design. Run it deliberately:
///
/// ```sh
/// cargo test -p autumn-web --release --features "plugin-sandbox,test-support" \
///   --test integration_tests plugin_sandbox::sandboxed_route_overhead -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "timing measurement; meaningful only in --release"]
async fn sandboxed_route_overhead_is_within_the_budget() {
    const ITERATIONS: usize = 400;
    const BUDGET_MICROS: u128 = 1_000;

    let plugin = plugin(guests::HELLO, ResourceLimits::default());
    let sandboxed = app(&plugin);
    let native = axum::Router::new().route(
        "/hello/greet",
        axum::routing::get(|| async { "hello from the sandbox" }),
    );

    let mut sandbox_micros = Vec::with_capacity(ITERATIONS);
    let mut native_micros = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let started = std::time::Instant::now();
        let (status, _) = get(sandboxed.clone(), "/hello/greet").await;
        sandbox_micros.push(started.elapsed().as_micros());
        assert_eq!(status, StatusCode::OK);

        let started = std::time::Instant::now();
        let (status, _) = get(native.clone(), "/hello/greet").await;
        native_micros.push(started.elapsed().as_micros());
        assert_eq!(status, StatusCode::OK);
    }

    sandbox_micros.sort_unstable();
    native_micros.sort_unstable();
    let p95 = |samples: &[u128]| samples[samples.len() * 95 / 100];
    let overhead = p95(&sandbox_micros).saturating_sub(p95(&native_micros));
    println!(
        "sandboxed p95 {sandbox}µs, native p95 {native}µs, overhead {overhead}µs",
        sandbox = p95(&sandbox_micros),
        native = p95(&native_micros),
    );
    assert!(
        overhead <= BUDGET_MICROS,
        "sandboxed p95 overhead {overhead}µs exceeds the {BUDGET_MICROS}µs budget"
    );
}
