//! Origin/edge conformance: the proof behind AC-2 and AC-3 of issue #1790.
//!
//! Every test here builds a **real** `wasm32-wasip1` capsule from this
//! example's own sources and sends requests through these lanes:
//!
//! | Lane | What runs | Driven by |
//! | --- | --- | --- |
//! | Native edge | `autumn_edge::serve_io` — the capsule's `main`, compiled for the host | the in-process host below |
//! | Wasm edge | the same code, compiled to `wasm32-wasip1` | `autumn_edge::host::EdgeArtifact` (wasmi) |
//! | Origin | the whole app: `TestApp` + the full middleware stack | `autumn_web::test` |
//! | Gateway | the capsule in front of the origin | `autumn_edge::gateway::EdgeGateway` |
//!
//! - **Tier A** (native edge vs wasm edge): byte-exact. Status, headers, body
//!   and fallthrough reason must be equal. Nothing is excused.
//! - **Tier B** (origin vs wasm edge): status and body exact. Headers are
//!   compared in both directions after projection. Only
//!   `VOLATILE_HEADERS` and `SECURITY_HEADERS` (which the host sets) are
//!   excused. For a declined case the origin must give its canonical answer.
//! - **Tier C** (gateway vs origin): for an edge-served request the client
//!   gets the origin's bytes, with only `VOLATILE_HEADERS` excused. For a
//!   declined request the gateway returns the origin's response unchanged.
//! - **Tier D**: Tiers A to C over 10,000 generated requests (fixed seed).
//!   This is the issue's success metric: zero divergence across >= 10k
//!   requests.
//!
//! Every test is `#[ignore]`d because it needs the `wasm32-wasip1` target
//! installed. CI runs them in the dedicated `edge-conformance` job:
//!
//! ```sh
//! rustup target add wasm32-wasip1
//! cargo test -p edge-greeting --test conformance -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `--test-threads=1` is not decoration: the suite installs a panic hook while
//! it drives the deliberately-panicking `/boom` handler, and a panic hook is
//! process-global.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use autumn_edge::conformance::{
    ConformanceCase, Expectation, SECURITY_HEADERS, Verdict, compare, compare_capsule,
    project_headers,
};
use autumn_edge::gateway::{EdgeGateway, Lane};
use autumn_edge::host::EdgeArtifact;
use autumn_edge::wire::{
    EdgeOutcome, EdgeRequest, EdgeResponse, FallthroughReason, GuestFrame, HostFrame, from_line,
    to_line,
};
use autumn_edge::{EdgeCapability, EdgeKv, serve_io};

/// The capsule target. One string, so a typo cannot make the suite silently
/// test the host build.
const TARGET: &str = "wasm32-wasip1";

/// The bin target `autumn build` looks for, and the one built here.
const CAPSULE_BIN: &str = "edge-capsule";

// ── the corpus ───────────────────────────────────────────────────────

/// The shared request corpus.
///
/// Each entry names a divergence class that could plausibly make two
/// implementations of "the same" router disagree: percent-encoding, a `%2F`
/// inside a segment, a trailing slash, repeated query keys, integer and float
/// rendering, a credential the edge must never see, and each of the four ways
/// the edge can decline.
const CORPUS: &[ConformanceCase] = &[
    ConformanceCase {
        name: "happy path",
        method: "GET",
        uri: "/greet/ada",
        headers: &[("accept", "text/plain")],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "credentials are stripped before the capsule sees them",
        method: "GET",
        uri: "/greet/ada",
        headers: &[
            ("cookie", "session=super-secret"),
            ("authorization", "Bearer super-secret"),
        ],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "credentials are invisible to a header-reading handler in both lanes",
        method: "GET",
        uri: "/whoami",
        headers: &[
            ("cookie", "session=super-secret"),
            ("authorization", "Bearer super-secret"),
            ("proxy-authorization", "Basic super-secret"),
        ],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "kv hit",
        method: "GET",
        uri: "/note/greeting",
        headers: &[],
        provided_capabilities: &[EdgeCapability::Kv],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "kv miss",
        method: "GET",
        uri: "/note/nothing-here",
        headers: &[],
        provided_capabilities: &[EdgeCapability::Kv],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "percent-encoded slash inside a segment",
        method: "GET",
        uri: "/greet/one%2Ftwo",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "percent-encoded unicode path",
        method: "GET",
        uri: "/greet/%C3%A9l%C3%A8ve",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "duplicate query keys",
        method: "GET",
        uri: "/stats?tag=b&tag=a&tag=b",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "float rendering",
        method: "GET",
        uri: "/stats?tag=one&tag=two",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "usize rendering through the primitive wrapper",
        method: "GET",
        uri: "/stats/count",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Served,
    },
    ConformanceCase {
        name: "trailing slash",
        method: "GET",
        uri: "/greet/ada/",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Fallthrough(FallthroughReason::UnknownRoute),
    },
    ConformanceCase {
        name: "unknown route",
        method: "GET",
        uri: "/nope",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Fallthrough(FallthroughReason::UnknownRoute),
    },
    ConformanceCase {
        name: "write method",
        method: "POST",
        uri: "/feedback",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Fallthrough(FallthroughReason::MethodNotEdgeEligible),
    },
    ConformanceCase {
        name: "kv route without the kv capability",
        method: "GET",
        uri: "/note/greeting",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Fallthrough(FallthroughReason::MissingCapability),
    },
    ConformanceCase {
        name: "panicking handler",
        method: "GET",
        uri: "/boom",
        headers: &[],
        provided_capabilities: &[],
        expect: Expectation::Fallthrough(FallthroughReason::CapsuleError),
    },
];

