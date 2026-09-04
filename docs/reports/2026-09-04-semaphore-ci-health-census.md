# 🚦 Semaphore: CI health census, 2026-09-04

## 🎯 Verdict path

Merging into `trunk-dev` waits on `.github/workflows/ci.yml`'s `pull_request`
run: `meta` → `lint` (+ `migration-guides`, `plugin-contract`, `supply-chain`
in parallel) → `test` (matrix: ubuntu/macos/windows-latest) → `coverage`
(non-blocking, Codecov only) / `loom`, plus the standalone `msrv`,
`sqlite-runtime`, `sim-sweep`, and `edge-conformance` jobs. No ambient retry
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
10:12 UTC, run numbers 8125-8212 across ~40 distinct PR branches — one
sitting's worth of this repository's actual PR throughput, not a
same-commit rerun campaign; see Reproduce below for why that campaign is the
right next step, not something this census fabricated a rate for).

- 97 completed, 78 cancelled (superseded by a same-PR push, expected and
  excluded from the rate below), 10 success, **9 failure** — of the 19 runs
  that actually reached a real verdict (10 + 9, i.e. completed minus
  cancelled), **9 failed: ~47.4%**. That is the number that matters, not the
  9.3%-of-all-97 a first pass over this data gives if cancelled runs are left
  in the denominator after being called out as excluded (~2 automatic
  reruns among the sample moved 2 of those 9 to green on attempt 2, both
  `Test (macos-latest)`).
- Failure-signature clustering, by job:

  | Job | Failures | Signature |
  |---|---|---|
  | `Lint` → `Clippy` | 2 | real clippy findings in WIP branches, both later fixed same-PR — not a suite defect |
  | `Test (macos-latest)` → `Run tests` | **6** | 4 distinct assertions, see below |
  | `Test (ubuntu-latest)` → `Run tests` | 1 | `starters::tests::embedded_saas_matches_example_saas` (same WIP-branch drift as below, also hit ubuntu) |
  | `Test (ubuntu-latest)` → `Run Docker-dependent tests` | 1 | unrelated to this census's macOS finding; not triaged further here |
  | `Windows Tier 1 journey` → app-builds-on-Windows | 1 | same WIP-branch compile error as the ubuntu Clippy hit, same PR |

  `Test (macos-latest)` failed in 6 of the 9 failing runs — every platform
  in the matrix runs the identical `cargo test --workspace` command, so a
  6/1/0 (macOS/ubuntu/windows) split on the same command is itself a
  measurement: something about the macOS runner class, not the test code
  path, is the shared variable. Of those 6, one (`embedded_saas_matches_
  example_saas`, a byte-for-byte starter/example drift check) is a genuine
  product-side catch in a still-in-progress branch — it also failed on
  ubuntu in the same run, so it is not a macOS-specific finding and is
  excluded from what follows. The other 5 cluster into 3 distinct
  timing-shaped assertions, each in a different test:

  1. **`live_upgrade::upgrades_in_place_under_load_without_dropping_a_
     connection_or_the_state`** — `assert_eq!(connect_errors, 0)` failed
     `left: 1, right: 0` at `examples/hot-upgrade/tests/live_upgrade.rs:268`,
     identically, twice, on two unrelated branches ~13 hours apart (runs
     8147 and 8161). Same test, same line, same value, same runner class —
     the strongest repeat signature in the sample.
  2. **`integration::cache_stampede::swr_serves_stale_and_refreshes_in_
     background`** — the background refresh never published within its
     1,000×25ms (~25s) poll budget (run 8190). This assertion was already
     hardened once for a documented macOS/windows scheduler-starvation flake
     (`elapsed < 150ms` → a `Notify`-gated deterministic wait, issue #1809,
     see the comment block at `cache_stampede.rs:354-364`); the failure seen
     here is a *different* assertion in the same test (line 501, the
     publish-visibility poll), so #1809's fix does not cover it.
  3. **`integration::sim_fault_plan::same_seed_replays_a_byte_identical_
     outcome_100_times`** — panicked enqueueing a probe job: `"job runtime
     is not initialized; register jobs with AppBuilder::jobs()"` (run 8159).

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
bearing caveat of this report. Three different tests, each already written
with generous, load-tolerant budgets by the standards of this codebase (a
25-second, 1000-attempt poll; a 5-second cutover-latency ceiling explicitly
sized for "a busy CI runner"), still failed only on `macos-latest` in this
sample. That pattern is consistent with GitHub's shared macOS runners being
more heavily contended than the sample's ubuntu/windows lanes — but it is
equally consistent with a genuine narrow product race that macOS's slower
scheduling merely exposes more often. Law 3 applies directly here: I read
`autumn/src/upgrade.rs`'s handoff design (the listening socket is `dup`'d
and handed to the successor as its stdin — inetd-style — so the same
underlying open file description stays live in *some* process throughout
the cutover; there is no rebind/reopen window in that path as designed).
That reading does not by itself explain the observed `connect_errors == 1`,
which means the mechanism is not yet named to the standard this role
requires, and the test-vs-product verdict is **not yet rendered**.

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

The next legitimate step is Tier 1, not Tier 3: same-commit rerun statistics
on `examples/hot-upgrade` and the `cache_stampede`/`sim_fault_plan` suites,
specifically on a macOS runner (or macOS-class local hardware), at N≥20:

```
# live_upgrade, the strongest repeat signature (2/2 identical failures observed)
for i in $(seq 1 20); do
  cargo test -p hot-upgrade --test live_upgrade -- --test-threads=1 \
    || echo "FAIL run $i"
done

# cache_stampede's publish-visibility poll
for i in $(seq 1 20); do
  cargo test -p autumn-web --test integration_tests -- \
    cache_stampede::swr_serves_stale_and_refreshes_in_background \
    || echo "FAIL run $i"
done

# sim_fault_plan's job-runtime-not-initialized panic. Plain #[test], not
# #[ignore]d (it runs as part of the base `cargo test --workspace` step, the
# one that actually failed here) — no `--ignored` flag, or this filters the
# test out entirely and every iteration "passes" by running nothing.
for i in $(seq 1 20); do
  cargo test -p autumn-web --test integration_tests -- \
    same_seed_replays_a_byte_identical_outcome_100_times \
    || echo "FAIL run $i"
done
```

Whichever of the three reproduces at a measurable rate names the mechanism;
whichever doesn't reproduce locally at all is the strongest evidence the
cause is runner-class contention rather than the test/product code, and
argues for a shuffled-order + load-experiment pass (Tier 1/3) pinned to
`macos-latest` specifically before touching any of the three tests.

This census opens no PR: it is a report, per this role's own rule that a
report is a legitimate, non-default outcome when the gate isn't cleared —
not a consolation prize for not finding a fix.
