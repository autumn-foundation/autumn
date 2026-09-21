# ⛏️ Prospect: is lower `-C debuginfo` a warm-edit rebuild win too, or cold-build-only? (pursue: -36% median vs 10% materiality line)

## 🎯 Question

The 2026-09-17 Onramp findings report
(`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md`,
PR #2829, issue #2795) measured `-C debuginfo=0` giving a ~18% cold-start
build win (close to, but on the honest pooled number just under, Onramp's own
20% floor) and `-C debuginfo=1` (line-tables-only) an ~8.7% win, but named two
explicit gaps before a human could decide which level (if any) to set in the
generated-project templates' `[profile.dev]`. Gap 2, verbatim: *"Only the cold
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
recompile, never a no-op). Two rounds, condition order interleaved
(baseline → d0 → d1 → d1 → baseline → d0) to control for drift across the
~35-minute session, matching the 2026-09-16 report's methodology.

**Stubs / shortcuts (the complete list):**
- Measures `cargo build -p hello` directly, not the actual `autumn dev`
  live-reload loop `dev-loop-latency.yml` gates (file-watcher trigger latency
  and the health-check poll after the binary restarts are not included) —
  compile+link is the dominant, and the only debuginfo-sensitive, component
  of that loop, but the absolute numbers here are not directly comparable to
  the gate's own reported figures, only the *relative* delta between
  conditions is (same caveat shape Onramp's own report gave for its
  cold-build proxy).
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

## 📊 Assay

Wall clock, steady-state runs only (each block's first run excluded as
warm-up; see Apparatus), n=6 per condition (2 rounds × 3 samples):

| Condition | samples (s) | median | mean | stdev |
|---|---|---|---|---|
| baseline (`debug=2`, current default) | 4.022, 4.101, 3.951, 3.998, 4.096, 4.042 | 4.032 | 4.035 | 0.058 |
| `-C debuginfo=0` (none) | 2.630, 2.597, 2.622, 2.512, 2.584, 2.491 | 2.591 | 2.573 | 0.058 |
| `-C debuginfo=1` (line-tables-only) | 2.560, 2.499, 2.472, 2.532, 2.517, 2.681 | 2.524 | 2.543 | 0.074 |

Relative to baseline median: **`debuginfo=0` -35.75%**, **`debuginfo=1`
-37.39%**.

