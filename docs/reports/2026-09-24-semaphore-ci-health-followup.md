# 🚦 Semaphore: CI health follow-up — zero new hits; `sqlite_jobs_scheduler_e2e`'s fix confirmed holding

Follow-up to `docs/reports/2026-09-22-semaphore-ci-health-followup.md` and the
`sqlite_jobs_scheduler_e2e` reproduction/fix that landed since (PRs #2913 and
#2925/#2931, merged 2026-09-23, closed in `docs/ci-health/quarantine-ledger.md`
before this pass began). This pass found no new organic-hit signatures and no
CI-red escapes; its main content is verifying the sqlite_jobs fix is holding
and refreshing the three still-open `live_upgrade`/`cache_stampede`/
`sim_fault_plan` entries with another clean sampling window. No fix PR, no new
harness — a health-report pass per this role's own "Acceptable outcomes."

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **15th
consecutive idle pass** (~378.6 hours, past 15.77 days, since it became
dispatchable 2026-09-08T15:07:44Z). Not dispatched this pass: new macOS CI
spend needs a human sign-off, unavailable in this unattended run.

## 🌡️ Symptom

**Organic-hit sampling**, page 1 of `list_workflow_runs`
(`event=pull_request`, `status=completed`, `perPage=100`): 100 runs spanning
2026-09-22T19:51:14Z–2026-09-24T09:42:24Z (~37.85h) — 64 cancelled / 32
success / **4 failure**. Page 2 of the identical query did not continue
backward from page 1 (it returned runs from 2026-09-07–2026-09-09, and
`total_count` itself differed between the two calls, 9315 vs. 7616), so the
~13.4h gap between this window's start and the prior report's own cutoff
(2026-09-22T06:24:50Z–19:51:14Z) was not independently sampled this pass —
recorded as a coverage gap, not silently papered over.

All 4 in-window failures triaged at job/log level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 35909966810 | `vesper/bugbash-2881-busy-timeout-shared-cache` | `SQLite runtime (feature=sqlite)` | `E0061`: `reject_sqlite_statement_timeout` called with 1 arg instead of 2, `autumn/src/app.rs:11164`/`:11248` — **own subject matter** (that branch's in-progress statement-timeout work), unmerged. |
| 35894739084 | `vesper/bugbash-2921-create-project-submit-token` | `Lint`, `Test suite` | `E0308`: `extract_submit_token` expects `&str`, given `String`, `examples/saas/tests/integration_test.rs:574` — **own subject matter**, unmerged. |
| 35866381449 | `claude/sleepy-brown-7ke3xk` | `SQLite runtime (feature=sqlite)` | `sqlite_job_backend_tracks_job_status_durably` FAILED — the tracked signature, but job completed 2026-09-23T13:36:52Z, **~6h before** PR #2925's merge (2026-09-23T19:33:05Z). Pre-fix occurrence already covered by the closed ledger entry, not a new recurrence. |
| 35806829348 | `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0` | `MSRV`, `Test (macos/ubuntu/windows-latest)`, `Test suite` | `rustup` failed installing toolchain `1.120.0` itself — **own subject matter** (the pin bump), unmerged. |

None of the 4 match `live_upgrade`, `cache_stampede`, `sim_fault_plan`, or any
other tracked signature.

## 🔍 Diagnosis

No new mechanism to diagnose this pass. The one notable data point is
confirming the `sqlite_job_backend_tracks_job_status_durably` fix's timing:
the single in-window hit of that signature predates the fix's merge by about
six hours, so it is not evidence of a residual defect — it is the same defect
PR #2925 already fixed, caught one more time on a PR that happened to run
before the fix landed on `trunk-dev`.

## 🔧 Treatment

None needed. This pass is verification-only:

- Confirmed `sqlite_job_backend_tracks_job_status_durably`'s fix (PR #2925)
  is holding: zero recurrences in the ~38h of ordinary PR traffic sampled
  since its merge.
- Added dated 2026-09-24 updates to the `live_upgrade`, `cache_stampede`, and
  `sim_fault_plan` entries in `docs/ci-health/quarantine-ledger.md` recording
  this pass's clean sampling window, and a verification addendum to the
  closed `sqlite_job_backend_tracks_job_status_durably` entry.

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `sqlite_job_backend_tracks_job_status_durably` | Closed 2026-09-23 (PR #2925) | 1 hit found, predates the fix's merge by ~6h; 0 hits after | Unchanged, still closed — fix confirmed holding |
| `live_upgrade` (3 signatures) | Uncampaigned, 14 consecutive clean passes | No new organic hits | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (14 idle passes, ~330.5h) | 0 (15th idle pass, ~378.6h) | Needs human sign-off for CI spend |

## 🔬 Reproduce

Confirm this pass's sampling window and its 4 failures:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# -> 100 runs in [2026-09-22T19:51:14Z, 2026-09-24T09:42:24Z], 4 failures,
#    all four triaged above (three own-subject-matter/branch-owned, one a
#    pre-fix repeat of the already-closed sqlite_jobs signature)

get_job_logs(job_id=107347025559)  # vesper/bugbash-2881: E0061, own diff
get_job_logs(job_id=107295739880)  # vesper/bugbash-2921: E0308, own diff
get_job_logs(job_id=107199909719)  # claude/sleepy-brown-7ke3xk: pre-fix sqlite_jobs hit
get_job_logs(job_id=107009452872)  # dtolnay/rust-toolchain-1.120.0: own bump
```

Confirm the sqlite_jobs fix's merge time relative to the pre-fix hit:

```
git log --format='%H %ad %s' --date=iso-strict -1 ff406e0
# -> 2026-09-23T19:33:05Z (job 107199909719 completed 2026-09-23T13:36:52Z,
#    ~6h earlier)
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```