/// The one case whose handler panics.
///
/// A panic compiles to a trap on wasm, which the host reports as
/// [`FallthroughReason::CapsuleError`]. Natively there is no host and no
/// unwind boundary inside the edge lane, so the same call unwinds into this
/// test — which is itself the parity statement worth asserting, and the reason
/// this one case takes a different path through the harness.
const TRAPPING_URI: &str = "/boom";

/// The status the origin answers each declined case with.
///
/// Stated per case rather than as "not a 5xx", because the point of transparent
/// fallthrough is that the origin gives the *canonical* answer — the one it
/// would have given had the edge never been in the path.
fn origin_status_for_fallthrough(uri: &str) -> u16 {
    match uri {
        // The write path the edge refused on method grounds: its handler runs
        // here and answers normally.
        "/feedback" => 200,
        // `reporting` (a default feature) catches the handler panic at the HTTP
        // layer and renders a 500 — the origin's existing behaviour, unchanged
        // by the edge lane.
        TRAPPING_URI => 500,
        // The capability case. The edge declined because *that host* offered no
        // `kv`; the origin always has one, so the request is simply served —
        // which is the entire promise of transparent fallthrough. Note the
        // status matches the "kv hit" case above: the same URI, the same
        // answer, reached by a different route.
        uri if uri.starts_with("/note/") => 200,
        // Trailing slash and unknown path: the origin's own 404 is the
        // canonical answer, and the edge declining is what lets the request
        // reach it.
        _ => 404,
    }
}

// ── the capsule under test ───────────────────────────────────────────

/// Build the capsule once per test process and keep it loaded.
///
/// `EdgeArtifact::from_bytes` compiles the module once; `run` instantiates a
/// fresh store per request, so nothing leaks between corpus entries.
fn artifact() -> &'static Arc<EdgeArtifact> {
    static ARTIFACT: OnceLock<Arc<EdgeArtifact>> = OnceLock::new();
    ARTIFACT.get_or_init(|| {
        let wasm = build_capsule();
        Arc::new(
            EdgeArtifact::from_bytes(&wasm).expect("the built artifact is a valid wasm module"),
        )
    })
}

/// `cargo build --target wasm32-wasip1 --release --bin edge-capsule`.
///
/// The same command `autumn build` runs, so what is proven here is what ships.
/// It builds into a target directory of its own: the outer `cargo test` holds
/// the workspace build lock while this runs, and a nested cargo pointed at the
/// same directory would wait for a lock it can never get.
fn build_capsule() -> Vec<u8> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = workspace_target_directory(&manifest_dir).join("edge-conformance");

    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--target",
            TARGET,
            "--release",
            "--bin",
            CAPSULE_BIN,
        ])
        .current_dir(&manifest_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("cargo is on PATH");

    assert!(
        status.success(),
        "could not build the edge capsule for {TARGET}.\n\
         If the target is missing, run: rustup target add {TARGET}"
    );

    let artifact = target_dir
        .join(TARGET)
        .join("release")
        .join(format!("{CAPSULE_BIN}.wasm"));
    std::fs::read(&artifact).unwrap_or_else(|err| panic!("reading {}: {err}", artifact.display()))
}

/// The workspace's `target/`, straight from cargo rather than guessed at from a
/// relative path.
fn workspace_target_directory(manifest_dir: &Path) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(manifest_dir)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON");
    let directory = metadata["target_directory"]
        .as_str()
        .expect("cargo metadata reports a target_directory");
    PathBuf::from(directory)
}

// ── lane 1: the native edge lane ─────────────────────────────────────

/// The host→guest byte stream, shared by the reader and the writer so a
/// `kv_get` can be answered mid-request exactly as a real host answers it.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<VecDeque<u8>>>);

impl Wire {
    fn push(&self, line: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(line.as_bytes());
    }

    fn pop_into(&self, out: &mut [u8]) -> usize {
        let mut queue = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut written = 0;
        while written < out.len() {
            match queue.pop_front() {
                Some(byte) => {
                    out[written] = byte;
                    written += 1;
                }
                None => break,
            }
        }
        written
    }
}

/// The capsule's stdin. An empty queue is EOF, which ends the guest loop — one
/// request per `serve_io` call, mirroring the fresh store `EdgeArtifact::run`
/// gives each request.
struct HostToGuest(Wire);

impl Read for HostToGuest {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(self.0.pop_into(buf))
    }
}