**Correction (caught by Codex review on PR #2882): an earlier draft of this
paragraph compared the `debuginfo=1`-vs-`debuginfo=0` percentage delta
(-2.56%) directly against the two conditions' stdevs in seconds (0.058,
0.074) and called it "smaller than either condition's own stdev" — invalid,
since it compares a dimensionless percentage to an absolute quantity in
different units. Redone properly:** mean `debuginfo=0` = 2.5727s (sd
0.0580), mean `debuginfo=1` = 2.5435s (sd 0.0738); the mean difference is
-0.0292s (-1.14% of `debuginfo=0`'s mean). A Welch's t-test (unequal
variance, n=6 each) gives t ≈ -0.76, df ≈ 9.5 — not statistically
significant at conventional thresholds. That is a **failure to find a
difference on n=6 samples per condition**, not a demonstrated equivalence:
this sample size cannot rule out a true difference of similar magnitude to
the observed one. The honest statement is narrower than the first draft's:
this data does not show `debuginfo=1` costing more than `debuginfo=0` on the
warm-edit axis, and a larger sample would be needed to state a tighter bound
— unlike the cold-build case, where Onramp's report found a large, clearly
resolved gap between them (~18% vs ~8.7%, no such ambiguity).

No condition's sample range overlaps another's (baseline min 3.951 > both
reduced-condition maxima; `debuginfo=0`/`debuginfo=1` overlap each other
completely) — a clean separation on 6 samples each, not a borderline call
the way Onramp's cold-build `debuginfo=0` number was against its 20% floor.

**Worst case probed:** the warm-up (first-in-block) samples, deliberately
excluded from the table above because they are not steady-state, are
themselves informative: baseline/`d0`/`d1` warm-ups ran 8.5s / 198.7s (first
entry into that condition, full-graph rebuild) / 200.9s (same, first entry) —
confirming the one-time "switching tax" is real and large, which is exactly
why it's excluded from a measurement about the *recurring* per-edit cost, not
folded in as if it happened on every edit.

## 🏁 Verdict

**Pursue** (material) — against the pre-set 10% line: both reduced debuginfo
levels change warm-edit median rebuild time by far more than 10% (-35.75% /
-37.39%), the opposite direction of the risk this assay was chartered to
probe. This is not a hidden recurring *cost* the Onramp report's open gap
worried about — it is a large recurring *win*, on the far more frequent warm
loop, that stacks with the (borderline) one-time cold-start win.

This changes the shape of the pending decision, not just its confidence:

1. **The trade-off is not "one-time cold-start win vs. permanent backtrace
   quality cost paid on every build,"** as the Onramp report framed it. It's
   "one-time cold-start win *and* a large, recurring per-edit win, vs. a
   permanent backtrace-quality cost" — the compile-time side of the ledger is
   bigger than Onramp's report alone showed, because most of a
   development session's builds are warm edits, not cold starts.
2. **`debuginfo=1` (line-tables-only) is no longer clearly the "smaller win"
   option.** On the cold build, Onramp measured it giving less than half of
   `debuginfo=0`'s saving (8.7% vs 18%), a large, clearly-resolved gap. On the
   warm edit — the loop a developer actually sits in for most of a session —
   this assay's n=6-per-condition sample found no statistically significant
   difference between the two (mean difference -0.0292s / -1.14%, Welch's
   t ≈ -0.76, df ≈ 9.5; see the correction in **📊 Assay**), though that is a
   failure to find a difference on a small sample, not a proof the two are
   equal. That still reopens the choice Onramp's report posed as a hard trade
   (bigger win vs. keeping backtraces): on the warm-edit axis specifically,
   this data gives no evidence that line-tables-only's full file:line
   backtrace resolution for local frames (the quality property Onramp's
   report measured directly) costs anything extra relative to the more
   aggressive `debuginfo=0` option — a claim a larger sample could sharpen
   further, in either direction.

This still does not resolve the decision by itself — gap 1 (the actual
scaffolded no-DB daemon project, not `examples/hello`) remains open, and
Onramp's own cold-build number is still shy of its 20% floor pending
re-measurement above the noise floor. But it removes "we don't know if this
is a hidden recurring cost" from the open-questions list, and replaces it
with a specific, load-bearing number the decider can weigh: the recurring win
is large, and `debuginfo=1` captures nearly all of it.

## 💰 Cost to productionize

Not a new build — this assay feeds an existing decision (issue #2795) rather
than proposing new code. If the maintainer picks `debug = "line-tables-only"`
for the generated-project templates' `[profile.dev]` (the option this
assay's finding makes more attractive, since it now captures ~99% of the
warm-edit win alongside its already-known backtrace-preservation property):
the change itself is the one line Onramp's report already scoped
(`autumn-cli/src/templates/Cargo.toml.tmpl`, `Cargo.api.toml.tmpl`), plus the
two still-open items neither report has closed:

- Gap 1 (still open, either report): re-measure against the actual
  `autumn new`-scaffolded project via `cold_start_driver.rs`, not
  `examples/hello`/`-p autumn-web` proxies, before shipping a template
  default change.
- Re-measure `debuginfo=0`'s cold-build number above the noise floor / on a
  dedicated or CI-caliber box, per Onramp's report (only relevant if
  `debuginfo=0` rather than `debuginfo=1` is the level under consideration).
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
# uncommitted changes (caught by Codex review on PR #2882: the `git
# checkout -- ...` revert step below discards *any* uncommitted edits to
# these two files, not just the experiment's own, and this recipe neither
# requires a clean tree nor backs up what was there first):
#   git worktree add /tmp/prospect-debuginfo-repro trunk-dev
#   cd /tmp/prospect-debuginfo-repro

# Pre-warm deps + autumn-web once:
cargo build -p hello

# One condition's block (repeat per condition; see Apparatus for why
# switching conditions mid-session is not representative and each needs its
# own warm-up):
cat >> Cargo.toml <<'EOF'

[profile.dev]
debug = 0            # or: debug = "line-tables-only"   (must be quoted!)
EOF

# Pay the one full-graph rebuild to enter the block (excluded from timing):
cargo build -p hello

# Then, 3+ times, alternating the literal so each edit is a real recompile.
# Use an actually-incrementing counter (caught by Codex review on PR #2882:
# a literal `vN` in the sed pattern below is not a variable -- every
# invocation after the first replaces "Hello, Autumn! vN" with the
# byte-identical "Hello, Autumn! vN", which is not a real edit and cannot
# reproduce the reported samples) and check cargo's own exit status
# explicitly (caught in the same review round: the original semicolon-chained
# `date; cargo build; date` form here still ran the trailing `date` and
# looked "successful" even when `cargo build` failed -- the exact failure
# mode this report's Apparatus section says invalidated the first `d1` pass)
# rather than silently recording a fast failure as a real sample:
i=0
while [ "$i" -lt 3 ]; do
  i=$((i + 1))
  sed -i "s/\"Hello, Autumn![^\"]*\"/\"Hello, Autumn! v${i}\"/" examples/hello/src/main.rs
  S=$(date +%s.%N); cargo build -p hello || { echo "cargo build failed" >&2; exit 1; }; E=$(date +%s.%N)
  echo "$E - $S" | bc   # no /usr/bin/time in this sandbox
done

# Revert between conditions:
git checkout -- Cargo.toml examples/hello/src/main.rs

# Sanity check for the TOML-quoting bug this assay hit: a manifest parse
# error makes `cargo build` fail (and "finish") in tens of milliseconds, not
# seconds — treat any sub-1s "successful" build here as suspect and check
# `cargo build`'s own exit code, not just its wall time.
```
