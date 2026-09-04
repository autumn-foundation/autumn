# 🚦 Semaphore: CI health census, 2026-09-04

## 🎯 Verdict path

Merging into `trunk-dev` waits on `.github/workflows/ci.yml`'s `pull_request`
run: `meta` → `lint` (+ `migration-guides`, `plugin-contract`, `supply-chain`
in parallel) → `test` (matrix: ubuntu/macos/windows-latest) → `coverage` /
`loom`, plus `lint` → `windows-tier1` (`ci.yml:965-967`, the Windows Tier 1
journey job — this census's failure sample includes one hit against it)
and the standalone `msrv`, `sqlite-runtime`, `sim-sweep`, and
`edge-conformance` jobs. Correction to an earlier revision of this report:
`coverage` is not "non-blocking, Codecov only" — only its final upload
step tolerates failure (`fail_ci_if_error: false`, `ci.yml:767`); the many
`cargo llvm-cov` invocations in its "Generate coverage" step
(`ci.yml:676-761`) carry no such tolerance, so a real compile or test
failure there fails the job like any other. No ambient retry
wrapper exists anywhere in the workflow — a red run stays red until a human
clicks "Re-run failed jobs" or pushes a fix, so Law 1 (an untrustworthy green
is worse than a red) is not currently at risk from retry-laundering. That is
the one clean bill of health this census hands back.

Push events to `trunk-dev` share a `cancel-in-progress: true` concurrency
group keyed on the branch, so a fast merge cadence cancels the in-flight
post-merge verification run before it finishes — 44 of the last 50 push-
triggered runs sampled ended `cancelled`, not `success` or `failure`. That
run exists to prove the merged tip is green; when it can't outrun the next
merge it proves nothing. It is advisory today (the real gate is the PR-time
run, which does not share that concurrency group), so this is a symptom of
throughput, not a correctness hole — flagged here in case anyone treats a
post-merge trunk-dev run as a live health signal.

## 🌡️ Symptom

**Harness**: GitHub Actions API (`actions_list`/`get_job_logs` via the
`github` MCP server), no new CI spend incurred. Sampled the 100 most recent
`pull_request`-triggered `ci.yml` runs (2026-09-03 16:13 UTC → 2026-09-04
10:12 UTC, run numbers 8124-8230 — a 107-wide span for 100 rows because
`run_number` increments per-workflow across every trigger, `pull_request`
included but not exclusive, so a handful of numbers in that span belong to
this same workflow's `push` runs and aren't in this pull_request-only
sample — across 17 distinct PR branches, i.e. ~6 runs per branch on
average from iteration/rerun churn — one sitting's worth of this
repository's actual PR throughput, not a same-commit rerun campaign; see
Reproduce below for why that campaign is the right next step, not
something this census fabricated a rate for).

- 97 completed, 78 cancelled (superseded by a same-PR push, expected and
  excluded from the rate below). Of the 19 that reached a real verdict, the
  *latest* conclusion is 10 success / 9 failure — **9/19 ≈ 47.4%**, the
  number that matters (not the 9/97 ≈ 9.3% a denominator that still includes
  cancelled runs gives). Four of those 19 runs needed more than one attempt:
  2 first attempts failed and the rerun (attempt 2) came back green — those
  2 are correctly counted in the "10 success," their first-attempt red
  otherwise invisible in a conclusion-only count — and 2 more failed *again*
  on rerun, which is why they stand in the "9 failure" rather than being
  written off as one-off blips. Reading first-attempt-only instead of latest
  conclusion gives 11/19 ≈ 57.9% red on the first try; either framing is
  defensible, but they are not interchangeable, and this report uses the
  latest-conclusion framing (9/19) throughout.
- Failure-signature clustering, by job:

  | Job | Failures | Signature |
  |---|---|---|
  | `Lint` → `Clippy` | 2 | real clippy findings in WIP branches, both later fixed same-PR — not a suite defect |
  | `Test (macos-latest)` → `Run tests` | **6** | 3 distinct assertions, see below |
  | `Test (ubuntu-latest)` → `Run tests` | 1 | `starters::tests::embedded_saas_matches_example_saas` (same WIP-branch drift as below, also hit ubuntu) |
  | `Test (ubuntu-latest)` → `Run Docker-dependent tests` | 1 | unrelated to this census's macOS finding; not triaged further here |
  | `Windows Tier 1 journey` → app-builds-on-Windows | 1 | same WIP-branch compile error as the ubuntu Clippy hit, same PR |

  `Test (macos-latest)` failed in 6 of the 9 failing runs. One of those six
  (`embedded_saas_matches_example_saas`, a byte-for-byte starter/example
  drift check) is a genuine product-side catch in a still-in-progress
  branch — it also failed on ubuntu in the same run, so it says nothing
  macOS-specific and is excluded below. The other 5 are all one of two
  cross-platform integration tests (`autumn/tests/integration/`, no
  platform `cfg`, compiled and run identically on every matrix leg) —
  **except** the dominant signature, `live_upgrade`, whose whole file is
  `#![cfg(unix)]` (`examples/hot-upgrade/tests/live_upgrade.rs:14`) and so
  structurally never runs on `windows-latest` at all. That means the fair
  comparison for `live_upgrade` is macOS-vs-ubuntu only, not the three-way
  split the earlier draft of this report used to argue "runner class, not
  code path" — a `cargo test --workspace` command being identical across
  the matrix does not make the set of tests it actually executes identical,
  and thanking Codex review for catching that distinction:

  1. **`live_upgrade::upgrades_in_place_under_load_without_dropping_a_
     connection_or_the_state`** — `assert_eq!(connect_errors, 0)` failed
     identically, `left: 1, right: 0` at
     `examples/hot-upgrade/tests/live_upgrade.rs:268`, on **three** unrelated
     branches within under 2 hours of each other by the job logs' own
     timestamps (20:51:17Z, 22:39:43Z, 22:44:42Z on 2026-09-03; runs 8132,
     8147, 8161) — all three on `macos-latest`, none on `ubuntu-latest`, the
     only other platform the test ever runs on. The real denominator, not
     just the failure count: of the 19 verdicted runs, 2 never reached the
     `test` matrix at all (blocked by an earlier `lint` failure), leaving 17
     that did. Of those, macOS's `Run tests` step failed 6 times (this test
     3 times, `cache_stampede` and `sim_fault_plan` once each, plus the
     unrelated `embedded_saas` drift) and succeeded 11 — and because a
     single failing test fails the whole `cargo test --workspace` step,
     every one of those 11 successes proves `live_upgrade` itself passed.
     That puts the macOS rate for this specific test between **3/17 ≈ 17.6%**
     (crediting the three runs whose `live_upgrade` outcome isn't directly
     provable — the `cache_stampede`, `sim_fault_plan`, and `embedded_saas`
     failures, none of which was checked for what `live_upgrade` itself did
     in that same run — as passes, since nothing shows otherwise) and
     **3/14 ≈ 21.4%** (counting only the 14 executions — 11 successes plus
     the 3 confirmed `live_upgrade` failures — whose `live_upgrade` outcome
     step-conclusion logic actually proves either way): **3 failures in 17
     eligible macOS executions, 14 of them confirmed, 3 unresolved**.
     Against that, **0 failures in 16 to 17 known ubuntu
     executions** (ubuntu's `Run tests` step failed only once in the 17,
     via the unrelated `embedded_saas` drift; its one other failure, run
     8190's Docker step, is a later step in the same job, so that run's
     `Run tests` step — and `live_upgrade` within it — passed). Thanking
     Codex review again here, twice over: the earlier "3-for-3" phrasing
     counted only failures and implied a 100% macOS fail rate, and the
     revision after that inverted the bound — more confirmed executions can
     only pull a fixed failure count's rate *down*, not up, so "at least 14
     known executions" supports *at most* 21.4%, not "≥21%". A rate in the
     17.6%-21.4% range against 0% is still the strongest repeat signature
     in the sample, just not the number either earlier revision gave.
  2. **`integration::cache_stampede::swr_serves_stale_and_refreshes_in_
     background`** — the background refresh never published within its
     1,000×25ms (~25s) poll budget (run 8190, macOS). This assertion was
     already hardened once for a documented `windows-latest` scheduler-
     starvation flake (`elapsed < 150ms` → a `Notify`-gated deterministic
     wait, issue #1809, see the comment block at
     `cache_stampede.rs:354-364`) — a different platform than the one this
     census caught it on, which is itself worth noting: the same test has
     now shown timing sensitivity on both non-Linux runners, just via
     different assertions; the failure seen here is a *different*
     assertion in the same test (line 501, the publish-visibility poll), so
     #1809's fix does not cover it. Cross-platform test, one macOS
     occurrence in the sample — suggestive, not yet a repeat signature on
     its own.
  3. **`integration::sim_fault_plan::same_seed_replays_a_byte_identical_
     outcome_100_times`** — panicked enqueueing a probe job: `"job runtime
     is not initialized; register jobs with AppBuilder::jobs()"` (run 8159,
     macOS). Cross-platform test, likewise one occurrence here.

