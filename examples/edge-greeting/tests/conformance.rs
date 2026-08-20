//! Origin/edge conformance: the proof behind AC-2 of issue #1790.
//!
//! Every test here builds a **real** `wasm32-wasip1` capsule from this
//! example's own sources and runs a shared request corpus through three lanes:
//!
//! | Lane | What runs | Driven by |
//! | --- | --- | --- |
//! | Native edge | `autumn_edge::serve_io` — the capsule's `main`, compiled for the host | the in-process host below |
//! | Wasm edge | the same code, compiled to `wasm32-wasip1` | `autumn_edge::host::EdgeArtifact` (wasmi) |
//! | Origin | the whole app: `TestApp` + the full middleware stack | `autumn_web::test` |
//!
//! **Tier A** (native edge vs wasm edge) is byte-exact: same status, same
//! headers, same body bytes, and the same fallthrough reason when the edge
//! declines. Nothing is excused — if these two disagree, compiling to wasm
//! changed the answer.
//!
//! **Tier B** (origin vs wasm edge) is where the projection earns its keep. The
//! origin stamps a `Date`, a request id and server-timing spans that the edge
//! lane structurally cannot emit, so headers are compared after
//! [`project_headers`]: every header the edge *did* emit must be present at the
//! origin with an identical value, and the body bytes must match exactly. For
//! the cases the edge declines, the origin is asked the same question and has
//! to answer it — that is what makes fallthrough transparent rather than a
//! 404 with extra steps.
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

use autumn_edge::conformance::{ConformanceCase, Expectation, Verdict, compare, project_headers};
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
fn artifact() -> &'static EdgeArtifact {
    static ARTIFACT: OnceLock<EdgeArtifact> = OnceLock::new();
    ARTIFACT.get_or_init(|| {
        let wasm = build_capsule();
        EdgeArtifact::from_bytes(&wasm).expect("the built artifact is a valid wasm module")
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

                assert_eq!(
                    origin.status, edge.status,
                    "[{}] origin answered {} and the edge answered {}",
                    case.name, origin.status, edge.status
                );
                assert_eq!(
                    body_text(&origin),
                    body_text(&edge),
                    "[{}] origin and edge bodies differ",
                    case.name
                );
                assert_eq!(
                    origin.body, edge.body,
                    "[{}] origin and edge body bytes differ",
                    case.name
                );

                // Every header the edge emitted must be present at the origin
                // with the same value. The origin may add more (it has a
                // middleware stack); it may not contradict.
                let origin_headers = project_headers(&origin.headers);
                for (name, value) in project_headers(&edge.headers) {
                    assert!(
                        origin_headers.contains(&(name.clone(), value.clone())),
                        "[{}] the edge emitted `{name}: {value}` and the origin did not \
                         (origin headers: {origin_headers:?})",
                        case.name
                    );
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
