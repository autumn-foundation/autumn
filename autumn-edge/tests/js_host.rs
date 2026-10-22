//! The JS host (`js/autumn-edge-host.mjs`) against the reference host.
//!
//! Every guest here is hand-written WAT. Each one runs through
//! [`EdgeArtifact`] (wasmi) and through the JS host under Node — both its
//! synchronous path and its replay-on-miss path — and the outcomes must be
//! equal. Where the two hosts word a host-side failure differently (a trap's
//! message comes from the engine), the test compares the reason and the
//! part each host controls instead.
//!
//! The suite also runs the JS host's own `node --test` gateway tests, so
//! `cargo test` is the one command for both languages.
//!
//! Needs `node` (18+) on `PATH`, or `NODE` naming it. Without one the tests
//! say so and pass locally; under `CI` they fail, so the lane cannot go
//! quietly dark.
#![cfg(feature = "host")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_edge::host::EdgeArtifact;
use autumn_edge::kv::InMemoryEdgeKv;
use autumn_edge::wire::{EdgeOutcome, EdgeRequest, FallthroughReason, GuestFrame, encode_body};
use autumn_edge::{EdgeCapability, EdgeKv};

// ── node ─────────────────────────────────────────────────────────────

fn node() -> Option<String> {
    let binary = std::env::var("NODE").unwrap_or_else(|_| "node".to_owned());
    let found = Command::new(&binary)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if found {
        return Some(binary);
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "`{binary}` was not found: the JS host suite needs Node 18+ in CI"
    );
    eprintln!("skipping: `{binary}` is not on PATH (set NODE to run the JS host suite)");
    None
}

fn js_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("js")
}

fn scratch() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "autumn-edge-js-host-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

// ── running both hosts ───────────────────────────────────────────────

struct Case {
    request: EdgeRequest,
    capabilities: Vec<EdgeCapability>,
}

impl Case {
    fn get(uri: &str) -> Self {
        Self {
            request: EdgeRequest::get(uri).with_header("accept", "text/plain"),
            capabilities: Vec::new(),
        }
    }

    fn with_kv(mut self) -> Self {
        self.capabilities.push(EdgeCapability::Kv);
        self
    }
}

#[derive(Debug)]
struct JsOutcome {
    sync: EdgeOutcome,
    replay: EdgeOutcome,
    kv_reads: Vec<String>,
}

fn kv_entries() -> Vec<(&'static str, &'static str)> {
    vec![("a", "alpha"), ("b", "beta \u{1F342}")]
}

fn reference_kv() -> InMemoryEdgeKv {
    kv_entries()
        .into_iter()
        .fold(InMemoryEdgeKv::new(), |kv, (key, value)| {
            kv.with(key, value)
        })
}

fn outcome_of(frame: &serde_json::Value) -> EdgeOutcome {
    serde_json::from_value::<GuestFrame>(frame.clone())
        .expect("the driver prints guest frames")
        .into_outcome()
        .expect("the driver prints terminal frames")
}

/// Run every case through the JS host. `None` when Node is missing locally.
fn run_js(wasm: &[u8], cases: &[Case]) -> Option<Vec<JsOutcome>> {
    let node = node()?;
    let dir = scratch();
    let wasm_path = dir.join("capsule.wasm");
    std::fs::write(&wasm_path, wasm).expect("write the capsule");
    let kv: serde_json::Map<String, serde_json::Value> = kv_entries()
        .into_iter()
        .map(|(key, value)| (key.to_owned(), encode_body(value.as_bytes()).into()))
        .collect();
    let job = serde_json::json!({
        "wasm": wasm_path,
        "kv": kv,
        "cases": cases.iter().map(|case| serde_json::json!({
            "method": case.request.method,
            "uri": case.request.uri,
            "headers": case.request.headers,
            "capabilities": case.capabilities,
            "identity": case.request.identity,
        })).collect::<Vec<_>>(),
    });
    let job_path = dir.join("job.json");
    std::fs::write(&job_path, job.to_string()).expect("write the job");

    let output = Command::new(node)
        .arg(js_dir().join("conformance-driver.mjs"))
        .arg(&job_path)
        .output()
        .expect("run the JS conformance driver");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "the JS driver failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("the driver prints UTF-8");
    let outcomes: Vec<JsOutcome> = stdout
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("one JSON object");
            JsOutcome {
                sync: outcome_of(&value["sync"]),
                replay: outcome_of(&value["replay"]),
                kv_reads: serde_json::from_value(value["kv_reads"].clone())
                    .expect("kv_reads is a list of keys"),
            }
        })
        .collect();
    assert_eq!(outcomes.len(), cases.len(), "one outcome per case");
    Some(outcomes)
}

