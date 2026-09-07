# ⛏️ Prospect: does `autumn-edge`'s host still deny the WASI escapes its own module doc claims? (pursue: 5/5 pre-registered criteria held, +1 supplemental)

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
  adversarial WASI-escape guests. **Correction (sixth Codex review round,
  filed against the completed report — see below): this claim was simply
  wrong.** `autumn-edge/tests/runtime_io.rs` already existed (22 tests,
  dated before this PR) covering routing, capability gating, credential
  stripping and frame-loop behavior through the crate's public API — real,
  substantial integration coverage this bullet missed entirely. It is not
  adversarial WASI-escape coverage (no test in it imports a hostile WASI
  function), so the underlying gap this assay closes is still real, but
  "no `tests/` directory" was a checkable, false statement, not an
  approximation. `plugin_sandbox`'s escape corpus
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

`autumn-edge/tests/host_wasi_deny_probes.rs` — six integration tests, gated
behind the existing non-default `host` feature, plus one new
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
  the six probes here.
- 5 pre-registered probes cover 4 of the plan doc's 17 named threat rows
  (R1–R17 — **correction below: the doc's table has no gaps; an earlier
  version of this bullet wrongly claimed R4 was missing from it**):
  filesystem (R1, `path_open`), a permissive-linker/unknown-import escape
  (R2, the invented `env::system` namespace — mirroring `plugin_sandbox`'s
  `HOST_COMMAND`), environment/argv leakage (R3, both the size call and the
  value call for each), and network egress via `sock_*` (R4, via
  `sock_connect` — the pre-registration's own named example). A sixth,
  supplemental probe covers R4 a second way (`sock_send`), added during
  review — see the pre-registration-fidelity update below for why it is
  reported separately rather than folded into the registered count.
  R5–R10 (a database import, resource/hang controls) and R11–R17
  (header/cookie stripping, manifest/artifact integrity, consent surfacing)
  are not covered by this file. Checked individually, not assumed:
  - **R6** (infinite loop) and **R7** (memory bomb) genuinely have
    dedicated, pre-existing tests in `autumn-edge/src/host.rs`'s own
    `#[cfg(test)]` suite (`a_runaway_loop_exhausts_its_fuel_and_falls_through`,
    `a_memory_hungry_module_is_refused_instead_of_killing_the_host`) — real
    coverage of those two specific threats, by a different mechanism than
    adversarial WASI-escape guests.
  - **R9** (a trap must not abort the host) holds in `autumn-edge` only as
    an inherent property of using `wasmi` (an interpreter: a guest trap
    surfaces as an ordinary `Result::Err`, never a process abort or OS
    signal, with no join-boundary/catch-unwind machinery needed the way a
    native-code worker would require) — not because a dedicated test
    triggers a real wasm trap and asserts the process survives. No such
    test exists in the pre-existing suite today.
  - **R10** (a slow guest starves the async runtime) has **no mitigation
    at all** in `autumn-edge`, dedicated test or otherwise: `grep -rn
    "spawn_blocking\|Semaphore\|concurrency" autumn-edge/src/` returns
    nothing. `EdgeArtifact::run` is a plain synchronous function; a caller
    awaiting it from an async context blocks that task for as long as the
    guest's fuel budget allows before the interpreter traps it. This is a
    real, previously-unstated gap this assay surfaces as a byproduct, not
    a threat this file claims to test — worth naming to whoever picks up
    Keystone's memo next, since `plugin_sandbox`'s own R10 control
    (`spawn_blocking` plus a bounded concurrency permit) has no
    `autumn-edge` analogue at all.

  R5, R8, and R11–R17 are either not reachable from `autumn-edge`'s API
  shape (no database seam, no manifest, credential headers stripped before
  the guest ever runs) or were simply out of scope for "cheapest apparatus
  that can falsify the specific baseline Keystone's memo questioned" — this
  was never meant to be a full port of the 17-item table, only enough of it
  to answer the pre-registered question.
  **Update (fourth Codex review round)**: the R1–R4 classification above
  was itself a correction, prompted by a review comment pointing out that
  R4 is explicitly defined in the plan doc
  (`docs/plans/2026-08-27-sandboxed-plugins-first-slice.md:45`, "Network
  egress via `sock_*`") and that the socket probes therefore exercise R4,
  not R2 as originally labeled here — confirmed by re-reading the full
  R1–R17 table (`grep -n '^| R' docs/plans/...`), which shows all seventeen
  rows present with no gaps. Reclassified accordingly; no apparatus change,
  write-up only.
  **Update (fifth Codex review round)**: the R6/R7/R9/R10 breakdown above
  is itself a correction too. An earlier version of this bullet claimed
  R10 already had dedicated coverage alongside R6/R7/R9. A review correctly
  pointed out `autumn-edge` has neither `spawn_blocking` nor a concurrency
  permit anywhere, confirmed by the grep above — R10 has no mitigation, and
  the claim was simply wrong. While re-verifying, R9's claim was also
  tightened from "dedicated coverage" to "holds inherently, untested" once
  checking showed no existing test actually triggers a real wasm trap and
  asserts survival, rather than wait for that gap to be found in a separate
  round. Write-up only, no apparatus change.
- **Negative controls, run and reverted, not merged**: to prove the
  "answers empty" probes actually falsify (rather than passing vacuously),
  two separate registrations were each temporarily edited to leak, the
  corresponding test rerun to confirm it failed, then reverted:
  `environ_sizes_get` was made to write a non-zero count (3, 40) instead of
  calling `write_two_zeroes` — `environ_is_actually_empty_not_just_documented`
  failed with `left: "the capsule trapped: wasm \`unreachable\` instruction
  executed"` vs. `right: "the capsule exited without answering"`; separately,
  `environ_get` was made to write a real value through its pointer-array
  argument — the same test failed the same way. Both edits were reverted;
  `git diff --stat autumn-edge/src/host.rs` shows zero net change to that
  file in this PR. No such control was run for the four link-time-refusal
  probes — an unresolved import failing to link is `wasmi`'s own behavior,
  not `autumn-edge` code this assay could plausibly edit to fake a pass, so
  the cheaper substitute is confidence from reading `define_wasi_shim`'s
  full body (grepped and read in the Prior Art check) rather than a second
  synthetic regression.
- **Update (Codex review round)** — two real gaps found and fixed, both
  confirmed by re-reading before changing anything:
  1. The pre-registration named `sock_connect` as the representative
     unimplemented real WASI function, but the first version of this probe
     imported `sock_send` instead — a real discrepancy between the
     committed criterion and what was actually exercised. Had the host
     implemented one but not the other, the suite would have stayed green
     while the report still claimed the `sock_connect` line was checked.
     Fixed by testing both independently
     (`socket_connect_import_is_refused_at_load`,
     `socket_send_import_is_refused_at_load`) rather than silently
     substituting one for the other in the write-up.
  2. The environ/args probes only ever called the `_sizes_get` functions,
     never `environ_get`/`args_get` themselves — both are registered
     independently in `define_wasi_shim`, so a regression that leaked real
     data through the value-returning call while the size call still
     reported zero would have passed unnoticed. Fixed by sentinel-filling
     the pointer-array and buffer regions before calling `environ_get`/
     `args_get` and trapping if either changed; confirmed to actually catch
     a leak with the second negative control described above.
- **Update (second Codex review round) — a pre-registration-fidelity
  correction, not a code change.** After the fix above, this report's Assay
  and Verdict sections described the result as "6/6 against the pre-set
  line," folding the new supplemental `sock_send` probe into the same
  denominator as the five criteria the pre-registration actually fixed
  (line 45 above names only `sock_connect`). A Codex review correctly
  flagged this: scoring a probe that was added *after* the pre-registration
  — and whose own write-up says so — as if it had been one of the
  registered lines is exactly the kind of post-hoc criterion-widening
  Prospect's own rules exist to prevent, independent of whether the extra
  probe happens to pass. No apparatus or code changed for this round — the
  `sock_send` probe stays (it is real, additional evidence, and cheap to
  keep) but is now reported as **5/5 pre-registered, plus 1 supplemental**
  throughout, never blended into a single inflated count.
- **Update (third Codex review round) — a real soundness gap in the R3
  probes themselves, found and fixed.** The environ/args probes checked
  offsets 0 and 4 for zero *without seeding them first*. WASM linear memory
  starts zero-initialized, so that check could not distinguish "the shim
  correctly computed and wrote zero" from "the shim did nothing at all, or
  failed silently, and the slot was already zero" — a no-op or
  silently-erroring `environ_sizes_get`/`args_sizes_get` would have passed
  this probe exactly as cleanly as the real, correct implementation.
  Verified with a negative control: `environ_sizes_get`'s registration was
  temporarily replaced with a closure that writes nothing and returns
  `errno::INVAL`, and the *pre-fix* probe passed regardless (not run as a
  committed state, only to confirm the gap before fixing it). Fixed by
  seeding the two size-output slots with the same non-zero sentinel used
  for the pointer-array/buffer regions, and by checking each call's own
  returned errno equals `SUCCESS` before trusting anything it wrote. Re-ran
  the same negative control against the *post-fix* probe and confirmed it
  now traps (the seeded sentinel survives an `INVAL`-returning no-op,
  correctly read as a failure); reverted the control immediately after —
  `git diff --stat autumn-edge/src/host.rs` again shows zero net change to
  that file in this PR.

## 📊 Assay

```
cargo test -p autumn-edge --features host --test host_wasi_deny_probes
```

| Probe | Threat class | Pre-registered? | Expected | Result |
|---|---|---|---|---|
| `filesystem_import_is_refused_at_load` | R1 — ambient filesystem | yes | refused at instantiation | **refused** (`"...could not be instantiated..."`) |
| `socket_connect_import_is_refused_at_load` | R4 — network egress via `sock_*` (`sock_connect`) | yes (the pre-registration's own named example) | refused at instantiation | **refused** |
| `invented_namespace_import_is_refused_at_load` | R2 — permissive linker / unknown import | yes | refused at instantiation | **refused** |
| `environ_is_actually_empty_not_just_documented` | R3 — environment (size + value calls, sentinel-seeded, errno-checked) | yes | clean exit, no trap | **clean exit** (`"the capsule exited without answering"`) |
| `args_are_actually_empty_not_just_documented` | R3 — argv (size + value calls, sentinel-seeded, errno-checked) | yes | clean exit, no trap | **clean exit** |
| `socket_send_import_is_refused_at_load` | R4 — network egress via `sock_*` (`sock_send`) | **no — supplemental, added during review** | refused at instantiation | **refused** |

**5/5 pre-registered criteria hold, against the pre-set pursue line, plus
1/1 supplemental probe also passing.** A Codex review on this PR correctly
flagged that an earlier version of this table folded the supplemental
`sock_send` probe into a "6/6" headline score — since `sock_send` was never
one of the five criteria fixed in the pre-registration (only `sock_connect`
was named there), counting it in the denominator after the fact would be
exactly the kind of post-hoc goalpost-widening Prospect's own rules ban,
even though the extra probe happens to pass. The two counts are now kept
separate: **5/5** is the number that carries the verdict against the
pre-set line; the `sock_send` probe is additional, welcome, but
non-registered evidence, reported alongside rather than blended in.

Full crate suite re-run clean alongside the new tests (`cargo test -p
autumn-edge --features host`: 65 existing library `#[cfg(test)]` tests +
22 existing `runtime_io` integration tests + 2 doctests — **corrected
count, sixth Codex review round: an earlier version of this line said "22
existing tests" total, missing the 65 library tests entirely, the same
`autumn-edge/tests/runtime_io.rs` oversight as the Prior-art bullet above**
— all passing, confirming the new dev-dependency and feature-gated test
file didn't disturb anything already there). `cargo fmt
-p autumn-edge` and `cargo clippy -p autumn-edge --features host
--all-targets -- -D warnings` clean (one pre-existing, unrelated
`unknown_lints` warning from a stale `clippy::unused_async_trait_impl` name
in workspace lint config, reproducible on `trunk-dev` before this PR, not
introduced by it).

Worst-case probing: the two negative controls (see stubs list) are the worst
case this assay could cheaply manufacture — an actual leak, caught, in both
the size-reporting and value-returning halves of the R3 contract. No attempt
was made to fuzz WAT shapes beyond the six hand-written probes (five
pre-registered, one supplemental); each targets one specific, named claim
from the module doc rather than searching for unknown ones.

## 🏁 Verdict: **pursue** (the baseline holds) — with a real, separate structural finding

**5/5 pre-registered probes confirm `autumn-edge`'s host still holds every
WASI-deny property this assay could cheaply check, unmodified, despite the
zero-commit gap Keystone's memo flagged** — plus a sixth, supplemental probe
(`sock_send`, added during review, not part of the registered denominator)
also passing. Against the pre-set line, this is not a security emergency:
the "do nothing yet" default in Keystone's memo (or either of its other two
options) is not being chosen blind to an active hole, at least not one of
these five pre-registered shapes.

That is not the same finding as "the two hosts are equivalently robust,"
and this assay does not claim it. What it adds to Keystone's memo, precisely:

- The **denial mechanism itself differs in a way this assay newly confirms
  empirically, not just by reading**: `plugin_sandbox` refuses an unresolved
  import **before instantiation**, by name, into a typed
  `SandboxLoadError::ForbiddenImports` — a `DeniedCapability` enum plus an
  `operation` field a caller can match on without parsing text.
  `autumn-edge` has no equivalent — `EdgeArtifact::from_bytes` does no
  import scan at all (its own doc comment says the *conformance suite* does
  that, as a side check on first-party artifacts, not the host); an
  unresolved import here fails inside `wasmi`'s own `linker.instantiate`
  call and surfaces as a free-text string inside a generic `CapsuleError`
  fallthrough. **Correction (Codex review):** an earlier version of this
  bullet claimed that string was "untyped, un-named" — checked directly by
  printing one (`"the capsule could not be instantiated: cannot find
  definition for import wasi_snapshot_preview1::path_open with type
  Func(...)"`), the import's module and name **are** present, just embedded
  in `wasmi`'s own `Display` formatting of its internal error type rather
  than exposed as a stable field. The real gap is narrower than originally
  stated: not "no operation name," but "no structured, typed value — a
  caller has to string-match against an upstream dependency's Debug/Display
  output, which carries no stability contract, instead of matching on an
  enum this crate owns." Both fail closed *today*, because
  `path_open`/`sock_connect`/`sock_send`/arbitrary namespaces are simply
  never defined — but that is an accident of what's implemented, not a
  designed, type-safe contract the way `plugin_sandbox`'s allowlist scan
  is. If `autumn-edge` ever needs to run a **less-trusted** artifact (its own
  module doc's stated purpose is proving native/edge parity for the app's
  *own* build, i.e. first-party) this gap between "happens to fail closed
  with a parseable-but-unstable message" and "fails closed by a checked,
  typed, logged contract" is exactly the kind of thing Keystone's option 1
  (converge) would fix for free, and option 2 (document as deliberate)
  would need to write down explicitly rather than
  leave implicit.
- This sharpens, rather than resolves, Keystone's own three options: it is
  evidence *against* urgency (nothing is silently broken today) and
  evidence *for* option 1's stated benefit (a shared, checked allowlift
  mechanism the ecosystem doesn't currently apply to `autumn-edge`) over
  option 3 (defer) if the deciding human weighs "no *typed* named-import
  diagnostic today, only a parseable string" as a real gap rather than an
  accepted one. That weighing is still
  the human decision Keystone's memo correctly declined to make — this
  assay only replaces "we assume it still holds" with "it does hold, checked
  five pre-registered ways plus one supplemental, and here specifically is
  the one place the two hosts' *designs* — not just their WASI surfaces —
  actually diverge."

## 💰 Cost to productionize

Not applicable in the usual sense — this assay's "pursue" is a clean bill of
health on the baseline, not a build recommendation. For whoever picks up
Keystone's memo next, this assay changes the cost estimate for option 1 only
by naming one more piece already worth converging (the import-allowlist
check itself, not just the WASI plumbing Keystone's memo already named) and
by leaving six now-permanent regression tests in place that option 1's own
"run the existing suite against both hosts" goal partially satisfies for
`autumn-edge`'s side today, without waiting on the broader convergence
decision.

## 🔬 Reproduce

```bash
cargo test -p autumn-edge --features host --test host_wasi_deny_probes -- --nocapture
cargo test -p autumn-edge --features host
cargo clippy -p autumn-edge --features host --all-targets -- -D warnings
```

Negative controls (do not merge — for reproducing the falsifiability checks
only): in `autumn-edge/src/host.rs`,
1. replace the `.func_wrap(WASI, "environ_sizes_get", write_two_zeroes)`
   registration with a closure that writes a non-zero count, rerun
   `environ_is_actually_empty_not_just_documented`, observe the trap, then
   revert;
2. separately, replace the `"environ_get"` registration's closure body
   (currently `|_caller, _environ, _buffer| errno::SUCCESS`) with one that
   writes a real value through the `environ` pointer argument, rerun the
   same test, observe the trap, then revert; and
3. separately again, replace `"environ_sizes_get"`'s registration with a
   closure that writes nothing and returns `errno::INVAL` (a silent no-op
   failure), rerun the same test, observe the trap (this is the case the
   third Codex review round's sentinel-seeding fix exists to catch — a
   pre-fix version of the probe would have passed here instead), then
   revert.

## 🗄️ Dismantle

The six probe tests are kept, not dismantled — like the query-budget
generalization assay's fixtures, they are cheap, real regression coverage
for a security property that had zero adversarial coverage before this PR,
and they cost nothing to leave in `autumn-edge`'s own test suite behind its
existing `host` feature gate. Nothing else was built: no shared harness
between the two hosts, no change to either `host.rs`. The convergence
question itself (Keystone's options 1–3) remains exactly where that memo
left it — a human decision, now made with one more checked data point.
