# ⛏️ Prospect: does the debuginfo cold-start win hold on the real scaffolded-project harness, not the `examples/hello` proxy? (undetermined: registered -14.48% vs 20% line, non-overlapping ranges, one unconfirmed exploratory gate FAIL)

## 🎯 Question

Two prior reports on issue #2795 — Onramp's 2026-09-17 cold-start findings
and this role's own 2026-09-21 warm-edit follow-up
(`docs/reports/2026-09-21-prospect-debuginfo-warm-edit-rebuild-cost.md`) —
both measured the `-C debuginfo` lever using proxies: `cargo build -p
autumn-web` directly, or `examples/hello`, not the actual `autumn
new`-scaffolded no-DB daemon project the real gate
(`.github/workflows/cold-start-latency.yml`, budget p50 100000ms / p95
130000ms / max 160000ms) measures. Both reports named this as an explicitly
open gap ("gap 1") blocking a template-default decision.

**Falsifiable question:** using the real harness
(`autumn-cli/src/cold_start_driver.rs`, invoked via `autumn dev-loop-bench
--cold-start`, which runs `autumn new` → repoints `autumn-web` at this
workspace → cold `cargo build` → boots → waits for a real `200`), does
setting `[profile.dev] debug = 1` ("limited") in the generated-project
Cargo.toml template change the measured cold-start time by an amount that
matters to the pending decision, or was the win reported by proxy measurement
an artifact of the proxy?

**Decision fed:** issue #2795 — which debuginfo level, if any, to set in
`autumn-cli/src/templates/Cargo.toml.tmpl` / `Cargo.api.toml.tmpl`'s
`[profile.dev]`. **Decider:** repo maintainer (same decider both prior
reports named).

## ⚖️ Pre-registration

Committed to this session's scratchpad
(`prereg-coldstart-debuginfo.md`) before the registered comparison began.
One exploratory run preceded it — disclosed below, not part of the
registered comparison.

- **Materiality line (reused, not re-derived): ≥20% relative reduction in
  measured cold-start time, matching Onramp's own floor** for this same
  lever on this same kind of measurement.
- **Second, independent criterion, pre-registered alongside the first:**
  does the condition change whether the existing gate's own budget
  (`p95 <= 130000ms`) passes or fails on this box.
- **Conditions:** this sandbox, single box, `rustc`/`cargo` 1.94.1
  (`rustc 1.94.1 (e408947bf 2026-03-25)` — added here per Codex review on
  PR #2993, which noted an earlier draft never stated the assay's compiler
  version despite measuring a predominantly-compile-time quantity),
  `autumn dev-loop-bench --cold-start --runs 1` per sample (no
  `--include-db`), warm `CARGO_HOME` registry cache from earlier work this
  session.
- **Time box:** this session, target ≲30 min of additional building (a
  condition switch costs a `cargo build -p autumn-cli` rebuild since
  templates are embedded via `include_str!` at CLI-compile time, plus
  ~100-130s per cold-start sample).
- **Riskiest assumption first:** that the harness can run live in this
  sandbox at all. Onramp's 2026-09-17 report found `crates.io` (the web
  frontend) returns `403` here; if the scaffolded throwaway project's own
  dependency resolution needed that host rather than
  `index.crates.io`/`static.crates.io`, gap 1 would be untestable in this
  sandbox and that finding would be the report.
- **Control:** current template default — no `[profile.dev]` override
  (`debug = true` / `-C debuginfo=2`).
- **Containment:** one local, uncommitted edit to
  `autumn-cli/src/templates/Cargo.toml.tmpl` (adding then removing
  `[profile.dev]\ndebug = 1`), reverted before this report was filed
  (`git status`/`git diff` confirmed clean afterward). No CI or template
  change proposed by this assay. No production data, no dependency added.

## 🔍 Prior art