fn run_reference(wasm: &[u8], case: &Case) -> EdgeOutcome {
    let artifact = EdgeArtifact::from_bytes(wasm).expect("a valid module");
    let kv = reference_kv();
    artifact
        .run(&case.request, &case.capabilities, &kv as &dyn EdgeKv)
        .expect("host-side setup never fails for these guests")
}

/// Both JS paths must equal the reference host exactly.
fn assert_identical(wasm: &[u8], cases: &[Case]) -> Option<Vec<JsOutcome>> {
    let js = run_js(wasm, cases)?;
    for (case, js) in cases.iter().zip(&js) {
        let reference = run_reference(wasm, case);
        assert_eq!(
            js.sync, reference,
            "sync JS host vs wasmi for {}",
            case.request.uri
        );
        assert_eq!(
            js.replay, reference,
            "replay JS host vs wasmi for {}",
            case.request.uri
        );
    }
    Some(js)
}

/// For host-side failures: same reason, and the same text after `marker`
/// (the part of the detail the host writes, not the engine).
#[track_caller]
fn assert_same_failure(reference: &EdgeOutcome, js: &EdgeOutcome, marker: &str) {
    let (
        EdgeOutcome::Fallthrough {
            reason: left,
            detail: left_detail,
        },
        EdgeOutcome::Fallthrough {
            reason: right,
            detail: right_detail,
        },
    ) = (reference, js)
    else {
        panic!("expected two fallthroughs, got {reference:?} and {js:?}");
    };
    assert_eq!(left, right, "{left_detail} vs {right_detail}");
    let tail = |detail: &str| detail.split_once(marker).map(|(_, tail)| tail.to_owned());
    assert_eq!(
        tail(left_detail),
        tail(right_detail),
        "the host-written part differs:\n  wasmi: {left_detail}\n  js:    {right_detail}"
    );
    assert!(
        tail(left_detail).is_some(),
        "`{marker}` is not in {left_detail}"
    );
}

// ── WAT guests ───────────────────────────────────────────────────────

/// What a scripted guest does, in order.
enum Op {
    /// Read everything stdin holds into the buffer.
    Read,
    /// Write these bytes to fd 1.
    Out(&'static str),
    /// Write the bytes the last `Read` got to fd 2.
    EchoToStderr,
    /// Write these bytes to fd 2.
    Err(&'static str),
    /// Trap.
    Trap,
}

fn wat_bytes(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "\\{byte:02x}");
        out
    })
}

const BUFFER: u32 = 1024;
const BUFFER_LEN: u32 = 32 * 1024;
const DATA: u32 = 65536;

/// A guest that runs `ops`. Memory: iovec at 0, counts at 8 and 24, the
/// read buffer at 1024, constant strings from 64 KiB.
fn scripted(ops: &[Op]) -> Vec<u8> {
    let mut data = String::new();
    let mut body = String::new();
    let mut at = DATA;
    for op in ops {
        match op {
            Op::Read => body.push_str("(call $read)\n"),
            Op::Out(text) | Op::Err(text) => {
                let fd = if matches!(op, Op::Out(_)) { 1 } else { 2 };
                let _ = writeln!(
                    data,
                    "(data (i32.const {at}) \"{}\")",
                    wat_bytes(text.as_bytes())
                );
                let _ = writeln!(
                    body,
                    "(call $write (i32.const {fd}) (i32.const {at}) (i32.const {}))",
                    text.len()
                );
                at += u32::try_from(text.len()).expect("small") + 16;
            }
            Op::EchoToStderr => {
                let _ = writeln!(
                    body,
                    "(call $write (i32.const 2) (i32.const {BUFFER}) (i32.load (i32.const 24)))"
                );
            }
            Op::Trap => body.push_str("unreachable\n"),
        }
    }
    let wat = format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  {data}
  (func $write (param $fd i32) (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 8))))
  (func $read
    (i32.store (i32.const 16) (i32.const {BUFFER}))
    (i32.store (i32.const 20) (i32.const {BUFFER_LEN}))
    (drop (call $fd_read (i32.const 0) (i32.const 16) (i32.const 1) (i32.const 24))))
  (func (export "_start")
    {body}))"#
    );
    wat::parse_str(&wat).expect("valid WAT")
}

