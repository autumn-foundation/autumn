# ⛏️ Prospect: does `autumn-edge`'s host still deny the WASI escapes its own module doc claims? (pursue: 5/5 probes held vs. 5/5 pre-set line)

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

Pre-registration commit: the commit that added this file to
`docs/reports/`, before `autumn-edge/tests/host_wasi_deny_probes.rs` or the
`wat` dev-dependency existed (`git log --follow -- docs/reports/2026-09-07-prospect-edge-host-wasi-deny-probes.md`).

## 🧪 Apparatus

`autumn-edge/tests/host_wasi_deny_probes.rs` — five new integration tests,
gated behind the existing non-default `host` feature, plus one new
`[dev-dependencies]` line (`wat = "1"`, matching the sibling `autumn` crate's
identical use for its own escape suite). Each probe is a hand-written WAT
module driven entirely through `autumn-edge`'s public API
(`EdgeArtifact::from_bytes`/`.run`, `EdgeRequest::get`, `EmptyEdgeKv`) — no
plugin_sandbox code, no shared harness, confirming the riskiest assumption
named in the pre-registration.

**Stubs list** (what was faked, and why it doesn't undercut the verdict):

- Every guest here answers nothing (`_start` either never runs, because
  linking already failed, or runs and returns without emitting a response
  frame). None speaks `autumn-edge`'s real wire protocol — that's
  deliberate: the signal this assay reads is entirely "did the WASI call
  succeed or fail," never "did the guest produce a valid answer." A future
  probe that needs to observe a *returned value* (not just "trap vs. no
  trap") would need to either emit a minimal valid response frame or extend
  this harness with a stderr-reporting convention; neither was needed for
  the five properties tested here.
- Only 5 of the plan doc's 17 named threats (R1–R17) are covered:
  filesystem (R1), one representative unimplemented real WASI function
  standing in for "closed world" (R2, via `sock_send`), one invented-namespace
  host escape (also R2-shaped, mirroring `plugin_sandbox`'s `HOST_COMMAND`),
  and environment/argv leakage (R3). R11–R17 (header/cookie stripping,
  manifest/artifact integrity, consent surfacing) are **plugin_sandbox-only
  concepts** — `autumn-edge` has no manifest, no declared-capability grant
  surface, and strips credential headers before the frame is built (i.e.
  before the guest ever runs), so those threats don't have an edge-host
  analogue to probe in the first place. R4/R6–R9 were in the plan doc's own
  gaps (its table skips R4 and jumps R3→R5); this assay did not chase them
  down since they're not part of Keystone's memo either. This is a
  targeted subset chosen for "cheapest apparatus that can falsify the
  specific baseline Keystone's memo questioned," not a full port of the
  17-item table.
- **Negative control, run and reverted, not merged**: to prove the two
  "answers empty" probes actually falsify (rather than passing vacuously),
  `environ_sizes_get`'s registration was temporarily edited to write a
  non-zero count (3, 40) instead of calling `write_two_zeroes`, the
  `environ_is_actually_empty_not_just_documented` test was rerun, and it
  failed exactly as expected — `left: "the capsule trapped: wasm
  \`unreachable\` instruction executed"` vs. `right: "the capsule exited
  without answering"`. The edit was then reverted; `git diff --stat
  autumn-edge/src/host.rs` shows zero net change to that file in this PR.
  No such control was run for the three link-time-refusal probes — an
  unresolved import failing to link is `wasmi`'s own behavior, not
  `autumn-edge` code this assay could plausibly edit to fake a pass, so the
  cheaper substitute is confidence from reading `define_wasi_shim`'s full
  body (grepped and read in the Prior Art check) rather than a second
  synthetic regression.

## 📊 Assay

```
cargo test -p autumn-edge --features host --test host_wasi_deny_probes
```

| Probe | Threat class | Expected | Result |
|---|---|---|---|
| `filesystem_import_is_refused_at_load` | R1 — ambient filesystem | refused at instantiation | **refused** (`"...could not be instantiated..."`) |
| `network_import_is_refused_at_load` | R2 — closed world, real fn | refused at instantiation | **refused** |
| `invented_namespace_import_is_refused_at_load` | R2 — closed world, invented namespace | refused at instantiation | **refused** |
| `environ_is_actually_empty_not_just_documented` | R3 — environment | clean exit, no trap | **clean exit** (`"the capsule exited without answering"`) |
| `args_are_actually_empty_not_just_documented` | R3 — argv | clean exit, no trap | **clean exit** |

5/5 against the pre-set pursue line. Full crate suite re-run clean alongside
the new tests (`cargo test -p autumn-edge --features host`: 22 existing
`#[cfg(test)]`/integration tests + 2 doctests, all passing, confirming the
new dev-dependency and feature-gated test file didn't disturb anything
already there). `cargo fmt -p autumn-edge` and `cargo clippy -p autumn-edge
--features host --all-targets -- -D warnings` clean (one pre-existing,
unrelated `unknown_lints` warning from a stale `clippy::unused_async_trait_impl`
name in workspace lint config, reproducible on `trunk-dev` before this PR,
not introduced by it).

Worst-case probing: the negative control (see stubs list) is the worst case
this assay could cheaply manufacture — an actual leak, caught. No attempt
was made to fuzz WAT shapes beyond the five hand-written probes; each targets
one specific, named claim from the module doc rather than searching for
unknown ones.

## 🏁 Verdict: **pursue** (the baseline holds) — with a real, separate structural finding

**5/5 probes confirm `autumn-edge`'s host still holds every WASI-deny
property this assay could cheaply check, unmodified, despite the zero-commit
gap Keystone's memo flagged.** Against the pre-set line, this is not a
security emergency: the "do nothing yet" default in Keystone's memo (or
either of its other two options) is not being chosen blind to an active
hole, at least not one of these five shapes.

That is not the same finding as "the two hosts are equivalently robust,"
and this assay does not claim it. What it adds to Keystone's memo, precisely:

- The **denial mechanism itself differs in a way this assay newly confirms
  empirically, not just by reading**: `plugin_sandbox` refuses an unresolved
  import **before instantiation**, by name, into a typed
  `SandboxLoadError::ForbiddenImports` a caller can inspect and log
  structurally. `autumn-edge` has no equivalent — `EdgeArtifact::from_bytes`
  does no import scan at all (its own doc comment says the *conformance
  suite* does that, as a side check on first-party artifacts, not the host);
  an unresolved import here fails inside `wasmi`'s own `linker.instantiate`
  call and surfaces as an untyped, un-named string inside a generic
  `CapsuleError` fallthrough (`"the capsule could not be instantiated:
  {err}"`) — no `DeniedCapability`, no operation name, nothing a caller could
  alert on differently from an ordinary buggy capsule. Both fail closed
  *today*, because `path_open`/`sock_send`/arbitrary namespaces are simply
  never defined — but that is an accident of what's implemented, not a
  designed, inspectable contract the way `plugin_sandbox`'s allowlist scan
  is. If `autumn-edge` ever needs to run a **less-trusted** artifact (its own
  module doc's stated purpose is proving native/edge parity for the app's
  *own* build, i.e. first-party) this gap between "happens to fail closed"
  and "fails closed by a checked, logged contract" is exactly the kind of
  thing Keystone's option 1 (converge) would fix for free, and option 2
  (document as deliberate) would need to write down explicitly rather than
  leave implicit.
- This sharpens, rather than resolves, Keystone's own three options: it is
  evidence *against* urgency (nothing is silently broken today) and
  evidence *for* option 1's stated benefit (a shared, checked allowlift
  mechanism the ecosystem doesn't currently apply to `autumn-edge`) over
  option 3 (defer) if the deciding human weighs "no named-import diagnostic
  today" as a real gap rather than an accepted one. That weighing is still
  the human decision Keystone's memo correctly declined to make — this
  assay only replaces "we assume it still holds" with "it does hold, checked
  five ways, and here specifically is the one place the two hosts' *designs*
  — not just their WASI surfaces — actually diverge."

## 💰 Cost to productionize

Not applicable in the usual sense — this assay's "pursue" is a clean bill of
health on the baseline, not a build recommendation. For whoever picks up
Keystone's memo next, this assay changes the cost estimate for option 1 only
by naming one more piece already worth converging (the import-allowlist
check itself, not just the WASI plumbing Keystone's memo already named) and
by leaving five now-permanent regression tests in place that option 1's own
"run the existing suite against both hosts" goal partially satisfies for
`autumn-edge`'s side today, without waiting on the broader convergence
decision.

## 🔬 Reproduce

```bash
cargo test -p autumn-edge --features host --test host_wasi_deny_probes -- --nocapture
cargo test -p autumn-edge --features host
cargo clippy -p autumn-edge --features host --all-targets -- -D warnings
```

Negative control (do not merge — for reproducing the falsifiability check
only): in `autumn-edge/src/host.rs`, replace the
`.func_wrap(WASI, "environ_sizes_get", write_two_zeroes)` registration with a
closure that writes a non-zero count, rerun
`environ_is_actually_empty_not_just_documented`, observe the trap, then
revert.

## 🗄️ Dismantle

The five probe tests are kept, not dismantled — like the query-budget
generalization assay's fixtures, they are cheap, real regression coverage
for a security property that had zero adversarial coverage before this PR,
and they cost nothing to leave in `autumn-edge`'s own test suite behind its
existing `host` feature gate. Nothing else was built: no shared harness
between the two hosts, no change to either `host.rs`. The convergence
question itself (Keystone's options 1–3) remains exactly where that memo
left it — a human decision, now made with one more checked data point.
