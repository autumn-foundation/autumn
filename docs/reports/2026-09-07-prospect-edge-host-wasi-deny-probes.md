# ⛏️ Prospect: does `autumn-edge`'s host still deny the WASI escapes its own module doc claims? (pre-registration)

## 🎯 Question

`docs/reports/2026-09-06-keystone-wasm-sandbox-host-duplication.md` found that
`autumn-edge/src/host.rs` (the wasmi sandbox host `autumn/src/plugin_sandbox/host.rs`
was explicitly "borrowed from") has received **zero commits** since the second
host landed, while the second host picked up hardening and its own adversarial
WAT escape-test suite that the first never got. Keystone's memo recommends
(option 1) running that escape suite "against both hosts so one suite proves
both," but does not do so itself — it is a findings memo, not an assay, and
correctly declines to implement anything because the question crosses a
security boundary.

**Falsifiable question:** does `autumn-edge/src/host.rs`'s wasmi shim,
*unmodified*, actually deny the same baseline WASI-escape primitives its own
module doc comment claims (no `path_open`, no socket import satisfied,
`environ_*`/`args_*` answer empty) when probed with hand-written adversarial
WAT guests — the same technique `plugin_sandbox`'s escape suite already uses,
just never pointed at this host?

**Decision:** whether Keystone's wasm-sandbox memo's "do nothing now, revisit
on a trigger" framing (option 3) is safe to default to, or whether this assay
turns up a live regression that makes option 1 (converge) or at minimum
"patch the gap now" urgent regardless of which option a human eventually
picks for the broader duplication question. This assay does **not** decide
between Keystone's options 1/2/3 — that stays a human call, per that memo's
own Ask-before flag — it only supplies the missing empirical data point
("does the un-audited host still hold its stated baseline") that every one of
those three options is currently being chosen without.

**Decider:** whoever picks up the Keystone wasm-sandbox memo next (maintainer,
or the Ledger/Bolt personas it names) — same decider named there.

## ⚖️ Pre-registration

- **Pursue line** (reframed for a security-property check: "pursue" here
  means *the baseline holds, no urgent action needed beyond adding the
  missing regression coverage*): all of the following hold against
  `autumn-edge`'s host, unmodified —
  1. A guest importing `wasi_snapshot_preview1::path_open` fails to
     instantiate (no ambient filesystem).
  2. A guest importing a real-but-unimplemented WASI function
     (`sock_connect`) fails to instantiate (closed world holds for at least
     this one representative case).
  3. A guest importing an entirely invented host namespace (`env::system`,
     mirroring `plugin_sandbox`'s own `HOST_COMMAND` probe) fails to
     instantiate (no accidental escape hatch via a namespace nobody
     declared).
  4. A guest that calls `environ_sizes_get`/`environ_get` and traps on any
     non-zero byte count exits *without trapping* (environment is actually
     empty, not just documented as empty).
  5. The same for `args_sizes_get` (empty argv).
- **Kill line:** any one of the five probes shows the opposite — a guest
  that imports `path_open`/`sock_connect`/`env::system` successfully
  instantiates and runs past the point of making that call, or an
  environ/args probe traps (meaning real data leaked through). Any single
  failure here is a live, exploitable regression in a shipped, feature-gated
  host and gets escalated immediately, independent of this report's own
  verdict-filing pace — this crosses the same security boundary Keystone
  flagged, so a kill result is not just "filed," it is surfaced to the user
  before this session does anything else.
- **Conditions:** new tests added to `autumn-edge`, run via `cargo test -p
  autumn-edge --features host`. WAT guests are hand-written (not compiled
  from Rust), matching `plugin_sandbox`'s own evidentiary reasoning in
  `autumn/src/plugin_sandbox/test_guests.rs` ("evidence that only runs on a
  CI runner with a wasm toolchain installed is evidence that mostly does not
  run") — compiled at test time by the pure-Rust `wat` crate, added as a new
  `[dev-dependencies]` entry (already a dependency of the sibling `autumn`
  crate for the identical purpose).
- **Time box:** same session, on the order of one to two hours — this is
  reading two files and writing ~5 small WAT modules against an already
  public, already-stable API (`EdgeArtifact::from_bytes`/`run`), not new
  design work.
- **Riskiest assumption, attacked first:** that `autumn-edge`'s host can be
  driven from a plain WAT guest through its *public* API without needing any
  of `plugin_sandbox`'s NDJSON wire-protocol machinery. Confirmed before
  writing any guest: `EdgeArtifact::from_bytes(wasm)` and `.run(&request,
  &capabilities, &kv)` are both `pub`, `EdgeRequest::get(uri)` is a public
  one-line constructor, and `EmptyEdgeKv` is a public zero-config `EdgeKv`
  impl — no plugin_sandbox-shaped scaffolding is needed at all. A guest that
  never emits a valid response frame is not a test-harness failure; it comes
  back as `EdgeOutcome::Fallthrough { reason: CapsuleError, detail }`, and the
  probes below are designed to read their pass/fail signal entirely from
  *whether instantiation/execution reaches that point*, never from a
  well-formed answer.
- **Containment:** purely additive test code in `autumn-edge/tests/` and one
  new dev-dependency line, gated behind the existing non-default `host`
  feature. No production code path touched, no change to
  `autumn-edge/src/host.rs` itself is in scope for this assay (if a kill
  result requires a fix, that is a separate, immediately-flagged follow-up,
  not silently folded into this report).
- **Prior-art check:** `autumn-edge` has no `tests/` directory today and its
  in-module `#[cfg(test)]` suite (16 tests, `autumn-edge/src/host.rs`) covers
  fuel exhaustion, memory limits, KV wiring, and frame parsing — zero
  adversarial WASI-escape guests. `plugin_sandbox`'s escape corpus
  (`autumn/src/plugin_sandbox/test_guests.rs`, `HELLO`/`READ_FILE`/
  `ENVIRONMENT`/`ARGUMENTS`/`HOST_COMMAND`/`UNDEFINED_WASI`/…) speaks that
  crate's own JSON-over-NDJSON wire protocol and cannot be pointed at
  `autumn-edge` unmodified — its guests expect to emit a `{"op":"response",…}`
  frame `autumn-edge`'s `from_line`/`to_line` do not parse. This is a real
  gap, not a re-dig: nothing in the tree already answers "does the reference
  host hold its own documented WASI-deny claims under adversarial input."

This report will be updated in place with the assay results and verdict
after the apparatus runs; this commit fixes the lines above before any
measurement exists.