/// A guest that evaluates `probe` (an i32 expression, usually a WASI call's
/// errno), and answers 200 with `x-probe: <two decimal digits>`.
fn probe(imports: &str, probe: &str) -> Vec<u8> {
    let prefix = r#"{"op":"response","status":200,"headers":[["x-probe",""#;
    let suffix = "\"]],\"body_b64\":\"\"}\n";
    let wat = format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  {imports}
  (memory (export "memory") 2)
  (data (i32.const {DATA}) "{prefix}")
  (data (i32.const {suffix_at}) "{suffix}")
  (func $write (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))))
  (func (export "_start")
    (local $value i32)
    (local.set $value {probe})
    (i32.store8 (i32.const 900) (i32.add (i32.const 48) (i32.rem_u (i32.div_u (local.get $value) (i32.const 10)) (i32.const 10))))
    (i32.store8 (i32.const 901) (i32.add (i32.const 48) (i32.rem_u (local.get $value) (i32.const 10))))
    (call $write (i32.const {DATA}) (i32.const {prefix_len}))
    (call $write (i32.const 900) (i32.const 2))
    (call $write (i32.const {suffix_at}) (i32.const {suffix_len}))))"#,
        prefix = wat_bytes(prefix.as_bytes()),
        suffix = wat_bytes(suffix.as_bytes()),
        suffix_at = DATA + 1024,
        prefix_len = prefix.len(),
        suffix_len = suffix.len(),
    );
    wat::parse_str(&wat).expect("valid WAT")
}

const OK_RESPONSE: &str = "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain\"]],\"body_b64\":\"aGk=\"}\n";

// ── the dialogue ─────────────────────────────────────────────────────

#[test]
fn a_response_frame_is_the_same_outcome() {
    let wasm = scripted(&[Op::Read, Op::Out(OK_RESPONSE)]);
    assert_identical(&wasm, &[Case::get("/a"), Case::get("/b").with_kv()]);
}

#[test]
fn a_frame_split_across_writes_is_reassembled() {
    let wasm = scripted(&[
        Op::Out("{\"op\":\"respo"),
        Op::Out("nse\",\"status\":204,\"head"),
        Op::Out("ers\":[]}\n"),
    ]);
    assert_identical(&wasm, &[Case::get("/split")]);
}

#[test]
fn output_after_the_terminal_frame_is_ignored() {
    let wasm = scripted(&[
        Op::Out(OK_RESPONSE),
        Op::Out("not json at all\n"),
        Op::Out("{\"op\":\"fallthrough\",\"reason\":\"unknown_route\",\"detail\":\"late\"}\n"),
    ]);
    assert_identical(&wasm, &[Case::get("/late")]);
}

#[test]
fn a_guest_fallthrough_carries_its_reason_and_detail() {
    let wasm = scripted(&[Op::Out(
        "{\"op\":\"fallthrough\",\"reason\":\"missing_capability\",\"detail\":\"needs kv\"}\n",
    )]);
    assert_identical(&wasm, &[Case::get("/needs")]);
}

#[test]
fn optional_response_fields_default_the_same_way() {
    let wasm = scripted(&[Op::Out("{\"op\":\"response\",\"status\":200}\n")]);
    assert_identical(&wasm, &[Case::get("/bare")]);
}