- Quarantine ledger: no formal ledger exists in this repo (no
  intake-form/owner/date convention). One `--skip` in `ci.yml`'s Docker
  sweep is the closest analogue — `cancelled_release_does_not_leak_lock`,
  commented `flaky wall-clock zero-duration-timeout race; needs
  deterministic/paused time to de-flake (tracked as a follow-up)` — which is
  a diagnosis-bearing skip but has no owner or diagnose-by date attached.
  That is this repo's one open admissions gap, not a new finding.
- Rerun-button / merge-queue telemetry: not available through the tools this
  census had access to (no merge-queue product in use here; GitHub's basic
  Actions API does not expose historical manual-rerun click counts beyond
  the `run_attempt` field already folded into the failure count above).
- Escape mining (reverts/hotfixes): not run this pass — flagged as a gap in
  this census, not attempted and not claimed.

## 🔍 Diagnosis

**Root-cause category**: runner-class / resource-contention timing
dependence, provisionally — **not yet confirmed**, and this is the load-
bearing caveat of this report. The primary evidence is `live_upgrade`,
which is written with a hard, zero-tolerance assertion
(`connect_errors == 0`, no slack at all, unlike this same test's own
300-read floor and 5-second latency ceiling, both explicitly sized "so a
busy CI runner cannot fail the run") and failed identically in 3 of 17
eligible macOS executions (17.6%-21.4% depending on three unresolved runs
— see Symptom above for the derivation) against 0 of 16-17 known
`ubuntu-latest` executions — the only other platform the unix-gated test
runs on. `cache_stampede` and `sim_fault_plan` each failed once more, also
on macOS, also in tests with no platform gate, but they don't share one
mechanism with each other or with `live_upgrade`: `cache_stampede`'s is a
generous, load-tolerant real-time poll (25 seconds, 1,000 attempts) by this
codebase's own standards; `sim_fault_plan` instead runs jobs under a
process-global lock (`job::global_job_runtime_test_lock`) and a paused,
single-threaded simulated clock — no wall-clock budget to compare — and
its failure ("job runtime is not initialized") reads as a setup/shared-
state defect, not an exhausted wait. One occurrence apiece is
corroborating that macOS is where this sample's timing- and state-shaped
failures land, not yet a repeat signature in its own right, and not
evidence for a single shared mechanism across all three. Together the
pattern is consistent with GitHub's shared macOS
runners being more heavily contended than this sample's ubuntu lane — but
it is equally consistent with a genuine narrow product race that macOS's
scheduling merely exposes more reliably. Law 3 applies directly here: I
read `autumn/src/upgrade.rs`'s handoff design (the listening socket is
`dup`'d and handed to the successor as its stdin — inetd-style — so the
same underlying open file description stays live in *some* process
throughout the cutover; there is no rebind/reopen window in that path as
designed). That reading does not by itself explain the observed
`connect_errors == 1`, which means the mechanism is not yet named to the
standard this role requires, and the test-vs-product verdict is **not yet
rendered**.

