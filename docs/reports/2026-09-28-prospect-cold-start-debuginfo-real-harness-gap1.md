# ⛏️ Prospect: does the debuginfo cold-start win hold on the real scaffolded-project harness, not the `examples/hello` proxy? (undetermined: -15.7% median vs 20% line, but non-overlapping ranges and a live gate failure)

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
- **Conditions:** this sandbox, single box, `autumn dev-loop-bench
  --cold-start --runs 1` per sample (no `--include-db`), warm `CARGO_HOME`
  registry cache from earlier work this session.
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

## 📊 Assay

Wall-clock, one sample per `autumn dev-loop-bench --cold-start --runs 1`
invocation, each a genuine fresh `autumn new` → cold build → boot → first
`200`:

| Condition | samples (ms) | median | mean |
|---|---|---|---|
| baseline (`debug=2`, current default) | 130327 (FAIL vs p95 130000), 114966 (PASS), 111781 (PASS) | 114966 | 119025 |
| `debug = 1` (`limited`) | 99355 (PASS), 94551 (PASS) | 96953 | 96953 |

Range check: baseline `[111781, 130327]`, `debug=1` `[94551, 99355]` —
**completely non-overlapping**, a 12426ms gap between `debug=1`'s slowest
sample and baseline's fastest.

Relative change, several ways (none cherry-picked as the headline without
showing the others):

- vs. baseline **median**: -15.67%
- vs. baseline **mean**: -18.52%
- **least-favorable pairing** (slowest `debug=1` vs. fastest baseline,
  i.e. the smallest defensible effect this data supports): -11.12%
- **most-favorable pairing** (fastest `debug=1` vs. slowest baseline): -27.46%

**Gate-stability finding:** one of the three baseline samples (r1,
130327ms, the exploratory pre-pre-registration run) exceeded the existing
gate's own p95 130000ms budget — a live, organic FAIL on the *current
default*, on this box, with no lever applied. Neither `debug=1` sample came
close to that budget (max 99355ms, 76% of budget). This is new information
neither prior report had: the gate this decision feeds is not comfortably
green today: it is close enough to its own budget to fail on ordinary
run-to-run variance, and this lever moves the measured value well clear of
that edge in every sample taken.

**Worst case:** each sample already *is* a worst-case-shaped measurement
by construction (a genuinely cold, from-scratch build in a fresh tempdir,
not an incremental rebuild) — there is no additional adversarial input to
probe for this specific question.

## 🏁 Verdict

**Undetermined against the pre-set 20% materiality line — narrow miss on
the central estimate, cleared only by the most-favorable, non-representative
pairing.** Median (-15.67%) and mean (-18.52%) both fall short of the
pre-registered 20% floor; the least-favorable pairing (-11.12%) falls well
short. Per this role's own rule, a miss against a pre-set line is a *no*,
not a quiet adjustment — so this assay does not claim the 20% floor is
cleared, even though the direction and rough size closely track Onramp's
own proxy-measured ~18% cold-start finding for the same lever.

**The second, independently pre-registered criterion is clearly cleared,
and is arguably the more decision-relevant fact:** the current default
already produced a live gate failure in this small sample (1 of 3 runs),
and every `debug=1` sample landed comfortably inside budget with room to
spare. Combined with the complete non-overlap between the two conditions'
ranges (not a formal significance test, but a real, visible separation
given only 2-3 samples per side), this is not "no effect on the real
harness" — proxy measurement was not an artifact — but this specific run's
n is too small, and too close to the pre-set line on the central estimate,
to hand the decider a clean "pursue" against the 20% floor as registered.

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
- Gates: `cold-start-latency.yml`'s own scheduled `measure` job is the
  natural place a confirmatory n≥5 run belongs (via `workflow_dispatch`
  with `runs: 5`), rather than another local sandbox apparatus — it
  already runs on the right box class and already emits the JSON report
  this analysis format consumes directly.

## 🔬 Reproduce

```bash
# From a clean checkout on this workspace's current trunk-dev tip.
cd autumn-cli

# Baseline (current default, no override):
cargo build -p autumn-cli
../target/debug/autumn dev-loop-bench --cold-start --runs 1 \
  --output /tmp/coldstart-baseline.json

# debug=1 ("limited") condition:
cd ..
# Append to autumn-cli/src/templates/Cargo.toml.tmpl:
printf '\n[profile.dev]\ndebug = 1\n' >> autumn-cli/src/templates/Cargo.toml.tmpl
cargo build -p autumn-cli
./target/debug/autumn dev-loop-bench --cold-start --runs 1 \
  --output /tmp/coldstart-debug1.json

# Revert before doing anything else:
git checkout -- autumn-cli/src/templates/Cargo.toml.tmpl
cargo build -p autumn-cli   # restores the baseline binary
git status --short   # must be clean
```

Each invocation is one independent sample (a fresh `autumn new` in a new
tempdir, repointed at this workspace's `autumn-web`, cold `cargo build`,
boot, first `200`). Repeat per condition for more samples; alternate
condition blocks (not a flat interleave, since each condition switch costs
a CLI rebuild) to guard against session-wide drift, as this assay did.