- Onramp 2026-09-17 (`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md`):
  proxy-measured `-p autumn-web` build, found `debug=0` ~18% and `debug=1`
  ~8.7% cold-start wins, explicitly flagged both the `examples/hello`-proxy
  gap and the untested-in-this-sandbox `crates.io`-403 risk for `-Z
  self-profile` tooling (a different, unrelated blocker, cited here only
  because it's the origin of the network-egress risk this assay's riskiest
  assumption reused).
- Prospect 2026-09-21 (`docs/reports/2026-09-21-prospect-debuginfo-warm-edit-rebuild-cost.md`):
  confirmed a large, properly-replicated warm-edit win for the same
  settings on the `examples/hello` proxy, and named "re-measure against the
  actual `autumn new`-scaffolded project via `cold_start_driver.rs`" as its
  own still-open cost-to-productionize item — this assay exists to close
  exactly that item, not to re-dig either prior pit.
- No existing report has run `cold_start_driver.rs` live with a
  `[profile.dev]` override — this is new ground.

## 🧪 Apparatus

Block design, matching the 2026-09-21 report's methodology: each condition
gets its own CLI build (template is baked in via `include_str!`), then N
timed samples of `autumn dev-loop-bench --cold-start --runs 1` in that
block.

Sequence actually run: baseline (r1, r2) → edit template, rebuild →
`debug=1` (r1, r2) → revert template, rebuild → baseline (r3, reversal
check).

**Stubs / shortcuts (the complete list):**
- One exploratory run (baseline r1) happened before the pre-registration
  file was written, to answer the riskiest-assumption question (does the
  harness even run here) before committing to a full block design. Its
  result (130327ms, a live gate FAIL) is reported below as data, not
  hidden, but is not treated as more authoritative than the other two
  baseline samples taken after pre-registration.
- `--runs 1` per invocation, called repeatedly, rather than `--runs N`
  in one invocation — equivalent (each `--runs 1` call is a fully
  independent fresh tempdir + scaffold + cold build), but means the
  harness's own p50/p95/max columns each report a single sample per line,
  not a real percentile; this report's own median/mean over the separate
  JSON reports is the actual statistic.
- Small n (2-3 per condition): each condition switch's CLI rebuild plus
  ~100-130s per sample made a larger n costly within the time box. This is
  disclosed as a real limitation, not stretched by extrapolation.
- Single box, not the actual `ubuntu-latest` GitHub Actions runner the
  real gate executes on — absolute numbers (and how close 130327ms sits to
  the 130000ms budget) may not transfer directly; the box class was not
  verified to match `cold-start-latency.yml`'s runner.
- Only the no-DB (`Hello`) shape was measured (`--include-db` omitted) —
  matches the gated budget, not the informational DB-backed shape.
- The scaffolded throwaway project's own dependency graph was never pinned
  or preserved (see **🔬 Reproduce**): `autumn new` emits no `Cargo.lock`,
  and each tempdir was discarded after its sample. Only the Autumn-side
  source is exactly reproducible; the resolved dependency graph is not.

## 📊 Assay

Wall-clock, one sample per `autumn dev-loop-bench --cold-start --runs 1`
invocation, each a genuine fresh `autumn new` → cold build → boot → first
`200`:

| Condition | samples (ms) | median (all data) | mean (all data) |
|---|---|---|---|
| baseline (`debug=2`, current default) | 130327 (FAIL vs p95 130000, **exploratory, pre-registration — see ⚖️ Pre-registration**), 114966 (PASS), 111781 (PASS) | 114966 | 119025 |
| `debug = 1` (`limited`) | 99355 (PASS), 94551 (PASS) | 96953 | 96953 |