/// The capsule's stdout: the in-process reference host.
///
/// Deliberately a re-implementation of what `autumn_edge::host` does around the
/// wasm instance. Both lanes are then driven by a host that only knows the
/// documented protocol — which is the claim the guide makes to anyone writing a
/// CDN shim.
struct GuestToHost {
    partial: Vec<u8>,
    wire: Wire,
    kv: Arc<dyn EdgeKv>,
    kv_provided: bool,
    outcome: Arc<Mutex<Option<EdgeOutcome>>>,
}

impl GuestToHost {
    fn on_line(&mut self, line: &str) {
        match from_line::<GuestFrame>(line) {
            Ok(GuestFrame::KvGet { key }) => {
                let value = if self.kv_provided {
                    self.kv.get(&key)
                } else {
                    None
                };
                self.wire
                    .push(&to_line(&HostFrame::KvValue { value }).expect("a kv_value serializes"));
            }
            Ok(frame) => {
                if let Some(answer) = frame.into_outcome() {
                    let mut slot = self.outcome.lock().unwrap_or_else(PoisonError::into_inner);
                    if slot.is_none() {
                        *slot = Some(answer);
                    }
                }
            }
            Err(err) => panic!("the native lane emitted a malformed frame: {err}\n{line}"),
        }
    }
}

impl Write for GuestToHost {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for byte in buf {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.partial);
                self.on_line(&String::from_utf8_lossy(&line));
            } else {
                self.partial.push(*byte);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run one request through the native edge lane.
fn run_native(
    request: &EdgeRequest,
    capabilities: &[EdgeCapability],
    kv: &Arc<dyn EdgeKv>,
) -> EdgeOutcome {
    let wire = Wire::default();
    wire.push(
        &to_line(&request.clone().into_host_frame(capabilities)).expect("a request serializes"),
    );

    let outcome = Arc::new(Mutex::new(None));
    let writer = GuestToHost {
        partial: Vec::new(),
        wire: wire.clone(),
        kv: Arc::clone(kv),
        kv_provided: capabilities.contains(&EdgeCapability::Kv),
        outcome: Arc::clone(&outcome),
    };

    serve_io(
        edge_greeting::handlers::edge_routes(),
        BufReader::new(HostToGuest(wire)),
        writer,
    )
    .expect("the in-memory transport never fails");

    let answer = outcome
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    answer.expect("the guest loop answers every request before EOF")
}

/// Run one request through the native edge lane, catching the unwind a
/// panicking handler produces.
fn run_native_catching_panics(
    request: &EdgeRequest,
    capabilities: &[EdgeCapability],
    kv: &Arc<dyn EdgeKv>,
) -> Option<EdgeOutcome> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result =
        std::panic::catch_unwind(AssertUnwindSafe(|| run_native(request, capabilities, kv)));
    std::panic::set_hook(previous);
    result.ok()
}

// ── lane 2: the wasm edge lane ───────────────────────────────────────

fn run_wasm(
    request: &EdgeRequest,
    capabilities: &[EdgeCapability],
    kv: &Arc<dyn EdgeKv>,
) -> EdgeOutcome {
    artifact()
        .run(request, capabilities, kv.as_ref())
        .expect("the reference host sets up cleanly")
}

// ── assertions ───────────────────────────────────────────────────────

#[track_caller]
fn assert_expectation(case: &ConformanceCase, lane: &str, outcome: &EdgeOutcome) {
    match (case.expect, outcome) {
        (Expectation::Served, EdgeOutcome::Served(_)) => {}
        (Expectation::Fallthrough(expected), EdgeOutcome::Fallthrough { reason, detail }) => {
            assert_eq!(
                *reason, expected,
                "[{}] {lane}: expected a `{expected}` fallthrough, got `{reason}` ({detail})",
                case.name
            );
        }
        (Expectation::Served, EdgeOutcome::Fallthrough { reason, detail }) => panic!(
            "[{}] {lane}: expected the edge to serve {}, but it declined with `{reason}` ({detail})",
            case.name, case.uri
        ),
        (Expectation::Fallthrough(expected), EdgeOutcome::Served(response)) => panic!(
            "[{}] {lane}: expected a `{expected}` fallthrough for {}, but the edge served {} \
             with {} body bytes",
            case.name,
            case.uri,
            response.status,
            response.body.len()
        ),
    }
}

#[track_caller]
fn assert_reproduced(case: &ConformanceCase, origin: &EdgeResponse, edge: &EdgeResponse) {
    match compare(origin, edge) {
        Verdict::Reproduced => {}
        Verdict::Diverged { detail } => panic!("[{}] the edge diverged — {detail}", case.name),
    }
}

