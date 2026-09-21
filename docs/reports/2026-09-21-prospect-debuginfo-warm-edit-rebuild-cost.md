# ⛏️ Prospect: is lower `-C debuginfo` a warm-edit rebuild win too, or cold-build-only? (pursue: -36% median vs 10% materiality line)

## 🎯 Question

The 2026-09-17 Onramp findings report
(`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md`,
PR #2829, issue #2795) measured `-C debuginfo=0` giving a ~18% cold-start
build win (close to, but on the honest pooled number just under, Onramp's own
20% floor) and `RUSTFLAGS="-C debuginfo=1"` an ~8.7% win, but named two
explicit gaps before a human could decide which level (if any) to set in the
generated-project templates' `[profile.dev]`.

**Terminology correction (caught by Codex review on PR #2882): that report's
own prose calls its `-C debuginfo=1` condition "line-tables-only," but
`rustc -C help` on this same toolchain lists `line-tables-only` and `limited`
(the level numeric `1` maps to) as distinct values — "debug info emission
level (0-2, none, line-directives-only, line-tables-only, limited, or
full)". They are not the same setting.** Onramp's report measured `1`/
`limited`; this report's own first drafts inherited its "line-tables-only"
label and then, independently, set up an apparatus using the actually-named
`line-tables-only` Cargo profile value — a *different*, more minimal level
than the one Onramp measured. Both mistakes are corrected below: Onramp's
condition is called `limited` here, and this assay's original
`line-tables-only` condition is kept but no longer treated as the same thing.

Gap 2, verbatim: *"Only the cold
build was measured. A `[profile.dev]` override in the generated-project
templates changes every subsequent `cargo build`/`cargo run`/`cargo test`
too, including the warm, incremental rebuilds `dev-loop-latency.yml` and
`dev-loop-scaling.yml` gate — not measured here at all, and debug-info
emission cost does not necessarily scale the same way for a small incremental
diff as it does for a from-scratch build."

**Falsifiable question:** does a `[profile.dev]` debuginfo reduction change
warm/incremental Rust-route-edit rebuild wall-clock time — the same change
class the `RustRouteEditHello` dev-loop gate measures
(`autumn-cli/src/dev_loop_bench.rs:106-110`, budget p50 ≤3,000ms / p95
≤5,000ms / max ≤10,000ms) — by an amount that would matter to the pending
decision, or is the cold-start win a one-time, cold-build-only effect with no
bearing on the far more frequent warm-edit loop?

**Decision fed:** the same "Decision needed" pick the Onramp report deferred
to a human — which debug-info level, if any, to set in the generated-project
templates' `[profile.dev]` (issue #2795). This assay closes that report's
explicitly named gap 2. **Decider:** repo maintainer (same decider the
Onramp report named).

## ⚖️ Pre-registration

Committed to `/tmp/.../scratchpad/prereg.md` in this sandbox before the first
timed measurement (containment note below explains why that file isn't part
of this PR).

- **Materiality line, set before measuring:** |relative change in median
  warm-edit wall time, reduced level vs. baseline `debug=2`| ≥ 10% counts as
  **material** — a real factor the decision must weigh, in whichever
  direction it points. < 10% counts as **immaterial** — the warm loop is a
  wash and only the cold-start number matters. 10% was chosen because the
  existing `RustRouteEditHello` budget has real headroom on this box class (a
  single-digit-second p95); a change below 10% would not move a project
  meaningfully within that budget either way, while ≥10% is large enough to
  be a real, statable input to a human's decision.
  **Note added after measuring (not a change to the pre-registered line
  itself — see the correction in Apparatus): this line was, and remains,
  defined over compile-and-link wall time specifically, which is what this
  apparatus measures. Whether the real `RustRouteEditHello` gate's own
  end-to-end percentage (which also includes file-watcher and health-check
  latency this apparatus doesn't measure) clears the same 10% is a related
  but distinct question this assay does not answer.**
- **Conditions:** same sandbox/box class Onramp's report used (4 vCPU,
  rustc/cargo 1.94.1), `examples/hello` (path-depends on `autumn-web`), deps
  and `autumn-web` pre-warmed. Wall clock via `date +%s.%N` (no
  `/usr/bin/time` in this sandbox, same as Onramp's report).
- **Time box:** same session (target under ~45 min of wall-clock building).
- **Riskiest assumption first:** that debuginfo has *any* measurable effect
  on an incremental rebuild at all — a changed line in `hello`'s own 39-line
  `main.rs` only recompiles that one small crate; every dependency rlib,
  `autumn-web` included, is already built and cached. The a priori risk was
  that this whole lever is cold-build-only and the honest answer is a fast,
  cheap "no effect" — which is exactly the gap Onramp flagged as unmeasured.
- **Control:** baseline = current default (no override, Cargo's own
  `debug=true`/`-C debuginfo=2`) — same control definition Onramp's report
  used.
- **Containment:** local, uncommitted edits to `examples/hello/src/main.rs`
  and a temporary, clearly-marked `[profile.dev]` block appended to the root
  `Cargo.toml`, both reverted by the apparatus script before this report was
  filed (verified with `git status`/`git diff` after every run in this
  session). No CI or template changes; no production data; no dependency
  added.

## 🔍 Prior art

- The Onramp 2026-09-17 report itself (cited above), whose own apparatus and
  numbers this assay treats as the control condition's expected baseline
  shape and whose gap 2 this assay exists to close.
- The 2026-09-16 report
  (`docs/reports/2026-09-16-onramp-test-sim-compile-gate-negative-result.md`)
  supplied the methodology this assay reuses: exclude each block's first run
  as a warm-up outlier, and interleave condition order rather than batching,
  to guard against sandbox drift.
- No existing report measures warm/incremental rebuild cost under a
  `[profile.dev]` debuginfo override — this is new ground, not a re-dig.

## 🧪 Apparatus

Block design, not a flat interleave: an early probe (`RUSTFLAGS="-C
debuginfo=0" cargo build -p hello` on an already-warm build) took over two
minutes and rebuilt `autumn_web` itself from source — confirming that
changing debuginfo mid-session invalidates Cargo's fingerprint for the
**whole** dependency graph, not just the touched crate. Interleaving
individual builds by flipping `RUSTFLAGS` per-run, as this assay's first
draft script did, would have measured "cost of switching conditions," not
"cost of a warm edit under a fixed condition" — the wrong quantity, since a
real project is scaffolded at one debug level from `autumn new` and never
switches. Fixed design: each condition gets its own block — set
`[profile.dev].debug` in the root `Cargo.toml` (the actual mechanism the
decision is about, not `RUSTFLAGS`), pay one full-graph rebuild to enter the
block (excluded from the measurement — not representative of a real dev
loop), then take 3 timed single-line edits to `hello()`'s return string
(alternated to a fresh literal each time, so every sample is a real
recompile, never a no-op). Planned as two rounds with condition order
interleaved (baseline → `debug=0` → `line-tables-only` → `line-tables-only`
→ baseline → `debug=0`) to control for drift across the ~35-minute session,
matching the 2026-09-16 report's methodology — realized as planned for
baseline and `debug=0`, but not for `line-tables-only`; see the stub below
and the correction in **📊 Assay**. (A fourth condition, `debug=1`/`limited`,
was added afterward in its own separately-interleaved pair of blocks — see
the stub below and **📊 Assay**.)

**Stubs / shortcuts (the complete list):**
- Measures `cargo build -p hello` directly, not the actual `autumn dev`
  live-reload loop `dev-loop-latency.yml` gates (file-watcher trigger latency
  and the health-check poll after the binary restarts are not included).
  **Correction (caught by Codex review on PR #2882): an earlier draft of
  this stub claimed "only the relative delta between conditions" carries
  over to the real gate. That's not right either.** File-watcher and
  health-check latency, if debuginfo-insensitive and roughly fixed, still
  changes the *percentage* once added to the denominator — a toy example:
  4s→3s is -25%, but if a fixed 1s of overhead is added to both (5s→4s),
  the same 1s of absolute saving reads as only -20%. So every percentage in
  this report (including the ones that clear the pre-registered 10% line)
  is a **compile-and-link-time percentage**, not a validated claim about the
  full end-to-end `RustRouteEditHello` loop's own percentage — which could
  sit below this report's numbers, by an amount this assay did not measure
  or bound. What *is* comparable, and unaffected by this dilution concern,
  is absolute wall-clock savings for the compile step itself (real seconds
  removed from every warm edit), and the direction of every effect measured.
  Bounding or measuring the omitted watcher/health-check overhead is a
  concrete follow-up, not yet done here.
- Uses `examples/hello` (a workspace member of this monorepo), not the actual
  `autumn new`-scaffolded no-DB daemon project — same Onramp-flagged gap 1,
  still open, not closed by this assay.
- `[profile.dev]` was set at this workspace's root `Cargo.toml` rather than a
  generated project's own manifest (a workspace member can't carry its own
  `[profile]` table); a real generated project would set it in its own root
  manifest, with the same effect.
- One bug found and fixed mid-assay, disclosed rather than silently
  corrected: the first pass wrote `debug = line-tables-only` unquoted into
  TOML for the `d1` condition. That's an invalid manifest, so `cargo build`
  failed near-instantly every time and the apparatus's naive timer recorded
  the failure's ~30ms exit as a "successful" build — caught by eye (30ms is
  not a plausible compile time), not by the script, which had no exit-code
  check on that first pass. Fixed (quote non-numeric profile values, check
  `cargo`'s exit code, abort loudly on failure) and the `d1` condition was
  re-run in full before this report was written. `baseline` and `d0`'s
  first-pass numbers were unaffected (verified against a surviving build log
  showing a genuine `unoptimized` 2.40s build) and are used as-is.
- **That `d1` re-run cost this assay `line-tables-only`'s independent-block
  design (caught by Codex review on PR #2882, third round on the same
  paragraph): the rerun script ran `run_block 1 d1 ...` immediately followed
  by `run_block 2 d1 ...`, never returning to another condition in between,
  so the two logged "blocks" are one continuously-held measurement period,
  not two independent re-entries** — confirmed by the second block's own
  warm-up build taking 2.5s, not the ~200s a genuine fresh entry costs
  elsewhere in this apparatus, and by re-reading the rerun script itself
  (`warm_edit_d1_rerun.sh`, apparatus scratch, not committed — per
  Containment, apparatus never merges — but its logic is reproducible: the
  same `set_profile`/`run_block` pair the **🔬 Reproduce** section below
  gives, called twice in a row for the same condition with no other
  condition in between). `baseline` and `debuginfo=0` do not have this
  problem; only `line-tables-only` does. See **📊 Assay**/**🏁 Verdict** for
  what this does and doesn't allow this report to claim.
- **`debug = "line-tables-only"` is not the same rustc setting as Onramp's
  `-C debuginfo=1` (caught by Codex review on PR #2882, fourth round):**
  see the terminology correction in **🎯 Question**. Rather than leave this
  as an unmeasured gap, a fourth condition — `debug = 1` (`limited`, the
  actual numeric level Onramp's report used) — was measured after this
  finding, with its own pair of genuinely independent blocks (interleaved
  with a fresh `baseline` pair, not with the other conditions above, since
  by this point in the session baseline/`debuginfo=0`/`line-tables-only` all
  had cached rlibs and could no longer force `limited`'s fingerprint out via
  a cheap switch — `baseline` was cheapest to alternate with). Its own
  paired baseline numbers are reported separately from the main table's, not
  pooled with them, because they came roughly 40 minutes later in the
  session and read measurably lower (see **📊 Assay**) — a real reminder
  that this sandbox's baseline itself drifts. **`debug=0` and `limited` are
  each compared against a baseline measured in the same narrow window as
  their own samples; `line-tables-only` is not (caught by Codex review on
  PR #2882: an earlier draft of this sentence claimed this held for every
  reduced condition, which conflicts with the unpaired-baseline correction
  in **📊 Assay** for `line-tables-only` specifically — its valid samples
  came from a rerun done later than, and not re-paired with, the baseline
  row it's divided by).**

## 📊 Assay

Wall clock, steady-state runs only (each block's first run excluded as
warm-up; see Apparatus), n=6 per condition (2 rounds × 3 samples):

| Condition | samples (s) | median | mean | stdev |
|---|---|---|---|---|
| baseline (`debug=2`, current default) | 4.022, 4.101, 3.951, 3.998, 4.096, 4.042 | 4.032 | 4.035 | 0.058 |
| `debug = 0` (none) | 2.630, 2.597, 2.622, 2.512, 2.584, 2.491 | 2.591 | 2.573 | 0.058 |
| `debug = "line-tables-only"` | 2.560, 2.499, 2.472, 2.532, 2.517, 2.681 | 2.524 | 2.543 | 0.074 |

Relative to baseline median: **`debug=0` -35.75%** (paired: both `debug=0`
and this baseline came from the same continuous script run, interleaved —
see Apparatus), **`line-tables-only` -37.39%, unpaired** (caught by Codex
review on PR #2882: `line-tables-only`'s valid samples came from a later,
separate rerun after the TOML-quoting bug was found and fixed — see
Apparatus — not from this same continuous run the baseline row above came
from. This report's own `limited` measurement later in the session showed
baseline drifting by several percent between measurement windows roughly
40 minutes apart; `line-tables-only`'s rerun happened closer in time than
that, but with no contemporaneous baseline re-measured alongside it, this
percentage divides a real, later-measured numerator by an earlier-measured
denominator with unknown drift between them. Treat -37.39% as a nominal
comparison, not a paired one — real drift and real effect are confounded in
this specific number the same way session drift and TOML-quoting bug fix are
already confounded in `line-tables-only`'s block-independence problem below).

A fourth condition, `debug = 1` (`limited` — the actual level Onramp's
report measured as its own "`-C debuginfo=1`"; see the terminology
correction in **🎯 Question**), was measured separately, ~40 minutes later in
the session, against its own freshly-paired baseline rather than the table
above (this sandbox's baseline itself drifted between the two measurement
windows — see below):

| Condition (2nd window) | samples (s) | median | mean | stdev |
|---|---|---|---|---|
| baseline (`debug=2`, re-measured) | 3.774, 3.860, 3.830, 3.698, 3.878, 3.758 | 3.802 | 3.800 | 0.068 |
| `debug = 1` (`limited`) | 2.815, 2.848, 2.823, 2.601, 2.778, 2.729 | 2.796 | 2.766 | 0.091 |

Relative to this window's own baseline median: **`limited` -26.45%**. Block
means (2 genuinely independent blocks each, interleaved
`limited`/`baseline`/`limited`/`baseline`): `limited` = [2.829s, 2.703s],
baseline (2nd window) = [3.821s, 3.778s] — no overlap, a clean separation
the same way the first window's baseline-vs-`debug=0` comparison was.

**Correction (caught by Codex review on PR #2882, fifth round): the first
draft of this paragraph called the -26.45% (warm) vs. -8.7% (cold) figures
"roughly three times" as if that ratio were itself a measured, meaningful
quantity. It isn't, and the claim is withdrawn.** Onramp's `-8.7%` number
comes from just 2 samples, both from a single batched block, of a
non-incremental full `autumn-web` crate build — unlike `baseline`/`debug=0`
in that same report, `debuginfo=1` never got the interleaved-block check
that would let anyone bound its own noise. And it's measuring a categorically
different workload from this assay's incremental single-crate `hello`
rebuild — different code being compiled, different compilation mode, no
shared apparatus. Matching the rustc setting (per **🎯 Question**'s
correction) makes the two numbers *comparable in kind*, not arithmetically
combinable into a ratio.

**What this assay's `limited` result does support, described honestly as
two separate observations rather than one derived ratio:** on the *cold*
build, Onramp measured `limited` giving a thin, weakly-replicated ~8.7%
saving, well under its own 20% floor. On the *warm* edit, this assay
measured `limited` giving a properly-replicated (2 independent blocks,
6 samples, no overlap with its own paired baseline) ~26.45% saving, well
over this assay's 10% materiality line. Both numbers stand on their own
apparatus's own footing; neither multiplies into the other. What they
jointly support is qualitative, not a multiplier: `limited` is not a
option whose benefit is confined to the one-time cold build, the way
Onramp's report's own framing (a real, if modest, cold-start win; an
open question everywhere else) might suggest — it has a separately-verified,
separately-large warm-edit win too.

**Correction (caught by Codex review on PR #2882, sixth round): this
report's earlier drafts repeatedly said Onramp's report "verified" or
"confirmed" that `limited` preserves backtrace file:line resolution. It
never did.** Re-reading Onramp's own empirical check
(`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md:180-194`):
its throwaway two-function binary only compares default debuginfo against
`-C debuginfo=0`. The claim that `debuginfo=1` "keeps file:line resolution"
(that report's line 123) is asserted in prose, alongside the same
"line-tables-only" mislabeling this report's **🎯 Question** section already
corrected — not measured. So this report's own claim was borrowing
authority Onramp's report never actually had for this specific level.

**Rather than downgrade the claim to "unverified" and move on, this gap was
closed directly: the same throwaway-binary method Onramp's report used, run
in this sandbox at `-C debuginfo=1`.**

```
$ rustc -C debuginfo=2 -o bt_default main.rs && RUST_BACKTRACE=full ./bt_default
   0: main::inner
             at ./main.rs:2:14
   1: main::main
             at ./main.rs:7:5
   ...
$ rustc -C debuginfo=1 -o bt_limited main.rs && RUST_BACKTRACE=full ./bt_limited
   0: main::inner
             at ./main.rs:2:14
   1: main::main
             at ./main.rs:7:5
   ...
$ rustc -C debuginfo=0 -o bt_none main.rs && RUST_BACKTRACE=full ./bt_none
   0: main::inner
   1: main::main
   ...
```

`debuginfo=1`/`limited` reproduces file:line resolution for local frames
identically to the full-debuginfo default (`at ./main.rs:2:14` in both);
`debuginfo=0` drops it entirely, exactly reproducing the asymmetry Onramp's
report found between default and `debuginfo=0` — just now with `limited`
actually placed on the map instead of assumed onto it. This is now a
genuinely confirmed result, by this assay, not a borrowed and mislabeled one
from Onramp's report.

**Correction (caught by Codex review on PR #2882, three rounds on this one
paragraph, about the original two-condition table only — the `limited` data
above came later and doesn't have this problem): draft 1 compared the
`line-tables-only`-vs-`debug=0` percentage delta directly against stdevs in
seconds — invalid, mixed units. Draft 2 fixed that with a Welch's t-test
treating all 6 samples per condition as independent — which round 2
correctly called pseudoreplication (3 samples within one block share that
block's warm-up/cache/thermal state) and which this draft redid at the block
level, treating each condition's 2 logged blocks as 2 independent units.
Round 3 caught that this was *still* wrong for `line-tables-only`
specifically: its "2 blocks" were produced by `run_block 1 d1 ...;
run_block 2 d1 ...` back to back in the rerun that fixed the TOML-quoting
bug (see Apparatus), with no other condition entered in between —
`set_profile` writes byte-identical `Cargo.toml` content both times, and
block 2's own warm-up build took 2.5s, not the ~200s a genuine fresh
re-entry costs (confirmed against `warm_edit_d1_rerun.sh`, apparatus scratch
described in Apparatus). So `line-tables-only` has **one**
independently-entered measurement period (6 back-to-back builds under
continuously-held state), not two, while baseline and `debug=0` each
genuinely do have two (separated by real intervening condition changes —
see the block order in Apparatus). That asymmetry means there is no valid
way to compare `line-tables-only` against `debug=0` at the block level
either: one side has 1 independent unit, the other has 2. The honest
statement is simply that this design cannot support a rigorous claim about
whether the two reduced levels differ from each other — not "no evidence of
a difference," not "within noise," just not measured with enough
independent repetition to say. (This is separate from, and doesn't affect,
the `limited`-vs-`debug=0` question, which this report doesn't address
either — `limited` was only measured against its own baseline, not against
`debug=0` or `line-tables-only`.) Onramp's cold-build report, by contrast,
found a large, clearly resolved gap between `debug=0` and `limited` (~18% vs
~8.7%) that this design flaw doesn't call into question.

Baseline's sample range doesn't overlap either reduced condition's in the
main table (baseline min 3.951 > both reduced-condition maxima; `debug=0`
and `line-tables-only` overlap each other completely). The
baseline-vs-`debug=0` comparison specifically still holds despite the
concerns above: both conditions were genuinely independently entered twice
(see Apparatus), and the effect size (~35%, baseline block means
4.025s/4.045s vs. `debug=0`'s 2.617s/2.529s) is far too large for plausible
block-to-block noise to close. The baseline-vs-`line-tables-only` comparison rests on weaker footing on two
separate counts: `line-tables-only`'s single independent period (this
paragraph), and its unpaired baseline denominator (see the correction where
this figure is first reported, above) — but the same rough logic still
applies: a gap this large (baseline's two genuinely independent blocks vs.
`line-tables-only`'s one measured period) is not the kind of thing this
assay's known confounds (warm-up exclusion, sandbox noise on the order of
tens of milliseconds, or the baseline drift the `limited` measurement
found — a few percent over ~40 minutes, not enough alone to manufacture a
gap this size) could produce by chance. The *direction* (faster than
baseline) is solid; the specific *-37.39%* figure is not, for the two
reasons above. All three baseline-vs-reduced-condition comparisons
in this report (`debug=0`, `line-tables-only`, and `limited` against its own
paired baseline) are a clean separation, not a borderline call the way
Onramp's cold-build `debug=0` number was against its 20% floor.

**Worst case probed:** the warm-up (first-in-block) samples, deliberately
excluded from the tables above because they are not steady-state, are
themselves informative: `debug=0`'s and `line-tables-only`'s first-ever
entries cost 198.7s and 200.9s respectively (full-graph rebuild), and
`limited`'s cost 216.987s — confirming the one-time "switching tax" is real
and large across every reduced level measured, which is exactly why it's
excluded from a measurement about the *recurring* per-edit cost, not folded
in as if it happened on every edit.

## 🏁 Verdict

**Pursue** (material) — against the pre-set 10% line, measured over
compile-and-link wall time (see the correction in **⚖️ Pre-registration**/
**🧪 Apparatus**: this assay doesn't measure, and this verdict doesn't claim
anything about, the *full* `RustRouteEditHello` loop's own end-to-end
percentage, which also includes file-watcher and health-check latency):
every reduced debuginfo level measured changes warm-edit median
compile-and-link time by far more than 10% (`limited` -26.45%, `debug=0`
-35.75%, `line-tables-only` -37.39% nominal — see below), the opposite
direction of the risk this assay was chartered to probe. This is not a
hidden recurring *cost* the Onramp report's open gap worried about — it is a
large recurring *win* on the compile-and-link portion of the far more
frequent warm loop, that stacks with the (borderline) one-time cold-start
win.

This changes the shape of the pending decision, not just its confidence:

1. **The trade-off is not "one-time cold-start win vs. permanent backtrace
   quality cost paid on every build,"** as the Onramp report framed it. It's
   "one-time cold-start win *and* a large, recurring per-edit win, vs. a
   permanent backtrace-quality cost" — the compile-time side of the ledger is
   bigger than Onramp's report alone showed, because most of a
   development session's builds are warm edits, not cold starts.
2. **`limited` (`debug = 1`, the actual level Onramp's report evaluated,
   and which this assay independently confirmed preserves backtrace
   file:line resolution — Onramp's own report only asserted this in prose
   and never measured it for `limited` specifically; see the correction in
   **📊 Assay**) is not confined to being a safe-but-marginal cold-start
   win — it has a separately-measured, separately-large warm-edit win too.**
   Onramp measured `limited` at ~8.7%
   on the cold build (from a thin, single-block sample of a different,
   non-incremental workload — see the correction in **📊 Assay**, this is
   not a number to build a ratio on). This assay measured the same setting
   at ~26.45% on the warm edit, from a properly-replicated, genuinely
   independent, interleaved design (see **📊 Assay**) — trustworthy on its
   own terms, in a way the comparison in point 3 below isn't. The two
   numbers aren't combinable into a multiplier, but together they say
   `limited` is not a marginal, cold-build-only lever.
3. **Whether `line-tables-only` (an even more minimal, distinct rustc level
   — see the terminology correction in **🎯 Question** — that this assay
   measured separately and by coincidence, not because it was the level
   Onramp evaluated) beats `limited` on the warm-edit axis is genuinely
   unmeasured, not resolved either way.** This assay's own attempt at that
   comparison came from one continuously-held measurement period rather than
   two independently-entered ones (see the correction in **📊 Assay**, caught
   over three rounds of review) — not measured with enough independent
   repetition to say which is cheaper. `line-tables-only` was also never
   directly verified (by this report or Onramp's) to preserve backtrace
   file:line resolution the way `limited` was — it likely does, since line
   tables are in the name, but that's an inference, not a measurement.

This still does not resolve the decision by itself — gap 1 (the actual
scaffolded no-DB daemon project, not `examples/hello`) remains open, and
Onramp's own `debug=0` cold-build number is still shy of its 20% floor
pending re-measurement above the noise floor. But it removes "we don't know
if this is a hidden recurring cost" from the open-questions list, and
replaces it with a specific, load-bearing number the decider can weigh for
the option Onramp's report actually evaluated for backtrace quality: `limited`
carries a large, validated recurring win, not just a marginal cold-start one.

## 💰 Cost to productionize

Not a new build — this assay feeds an existing decision (issue #2795) rather
than proposing new code. If the maintainer picks `debug = 1` (`limited`) for
the generated-project templates' `[profile.dev]` (the option with the
strongest evidence behind it: this assay independently confirmed it
preserves backtrace file:line resolution — Onramp's report only asserted
this in prose and never measured it for `limited` specifically, see the
correction in **📊 Assay** — and this assay adds a validated, large
recurring warm-edit win; both are measurements of the *same* rustc setting,
confirmed via `rustc -C help`): the change itself is the one line Onramp's
report already scoped (`autumn-cli/src/templates/Cargo.toml.tmpl`,
`Cargo.api.toml.tmpl`), plus the still-open items neither report has closed:

- Gap 1 (still open, either report): re-measure against the actual
  `autumn new`-scaffolded project via `cold_start_driver.rs`, not
  `examples/hello`/`-p autumn-web` proxies, before shipping a template
  default change.
- New: measure or bound the `autumn dev` live-reload loop's own
  file-watcher-trigger and health-check-poll latency, which this assay's
  compile-and-link-only apparatus omits — needed to know whether the real
  `RustRouteEditHello` gate's own end-to-end percentage clears 10% too, or
  is diluted below it by fixed, debuginfo-insensitive overhead (see the
  correction in **🧪 Apparatus**).
- Re-measure `debug=0`'s cold-build number above the noise floor / on a
  dedicated or CI-caliber box, per Onramp's report (only relevant if
  `debug=0` rather than `limited` is the level under consideration —
  `debug=0` does not preserve backtrace file:line resolution, confirmed
  empirically by both Onramp's report and this one).
- New: a properly-blocked `line-tables-only`-vs-`limited` warm-edit
  comparison, only needed if the decider wants to consider `line-tables-only`
  specifically instead of `limited` — this assay's own attempt doesn't
  answer it (see the correction in **📊 Assay**), and `line-tables-only`'s
  backtrace-quality property is itself unverified (by either report),
  unlike `limited`'s.
- Which build agents' gates: `cold-start-latency.yml` needs a green run
  against the new template default before it merges. **`dev-loop-latency.yml`
  does not yet give equivalent evidence for the warm-edit path — caught by
  Codex review on PR #2882, correcting this report's first draft, which
  wrongly treated a green run of it as validation.** Its `measure` job (not
  gated to PRs; scheduled/manual only) calls `autumn dev-loop-bench` without
  `--dry-run`, which reaches `dev_loop_bench::run`'s live branch — but that
  branch calls `build_placeholder_results` (`autumn-cli/src/dev_loop_bench.rs:455-459`),
  which computes every change class's stats from `compute_stats(&[])` (zero
  samples, so every field is `0`) and therefore always passes every budget.
  The live HTTP-polling measurement driver referenced in that function's own
  comment is not wired up yet. So today, a green `dev-loop-latency.yml` run
  is not evidence about a template default change — before shipping one, the
  live driver needs to exist (a separate, larger prerequisite this assay did
  not scope) or the change needs its own ad hoc measurement of the kind this
  assay performed, repeated against the real scaffolded project (gap 1,
  above). Keystone should be looped in before committing to a template
  default change either way, per its own architecture-review remit, since
  this is a permanent trade-off for every generated project, not a PR-level
  call.

## 🔬 Reproduce

```bash
# Run this from a disposable clone or worktree, not a working copy with
# uncommitted changes (caught by Codex review on PR #2882: the revert step
# discards *any* uncommitted edits to these two files, not just the
# experiment's own, and this recipe neither requires a clean tree nor backs
# up what was there first):
#   git worktree add --detach /tmp/prospect-debuginfo-repro origin/trunk-dev
#   cd /tmp/prospect-debuginfo-repro

# Pre-warm deps + autumn-web once:
cargo build -p hello

set_profile() {   # "" (baseline/no override), "0", "1" (limited), or "line-tables-only"
  git checkout -- Cargo.toml
  if [ -n "$1" ]; then
    if [ "$1" = "0" ] || [ "$1" = "1" ] || [ "$1" = "2" ]; then
      printf '\n[profile.dev]\ndebug = %s\n' "$1" >> Cargo.toml   # integer: unquoted
    else
      printf '\n[profile.dev]\ndebug = "%s"\n' "$1" >> Cargo.toml # string: must be quoted!
    fi
  fi
}

# One condition's block: enter the condition, pay the one full-graph rebuild
# (excluded from timing -- see Apparatus for why switching conditions
# mid-session is not representative and each needs its own warm-up), then
# take 3 timed edits with an actually-incrementing counter (caught by Codex
# review on PR #2882: an earlier draft's sed used the literal string "vN",
# not a variable, so every invocation after the first rewrote the file to
# byte-identical content -- not a real edit) and check cargo's own exit
# status explicitly (caught in the same review round: `date; cargo build;
# date` chained with plain semicolons still "succeeds" when `cargo build`
# fails -- the exact failure mode this report's Apparatus section says
# invalidated the first `d1` pass):
run_block() {
  set_profile "$1"
  sed -i 's/"Hello, Autumn![^"]*"/"Hello, Autumn! warmup"/' examples/hello/src/main.rs
  cargo build -p hello || { echo "cargo build failed (warm-up)" >&2; exit 1; }
  i=0
  while [ "$i" -lt 3 ]; do
    i=$((i + 1))
    sed -i "s/\"Hello, Autumn![^\"]*\"/\"Hello, Autumn! v${i}\"/" examples/hello/src/main.rs
    S=$(date +%s.%N); cargo build -p hello || { echo "cargo build failed" >&2; exit 1; }; E=$(date +%s.%N)
    awk -v s="$S" -v e="$E" 'BEGIN { printf "%.3f\n", e - s }'   # no /usr/bin/time in this sandbox
  done
}

# IMPORTANT (caught by Codex review on PR #2882, sixth round): this
# round-robin is a CORRECTED FOLLOW-UP DESIGN, not a literal replay of how
# this report's own numbers were collected. The real sessions ran as two
# separate sequences, neither of which is this loop:
#   1. baseline -> debug=0 -> line-tables-only(buggy,discarded) ->
#      line-tables-only(buggy,discarded) -> baseline -> debug=0
#      (the buggy line-tables-only entries used unquoted TOML and never
#      really measured anything; see the stub in Apparatus)
#   2. A separate rerun, later: line-tables-only -> line-tables-only
#      (fixed TOML quoting, but back to back -- see the pseudoreplication
#      correction in Assay)
#   3. A separate run, later still: limited -> baseline -> limited -> baseline
# Running THIS script instead -- covering all four conditions in a genuine
# round-robin, each pair separated by the other three -- does not
# reproduce any of those three real sequences or their exact numbers; it is
# the design a follow-up assay should use to get a result this report's own
# apparatus couldn't validly produce for line-tables-only-vs-limited. Every
# condition's two occurrences are separated by the other three here, so
# every block pays a genuine fresh re-entry:
for cond in "" 0 line-tables-only 1 "" 0 line-tables-only 1; do
  echo "== condition: '${cond:-baseline}' =="
  run_block "$cond"
done

set_profile ""
git checkout -- examples/hello/src/main.rs

# Sanity check for the TOML-quoting bug this assay hit: a manifest parse
# error makes `cargo build` fail (and "finish") in tens of milliseconds, not
# seconds — treat any sub-1s "successful" build here as suspect and check
# `cargo build`'s own exit code, not just its wall time.
```