## 🔧 Treatment

None applied. The hard gate for a fix PR — a rerun-rate baseline from a
committed harness, a named mechanism, the test-vs-product verdict, and a
post-fix 0/N revert-checked measurement — is not cleared by a single day's
organic PR sample, however suggestive the clustering. Shipping a timeout
bump, a sleep, or a widened tolerance against any of the three tests above
without that work is exactly the laundering this role exists to refuse.

## 📊 Measurement

Before: as tabulated above (organic-traffic census, not a controlled rerun;
n=9 failures, n=19 completed non-cancelled PR runs (97 completed minus 78
cancelled), one calendar day). No
after-measurement — no change shipped this pass.

## 🔬 Reproduce

An earlier revision of this section proposed reruns filtered to just the
named test (`cargo test ... -- <test name>`). Codex review correctly
rejected that: all three tests live in the consolidated `integration_tests`
binary (or, for `live_upgrade`, its own binary), and every CI failure this
census found happened while that binary ran as part of the full `Run
tests` step (`cargo test --workspace`) — thousands of tests, much of it
concurrent, on a runner that had already been building and testing for a
while. A filtered single-test rerun strips out exactly the load the
runner-contention hypothesis in Diagnosis depends on: a clean result from
it would show the test has no bug *in isolation*, not that macOS isn't
contended. It answers a real but different, narrower question than the one
this report is asking.

