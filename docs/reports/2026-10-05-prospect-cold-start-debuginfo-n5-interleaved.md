# ⛏️ Prospect: does `[profile.dev] debug = 1` cut real-harness cold start by ≥20% at n=5? (undetermined: -12.75% median vs 20% line; non-overlapping ranges, above the 10% kill line)

## 🎯 Question

Re-charter of `2026-09-28-prospect-cold-start-debuginfo-real-harness-gap1.md`, which ended
undetermined at n=2 per side (-14.48%) and named n≥5 as the missing input.
**Falsifiable:** on the real harness (`autumn dev-loop-bench --cold-start --runs 1`), does
`[profile.dev] debug = 1` in the scaffolded `Cargo.toml` cut median cold-start time by ≥20%
versus the current default?
**Decision fed:** issue #2795 (template debuginfo default). **Decider:** repo maintainer.

## ⚖️ Pre-registration

`docs/reports/assay-prereg/2026-10-05-coldstart-debuginfo-n5-prereg.md`, commit **f830162**,
committed before the apparatus was built. Pursue: median reduction ≥20% and non-overlapping
ranges. Kill: <10%. Between (or overlap): undetermined. Time box: one session. Riskiest
assumption: baseline noise (~15–20% spread) swamps the effect at n=5. Lines unchanged below.

## 🔍 Prior art

The 09-28 report (n=2/side), the 09-21 warm-edit report and Onramp's 09-17 findings. New
information is only the larger n and interleaving; nothing re-assayed without it.

## 🧪 Apparatus

Two release builds of `autumn-cli` from a scratch `git archive` of trunk-dev at the
pre-registration commit, identical except `[profile.dev]\ndebug = 1` appended to
`templates/Cargo.toml.tmpl` in one. Ten fresh-tempdir cold starts run interleaved
B,D,B,D,… (5 each) by `run.sh` (below). Repo tree untouched; apparatus not committed.

**Stubs / limits:** single 4-vCPU sandbox, not `ubuntu-latest`; no-DB shape only; scaffold
pins its own toolchain (`1.88.0` per `new.rs`); the `Cargo.toml.tmpl` edit only (the API
template is not covered).
**Deviation from the registered conditions:** the plan said "warmed CARGO_HOME", but the
registry cache was cold, so baseline sample 1 (136307 ms) paid one-time warm-up and is **not a
registered-condition sample**. Per Codex review on PR #3139 it is excluded from the primary
numbers and replaced by a baseline sample 6 taken afterwards on the warmed cache. Sample 6 was
run after the interleaved block, not inside it (an ordering confound, disclosed). Sample 1 is
still listed below as a run that happened.

## 📊 Assay

Harness-reported cold-start ms, every run:

| i | baseline | debug=1 |
|---|---|---|
| 1 | 136307 (cold cache; excluded, not a registered sample) | 101268 |
| 2 | 114976 | 99724 |
| 3 | 112291 | 99118 |
| 4 | 112954 | 101272 |
| 5 | 116371 | 100317 |
| 6 | 155322 (replacement, run after the block) | — |

Registered set: baseline = samples 2–6, debug=1 = samples 1–5.
- Baseline median 114976 (range 112291–155322); debug=1 median 100317 (range 99118–101272).
- **Median reduction: 12.75%.** (Including cold-cache sample 1 as well: also 12.75%; the
  median is robust to both choices.)
- Non-overlap holds (max debug=1 101272 < min baseline 112291).
- Spread: debug=1 ~2%; baseline 112–155s. Sample 6 is a 155s outlier, so baseline variance
  is larger than the 4% of samples 2–5 alone suggested.
- Gate budget (p95 130000ms): baseline 1/5 registered samples exceeded it (sample 6, 155322ms,
  warm cache); debug=1 0/5, max 77.9% of budget. Secondary, not verdict-bearing. Two of the
  seven baseline runs overall (1 and 6) exceeded the budget on this box.
- Worst case: each sample is already a from-scratch build.

## 🏁 Verdict

**Undetermined, by two independent routes under the pre-registration.**
1. **Noise rule:** the registered baseline spans 112291–155322 ms (~37% of its median), above
   the pre-registered 20% threshold, so the plan's own rule returns *undetermined-by-noise*.
   The 155s outlier (sample 6) is the cause; samples 2–5 alone span ~4%, but dropping an
   outlier after seeing it is exactly what the registration forbids.
2. **Lines:** the 12.75% median misses the 20% pursue line and clears the 10% kill line, so
   it is neither pursue nor kill. The line is not moved.

What the data supports, with that qualification: in all 5 paired runs `debug = 1` finished
faster than every baseline sample and its own spread was ~2%, in the same direction as the
09-28 assay. That is consistent with a real ~12% effect, but at n=5 with a noisy baseline it is
not a settled number, and the 12.75% median should be quoted only alongside the noise
verdict. Whether ~14 s on a ~115 s cold start justifies a template default (and its effect on
debugger/backtrace quality) is the decider's call. Together with 09-28 (-14.48%) and Onramp
(~8.7% by proxy), estimates span 9–14% and none reached 20%; a re-charter should reconsider
the 20% floor and use a quieter baseline (dedicated runner, more samples) rather than repeat
this one.

## 💰 Cost to productionize

Not a pursue verdict. If the decider adopts it anyway: two template lines
(`Cargo.toml.tmpl`, `Cargo.api.toml.tmpl`), a changelog fragment, and a check of the
scaffold/generator conformance tests that snapshot the template. Gates: Keystone for the
debuggability tradeoff, Echo for the docs note.

## 🔬 Reproduce

```bash
S=$(mktemp -d); git archive f830162 | (mkdir $S/ws && tar -x -C $S/ws)
cd $S/ws && CARGO_TARGET_DIR=$S/tgt cargo build -p autumn-cli --release && cp $S/tgt/release/autumn $S/autumn-base
printf '\n[profile.dev]\ndebug = 1\n' >> autumn-cli/src/templates/Cargo.toml.tmpl
CARGO_TARGET_DIR=$S/tgt cargo build -p autumn-cli --release && cp $S/tgt/release/autumn $S/autumn-d1
# warm CARGO_HOME first (one throwaway run), then, from $S/ws (run baseline once more
# afterwards if you want a 5th warmed sample like this report):
for i in 1 2 3 4 5; do for c in base d1; do
  $S/autumn-$c dev-loop-bench --cold-start --runs 1 --output $S/out-$c-$i.json; done; done
# read results[0].stats.p50_ms from each JSON
```