**Correction (caught by Codex review on PR #2993): the "all data" median/mean
above, and this report's earlier headline percentages, folded the
exploratory r1 sample into the baseline statistic despite this report's own
Pre-registration section saying r1 predates registration and is not part
of the registered comparison.** The figure that should actually be checked
against the pre-set 20% line uses only the two post-registration baseline
samples (r2=114966, r3=111781):

| Comparison | baseline stat | `debug=1` stat | relative change |
|---|---|---|---|
| **Registered only** (r2, r3 vs. `debug=1` r1, r2) | median = mean = 113373.5 | median = mean = 96953 | **-14.48%** |
| All data, incl. exploratory r1 (reported as a secondary, exploratory summary, not the registered figure) | median 114966, mean 119025 | median = mean = 96953 | median -15.67%, mean -18.52% |

Range check: registered baseline `[111781, 114966]`, `debug=1` `[94551,
99355]` — **completely non-overlapping**, a 12426ms gap between `debug=1`'s
slowest sample and baseline's fastest (unchanged whether or not the
exploratory r1 sample is included, since r1 was the baseline's *slowest*
sample, not its fastest).

Further relative-change pairings, all computed against the registered
baseline samples only (none cherry-picked as the headline without showing
the others):

- **least-favorable pairing** (slowest `debug=1` vs. fastest registered
  baseline, i.e. the smallest defensible effect this data supports): -11.12%
- **most-favorable pairing** (fastest `debug=1` vs. slowest registered
  baseline): -17.76%

**Gate-stability finding (exploratory, not registered — see the correction
in 🏁 Verdict):** one of the three baseline samples (r1, 130327ms, the
pre-pre-registration run) exceeded the existing gate's own p95 130000ms
budget — a live, organic FAIL on the *current default*, on this box, with
no lever applied. The two baseline samples taken *after* registration (r2,
r3) both passed, so this FAIL is not independently confirmed by the
registered comparison alone. Neither `debug=1` sample came close to that
budget (max 99355ms, 76% of budget). Reported as real data, not hidden,
but its evidentiary weight is that of a single exploratory observation:
the gate this decision feeds *may* not be comfortably green today, close
enough to its own budget to fail on ordinary run-to-run variance — a
hypothesis this assay raises but does not itself confirm.

**Worst case:** each sample already *is* a worst-case-shaped measurement
by construction (a genuinely cold, from-scratch build in a fresh tempdir,
not an incremental rebuild) — there is no additional adversarial input to
probe for this specific question.

## 🏁 Verdict

