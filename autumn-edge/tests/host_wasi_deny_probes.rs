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

/// R2 (real function, never implemented): a guest that imports a real WASI
/// socket call must fail to link — the shim's closed world excludes the
/// network the same way it excludes the filesystem.
#[test]
fn network_import_is_refused_at_load() {
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
/// "answer empty," which is a documentation claim about a function
/// (`write_two_zeroes`) that could regress silently — a wrong-but-plausible
/// implementation still links and still answers *something*. This guest
/// traps deliberately if either returned size is non-zero, so "the capsule
/// exited without answering" (clean fallthrough, no trap) is the only way to
/// pass, and a real leak would show up as a distinctly different, trapped
/// detail instead.
#[test]
fn environ_is_actually_empty_not_just_documented() {
    let outcome = run(r#"(module
  (import "wasi_snapshot_preview1" "environ_sizes_get"
    (func $environ_sizes_get (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (drop (call $environ_sizes_get (i32.const 0) (i32.const 4)))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 0)) (then (unreachable)))))
"#);
    let EdgeOutcome::Fallthrough { reason, detail } = &outcome else {
        panic!("expected a fallthrough, the guest was served: {outcome:?}");
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert_eq!(
        detail, "the capsule exited without answering",
        "a trapped detail here means environ_sizes_get returned a non-zero \
         count -- a real leak, not the documented empty environment: {detail}"
    );
}

/// R3 (arguments): the same probe for `args_sizes_get`.
#[test]
fn args_are_actually_empty_not_just_documented() {
    let outcome = run(r#"(module
  (import "wasi_snapshot_preview1" "args_sizes_get"
    (func $args_sizes_get (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (drop (call $args_sizes_get (i32.const 0) (i32.const 4)))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const 0)) (then (unreachable)))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 0)) (then (unreachable)))))
"#);
    let EdgeOutcome::Fallthrough { reason, detail } = &outcome else {
        panic!("expected a fallthrough, the guest was served: {outcome:?}");
    };
    assert_eq!(*reason, FallthroughReason::CapsuleError);
    assert_eq!(
        detail, "the capsule exited without answering",
        "a trapped detail here means args_sizes_get returned a non-zero \
         count -- a real leak, not the documented empty argv: {detail}"
    );
}
