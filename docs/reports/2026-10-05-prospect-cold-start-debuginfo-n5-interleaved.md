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
registry cache was cold. Sample base-1 (the first run) therefore paid one-time warm-up
(downloads, possibly toolchain install). It is reported in the primary numbers; the
excluded-r1 figure is a disclosed secondary, not the verdict.

## 📊 Assay

Harness-reported cold-start ms, every run:

| i | baseline | debug=1 |
|---|---|---|
| 1 | 136307 (warm-up) | 101268 |
| 2 | 114976 | 99724 |
| 3 | 112291 | 99118 |
| 4 | 112954 | 101272 |
| 5 | 116371 | 100317 |

- Baseline median 114976 (range 112291–136307); debug=1 median 100317 (range 99118–101272).
- **Median reduction: 12.75%.** Excluding warm-up sample base-1: 11.98%.
- Non-overlap holds (max debug=1 101272 < min baseline 112291). debug=1 spread is ~2%,
  baseline ~21% with warm-up (~4% without).
- Gate budget (p95 130000ms): baseline 1/5 exceeded it (the warm-up sample), debug=1 0/5;
  debug=1's max is 78% of budget. Secondary, not verdict-bearing.
- Worst case: each sample is already a from-scratch build.

## 🏁 Verdict

**Undetermined against the registered lines.** 12.75% misses the 20% pursue line and clears
the 10% kill line, so by the pre-registration this is neither pursue nor kill. The line is
not moved. What the data does settle: the effect is real and reproducible (non-overlapping
ranges, tight debug=1 spread, same sign as the earlier runs) but its size is ~12–13%, roughly
a 14s saving on a ~115s cold start. Whether that is worth a template default (and its
effect on debugger/backtrace quality) is a judgement call for the decider, not a number this
assay can supply. Together with 09-28 (-14.48%) and Onramp (~8.7% by proxy), the estimates
span 9–14%; none of three independent measurements reached 20%, so the 20% floor looks
unreachable for `debug = 1` — a re-charter should reconsider the floor explicitly rather
than re-run this one.

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
# warm CARGO_HOME first (one throwaway run), then, from $S/ws:
for i in 1 2 3 4 5; do for c in base d1; do
  $S/autumn-$c dev-loop-bench --cold-start --runs 1 --output $S/out-$c-$i.json; done; done
# read results[0].stats.p50_ms from each JSON
```