fn body_text(response: &EdgeResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

/// The comparable view of a response: status, projected headers, body bytes.
///
/// Used for the origin's self-equality check, because the origin is *supposed*
/// to differ from itself in the projected-away headers — it mints a fresh
/// `x-request-id` per request, and a determinism check that failed on that
/// would be checking the wrong thing.
fn comparable(response: &EdgeResponse) -> (u16, Vec<(String, String)>, &[u8]) {
    (
        response.status,
        project_headers(&response.headers),
        &response.body,
    )
}

// ── Tier A: native edge lane vs wasm edge lane ───────────────────────

#[test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
fn tier_a_the_capsule_reproduces_the_native_edge_lane_byte_for_byte() {
    let kv = edge_greeting::demo_kv();
    println!("\n  Tier A — native edge lane vs wasm capsule\n");

    for case in CORPUS {
        let request = case.request();

        if case.uri == TRAPPING_URI {
            // A handler that panics has no answer to reproduce. What parity
            // means here is that neither lane invents one: the native lane
            // unwinds, and the wasm lane's trap becomes a fallthrough.
            assert!(
                run_native_catching_panics(&request, case.provided_capabilities, &kv).is_none(),
                "[{}] the native lane was expected to unwind",
                case.name
            );
            let edge = run_wasm(&request, case.provided_capabilities, &kv);
            assert_expectation(case, "wasm", &edge);
            println!("    {:<52} trap → {:?}", case.name, case.expect);
            continue;
        }

        // Determinism first: a lane that disagrees with itself would make any
        // cross-lane verdict meaningless.
        let native = run_native(&request, case.provided_capabilities, &kv);
        assert_eq!(
            native,
            run_native(&request, case.provided_capabilities, &kv),
            "[{}] the native lane is not deterministic",
            case.name
        );
        let edge = run_wasm(&request, case.provided_capabilities, &kv);
        assert_eq!(
            edge,
            run_wasm(&request, case.provided_capabilities, &kv),
            "[{}] the capsule is not deterministic across fresh instantiations",
            case.name
        );

        assert_expectation(case, "native", &native);
        assert_expectation(case, "wasm", &edge);

        if let (EdgeOutcome::Served(native), EdgeOutcome::Served(edge)) = (&native, &edge) {
            assert_reproduced(case, native, edge);
            // `compare` projects headers. Between two edge lanes nothing needs
            // excusing, so require the raw frames to be identical too.
            assert_eq!(
                native, edge,
                "[{}] the projected comparison passed but the raw responses differ",
                case.name
            );
            println!(
                "    {:<52} served {} ({} bytes)",
                case.name,
                edge.status,
                edge.body.len()
            );
        } else {
            println!("    {:<52} {:?}", case.name, case.expect);
        }
    }
}

// ── Tier B: the origin vs the wasm edge lane ─────────────────────────

/// The origin app: the same routes `src/main.rs` mounts, with the same KV
/// behind the same seam, driven through the full middleware stack.
///
/// The route list is spelled out again rather than shared with `main.rs`
/// because a `fn main` is not reachable from a test binary. `with_edge_kv` is
/// one line — `self.layer(EdgeCache::layer(kv))` — so the layer call below
/// wires exactly what the binary wires.
fn origin() -> autumn_web::test::TestClient {
    autumn_web::test::TestApp::new()
        .routes(autumn_web::routes![
            edge_greeting::handlers::greet,
            edge_greeting::handlers::note,
            edge_greeting::handlers::stats,
            edge_greeting::handlers::count,
            edge_greeting::handlers::whoami,
            edge_greeting::handlers::boom,
            edge_greeting::origin::feedback,
        ])
        .layer(autumn_edge::EdgeCache::layer(edge_greeting::demo_kv()))
        .with_entropy(autumn_web::entropy::SeededEntropy::new(0x1790))
        .build()
}

/// Drive one corpus case through the origin.
///
/// `accept-encoding: identity` is the one addition: the origin compresses and
/// the edge lane does not, and a gzip frame is not a divergence in the handler
/// — it is a different question being asked.
async fn origin_response(
    client: &autumn_web::test::TestClient,
    case: &ConformanceCase,
) -> EdgeResponse {
    let mut request = if case.method == "POST" {
        client.post(case.uri)
    } else {
        client.get(case.uri)
    };
    request = request.header("accept-encoding", "identity");
    for (name, value) in case.headers {
        request = request.header(name, value);
    }

    // The origin's panic *is* the expected behaviour for `/boom` (the reporting
    // layer turns it into a 500), so its unwind message is noise here, not
    // news. The hook is process-global, which is why this suite runs with
    // `--test-threads=1`.
    let previous = (case.uri == TRAPPING_URI).then(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        previous
    });
    let response = request.send().await;
    if let Some(previous) = previous {
        std::panic::set_hook(previous);
    }

    EdgeResponse {
        status: response.status.as_u16(),
        headers: response.headers.clone(),
        body: response.body.clone(),
    }
}

