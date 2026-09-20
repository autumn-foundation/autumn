# 🚦 Semaphore: CI health follow-up — zero new hits, and a second rerun harness (job_tracking) built but not yet dispatchable

Follow-up to `docs/reports/2026-09-17-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR —
nothing found this pass clears this role's own hard gate for a determinism
PR. This pass's one shipped change is a new CI-health harness, not a fix:
`.github/workflows/manual-job-tracking-rerun-check.yml`, a same-commit
rerun-rate harness for `job_tracking_stores_integration`'s tracked TTL-expiry
flake, recommended by the 2026-09-15 follow-up but never built until now. It
turns out not to be dispatchable from this PR's own branch — see Treatment.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **10th consecutive
idle pass** (~235 hours, closing in on 10 days, since it became dispatchable
2026-09-08T15:07:44Z).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from the 2026-09-17 report's
own cutoff (2026-09-17T09:59:29Z, exclusive) to 2026-09-18T07:33:38Z (~21.6h;
a single `perPage=100`/`page=1` query's own span, 2026-09-17T09:28:44Z–
2026-09-18T07:33:38Z, fully covered the window with margin on both ends, so
no second page was needed this pass, unlike several recent passes) — 96 runs
in-window: 80 cancelled, 14 success, 2 failure. **Caveat carried from prior
passes**: only the 2 run-level failures were inspected at job level; the 80
`cancelled`-overall runs were not, so — since `ci.yml` runs with
`cancel-in-progress: true` and a job can fail before its run is superseded
and marked `cancelled` — the "no repeat" claims below are scoped to the runs
actually inspected, not proven-exhaustive across the full 96-run window.

Both failures triaged by job/log inspection, neither matching any of the four
tracked signatures (`live_upgrade`'s three, `cache_stampede`,
`sim_fault_plan`, `job_tracking_stores_integration`):

- **`claude/elegant-ptolemy-scqmqh`** (run 35254153432): the `Lint` job's own
  `Determinism seam gate` step failed — a repo-hygiene self-check on this
  branch's own diff, not a CI infra issue.
- **`dependabot/cargo/validator-0.21.0`** (run 35232563734): two jobs failed.
  `Supply chain (cargo-deny)` failed on the same pre-existing
  `fuzz/Cargo.lock --locked` staleness this ledger has already attributed to
  this branch on prior passes (unrelated to any tracked signature).
  `Test (Docker)` failed with a **new failure shape on this branch**, not
  previously logged: the `validator` 0.21.0 bump itself breaks
  `examples/ledger-admin-bulk-app`'s `PostForm`/`update` handler —
  `E0277`/`E0599` on `Validate`/`IntoChangeset` trait bounds no longer
  satisfied once `validator` moves to 0.21.0. This is squarely the
  dependency bump this PR exists to land turning out to be breaking, not a
  CI health issue — the PR's own subject matter failing, exactly as
  expected of a dependabot major/minor bump that changes a trait's shape.

## 🔍 Diagnosis

Nothing this pass rises to a diagnosis-worthy finding on its own — both
failures are branch-owned (one repo-hygiene self-check, one a dependency
bump breaking its own callers). The standing diagnoses on the four tracked
entries are unchanged; see the ledger for their full histories.

## 🔧 Treatment

No fix PR. Instead, this pass builds the rerun harness the 2026-09-15 report
recommended for `job_tracking_stores_integration` and never got built in the
three passes since:

**`manual-job-tracking-rerun-check.yml`** — `workflow_dispatch`-only, builds
the `autumn-web` `integration_tests` binary once with the exact feature set
`ci.yml`'s `test-docker` job uses
(`test-support,offline-sync,ws,mail,redis,i18n,collab`), then loops
`cargo test ... -- --ignored --test-threads=1
integration::job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`
20 or 50 times against a fresh testcontainers Postgres container per
iteration, logging each iteration's pass/fail to its own uploaded artifact
(so a failing iteration's log isn't lost in a combined tail — the same
truncation problem that has repeatedly hampered diagnosing `live_upgrade`).
Unlike `manual-macos-contention-check.yml`, this doesn't add a new runner
class or new CI spend — it's the identical `ubuntu-latest` + Docker shape
`test-docker` already runs on every PR, just isolated to one test and
looped — so it doesn't carry that harness's "ask before running" gate.

**It is not dispatchable yet.** Attempting to dispatch it against this
harness's own authoring branch (`claude/sleepy-brown-r5urcw`) returned `404
Not Found` from the workflow-dispatches API endpoint. `workflow_dispatch`
only accepts a workflow that already exists on the repository's *default*
branch (`trunk-dev`) — the `ref` input picks which commit's code and
workflow *version* actually runs, but the workflow has to be registered on
the default branch first to be dispatchable at all. This is the identical
gotcha `manual-macos-contention-check.yml` hit before it: per this ledger's
own `live_upgrade` entry, that harness "only became dispatchable... when
#2627 fixed its parse error" landed on `trunk-dev`. Recorded here rather
than silently worked around, since it means this pass ships a harness with
zero rerun data behind it — a Harness PR (this role's own acceptable-outcome
#4), not a Determinism PR.

- **Ledger updated**: `live_upgrade` gets a 2026-09-18 dated update (zero new
  hits, harness idle 10th pass, cross-referencing this new harness);
  `cache_stampede` and `sim_fault_plan` each get a one-line 2026-09-18
  no-repeat update; `job_tracking_stores_integration` gets the full account
  of the new harness, why it isn't dispatchable yet, and the exact next step
  (dispatch at `iterations: "50"` — the low-rate side of this role's own
  ≥20/≥50 split, since n=2 organic in roughly two weeks of ambient PR traffic
  is comfortably under 10%) once this PR merges.
- **No other action needed** — both of this pass's failures are squarely
  branch-owned.

## 📊 Measurement

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` (all three signatures) | No occurrence in the 2 runs inspected at job level (not proven-exhaustive across all 96 — see caveat in Symptom) | Unchanged, uncampaigned |
| `cache_stampede` | No occurrence (same caveat) | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence (same caveat) | Unchanged, n=1, uncampaigned |
| `job_tracking_stores_integration` | No occurrence (same caveat) | Unchanged, n=2 — **rerun harness now built, not yet dispatchable (needs merge to `trunk-dev` first)** |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 10th consecutive idle pass, ~235h |
| `manual-job-tracking-rerun-check.yml` | New this pass, 0 dispatches (not yet possible pre-merge) | Built, not yet usable |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287 (ci.yml's numeric workflow ID),
             event=pull_request, perPage=100, page=1)
# window: created_at > 2026-09-17T09:59:29Z (exclusive) -> 96 runs:
# 80 cancelled, 14 success, 2 failure
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(job_id, return_content=true, tail_lines>=80)
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=350840451 (manual-macos-contention-check.yml),
             event=workflow_dispatch)
# → total_count: 0
```

Confirm the new job_tracking harness's dispatch gotcha (run from a branch
that is not yet the default branch):

```
actions_run_trigger(run_workflow, owner=autumn-foundation, repo=autumn,
                     workflow_id=manual-job-tracking-rerun-check.yml,
                     ref=claude/sleepy-brown-r5urcw,
                     inputs={sha: "<commit>", iterations: "50"})
# → 404 Not Found (workflow not yet registered on trunk-dev)
```

Once this PR merges, the intended dispatch is:

```
actions_run_trigger(run_workflow, owner=autumn-foundation, repo=autumn,
                     workflow_id=manual-job-tracking-rerun-check.yml,
                     ref=trunk-dev,
                     inputs={sha: "<trunk-dev tip>", iterations: "50"})
```