**Undetermined against the pre-set 20% materiality line — a clear miss
using only the registered comparison, not just a narrow one.** **Correction
(caught by Codex review on PR #2993): an earlier draft of this paragraph
led with the all-data figures (median -15.67%, mean -18.52%), which fold in
the exploratory pre-registration baseline sample (r1) this report's own
Pre-registration section says is excluded from the registered comparison.**
Using only the two post-registration baseline samples, the registered
figure is **-14.48%** (see **📊 Assay**) — further from the 20% line than
the all-data figures suggested, not closer. The least-favorable pairing
(-11.12%) falls further still. Per this role's own rule, a miss against a
pre-set line is a *no*, not a quiet adjustment — so this assay does not
claim the 20% floor is cleared. Separately: an earlier draft of this
paragraph also said the result "closely tracks Onramp's own proxy-measured
~18% cold-start finding for the same lever." That was wrong too — Onramp's
~18% figure belongs to the *different* `debug=0` condition; Onramp's own
`debug=1` finding was ~8.7%, from a thin, single-block sample of a
non-incremental `-p autumn-web` build. This assay's registered `debug=1`
result (-14.48%) is directionally consistent with Onramp's `debug=1`
finding but is not a close replication of it — it reads noticeably larger,
on a different workload (the actual scaffolded project, not a direct crate
build), so the two numbers are not the same measurement landing twice; they
merely agree on sign.

**The second, pre-registered criterion is more decision-relevant, but its
strength needs a correction too.** **Correction (caught by Codex review on
PR #2993): an earlier draft called this criterion "clearly cleared" and
"independently pre-registered."** The only baseline sample that actually
failed the gate is r1 — the exploratory run that happened *before* the
pre-registration file was written (see **🧪 Apparatus**). Both baseline
samples taken *after* registration (r2, r3) passed. So the registered
comparison alone does not independently demonstrate a gate failure; the
one FAIL in this report rests on data collected ahead of the criterion it's
now cited against. The honest framing is: this criterion is **not**
cleared by the registered dataset on its own — it is an exploratory,
pre-registration observation, reported as data (not hidden), that a
production-representative sample would need to confirm or refute with its
own post-registration failure before the decider should weigh it. What the
registered comparison *does* support independently: the complete
non-overlap between the two conditions' ranges (not a formal significance
test, but a real, visible separation given only 2 registered samples per
side) —
this is not "no effect on the real harness," proxy measurement was not an
artifact — but this specific run's n is too small, and too close to the
pre-set 20% line on the central estimate, to hand the decider a clean
"pursue."

**What gap 1 actually resolves to:** the harness runs live in this sandbox
without hitting the `crates.io`-403 risk this assay's own riskiest
assumption named (dependency resolution used `index.crates.io`/
`static.crates.io` successfully every time) — so gap 1 is now proven
*testable*, cheaply, and the apparatus built here (a template edit +
rebuild + `dev-loop-bench --cold-start`) is reusable directly. The open
item is precision, not feasibility.

## 💰 Cost to productionize

Not a pursue verdict, so no build is proposed. What a confirmatory
follow-up needs, scoped from this assay's own stubs list:

- Re-run this exact block design with n≥5 per condition (this assay's own
  numbers show baseline alone spans ~15% run-to-run on this box, so n=2-3
  cannot pin the estimate tightly against a 20% line).
- Run on the same runner class `cold-start-latency.yml` actually uses
  (`ubuntu-latest`), not this sandbox, before the absolute 130000ms-budget
  proximity finding is treated as representative of the real gate.
- The gate-stability finding (one gate FAIL out of 3 baseline samples) is
  itself worth a short, separate note to the decider even independent of
  which debuginfo level is chosen — the existing budget has less headroom
  on ordinary hardware than a run of all-green scheduled gate executions
  might suggest.
- Gates: `cold-start-latency.yml`'s `measure` job is the natural place to
  run this on the right box class, but **correction (caught by Codex review
  on PR #2993): a single `workflow_dispatch` with `runs: 5` does not
  reproduce this comparison.** The workflow checks out one ref and builds
  whatever `[profile.dev]` is committed in that ref's own template — it
  takes no input to toggle `debug`, so one dispatch measures only one
  condition, not both, and cannot perform the baseline→`debug=1`→baseline
  reversal this assay used to separate the effect from drift. **Further
  correction (caught by Codex review on PR #2993, second round): "two
  dispatches, one per condition" is not sufficient either.** Each
  `workflow_dispatch` provisions its own fresh `ubuntu-latest` runner, so
  all 5 samples of one dispatch share one machine and all 5 of the other
  share a different machine — runner-to-runner performance variance would
  then be perfectly confounded with condition, exactly the position/order
  confound this assay's own local reversal checks existed to rule out, and
  two single dispatches cannot rule it out. A real confirmatory follow-up
  needs **multiple dispatches per condition, interleaved across separate
  runner allocations** (e.g. baseline → `debug=1` → baseline → `debug=1`,
  each its own dispatch on its own fresh runner, comparing dispatch-level
  medians across conditions the way this assay compared same-box blocks) —
  not one dispatch per condition, and not a claim that same-day timing
  alone substitutes for that.

## 🔬 Reproduce

```bash
# Pinned to this PR's actual base commit (caught by Codex review on PR
# #2993: an earlier draft of this recipe said "current trunk-dev tip," a
# floating reference that resolves to different template/harness/
# dependency code as trunk-dev advances, defeating reproduction of the
# numbers this report reports). CORRECTION (caught by Codex review on PR
# #2993, second round): an earlier draft left these two lines as comments,
# so copying the block skipped them and built against the caller's current
# checkout instead of the pinned commit. Made executable:
set -e
git worktree add --detach /tmp/prospect-coldstart-repro c304e8f89c7a3c7f1fb58c23bf4175904633eb5d
cd /tmp/prospect-coldstart-repro

# Pin the exact toolchain this assay measured with (caught by Codex review
# on PR #2993: an earlier draft left the active toolchain unpinned — this
# repo ships no rust-toolchain.toml, only an MSRV, so a bare `cargo` here
# resolves whatever "stable" means on the calling machine, not necessarily
# the toolchain every number in this report was measured on):
rustup toolchain install 1.94.1
rustup override set 1.94.1
[ "$(rustc --version)" = "rustc 1.94.1 (e408947bf 2026-03-25)" ] || { echo "toolchain pin failed: got $(rustc --version)" >&2; exit 1; }
# DISCLOSED LIMITATION (caught by Codex review on PR #2993): pinning this
# commit pins the Autumn source (templates, harness, `autumn-web` itself),
# but NOT the scaffolded throwaway project's own dependency graph.
# `autumn new` emits no `Cargo.lock`, and `cold_start_driver` runs an
# unlocked `cargo build` in a fresh tempdir for every sample — so as
# `maud`/`diesel_migrations`/`tokio`/their transitive deps publish new
# compatible releases over time, a later run of this recipe resolves a
# different dependency graph than this assay measured, even pinned to the
# same commit. This assay did not preserve the generated `Cargo.lock` from
# its own runs (each ran in an ephemeral tempdir since discarded), so
# reproducing the *exact* graph measured here is not currently possible —
# only the Autumn-side inputs are pinned. A follow-up wanting exact
# reproduction should capture and commit the scaffolded project's
# `Cargo.lock` alongside its own report.
# IMPORTANT (caught by Codex review on PR #2993): each `--runs 1` sample
# below must write to its OWN output path — repeating the same fixed path,
# as an earlier draft of this recipe did, silently overwrites the previous
# sample and leaves no way to recover the multi-sample median/mean this
# report computes. Use an incrementing index (or run `--runs N` once per
# condition instead, which reports its own genuine percentiles from N
# samples in one JSON file, at the cost of not reproducing this report's
# own block-switching-between-single-run-invocations method exactly).

# IMPORTANT (caught by Codex review on PR #2993, second round): this
# assay's actual sequence was baseline(r1,r2) -> debug=1(r1,r2) ->
# baseline(r3) — not all 3 baseline samples back to back. Collecting all
# three baseline samples in one loop, as an earlier draft of this recipe
# did, omits the reversal check (a third baseline sample collected AFTER
# the debug=1 block, to separate the condition effect from the ~15%
# session-wide drift this report's own baseline samples show) and cannot
# reproduce that part of the design. Split as below.

# Baseline block 1 (r1, r2):
cargo build -p autumn-cli
for i in 1 2; do
  ./target/debug/autumn dev-loop-bench --cold-start --runs 1 \
    --output "/tmp/coldstart-baseline-r${i}.json"
done

# debug=1 ("limited") condition (r1, r2):
printf '\n[profile.dev]\ndebug = 1\n' >> autumn-cli/src/templates/Cargo.toml.tmpl
cargo build -p autumn-cli
for i in 1 2; do
  ./target/debug/autumn dev-loop-bench --cold-start --runs 1 \
    --output "/tmp/coldstart-debug1-r${i}.json"
done

# Revert, then take the reversal-check baseline sample (r3) AFTER the
# debug=1 block, not batched with r1/r2:
git checkout -- autumn-cli/src/templates/Cargo.toml.tmpl
cargo build -p autumn-cli   # restores the baseline binary
./target/debug/autumn dev-loop-bench --cold-start --runs 1 \
  --output "/tmp/coldstart-baseline-r3.json"
git status --short   # must be clean
```

Each invocation is one independent sample (a fresh `autumn new` in a new
tempdir, repointed at this workspace's `autumn-web`, cold `cargo build`,
boot, first `200`). Repeat per condition for more samples; alternate
condition blocks (not a flat interleave, since each condition switch costs
a CLI rebuild) to guard against session-wide drift, as this assay did.
