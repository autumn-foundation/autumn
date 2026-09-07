//! Adversarial WAT probes against `autumn-edge`'s reference wasmi host.
//!
//! `autumn-edge/src/host.rs`'s module doc claims a specific WASI-deny
//! contract ("no `path_open`, ... no socket import ... `environ_*`/`args_*`
//! answer empty"), but until this file, nothing exercised that contract with
//! a hostile guest — the crate's own `#[cfg(test)]` suite covers fuel,
//! memory limits, KV wiring and frame parsing, and the sibling
//! `plugin_sandbox` escape suite this was borrowed from speaks a different
//! wire protocol and cannot be pointed at this host unmodified. See
//! `docs/reports/2026-09-07-prospect-edge-host-wasi-deny-probes.md`.
//!
//! Every guest here is hand-written WAT, compiled at test time by the
//! pure-Rust `wat` crate — no `wasm32-wasip1` toolchain is needed to run
//! this file, matching `plugin_sandbox`'s own reasoning for doing the same.
#![cfg(feature = "host")]

use autumn_edge::host::EdgeArtifact;
use autumn_edge::kv::EmptyEdgeKv;
use autumn_edge::wire::{EdgeOutcome, EdgeRequest, FallthroughReason};

fn run(wat: &str) -> EdgeOutcome {
    let wasm = wat::parse_str(wat).expect("valid WAT");
    let artifact = EdgeArtifact::from_bytes(&wasm).expect("valid module");
    artifact
        .run(&EdgeRequest::get("/probe"), &[], &EmptyEdgeKv)
        .expect("host-side setup never fails for these probes")
}

fn assert_denied_at_load(outcome: &EdgeOutcome) {
    let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
        panic!("expected a fallthrough, the guest was served: {outcome:?}");
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert!(
        detail.contains("could not be instantiated"),
        "expected an instantiation-time refusal, got: {detail}"
    );
}