That leaves two tiers, and they are not substitutes for each other:

1. **The load-faithful rerun (Tier 1, the one this diagnosis actually
   needs)**: rerun the exact CI command, unfiltered, and grep each run's
   output for the target test's own result line. This is expensive — the
   `Run tests` step itself took 1,501-1,607s (25-27 minutes) in the census
   sample — so even N=10 here is a ~4-5 hour campaign, not a quick check:

   ```
   # Run on macOS hardware/a macos-latest-class runner. Mirrors the actual
   # failing step; do not add a name filter or you're back to the isolated
   # case Codex flagged.
   for i in $(seq 1 10); do
     cargo test --workspace 2>&1 | tee "run-$i.log"
     grep -E "upgrades_in_place_under_load_without_dropping|cache_stampede::swr_serves_stale_and_refreshes_in_background|sim_fault_plan::same_seed_replays_a_byte_identical_outcome_100_times" "run-$i.log"
   done
   ```

2. **The isolated rerun (Tier 3, a narrower, cheaper, and strictly weaker
   check)**: useful only for ruling out a bug that reproduces with no load
   at all — a positive result there is still informative (bug confirmed
   independent of contention), but a clean 0/N is not evidence against the
   contention hypothesis and must not be read as one, unlike what an
   earlier revision of this report claimed:

   ```
   for i in $(seq 1 20); do
     cargo test -p hot-upgrade --test live_upgrade -- --test-threads=1 \
       || echo "FAIL run $i"
   done
   for i in $(seq 1 20); do
     cargo test -p autumn-web --test integration_tests -- \
       cache_stampede::swr_serves_stale_and_refreshes_in_background \
       || echo "FAIL run $i"
   done
   # Plain #[test], not #[ignore]d -- no --ignored flag, or this filters
   # the test out entirely and every iteration "passes" by running nothing.
   for i in $(seq 1 20); do
     cargo test -p autumn-web --test integration_tests -- \
       same_seed_replays_a_byte_identical_outcome_100_times \
       || echo "FAIL run $i"
   done
   ```

Run (1) first. If it reproduces at a measurable rate, that confirms the
failure is real under realistic load and gives a rate to work from — it
does *not* by itself name the mechanism, since a reproducing failure is
equally consistent with runner contention and with a genuine product race
that macOS's scheduling exposes more often (the two explanations Diagnosis
leaves open). Separating them needs (2) run alongside it (does the same
failure show up with no concurrent load at all?) and a shuffled-order pass
on the full suite. If (1) instead comes back clean at N=10, that is real
evidence against the contention hypothesis specifically (unlike a clean
(2), which never was). Either way, (2) alone was never enough to conclude
anything about runner-class contention, and this report was wrong to imply
otherwise.

This census opens no *fix* PR: it lands as a report, per this role's own
rule that a report is a legitimate, non-default outcome when the gate isn't
cleared — not a consolation prize for not finding a fix.
