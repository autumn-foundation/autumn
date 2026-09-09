# 🚦 Semaphore: CI health follow-up — the macOS-only hypothesis breaks, `cache_stampede` repeats, the harness is still undispatched

Follow-up to `docs/reports/2026-09-08-semaphore-macos-contention-harness-fix.md`
(#2627, merged) and the running investigation in
`docs/ci-health/quarantine-ledger.md`. No fix PR — the hard gate for one still
isn't cleared — but this pass finds two new organic hits that materially change
the working diagnosis, and confirms the rerun harness fixed four days ago has
still never been run.

## 🎯 Verdict path

`trunk-dev`'s tip (`c456bcd`) is green; the last completed push-triggered
`ci.yml` run succeeded. No branch-protection or merge-queue regressions found.

`manual-macos-contention-check.yml` — fixed and `actionlint`-clean since #2627
(2026-09-08) — still has **zero `workflow_dispatch` runs** (`total_count: 0`
against its own run history via the Actions API, checked 2026-09-09). It has
been dispatchable for four days and nobody has run it.

## 🌡️ Symptom

Sampled the 200 most-recently-completed `pull_request`-triggered `ci.yml` runs
(2026-09-08T14:55Z–2026-09-09T09:47Z, via the Actions API): 164 cancelled
(superseded by newer pushes on the same PR — expected, not a health signal),
27 success, **9 failure**. Triaged each failure by job/log inspection rather
than trusting the run-level conclusion:

- **6 are ordinary PR-under-development failures**, not CI health issues:
  `Clippy` failures on two commits of an in-progress `#[derivation]` feature
  branch, a `Migration version gate` failure on the same branch, a
  `cargo-deny` finding and a genuine compile/test break on a
  `dependabot/cargo/validator-0.21.0` bump (the new `validator` release needs
  code changes this PR doesn't have yet), and repeated failures on
  `feat-cms-starter`'s own new code across its many "round-N review fixes"
  commits. None of these are flakes — they're WIP branches failing on their
  own not-yet-fixed changes, working as intended.
- **1 is a new, unrelated test on an unmerged feature branch**:
  `integration::custom_domain_issuance::a_stale_order_does_not_delete_the_successors_certificate`
  failed on `macos-latest` on PR #2637's branch (`claude/issue-1635-tdd-dv8zqj`,
  "custom domains + per-domain ACME certs", not yet merged). Single
  occurrence, on that PR's own new code — not part of the tracked cluster,
  logged here only so a repeat is recognized rather than rediscovered.
- **2 hit the tracked macOS/hot-upgrade timing cluster** in
  `docs/ci-health/quarantine-ledger.md`, and both change the picture:

  1. **`live_upgrade::upgrades_in_place_under_load_without_dropping_a_connection_or_the_state`
     — first-ever hit on a Linux runner, at a different assertion.** Run
     34317587464 (PR #2645), job `Coverage (workspace)`, on a plain hosted
     `ubuntu-latest` runner (confirmed via the job's own `labels`, not the
     `heavy_runs_on`-routed `test` job). This job runs `cargo llvm-cov`,
     which instruments and roughly doubles the cost of the wrapped test
     binary. Failure: `tests/live_upgrade.rs:567` — `"the new build should
     have served part of the load"` — a *different* assertion than the
     3 macOS hits in the 2026-09-04 census (those failed the connection-error
     check, ~line 551). The new build's version string never showed up in the
     read set during the test's fixed observation window.
  2. **`cache_stampede::swr_serves_stale_and_refreshes_in_background` —
     second hit, exact same assertion as the first.** Run 34297324354
     (branch `dependabot/cargo/validator-0.21.0`), job `Test (macos-latest)`:
     `autumn/tests/integration/cache_stampede.rs:501:6`, `"background refresh
     must publish the new value after it finishes computing"` — the identical
     panic site as the single occurrence logged 2026-09-03. Six days apart,
     same platform, same line: this is now a confirmed-repeat signature, not
     the "suggestive, not yet a repeat" status the ledger carried until today.

## 🔍 Diagnosis

**Verdict not rendered — but the working hypothesis needs revision.** Since
2026-09-04 this cluster has been framed as "macOS runner contention,"
justifying a macOS-only rerun harness. Finding 1 above is a hit on a
completely different, non-contended, plain hosted Linux runner — the one
thing the two hits share isn't the OS, it's that the runner was doing
meaningfully more work per wall-clock second than a bare `cargo test`
(coverage instrumentation on Linux; whatever `macos-latest`'s baseline
contention is on the other three). That points at a mechanism in the *test's*
design — a fixed-duration load-generation window racing a real process
cutover — that any sufficiently slow or loaded execution can lose, not a
macOS-specific scheduling quirk. This is still a hypothesis, not a
confirmed root cause: distinguishing "test's window is too tight under load"
from "a genuine narrow race in the hot-upgrade handoff that slow execution
exposes more reliably" is exactly what the still-undispatched rerun campaign
exists to do, and per Semaphore's law 3 (product/test verdict rendered first)
neither test's tolerance nor the product code should change until that
campaign renders it.

`cache_stampede`'s second hit doesn't change its diagnosis (still open, still
undiagnosed), but it does change its priority: two hits of the identical
assertion six days apart is stronger evidence than the ledger's previous
"one occurrence, suggestive" framing credited it.

## 🔧 Treatment

None. Per the hard gate, a fix requires a named mechanism plus baseline/after
rerun measurement from a real campaign — this pass has two more organic data
points, not a campaign. What ships instead:

- **`docs/ci-health/quarantine-ledger.md` updated** with both new hits, their
  exact signatures, and the revised (no-longer-macOS-only) framing for
  `live_upgrade`.
- **Recommendation for a human**: dispatch
  `manual-macos-contention-check.yml` now. It has been fixed and idle for
  four days while the organic sample keeps accumulating one data point at a
  time (now 4 hits across the tracked corpus: 3 macOS + 1 Linux, plus a
  second `cache_stampede` repeat) — the exact scenario the harness exists to
  short-circuit. Given finding 1 above, the campaign should not stay
  macOS-only forever: a companion rerun of the `Coverage (workspace)` job
  shape (or several samples of the plain `cargo test --workspace` under
  `cargo llvm-cov` wrapping) would test the "slow execution, not the OS"
  hypothesis directly. That is a second harness, not this one — flagged for
  a future pass, not built here, since this session found the evidence for
  it but building an untested second harness on top of an already-undispatched
  first one would compound the same problem rather than fix it.

**Noted, not actioned** (cosmetic, no fix warranted): `manual-macos-contention-check.yml`
still has its pre-#2627 broken copy live on any branch forked before that fix
merged — `feat-cms-starter` alone logged ~20 zero-job "failure" runs in the
sampling window from its own repeated pushes. These don't appear on the PR's
check-runs list (confirmed against PR #2621) and cost no compute (the file
fails to parse before any job starts), so they're Actions-tab noise only, not
a verdict-fatigue risk to a reviewer. They resolve themselves as those
branches merge or rebase past #2627; not worth a mass rebase campaign for a
cosmetic-only defect.

## 📊 Measurement

Both hits are organic-sample data points, not rerun-campaign results — no
before/after to report, consistent with "no fix this pass." Running tallies
against the ledger's ≥20 (≥50 for sub-10% rates) bar:

| Test | Hits (this pass) | Cumulative organic hits | Platforms seen |
|---|---|---|---|
| `live_upgrade` (connection-error assertion) | 0 | 3/17 macOS (2026-09-03/04) | macOS only |
| `live_upgrade` (new-build-never-served assertion) | 1 | 1 (2026-09-09) | Linux (coverage-instrumented) |
| `cache_stampede` (line 501) | 1 | 2 (2026-09-03, 2026-09-09) | macOS only, both times |
| `sim_fault_plan` | 0 | 1 (2026-09-03) | macOS only |

`manual-macos-contention-check.yml`: 0 → 0 `workflow_dispatch` runs (no
change; still nobody has dispatched it).

## 🔬 Reproduce

Sample used for this pass:

```
# via the GitHub Actions API (MCP github server), not shown as raw curl since
# this session used mcp__github__actions_list / get_job_logs directly:
list_workflow_runs(ci.yml, event=pull_request, status=completed, perPage=100, page=1)
list_workflow_runs(ci.yml, event=pull_request, status=completed, perPage=100, page=2)
# → 200 runs, 9 failures; each failure's jobs pulled via list_workflow_jobs,
# each failing job's log pulled via get_job_logs(return_content=true) and
# grepped for `panicked at|failures:|test result: FAILED`.
```

Confirm the harness is still undispatched:

```
list_workflow_runs(manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```

Dispatch (still requires sign-off — new macOS CI spend): `sha` pinned to a
green `trunk-dev` commit, `samples: "20"`.