/// R1: no ambient filesystem. A guest that imports `path_open` — the one
/// import `plugin_sandbox`'s `READ_FILE` guest uses to read `/etc/passwd` —
/// must fail to link, because `autumn-edge`'s shim never defines it.
#[test]
fn filesystem_import_is_refused_at_load() {
    assert_denied_at_load(&run(r#"(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))
"#));
}

/// R4 (network egress via `sock_*`): a guest that imports the exact socket
/// call the pre-registration names (`sock_connect`) must fail to link — the
/// shim's closed world excludes the network the same way it excludes the
/// filesystem. A Codex review on this PR correctly flagged that an earlier
/// version of this probe imported `sock_send` instead, which never
/// exercised the pre-registered `sock_connect` criterion at all: had the
/// host implemented one but not the other, this suite would have stayed
/// green while still claiming the `sock_connect` line was checked. Both are
/// now probed independently. (A later review round also caught that this
/// doc comment originally mislabeled the threat as R2 — R2 is the linker's
/// general unknown-import behavior, exercised below by the invented-namespace
/// probe; R4 is the plan doc's own row for `sock_*` specifically.)
#[test]
fn socket_connect_import_is_refused_at_load() {
    assert_denied_at_load(&run(r#"(module
  (import "wasi_snapshot_preview1" "sock_connect"
    (func $sock_connect (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))
"#));
}

/// R4 (network egress via `sock_*`), second representative: the same
/// closed-world claim for `sock_send`, kept as an independent probe rather
/// than folded into the one above so a failure names the specific import
/// that regressed.
#[test]
fn socket_send_import_is_refused_at_load() {
    assert_denied_at_load(&run(r#"(module
  (import "wasi_snapshot_preview1" "sock_send"
    (func $sock_send (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))
"#));
}

/// R2 (invented namespace): a guest that imports a host escape hatch nobody
/// declared — `plugin_sandbox`'s own `HOST_COMMAND` probe's shape — must
/// fail to link. Nothing about `wasmi`'s linker special-cases `wasi_*`
/// naming; an unresolved import from any module name is refused.
#[test]
fn invented_namespace_import_is_refused_at_load() {
    assert_denied_at_load(&run(r#"(module
  (import "env" "system" (func $system (param i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))
"#));
}

/// R3 (environment): the module doc says `environ_get`/`environ_sizes_get`
/// "answer empty," which is a documentation claim about two independently
/// registered functions (`write_two_zeroes` for the sizes call, a separate
/// closure for `environ_get` itself) that could regress silently — a
/// wrong-but-plausible implementation still links and still answers
/// *something*. A Codex review on this PR correctly flagged that an earlier
/// version of this probe only called `environ_sizes_get`, leaving
/// `environ_get` itself uncovered: a regression that writes real data
/// through `environ_get` while the sizes call still reports zero would have
/// passed unnoticed. A second Codex review then flagged that the sizes-get
/// half of that fix was itself hollow: WASM linear memory starts
/// zero-initialized, so reading zero back from an unseeded output slot
/// proves nothing about whether `environ_sizes_get` actually wrote it — a
/// no-op or a silently-failing call would read identically. This guest now
/// seeds *every* output location (the two size slots, the pointer-array
/// region, and the string-buffer region) with a non-zero sentinel first,
/// and additionally checks each call's own returned errno is `SUCCESS`
/// before trusting what it wrote. "The capsule exited without answering"
/// (clean fallthrough, no trap) is the only way to pass; a real leak, a
/// silent failure, or a no-op in any of the four checked locations shows up
/// as a distinctly different, trapped detail instead.
#[test]
fn environ_is_actually_empty_not_just_documented() {
    let outcome = run(r#"(module
  (import "wasi_snapshot_preview1" "environ_sizes_get"
    (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get"
    (func $environ_get (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    ;; Sentinel-fill the size-output slots (0, 4), the pointer-array region
    ;; (100..108) and the string-buffer region (200..208) *before* any call,
    ;; so a pass proves each function actually wrote what it claims rather
    ;; than relying on WASM's zero-initialized memory to fake a pass.
    (i32.store (i32.const 0) (i32.const 0xdeadbeef))
    (i32.store (i32.const 4) (i32.const 0xdeadbeef))
    (i32.store (i32.const 100) (i32.const 0xdeadbeef))
    (i32.store (i32.const 104) (i32.const 0xdeadbeef))
    (i32.store (i32.const 200) (i32.const 0xdeadbeef))
    (i32.store (i32.const 204) (i32.const 0xdeadbeef))
    (if (i32.ne (call $environ_sizes_get (i32.const 0) (i32.const 4)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (call $environ_get (i32.const 100) (i32.const 200)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 100)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 104)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 200)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 204)) (i32.const 0xdeadbeef)) (then (unreachable)))))
"#);
    let EdgeOutcome::Fallthrough { reason, detail } = &outcome else {
        panic!("expected a fallthrough, the guest was served: {outcome:?}");
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert_eq!(
        detail, "the capsule exited without answering",
        "a trapped detail here means environ_sizes_get reported a non-zero \
         count, or environ_get wrote through the sentinel buffers -- a real \
         leak, not the documented empty environment: {detail}"
    );
}

/// R3 (arguments): the same probe for `args_sizes_get`/`args_get`, same
/// two Codex-review gaps closed the same way — sentinel-seeded output
/// locations (including the size slots, per the second review round) and a
/// checked `SUCCESS` return from both calls.
#[test]
fn args_are_actually_empty_not_just_documented() {
    let outcome = run(r#"(module
  (import "wasi_snapshot_preview1" "args_sizes_get"
    (func $args_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_get"
    (func $args_get (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 0xdeadbeef))
    (i32.store (i32.const 4) (i32.const 0xdeadbeef))
    (i32.store (i32.const 100) (i32.const 0xdeadbeef))
    (i32.store (i32.const 104) (i32.const 0xdeadbeef))
    (i32.store (i32.const 200) (i32.const 0xdeadbeef))
    (i32.store (i32.const 204) (i32.const 0xdeadbeef))
    (if (i32.ne (call $args_sizes_get (i32.const 0) (i32.const 4)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (call $args_get (i32.const 100) (i32.const 200)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 100)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 104)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 200)) (i32.const 0xdeadbeef)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 204)) (i32.const 0xdeadbeef)) (then (unreachable)))))
"#);
    let EdgeOutcome::Fallthrough { reason, detail } = &outcome else {
        panic!("expected a fallthrough, the guest was served: {outcome:?}");
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert_eq!(
        detail, "the capsule exited without answering",
        "a trapped detail here means args_sizes_get reported a non-zero \
         count, or args_get wrote through the sentinel buffers -- a real \
         leak, not the documented empty argv: {detail}"
    );
}
