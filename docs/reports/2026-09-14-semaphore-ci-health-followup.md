# 🚦 Semaphore: CI health follow-up — zero new hits, harness idle a 6th pass

Follow-up to `docs/reports/2026-09-13-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass: nothing in the sampled window matches any of the four
actively-tracked flaky signatures, and the two organic failures found both
triage cleanly to WIP-branch-owned defects rather than CI health issues.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green, and the required gate developers wait on is
`Test suite` (`test-gate`), fed by `[test, trybuild, test-features,
test-docker]`. `manual-macos-contention-check.yml` remains dispatch-only —
**still zero `workflow_dispatch` runs**, now a 6th consecutive idle pass
(~137 hours since it became dispatchable at 2026-09-08T15:07:44Z, checked
2026-09-14T~08:0xZ).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from the 2026-09-13 report's
own cutoff (2026-09-13T09:02Z) to 2026-09-14T08:00:19Z (~23 hours, one
`perPage=100` page — the page's own span, 2026-09-13T06:40:40Z–
2026-09-14T08:00:19Z, fully covers the window with margin on both ends, so no
second page was needed this pass). 71 runs in window: 45 cancelled, 24
success, 2 failure. Triaged both failures by job/log inspection:

- **`dependabot/cargo/validator-0.21.0`** (run 34789054745,
  2026-09-13T23:13:00Z): two job failures, both direct, deterministic
  consequences of the dependency bump this PR itself proposes, not flakes.
  `Supply chain (cargo-deny)` fails because `fuzz/Cargo.lock` needs
  regenerating for the new `validator` version and `--locked` refuses to do
  that implicitly (`error: cannot update the lock file .../fuzz/Cargo.lock
  because --locked was passed`). `Test (Docker)` fails to compile:
  `validator` 0.21.0 changed enough that `PostForm` (`src/routes/posts.rs`)
  no longer satisfies `autumn_web::prelude::Validate`, so
  `form.into_changeset()` doesn't resolve (`E0599`). Both failures are this
  dependabot branch's own responsibility to resolve (update the lockfile,
  and either adapt to the new `validator` API or hold the bump) — not a CI
  health issue.
- **`claude/happy-edison-fstb1z`** (run 34748994796, 2026-09-13T09:08:21Z):
  `Test (windows-latest)` fails at
  `edge_conformance_ci_coverage::build_edge_capsule_step_reuses_the_conformance_suites_target_dir`:
  `"ci.yml no longer has an edge-conformance: job"` — a repo-hygiene test
  that asserts `ci.yml` still carries that job block. This branch was
  already flagged in the 2026-09-13 report for its own in-progress `Clippy`
  churn (9 runs, one person iterating on lint fixes); this is a different
  symptom of the same WIP branch mid-editing `ci.yml`, not an environment or
  cross-branch issue.
- **Zero hits on any of the four actively-tracked flaky tests** —
  `live_upgrade` (three signatures), `cache_stampede`, `sim_fault_plan`,
  `job_tracking_stores_integration` (six signatures total) — in the sampled
  window.

## 🔍 Diagnosis

Neither failure is CI-health-relevant. The dependabot pair is a version bump
whose own lockfile and downstream compile break are exactly what a `--locked`
supply-chain gate and a full build are supposed to catch before merge —
working as intended, not a defect in the pipeline. The Windows failure is a
WIP branch's own repo-hygiene self-check firing because that branch's own
diff (still in progress) hasn't finished updating `ci.yml` — also working as
intended. No test-vs-product verdict is needed for either: both are
attributable to the branch's own uncommitted-or-incomplete change, not to
test or product code on `trunk-dev`.

## 🔧 Treatment

No fix PR — nothing to fix. Ledger updated with a 2026-09-14 dated note on
each of the four tracked entries (zero new hits, harness still idle) and the
recommendation to dispatch `manual-macos-contention-check.yml` carried
forward unchanged.

- **Recommendation for a human, unchanged from the last five passes**:
  dispatch `manual-macos-contention-check.yml` (`samples: "20"`) against a
  `trunk-dev` commit at or after `8fae8af`. ~137 hours idle since it became
  dispatchable is now the better part of six days without even the partial
  evidence it could be producing for the `live_upgrade`/`cache_stampede`/
  `sim_fault_plan` investigation.
- **No action needed** on the dependabot lockfile/compile break or the
  `happy-edison-fstb1z` Windows failure — each belongs to its own branch's
  author.

## 📊 Measurement

No rerun campaign this pass — organic sampling only.

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` (3 signatures) | 0 new hits in ~23h window | Unchanged; harness still undispatched, 6th pass |
| `cache_stampede` | 0 new hits | Unchanged, undiagnosed |
| `sim_fault_plan` | 0 new hits | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | 0 new hits (still n=1 total) | Unchanged, undiagnosed |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 6th consecutive idle pass, ~137h |
| `dependabot/cargo/validator-0.21.0` lockfile+compile break | Triaged, branch-owned | Not a CI health issue |
| `claude/happy-edison-fstb1z` Windows edge-conformance self-check | Triaged, branch-owned | Not a CI health issue |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at in [2026-09-13T09:02Z, 2026-09-14T08:00:19Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(run_id, failed_only=true, return_content=true)
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