#[test]
fn every_malformed_frame_is_a_capsule_error() {
    let lines: &[&'static str] = &[
        "not json\n",
        "[1,2]\n",
        "{\"op\":\"nope\"}\n",
        "{\"op\":\"response\",\"status\":70000}\n",
        "{\"op\":\"response\",\"status\":200.5}\n",
        "{\"op\":\"response\",\"status\":200,\"headers\":[[\"a\",\"b\",\"c\"]]}\n",
        "{\"op\":\"response\",\"status\":200,\"headers\":[[\"a\",1]]}\n",
        "{\"op\":\"response\",\"status\":200,\"body_b64\":\"aGk\"}\n",
        "{\"op\":\"response\",\"status\":200,\"body_b64\":\"a Gk=\"}\n",
        "{\"op\":\"fallthrough\",\"reason\":\"tired\",\"detail\":\"x\"}\n",
        "{\"op\":\"fallthrough\",\"reason\":\"unknown_route\"}\n",
        "{\"op\":\"kv_get\"}\n",
    ];
    for line in lines {
        let wasm = scripted(&[Op::Out(line)]);
        let case = Case::get("/malformed");
        let Some(js) = run_js(&wasm, std::slice::from_ref(&case)) else {
            return;
        };
        let reference = run_reference(&wasm, &case);
        // The parser's own message differs (serde vs `JSON.parse`); the
        // reason and the host's framing of it must not.
        for outcome in [&reference, &js[0].sync, &js[0].replay] {
            let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
                panic!("{line:?} must not be served: {outcome:?}");
            };
            assert_eq!(
                *reason,
                FallthroughReason::CapsuleError,
                "{line:?}: {detail}"
            );
            assert!(
                detail.starts_with("the capsule emitted a malformed frame: "),
                "{line:?}: {detail}"
            );
        }
    }
}

#[test]
fn a_capsule_that_never_answers_falls_through_identically() {
    assert_identical(&scripted(&[Op::Read]), &[Case::get("/silent")]);
    assert_identical(&scripted(&[Op::Err("  a note  ")]), &[Case::get("/note")]);
}

#[test]
fn a_trap_keeps_the_stderr_excerpt() {
    let wasm = scripted(&[Op::Err("boom\n"), Op::Trap]);
    let case = Case::get("/boom");
    let Some(js) = run_js(&wasm, std::slice::from_ref(&case)) else {
        return;
    };
    let reference = run_reference(&wasm, &case);
    assert_same_failure(&reference, &js[0].sync, "(stderr: ");
    assert_same_failure(&reference, &js[0].replay, "(stderr: ");
}

#[test]
fn the_request_frame_and_kv_values_are_the_same_bytes() {
    // The guest echoes what it reads to stderr, then traps: the stderr
    // excerpt in the fallthrough detail carries the exact frames each host
    // sent. Credentials must be gone, headers canonical, values identical.
    let wasm = scripted(&[
        Op::Read,
        Op::EchoToStderr,
        Op::Out("{\"op\":\"kv_get\",\"key\":\"b\"}\n"),
        Op::Read,
        Op::EchoToStderr,
        Op::Out("{\"op\":\"kv_get\",\"key\":\"missing\"}\n"),
        Op::Read,
        Op::EchoToStderr,
        Op::Trap,
    ]);
    let mut with_credentials = Case::get("/frame?q=1").with_kv();
    with_credentials.request = with_credentials
        .request
        .with_header("Cookie", "sid=1")
        .with_header("X-B", "2")
        .with_header("authorization", "Bearer x")
        .with_header("x-a", "1")
        .with_header("X-B", "1");
    let cases = [with_credentials, Case::get("/no-kv")];
    let Some(js) = run_js(&wasm, &cases) else {
        return;
    };
    for (case, js) in cases.iter().zip(&js) {
        let reference = run_reference(&wasm, case);
        assert_same_failure(&reference, &js.sync, "(stderr: ");
        assert_same_failure(&reference, &js.replay, "(stderr: ");
    }
    let EdgeOutcome::Fallthrough { detail, .. } = &js[0].sync else {
        panic!("a trap");
    };
    assert!(
        !detail.contains("sid=1") && !detail.contains("Bearer"),
        "{detail}"
    );
    assert!(
        detail.contains("\"value_b64\":null"),
        "a miss crosses as null: {detail}"
    );
}

#[test]
fn replay_on_miss_reads_each_key_once_and_only_with_the_capability() {
    let wasm = scripted(&[
        Op::Read,
        Op::Out("{\"op\":\"kv_get\",\"key\":\"a\"}\n"),
        Op::Read,
        Op::Out("{\"op\":\"kv_get\",\"key\":\"b\"}\n"),
        Op::Read,
        Op::Out("{\"op\":\"kv_get\",\"key\":\"a\"}\n"),
        Op::Read,
        Op::Out(OK_RESPONSE),
    ]);
    let Some(js) = assert_identical(&wasm, &[Case::get("/kv").with_kv(), Case::get("/plain")])
    else {
        return;
    };
    assert_eq!(js[0].kv_reads, ["a", "b"], "one read per distinct key");
    assert!(js[1].kv_reads.is_empty(), "no capability, no read");
}