#[tokio::test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
async fn tier_b_the_origin_agrees_with_the_capsule_and_answers_every_decline() {
    let kv = edge_greeting::demo_kv();
    let client = origin();
    println!("\n  Tier B — origin app vs wasm capsule\n");

    for case in CORPUS {
        let request = case.request();
        let origin = origin_response(&client, case).await;
        let again = origin_response(&client, case).await;

        match case.expect {
            Expectation::Served => {
                assert_eq!(
                    comparable(&origin),
                    comparable(&again),
                    "[{}] the origin is not deterministic",
                    case.name
                );

                let EdgeOutcome::Served(edge) = run_wasm(&request, case.provided_capabilities, &kv)
                else {
                    panic!("[{}] the edge declined a case it should serve", case.name);
                };

                // Status, body and headers in both directions. Only the
                // headers the host sets (`SECURITY_HEADERS`) are excused.
                if let Verdict::Diverged { detail } = compare_capsule(&origin, &edge) {
                    panic!("[{}] origin and capsule diverged — {detail}", case.name);
                }

                println!(
                    "    {:<52} both {} ({} bytes)",
                    case.name,
                    edge.status,
                    edge.body.len()
                );
            }
            Expectation::Fallthrough(reason) => {
                // Only the status is asserted to be stable here. The origin's
                // own error documents embed the request id in the *body* (RFC
                // 9457 `request_id`), so a declined request is not byte-stable
                // at the origin — and it does not need to be: nothing is being
                // reproduced, the origin is simply answering. What must hold is
                // that it answers, with its canonical status.
                assert_eq!(
                    origin.status, again.status,
                    "[{}] the origin answered a declined request inconsistently",
                    case.name
                );

                let outcome = run_wasm(&request, case.provided_capabilities, &kv);
                assert_expectation(case, "wasm", &outcome);

                let expected = origin_status_for_fallthrough(case.uri);
                assert_eq!(
                    origin.status,
                    expected,
                    "[{}] the origin must answer a declined request with {expected}, got {} \
                     (body: {})",
                    case.name,
                    origin.status,
                    body_text(&origin)
                );

                println!(
                    "    {:<52} edge {reason} → origin {}",
                    case.name, origin.status
                );
            }
        }
    }
}

// ── the sandbox ──────────────────────────────────────────────────────

/// Imports a capsule is allowed to declare.
///
/// A superset of what a capsule actually needs: `wasi-libc`'s startup imports
/// vary with the toolchain, and a shim that answers one more inert call is not
/// a widened sandbox. What matters is what is *absent*.
const IMPORT_ALLOWLIST: &[&str] = &[
    "wasi_snapshot_preview1::args_get",
    "wasi_snapshot_preview1::args_sizes_get",
    "wasi_snapshot_preview1::clock_time_get",
    "wasi_snapshot_preview1::environ_get",
    "wasi_snapshot_preview1::environ_sizes_get",
    "wasi_snapshot_preview1::fd_close",
    "wasi_snapshot_preview1::fd_fdstat_get",
    "wasi_snapshot_preview1::fd_read",
    "wasi_snapshot_preview1::fd_seek",
    "wasi_snapshot_preview1::fd_write",
    "wasi_snapshot_preview1::proc_exit",
    "wasi_snapshot_preview1::random_get",
    "wasi_snapshot_preview1::sched_yield",
];

/// Import name fragments that would mean the capsule can reach the world.
const FORBIDDEN_IMPORT_FRAGMENTS: &[&str] = &["path_", "fd_prestat", "sock_", "poll_oneoff"];

#[test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
fn the_capsule_imports_nothing_that_could_reach_a_filesystem_or_a_socket() {
    let imports = artifact().imports();
    println!("\n  capsule imports: {imports:?}\n");

    assert!(
        !imports.is_empty(),
        "a capsule that imports nothing cannot be speaking the dialogue"
    );

    for import in &imports {
        assert!(
            IMPORT_ALLOWLIST.contains(&import.as_str()),
            "`{import}` is not in the edge sandbox allowlist. If a toolchain \
             update added a genuinely inert import, add it here *and* to the \
             shim in autumn-edge/src/host.rs — deliberately, not reflexively."
        );
        for fragment in FORBIDDEN_IMPORT_FRAGMENTS {
            assert!(
                !import.contains(fragment),
                "`{import}` would give the capsule ambient authority the edge \
                 lane promises it does not have"
            );
        }
    }
}

// ── the corpus itself ────────────────────────────────────────────────

#[test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
fn the_corpus_exercises_every_fallthrough_reason() {
    for reason in [
        FallthroughReason::UnknownRoute,
        FallthroughReason::MethodNotEdgeEligible,
        FallthroughReason::MissingCapability,
        FallthroughReason::CapsuleError,
    ] {
        assert!(
            CORPUS
                .iter()
                .any(|case| case.expect == Expectation::Fallthrough(reason)),
            "no corpus case produces a `{reason}` fallthrough"
        );
    }
    assert!(
        CORPUS.iter().any(|case| case.expect == Expectation::Served),
        "a corpus with nothing served proves nothing about byte-identity"
    );
}

// ── Tier C: the gateway in front of the origin (AC-3) ────────────────

/// What the origin returned behind the gateway: status, headers, body.
type Recorded = Arc<Mutex<Vec<EdgeResponse>>>;

/// The origin `Router`, wrapped to record each response it gives.
///
/// The gateway must return that response unchanged. Recording it at the
/// origin is what lets the test prove "unchanged" byte for byte.
fn recording_origin(
    router: &axum::Router,
    recorded: &Recorded,
) -> impl tower::Service<
    http::Request<axum::body::Body>,
    Response = http::Response<axum::body::Body>,
    Error = std::convert::Infallible,
    Future = impl Send,
