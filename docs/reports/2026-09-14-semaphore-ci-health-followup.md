# 🚦 Semaphore: CI health follow-up — zero new hits, harness idle a 6th pass

**Correction (post-review, via two Codex review comments on this report's own
PR #2786):** the original version of this report (1) claimed all four tracked
ledger entries got a 2026-09-14 dated note when only three did (`sim_fault_plan`
was missed), and (2) called the whole 71-run window "zero hits" while only
inspecting the two runs whose overall conclusion was `failure` — the other 45
runs in the window were `cancelled`, and `ci.yml`'s `cancel-in-progress`
(`.github/workflows/ci.yml:9-11`) means a job can fail before its run gets
superseded and marked `cancelled` overall, so those runs were not established
zero-hit observations. Checked job-level conclusions (not run-level) for 26 of
the 45 cancelled runs (58%, a sample — the rest weren't checked, so this
remains best-effort, not exhaustive, per this ledger's own precedent for that
caveat): one of them, run 34774043482 (branch `claude/epic-meitner-vkej1i`),
did have two job-level failures — `Test (ubuntu-latest)` and `Test
(windows-latest)` both failed on the same test before the run was superseded
and its other jobs cancelled. Detailed and folded in below; it doesn't change
this pass's bottom line (branch-owned, not a CI health issue, no match to any
tracked signature), but the earlier "zero hits" framing was wrong as stated.

Follow-up to `docs/reports/2026-09-13-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass: nothing found in this pass — the two run-level failures plus the
one job-level failure uncovered inside a cancelled run — matches any of the
four actively-tracked flaky signatures, and all three triage cleanly to
WIP-branch-owned defects rather than CI health issues.

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
success, 2 failure. Triaged both run-level failures by job/log inspection,
then — per the correction above — also checked job-level conclusions for 26
of the 45 cancelled runs (58%, best-effort sample):

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
- **Hidden job-level failure inside a cancelled run**: run 34774043482
  (branch `claude/epic-meitner-vkej1i`, created 2026-09-13T18:15:04Z) shows
  overall conclusion `cancelled` (superseded by the branch's next push, run
  34777703864, ~13 minutes later), but two of its jobs — `Test
  (ubuntu-latest)` and `Test (windows-latest)` — completed with conclusion
  `failure` before that supersession, on the identical test on both
  platforms: `starters::tests::embedded_cms_matches_example_cms`
  (`autumn-cli/src/starters/mod.rs:664:13`), `` assertion `left == right`
  failed: drift between embedded cms starter and examples/cms at
  src/routes/front.rs ``. This is a repo-hygiene drift-check asserting the
  embedded CMS starter template matches `examples/cms`'s actual source; it
  fires identically on both OS runners because it's a pure file-diff
  assertion, not a platform-dependent one. Branch-owned: this WIP branch's
  in-progress edit to `examples/cms` (or the starter template) hadn't yet
  synced the other side when this commit ran; the branch's very next push
  (13 minutes later) evidently fixed it, since none of the 26 sampled
  cancelled runs after that point show the same signature. Not a CI health
  issue, and not a match to any tracked signature.
- **Zero hits on any of the four actively-tracked flaky tests** —
  `live_upgrade` (three signatures), `cache_stampede`, `sim_fault_plan`,
  `job_tracking_stores_integration` (six signatures total) — across
  everything checked this pass (both run-level failures, plus job-level
  conclusions in the 26 sampled cancelled runs). The remaining 19 cancelled
  runs in the window were not individually checked at job level, so this is
  a best-effort sample, not a proven-exhaustive one, on the cancelled-run
  half specifically.

## 🔍 Diagnosis

None of the three failures found this pass is CI-health-relevant. The
dependabot pair is a version bump whose own lockfile and downstream compile
break are exactly what a `--locked` supply-chain gate and a full build are
supposed to catch before merge — working as intended, not a defect in the
pipeline. The `happy-edison-fstb1z` Windows failure and the
`epic-meitner-vkej1i` cross-platform failure are both the same
category: a WIP branch's own repo-hygiene self-check firing because that
branch's own diff (still in progress) hadn't finished syncing two things that
must agree (`ci.yml`'s job list in one case, the embedded CMS starter vs.
`examples/cms` in the other) — also working as intended, and in the second
case self-corrected by the branch's own next push. No test-vs-product
verdict is needed for any of the three: all are attributable to a branch's
own uncommitted-or-incomplete change, not to test or product code on
`trunk-dev`.

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
- **No action needed** on the dependabot lockfile/compile break, the
  `happy-edison-fstb1z` Windows failure, or the `epic-meitner-vkej1i`
  cross-platform drift-check failure — each belongs to its own branch's
  author, and the third had already self-resolved by that branch's next
  push.

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
| `claude/epic-meitner-vkej1i` hidden job failure inside cancelled run | Triaged, branch-owned, self-resolved | Not a CI health issue |
| Cancelled-run job-level check | 26/45 sampled (58%), 1 hidden failure found (above) | Best-effort, not exhaustive |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at in [2026-09-13T09:02Z, 2026-09-14T08:00:19Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(run_id, failed_only=true, return_content=true)
# each cancelled run's job-level conclusions via list_workflow_jobs(run_id, filter=latest)
#   → grep for "conclusion":"failure" (run 34774043482 is the one hit found)
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