#[test]
fn replay_stops_at_the_distinct_key_budget() {
    // 40 distinct keys: `k` followed by one byte (`0`, `1`, ... `W`) that
    // the loop increments.
    let wat = format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const {DATA}) "{{\"op\":\"kv_get\",\"key\":\"kA\"}}\n")
  (data (i32.const 70000) "{ok}")
  (func $write (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))))
  (func $read
    (i32.store (i32.const 16) (i32.const {BUFFER}))
    (i32.store (i32.const 20) (i32.const {BUFFER_LEN}))
    (drop (call $fd_read (i32.const 0) (i32.const 16) (i32.const 1) (i32.const 24))))
  (func (export "_start")
    (local $i i32)
    (call $read)
    (block $done (loop $next
      (br_if $done (i32.ge_u (local.get $i) (i32.const 40)))
      (i32.store8 (i32.const {key_byte}) (i32.add (i32.const 48) (local.get $i)))
      (call $write (i32.const {DATA}) (i32.const 27))
      (call $read)
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $next)))
    (call $write (i32.const 70000) (i32.const {ok_len}))))"#,
        key_byte = DATA + 23,
        ok = wat_bytes(OK_RESPONSE.as_bytes()),
        ok_len = OK_RESPONSE.len(),
    );
    let wasm = wat::parse_str(&wat).expect("valid WAT");
    let case = Case::get("/many").with_kv();
    let Some(js) = run_js(&wasm, std::slice::from_ref(&case)) else {
        return;
    };
    // The synchronous paths answer every key, as the reference host does.
    assert_eq!(js[0].sync, run_reference(&wasm, &case));
    assert!(js[0].sync.served().is_some());
    // Replay gives up past the budget: the origin serves the request.
    let EdgeOutcome::Fallthrough { reason, detail } = &js[0].replay else {
        panic!("replay should fall through, got {:?}", js[0].replay);
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert!(detail.contains("distinct kv keys"), "{detail}");
    assert_eq!(
        js[0].kv_reads.len(),
        32,
        "MAX_KV_ROUNDS reads, then the decline"
    );
}

// ── the WASI surface ─────────────────────────────────────────────────

const FD_WRITE_ONLY: &str = "";

#[test]
fn every_errno_matches_the_reference_shim() {
    let probes: &[(&str, &str)] = &[
        // fd_write: an iovec past the end of memory, then a bad fd.
        (
            FD_WRITE_ONLY,
            "(block (result i32) (i32.store (i32.const 600) (i32.const 0)) (i32.store (i32.const 604) (i32.const -16)) (call $fd_write (i32.const 1) (i32.const 600) (i32.const 1) (i32.const 620)))",
        ),
        (
            FD_WRITE_ONLY,
            "(call $fd_write (i32.const 3) (i32.const 600) (i32.const 0) (i32.const 620))",
        ),
        (
            FD_WRITE_ONLY,
            "(call $fd_write (i32.const 1) (i32.const -8) (i32.const 1) (i32.const 620))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))"#,
            "(call $fd_read (i32.const 1) (i32.const 600) (i32.const 1) (i32.const 620))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))"#,
            "(call $fd_read (i32.const 0) (i32.const 600) (i32.const 1) (i32.const 999999))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_seek" (func $fd_seek (param i32 i64 i32 i32) (result i32)))"#,
            "(call $fd_seek (i32.const 1) (i64.const 0) (i32.const 0) (i32.const 600))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_fdstat_get" (func $fdstat (param i32 i32) (result i32)))"#,
            "(call $fdstat (i32.const 5) (i32.const 600))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_fdstat_get" (func $fdstat (param i32 i32) (result i32)))"#,
            "(block (result i32) (drop (call $fdstat (i32.const 1) (i32.const 600))) (i32.load8_u (i32.const 600)))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_prestat_get" (func $prestat (param i32 i32) (result i32)))"#,
            "(call $prestat (i32.const 3) (i32.const 600))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "fd_prestat_dir_name" (func $dir (param i32 i32 i32) (result i32)))"#,
            "(call $dir (i32.const 3) (i32.const 600) (i32.const 10))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "random_get" (func $random (param i32 i32) (result i32)))"#,
            "(call $random (i32.const 131000) (i32.const 1000))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "clock_time_get" (func $clock (param i32 i64 i32) (result i32)))"#,
            "(block (result i32) (i64.store (i32.const 600) (i64.const -1)) (i32.add (call $clock (i32.const 0) (i64.const 1) (i32.const 600)) (i32.wrap_i64 (i64.load (i32.const 600)))))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "environ_sizes_get" (func $sizes (param i32 i32) (result i32)))"#,
            "(block (result i32) (i64.store (i32.const 600) (i64.const -1)) (i32.add (call $sizes (i32.const 600) (i32.const 604)) (i32.wrap_i64 (i64.load (i32.const 600)))))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "args_sizes_get" (func $sizes (param i32 i32) (result i32)))"#,
            "(call $sizes (i32.const 600) (i32.const -4))",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "sched_yield" (func $yield (result i32)))"#,
            "(i32.add (call $yield) (i32.const 7))",
        ),
    ];
    for (extra, expression) in probes {
        let wasm = probe(extra, expression);
        let Some(_) = assert_identical(&wasm, &[Case::get("/probe")]) else {
            return;
        };
    }
}

