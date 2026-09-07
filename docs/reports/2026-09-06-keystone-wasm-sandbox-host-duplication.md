# 🏛️ Keystone [findings]: two independently-maintained wasmi sandbox hosts have already diverged

- Status: Findings memo (not an RFC — see Reversibility; also an Ask-before item)
- Date: 2026-09-06
- Author: Keystone (architecture review agent)

## 🎯 Scope

System boundary examined: the deny-by-default `wasmi` guest-sandboxing shim —
the code that gives a compiled-to-wasm guest exactly the ambient authority a
manifest/capability list grants it and nothing else. Two independent, full
implementations of this shim exist in the tree:

- `autumn-edge/src/host.rs` (1,082 lines) — the reference host behind the
  `#[edge]` read lane's native-only `host` feature (ADR 0011, landed
  2026-08-20, PR #1790/#2243).
- `autumn/src/plugin_sandbox/host.rs` (7,054 lines) — the untrusted
  third-party plugin runtime (issue #1609, landed 2026-09-03, PR #2349).

The second was explicitly derived from the first — its own planning doc
(`docs/plans/2026-08-27-sandboxed-plugins-first-slice.md`) says so ("borrowing
the parts of `autumn-edge` that already work") and names the risk this memo
confirms, in its own Black-hat section, without resolving it: *"Two wasm
hosts now exist in-tree (edge and plugin) and could drift."*

Reproduce every claim below with the commands in 🔬 Reproduce.

## 📈 Evidence (Tier 2 — repository record)

1. **Core WASI memory-access plumbing is duplicated verbatim.** `memory_of`,
   `read_u32`, `write_u32`, and `iovec` — the functions every guest-facing
   WASI call goes through to touch guest linear memory — are byte-identical
   between the two files modulo one lifetime parameter (`HostState<'kv>` vs
   `HostState`). Confirmed by direct diff, not name-matching. `write_two_zeroes`
   (`autumn-edge/src/host.rs:801`, `autumn/src/plugin_sandbox/host.rs:3622`) is
   the same logic reached through the same two calls, but not textually
   identical: `autumn-edge` takes `Caller` by value and reborrows it as
   `&mut caller` at each call site, while `plugin_sandbox` takes `&mut Caller`
   directly and passes it straight through — a real difference in the two
   files' calling convention for this helper, not just a lifetime
   parameter, even though the two bodies decide the same thing the same way.

2. **The guest-visible WASI surface has already diverged.** `plugin_sandbox`
   serves `clock_res_get` and `fd_tell` as real, answered capabilities
   (`SERVED_IMPORTS` table, `autumn/src/plugin_sandbox/host.rs`). Neither name
   appears anywhere in `autumn-edge/src/host.rs` — a guest that imports either
   links and runs under the plugin sandbox and fails to instantiate at all
   under the edge host. This is not a planned, documented difference; nothing
   in ADR 0011 or the sandboxed-plugins plan says the two lanes' WASI surfaces
   are allowed to disagree.

3. **The divergence was present from the second host's first commit**, not
   something that crept in over months. `plugin_sandbox/host.rs` was
   authored fresh (not as an edit to `autumn-edge/src/host.rs`) in the single
   commit that introduced it, and immediately shipped with a different served
   set than the file it was "borrowed from."

4. **No commit has touched `autumn-edge/src/host.rs` since `plugin_sandbox`
   landed** (2026-09-03 → 2026-09-06, `git log` empty for that path over that
   range) — including the hardening the second implementation shipped for
   itself. `plugin_sandbox`'s `write_stdout`/`stderr_excerpt`/`take_stdin`
   carry substantial comments and logic absent from `autumn-edge`'s versions
   of the same three functions: geometric buffer growth capped at a computed
   ceiling, strict (`String::from_utf8`) rather than lossy stdout decoding
   with an explicit rationale ("lossy decoding... materializes 4x the budget
   in host memory... outside the ceiling"), and a chunk-bounded `take_stdin`.
   `autumn-edge`'s equivalents still use the simpler, earlier-written shape
   (`String::from_utf8_lossy(...).into_owned()`, unbounded-until-budget
   `Vec::push`). I am **not** asserting this is an exploitable defect in
   `autumn-edge` today — its budget is a small fixed constant
   (`STDOUT_LINE_BUDGET_BYTES = 64 MiB`, no per-manifest size or
   concurrency-based footprint accounting the way `plugin_sandbox` has — see
   Scope note below) — only that a hardening rationale was written down once,
   for one file, and nothing connects it to the sibling file it was copied
   from.

5. **The threat models are not identical, which is exactly the missing
   documentation.** `autumn-edge`'s host runs the *app's own* wasm build
   (first-party, same source tree, used to prove native/edge byte-identity)
   behind a native-only `host` feature; `plugin_sandbox` runs third-party,
   adversarial artifacts by design (manifest + SHA-256 pinning, an 17-item
   adversarial threat table, a WAT-based escape-test suite). That is a
   legitimate reason the two shims could be allowed to diverge — but it is
   not written down anywhere. ADR 0011's "no second response path that could
   drift" is about router/handler dispatch parity between the edge and origin
   lanes, a different axis entirely; it does not address the host-runtime
   plumbing this memo is about, and no other ADR does either (confirmed:
   grep across `docs/adr/` for `plugin_sandbox` or "two wasm hosts" returns
   nothing).

## 🧭 Do nothing / decide later

No Tier-1 data exists (this is a framework, not yet an operated service at
scale), and nothing about the current state has caused an incident. The cost
is not hypothetical, though: it has already happened once, within three days
of the second host shipping, exactly along the axis its own design doc
predicted. Left alone for the next 12 months:

- Every future change to "what a wasm guest may do" or "how the shim defends
  itself" (a new WASI import, a `wasmi` upgrade that changes trap behavior, a
  fix for a discovered escape) has to be independently remembered and applied
  in two places, with nothing that fails a build or a review if one is
  missed — as already happened once.
- The only rigorous adversarial test suite for this shim shape (the
  `plugin_sandbox` WAT escape tests, R1-R17) covers one of the two hosts.
  A fix mirrored into `autumn-edge` by hand would ship with no equivalent
  proof it actually closed the gap there.
- `autumn-edge`'s host is reachable to whatever runs the `#[edge]` conformance
  suite and any first-party edge routes; it is lower-exposure than
  `plugin_sandbox`'s third-party surface, so the honest 12-month cost here is
  "silent behavioral disagreement between two things that look like they
  should agree," not "known open vulnerability."

## 💡 Mechanism

Two structurally identical, explicitly-related implementations of the same
kind of security-relevant boundary live in two crates with no shared code, no
shared conformance suite, and no cross-reference from one's commits to the
other's. Extending or hardening one does not touch, remind about, or test the
other. This is not a hypothetical "could drift" — the WASI surface already
disagrees, and one file has hardening logic and rationale the other lacks.

## 🔧 Recommendation — not a decision, and an Ask-before item

This crosses a security boundary (the guest-sandboxing shim), so per this
role's own rule I am not proposing to implement anything here — that decision
needs a human, regardless of how cheap the refactor would be to reverse.
Recorded as a findings memo, the same way the query_budget coverage-gap
finding was: real, evidence-backed, and explicitly handed off rather than
executed.

**Reversibility, for whoever picks this up:** consolidating the shared
plumbing into one internal, non-public module both crates depend on is a
two-way door — neither crate's public API changes, and un-inlining it back
into two copies is a bounded, low-single-digit-engineer-day revert. That
reversal cost alone would put it below this framework's ~2-engineer-week
RFC bar; it is flagged here only because it also touches the "security
boundary" Ask-before category, which overrides the reversibility bar.

Concrete options for a human to choose between:

1. **Converge.** Extract the shared plumbing (`memory_of`/`read_u32`/
   `write_u32`/`iovec`/`write_two_zeroes`, and ideally the buffered-line/NDJSON
   dialogue state machine) into one internal module both `autumn-edge` and
   `autumn`'s `plugin_sandbox` depend on, backport the `write_stdout`/
   `stderr_excerpt` hardening, close the `clock_res_get`/`fd_tell` surface
   gap (serve or explicitly deny both, in both hosts), and run the existing
   WAT escape-test suite (R1-R17) against both hosts so one suite proves both.
2. **Document the split as deliberate.** If first-party-only vs.
   adversarial-third-party is judged a real enough difference in threat model
   that the two hosts should stay independent, say so next to `EdgeKv`'s
   doc comment or in ADR 0011 — including that the WASI surfaces are
   permitted to disagree — so the next person who finds this divergence does
   not have to re-derive it from a diff.
3. **Do neither now**, and record it as a deferral instead, with a trigger:
   revisit if a third wasmi-based host is added (making it three independent
   copies), or if a WASI-shim security fix is made to one host and not
   mirrored to the other within the same PR.

## ⚖️ Alternatives considered

- **Say nothing, since neither host has caused an incident.** Rejected: the
  drift the design doc predicted has already happened inside three days, and
  the cost of writing it down now (this memo) is a few minutes; the cost of
  re-discovering it after a real divergence-caused bug is a debugging session
  plus reduced trust in the sandbox.
- **File it as an RFC recommending option 1 outright.** Rejected: it touches
  a security boundary, which this role's own rules place outside RFC/PR-level
  authority regardless of reversal cost — a human decision, not a document,
  is what the rule calls for.

## 🔬 Reproduce

```bash
# Line counts and landing dates
wc -l autumn-edge/src/host.rs autumn/src/plugin_sandbox/host.rs
git log origin/trunk-dev --follow --diff-filter=A --format='%ad %s' --date=short -- autumn-edge/src/host.rs | tail -1
git log origin/trunk-dev --follow --diff-filter=A --format='%ad %s' --date=short -- autumn/src/plugin_sandbox/host.rs | tail -1

# No commits to the edge host since the plugin sandbox landed
git log origin/trunk-dev --oneline --since=2026-09-03 -- autumn-edge/src/host.rs

# Duplicated plumbing (byte-identical modulo one lifetime parameter):
# memory_of / read_u32 / write_u32 / iovec. Ranges end exactly at iovec's
# closing brace -- past it the two files diverge into unrelated doc comments
# and SERVED_IMPORTS/WASI-registration code, which is not part of this claim.
diff <(sed -n '453,490p' autumn-edge/src/host.rs) \
     <(sed -n '2843,2880p' autumn/src/plugin_sandbox/host.rs)

# write_two_zeroes: same two calls, same branching, but a real calling-
# convention difference (by-value + reborrow vs. by-ref), not just a lifetime
diff <(sed -n '801,814p' autumn-edge/src/host.rs) \
     <(sed -n '3622,3635p' autumn/src/plugin_sandbox/host.rs)

# Diverged WASI surface: present in plugin_sandbox, absent from autumn-edge
grep -n '"clock_res_get"\|"fd_tell"' autumn/src/plugin_sandbox/host.rs
grep -n 'clock_res_get\|fd_tell' autumn-edge/src/host.rs   # no output

# The design doc's own self-flagged, unresolved risk. The phrase wraps across
# a markdown line break ("...could\ndrift."), so a plain single-line grep for
# "could drift" finds nothing -- use a multiline-capable match instead.
rg -U 'could\s+drift' docs/plans/2026-08-27-sandboxed-plugins-first-slice.md

# No prior ADR or report names this pairing. Query the tree *before* this
# report was added, not a fixed "HEAD^" — this file may since have gained
# follow-up commits (typo/accuracy fixes), which would shift what HEAD^ means
# and make a hardcoded HEAD^ match this report itself.
report=docs/reports/2026-09-06-keystone-wasm-sandbox-host-duplication.md
added_at=$(git log --diff-filter=A --format=%H -- "$report" | tail -1)
git grep -rln "plugin_sandbox" "${added_at}^" -- docs/adr/ docs/reports/   # no output
```