> + Clone
+ Send
+ 'static {
    let router = router.clone();
    let recorded = Arc::clone(recorded);
    tower::service_fn(move |request: http::Request<axum::body::Body>| {
        let router = router.clone();
        let recorded = Arc::clone(&recorded);
        async move {
            let response = tower::ServiceExt::oneshot(router, request)
                .await
                .unwrap_or_else(|never| match never {});
            let (parts, bytes) = buffer(response).await;
            recorded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bytes.clone());
            Ok(http::Response::from_parts(
                parts,
                axum::body::Body::from(bytes.body),
            ))
        }
    })
}

/// Read a response into an [`EdgeResponse`], keeping its parts.
async fn buffer(
    response: http::Response<axum::body::Body>,
) -> (http::response::Parts, EdgeResponse) {
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("an in-memory body reads");
    let headers = parts
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let response = EdgeResponse {
        status: parts.status.as_u16(),
        headers,
        body: body.to_vec(),
    };
    (parts, response)
}

/// The HTTP request a case describes. `accept-encoding: identity` for the
/// same reason as [`origin_response`].
fn http_request(
    method: &str,
    uri: &str,
    headers: &[(String, String)],
) -> http::Request<axum::body::Body> {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept-encoding", "identity");
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(axum::body::Body::empty())
        .expect("a generated request is valid HTTP")
}

/// The origin router, and the security headers it sets on every response.
struct Origin {
    router: axum::Router,
    security_headers: Vec<(http::HeaderName, http::HeaderValue)>,
}

impl Origin {
    /// Build the origin. Read its security headers from one response, as an
    /// operator copies them into the CDN configuration.
    async fn new() -> Self {
        let router = origin().into_router();
        let response =
            tower::ServiceExt::oneshot(router.clone(), http_request("GET", "/stats/count", &[]))
                .await
                .unwrap_or_else(|never| match never {});
        let security_headers: Vec<_> = response
            .headers()
            .iter()
            .filter(|(name, _)| SECURITY_HEADERS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        assert!(
            !security_headers.is_empty(),
            "the origin sets security headers; the gateway must copy them"
        );
        Self {
            router,
            security_headers,
        }
    }
}

/// The gateway the guide describes: the real capsule, the real origin.
fn gateway(
    origin: &Origin,
    recorded: &Recorded,
    capabilities: &[EdgeCapability],
) -> EdgeGateway<
    impl tower::Service<
        http::Request<axum::body::Body>,
        Response = http::Response<axum::body::Body>,
        Error = std::convert::Infallible,
        Future = impl Send,
    > + Clone
    + Send
    + 'static,
> {
    let gateway = EdgeGateway::new(
        Arc::clone(artifact()),
        recording_origin(&origin.router, recorded),
    )
    .with_response_headers(origin.security_headers.clone());
    if capabilities.contains(&EdgeCapability::Kv) {
        gateway.with_kv(edge_greeting::demo_kv())
    } else {
        gateway
    }
}

/// Check one request through the gateway.
///
/// - Edge lane: the client gets what the origin would send (`direct`), with
///   only [`VOLATILE_HEADERS`](autumn_edge::conformance::VOLATILE_HEADERS)
///   excused. The origin is not asked.
/// - Fallthrough: the gateway returns, unchanged, the response the origin
///   gave it — and the origin was asked exactly once.
async fn check_gateway(
    case: &Generated,
    origin: &Origin,
    edge: &EdgeOutcome,
    direct: Option<&EdgeResponse>,
) -> Result<Lane, String> {
    let recorded = Recorded::default();
    let name = &case.name;
    let response = gateway(origin, &recorded, case.capabilities)
        .handle(http_request(case.method, &case.uri, &case.headers))
        .await;
    let lane = *response
        .extensions()
        .get::<Lane>()
        .ok_or("no lane recorded")?;
    let (_, got) = buffer(response).await;
    let origin_calls =
        std::mem::take(&mut *recorded.lock().unwrap_or_else(PoisonError::into_inner));

    match (edge, lane) {
        (EdgeOutcome::Served(edge), Lane::Edge) => {
            if !origin_calls.is_empty() {
                return Err(format!(
                    "[{name}] the edge served it, but the origin was asked too"
                ));
            }
            if got.body != edge.body {
                return Err(format!("[{name}] the gateway changed the edge body"));
            }
            let direct = direct.ok_or_else(|| format!("[{name}] no origin answer to compare"))?;
            if let Verdict::Diverged { detail } = compare(direct, &got) {
                return Err(format!(
                    "[{name}] the client got different bytes from the edge — {detail}"
                ));
            }
        }
        (EdgeOutcome::Fallthrough { reason, .. }, Lane::Fallthrough(lane_reason))
            if *reason == lane_reason =>
        {
            let [origin] = origin_calls.as_slice() else {
                return Err(format!(
                    "[{name}] a `{reason}` fallthrough asked the origin {} time(s), not once",
                    origin_calls.len()
                ));
            };
            if got != *origin {
                return Err(format!(
                    "[{name}] the gateway changed the origin response (origin {}, gateway {})",
                    origin.status, got.status
                ));
            }
        }
        (edge, lane) => {
            return Err(format!(
                "[{name}] the capsule answered {:?} but the gateway used lane {lane:?}",
                edge.fallthrough_reason()
            ));
        }
    }
    Ok(lane)
}

#[tokio::test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
async fn tier_c_the_gateway_serves_from_the_edge_or_forwards_to_the_origin_unchanged() {
    let kv = edge_greeting::demo_kv();
    let origin = Origin::new().await;
    println!("\n  Tier C — gateway (capsule + origin) vs the lanes behind it\n");

    for case in CORPUS {
        let edge = run_wasm(&case.request(), case.provided_capabilities, &kv);
        // The origin's panic for `/boom` is expected; silence its report.
        let previous = (case.uri == TRAPPING_URI).then(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            previous
        });
        let case_request = Generated::from(case);
        let direct = match edge {
            EdgeOutcome::Served(_) => Some(origin_answer(&origin.router, &case_request).await),
            EdgeOutcome::Fallthrough { .. } => None,
        };
        let lane = check_gateway(&case_request, &origin, &edge, direct.as_ref()).await;
        if let Some(previous) = previous {
            std::panic::set_hook(previous);
        }
        let lane = lane.unwrap_or_else(|failure| panic!("{failure}"));

        let expected = match case.expect {
            Expectation::Served => Lane::Edge,
            Expectation::Fallthrough(reason) => Lane::Fallthrough(reason),
        };
        assert_eq!(lane, expected, "[{}] wrong lane", case.name);
        println!("    {:<52} {lane:?}", case.name);
    }
}