#[test]
fn entropy_is_the_reference_hosts_fixed_sequence() {
    // Two random_get calls (the state carries across them), hex-encoded into
    // a response header: the JS host must produce the same SplitMix64 bytes.
    let prefix = r#"{"op":"response","status":200,"headers":[["x-random",""#;
    let suffix = "\"]],\"body_b64\":\"\"}\n";
    let wat = format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "random_get" (func $random (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 3072) "0123456789abcdef")
  (data (i32.const {DATA}) "{prefix}")
  (data (i32.const {suffix_at}) "{suffix}")
  (func $write (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))))
  (func (export "_start")
    (local $i i32) (local $b i32)
    (drop (call $random (i32.const 2048) (i32.const 24)))
    (drop (call $random (i32.const 2072) (i32.const 8)))
    (block $done (loop $next
      (br_if $done (i32.ge_u (local.get $i) (i32.const 32)))
      (local.set $b (i32.load8_u (i32.add (i32.const 2048) (local.get $i))))
      (i32.store8 (i32.add (i32.const 4096) (i32.shl (local.get $i) (i32.const 1)))
        (i32.load8_u (i32.add (i32.const 3072) (i32.shr_u (local.get $b) (i32.const 4)))))
      (i32.store8 (i32.add (i32.const 4097) (i32.shl (local.get $i) (i32.const 1)))
        (i32.load8_u (i32.add (i32.const 3072) (i32.and (local.get $b) (i32.const 15)))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $next)))
    (call $write (i32.const {DATA}) (i32.const {prefix_len}))
    (call $write (i32.const 4096) (i32.const 64))
    (call $write (i32.const {suffix_at}) (i32.const {suffix_len}))))"#,
        prefix = wat_bytes(prefix.as_bytes()),
        suffix = wat_bytes(suffix.as_bytes()),
        suffix_at = DATA + 1024,
        prefix_len = prefix.len(),
        suffix_len = suffix.len(),
    );
    let wasm = wat::parse_str(&wat).expect("valid WAT");
    let Some(js) = assert_identical(&wasm, &[Case::get("/random")]) else {
        return;
    };
    let served = js[0].sync.served().expect("served");
    assert_eq!(served.headers[0].0, "x-random");
    assert_eq!(served.headers[0].1.len(), 64);
}

#[test]
fn a_denied_import_fails_to_link_and_names_itself() {
    for (name, signature) in [
        (
            "path_open",
            "(param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)",
        ),
        ("sock_accept", "(param i32 i32 i32) (result i32)"),
        ("poll_oneoff", "(param i32 i32 i32 i32) (result i32)"),
    ] {
        let wat = format!(
            r#"(module
  (import "wasi_snapshot_preview1" "{name}" (func {signature}))
  (memory (export "memory") 1)
  (func (export "_start")))"#
        );
        let wasm = wat::parse_str(&wat).expect("valid WAT");
        let case = Case::get("/denied");
        let Some(js) = run_js(&wasm, std::slice::from_ref(&case)) else {
            return;
        };
        let reference = run_reference(&wasm, &case);
        for outcome in [&reference, &js[0].sync, &js[0].replay] {
            let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
                panic!("{name} must not link: {outcome:?}");
            };
            assert_eq!(*reason, FallthroughReason::CapsuleError);
            assert!(detail.contains("could not be instantiated"), "{detail}");
            assert!(
                detail.contains(name),
                "the refusal names `{name}`: {detail}"
            );
        }
    }
}

#[test]
fn every_listed_import_links_in_both_hosts() {
    let signatures = [
        ("fd_read", "(param i32 i32 i32 i32) (result i32)"),
        ("fd_write", "(param i32 i32 i32 i32) (result i32)"),
        ("random_get", "(param i32 i32) (result i32)"),
        ("clock_time_get", "(param i32 i64 i32) (result i32)"),
        ("environ_get", "(param i32 i32) (result i32)"),
        ("environ_sizes_get", "(param i32 i32) (result i32)"),
        ("args_get", "(param i32 i32) (result i32)"),
        ("args_sizes_get", "(param i32 i32) (result i32)"),
        ("proc_exit", "(param i32)"),
        ("sched_yield", "(result i32)"),
        ("fd_close", "(param i32) (result i32)"),
        ("fd_seek", "(param i32 i64 i32 i32) (result i32)"),
        ("fd_fdstat_get", "(param i32 i32) (result i32)"),
        ("fd_prestat_get", "(param i32 i32) (result i32)"),
        ("fd_prestat_dir_name", "(param i32 i32 i32) (result i32)"),
    ];
    let listed: Vec<String> = signatures
        .iter()
        .map(|(name, _)| format!("wasi_snapshot_preview1::{name}"))
        .collect();
    assert_eq!(listed, autumn_edge::js::HOST_WASI_IMPORTS);
    let imports = signatures
        .iter()
        .fold(String::new(), |mut out, (name, signature)| {
            let _ = writeln!(
                out,
                "(import \"wasi_snapshot_preview1\" \"{name}\" (func {signature}))"
            );
            out
        });
    let wat = format!(
        r#"(module
  {imports}
  (memory (export "memory") 2)
  (data (i32.const 0) "{ok}")
  (func (export "_start")
    (i32.store (i32.const 1024) (i32.const 0))
    (i32.store (i32.const 1028) (i32.const {ok_len}))
    (drop (call 1 (i32.const 1) (i32.const 1024) (i32.const 1) (i32.const 1040)))))"#,
        ok = wat_bytes(OK_RESPONSE.as_bytes()),
        ok_len = OK_RESPONSE.len(),
    );
    let wasm = wat::parse_str(&wat).expect("valid WAT");
    let Some(js) = assert_identical(&wasm, &[Case::get("/all")]) else {
        return;
    };
    assert!(js[0].sync.served().is_some(), "{:?}", js[0].sync);
}

#[test]
fn a_module_without_start_is_refused_the_same_way() {
    let wasm = wat::parse_str(r#"(module (memory (export "memory") 1))"#).expect("valid WAT");
    assert_identical(&wasm, &[Case::get("/no-start")]);
}

#[test]
fn a_memory_hungry_module_is_refused() {
    // One page past the 256 MiB budget.
    let wasm =
        wat::parse_str(r#"(module (memory (export "memory") 4097) (func (export "_start")))"#)
            .expect("valid WAT");
    let case = Case::get("/hungry");
    let Some(js) = run_js(&wasm, std::slice::from_ref(&case)) else {
        return;
    };
    let reference = run_reference(&wasm, &case);
    for outcome in [&reference, &js[0].sync, &js[0].replay] {
        assert_eq!(
            outcome.fallthrough_reason(),
            Some(FallthroughReason::CapsuleError),
            "{outcome:?}"
        );
        let EdgeOutcome::Fallthrough { detail, .. } = outcome else {
            unreachable!()
        };
        assert!(detail.contains("could not be instantiated"), "{detail}");
    }
}

// ── the gateway, in JavaScript ───────────────────────────────────────

#[test]
fn the_js_gateway_suite_passes() {
    let Some(node) = node() else {
        return;
    };
    let output = Command::new(node)
        .arg("--test")
        .arg(js_dir().join("test").join("gateway.test.mjs"))
        .output()
        .expect("run node --test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the JS gateway suite failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A filter or a path mistake that runs nothing must not read as a pass.
    assert!(
        stdout.lines().any(|line| line.trim_start().starts_with("# pass")
            && !line.trim_end().ends_with(" 0")),
        "the JS gateway suite ran no tests:\n{stdout}"
    );
}