// ── Tier D: a generated corpus (the >= 10k success metric) ───────────

/// How many generated requests one run drives through every lane.
const GENERATED_REQUESTS: usize = 10_000;

/// Fixed, so a failure replays. Print it on failure; change it to explore.
const GENERATOR_SEED: u64 = 0x1790_0000_0000_0001;

/// `splitmix64`: small, fast, and the same on every platform.
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`.
    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bound fits u64"))
            .expect("a value below a usize bound fits usize")
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// `true` with probability `percent / 100`.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// One path segment or query value: plain, reserved, and percent-encoded
    /// characters, including multi-byte UTF-8 and invalid UTF-8.
    fn segment(&mut self) -> String {
        const PIECES: &[&str] = &[
            "a",
            "b",
            "z",
            "A",
            "Q",
            "0",
            "7",
            "-",
            "_",
            "~",
            ".",
            "!",
            "$",
            "'",
            "(",
            ")",
            "*",
            "+",
            ",",
            ";",
            ":",
            "@",
            "%20",
            "%2F",
            "%25",
            "%3F",
            "%23",
            "%C3%A9",
            "%E2%9C%93",
            "%F0%9F%8D%82",
            "%FF",
            "%00",
        ];
        let len = 1 + self.below(10);
        let mut out: String = (0..len).map(|_| *self.pick(PIECES)).collect();
        // `.` and `..` are path syntax, not data.
        if out == "." || out == ".." {
            out.push('a');
        }
        out
    }

    fn query(&mut self) -> String {
        const KEYS: &[&str] = &["tag", "tag", "a", "b", "x%20y", "t%C3%A9"];
        let pairs = self.below(6);
        let mut parts = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            let key = *self.pick(KEYS);
            if self.chance(10) {
                parts.push(key.to_owned());
            } else {
                parts.push(format!("{key}={}", self.segment()));
            }
        }
        parts.join("&")
    }

    fn uri(&mut self) -> String {
        match self.below(9) {
            0 | 1 => format!("/greet/{}", self.segment()),
            2 => format!("/note/{}", self.pick(&["greeting", "release", "missing"])),
            3 => format!("/note/{}", self.segment()),
            4 => format!("/stats?{}", self.query()),
            5 => "/stats/count".to_owned(),
            6 => "/whoami".to_owned(),
            7 => format!("/greet/{}/", self.segment()),
            _ => format!("/{}", self.segment()),
        }
    }

    fn method(&mut self) -> &'static str {
        match self.below(20) {
            0..=13 => "GET",
            14 | 15 => "HEAD",
            16 => "POST",
            17 => "PUT",
            18 => "DELETE",
            _ => "PATCH",
        }
    }

    fn headers(&mut self) -> Vec<(String, String)> {
        const HEADERS: &[(&str, &str)] = &[
            ("accept", "text/html"),
            ("accept", "*/*"),
            ("accept-language", "fr-CA, en;q=0.8"),
            ("user-agent", "conformance/1"),
            ("x-forwarded-for", "203.0.113.7"),
            ("if-none-match", "\"abc\""),
            ("cookie", "session=super-secret"),
            ("authorization", "Bearer super-secret"),
            ("proxy-authorization", "Basic super-secret"),
        ];
        let count = self.below(4);
        (0..count)
            .map(|_| {
                let (name, value) = *self.pick(HEADERS);
                (name.to_owned(), value.to_owned())
            })
            .collect()
    }
}

/// One generated request.
struct Generated {
    name: String,
    method: &'static str,
    uri: String,
    headers: Vec<(String, String)>,
    capabilities: &'static [EdgeCapability],
}

impl From<&ConformanceCase> for Generated {
    fn from(case: &ConformanceCase) -> Self {
        Self {
            name: case.name.to_owned(),
            method: case.method,
            uri: case.uri.to_owned(),
            headers: case.request().headers,
            capabilities: case.provided_capabilities,
        }
    }
}

fn generate(seed: u64, count: usize) -> Vec<Generated> {
    let mut generator = Generator(seed);
    (0..count)
        .map(|index| {
            let method = generator.method();
            let uri = generator.uri();
            let headers = generator.headers();
            let capabilities: &'static [EdgeCapability] = if generator.chance(80) {
                &[EdgeCapability::Kv]
            } else {
                &[]
            };
            Generated {
                name: format!("#{index} {method} {uri}"),
                method,
                uri,
                headers,
                capabilities,
            }
        })
        .collect()
}

/// Send one request straight to the origin router.
async fn origin_answer(router: &axum::Router, case: &Generated) -> EdgeResponse {
    let request = http_request(case.method, &case.uri, &case.headers);
    let response = tower::ServiceExt::oneshot(router.clone(), request)
        .await
        .unwrap_or_else(|never| match never {});
    buffer(response).await.1
}

/// Every way one generated request can diverge. `Ok` is the lane it took.
async fn check_generated(
    case: &Generated,
    origin: &Origin,
    kv: &Arc<dyn EdgeKv>,
) -> Result<Lane, String> {
    let request = EdgeRequest {
        method: case.method.to_owned(),
        uri: case.uri.clone(),
        headers: case.headers.clone(),
        body: Vec::new(),
        identity: None,
    };

    // Tier A: native edge lane vs wasm capsule, raw and exact.
    let native = run_native(&request, case.capabilities, kv);
    let edge = run_wasm(&request, case.capabilities, kv);
    if native != edge {
        return Err(format!(
            "[{}] native and wasm lanes differ:\n  native {native:?}\n  wasm   {edge:?}",
            case.name
        ));
    }

    // The decline reason is the one the rules give.
    let write = !matches!(case.method, "GET" | "HEAD");
    if let EdgeOutcome::Fallthrough { reason, detail } = &edge {
        let expected_reason = write || *reason != FallthroughReason::MethodNotEdgeEligible;
        let no_capsule_error = *reason != FallthroughReason::CapsuleError;
        if !expected_reason || !no_capsule_error {
            return Err(format!("[{}] unexpected `{reason}`: {detail}", case.name));
        }
    } else if write {
        return Err(format!("[{}] the edge served a write", case.name));
    }

    // Tier B: the origin agrees with every response the edge serves, in both
    // directions. Only the headers the host sets are excused.
    let direct = match &edge {
        EdgeOutcome::Served(served) => {
            let direct = origin_answer(&origin.router, case).await;
            if let Verdict::Diverged { detail } = compare_capsule(&direct, served) {
                return Err(format!(
                    "[{}] origin and capsule diverged — {detail}",
                    case.name
                ));
            }
            Some(direct)
        }
        EdgeOutcome::Fallthrough { .. } => None,
    };

    // Tier C: the gateway takes the lane the capsule chose, and the client gets
    // the origin's bytes either way.
    check_gateway(case, origin, &edge, direct.as_ref()).await
}

#[tokio::test]
#[ignore = "requires wasm32-wasip1 target (edge-conformance CI job)"]
async fn tier_d_a_generated_corpus_shows_zero_divergence_across_every_lane() {
    let kv = edge_greeting::demo_kv();
    let origin = Origin::new().await;
    let corpus = generate(GENERATOR_SEED, GENERATED_REQUESTS);
    assert!(
        corpus.len() >= 10_000,
        "the success metric is >= 10k conformance-tested requests"
    );

    let started = std::time::Instant::now();
    let mut lanes: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut failures = Vec::new();
    for case in &corpus {
        match check_generated(case, &origin, &kv).await {
            Ok(lane) => *lanes.entry(format!("{lane:?}")).or_default() += 1,
            Err(failure) => failures.push(failure),
        }
    }

    println!(
        "\n  Tier D — {} generated requests (seed {GENERATOR_SEED:#x}) in {:.1?}\n",
        corpus.len(),
        started.elapsed()
    );
    for (lane, count) in &lanes {
        println!("    {lane:<52} {count}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} generated requests diverged (seed {GENERATOR_SEED:#x}). First 20:\n{}",
        failures.len(),
        corpus.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        lanes.get("Edge").copied().unwrap_or_default() >= corpus.len() / 3,
        "too few requests reached the edge lane to prove byte-identity: {lanes:?}"
    );
}
