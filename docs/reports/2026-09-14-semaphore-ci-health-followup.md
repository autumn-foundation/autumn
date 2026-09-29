# 🚦 Semaphore: CI health follow-up — zero new hits, harness idle a 6th pass

**Correction (post-review, via four Codex review comments on this report's
own PR #2786), superseding all earlier counts in this report:** the original
version had four distinct problems, fixed in order below:

1. Claimed all four tracked ledger entries got a 2026-09-14 dated note when
   only three did (`sim_fault_plan` was missed) — fixed.
2. Called the whole 71-run window "zero hits" while only inspecting the two
   runs whose overall conclusion was `failure`. The other 45 runs were
   `cancelled`, and `ci.yml`'s `cancel-in-progress` (`.github/workflows/ci.yml:9-11`)
   means a job can fail before its run gets superseded and marked `cancelled`
   overall, so those runs were not established zero-hit observations —
   fixed by checking job-level conclusions (below).
3. **The job-level check's own sample size was miscounted (said 26/45,
   actually 28/45)** — a plain counting error caught while reproducing the
   claim for this correction, not a Codex finding. The 28 checked are the 28
   most-recently-created cancelled runs in the window, a contiguous prefix
   by `created_at` descending: every cancelled run from 34773833346
   (2026-09-13T18:10:58Z) through 34819892079 (2026-09-14T07:52:34Z)
   inclusive. Full ID list for the 28 checked: 34773833346, 34774043482,
   34777703864, 34783448860, 34784140368, 34785395381, 34786240747,
   34787566112, 34788314580, 34789201821, 34790105486, 34814835029,
   34815233853, 34816097108, 34816911180, 34817165542, 34817593424,
   34817649859, 34817766094, 34818037515, 34818239526, 34818372089,
   34818471930, 34818972951, 34819023646, 34819591201, 34819796023,
   34819892079.
   **Correction (post-review, via a sixth Codex review comment on PR
   #2786): the 17 unchecked cancelled runs were given only by their first
   and last ID (34748838991, 34771611140), which — run IDs not being
   contiguous — doesn't reconstruct the omitted set.** Full ID list for the
   17 unchecked: 34748838991, 34751319090, 34751685712, 34751893036,
   34752172087, 34752442849, 34752618284, 34752775099, 34753966290,
   34754350011, 34760451392, 34760541936, 34764444776, 34769130236,
   34769599692, 34771267541, 34771611140.
4. **The one hidden job-level failure this check found (run 34774043482) had
   two other jobs — `Test (Docker)` and `Test (macos-latest)` — that ran for
   ~38 and ~40 minutes respectively before being cancelled, long enough to
   have hit a tracked flaky signature and gone unnoticed by a
   conclusion-only check.** Fetched and grepped both jobs' full logs for the
   four tracked signature names and for any panic/failure marker: the Docker
   job actually ran and passed `job_tracking_stores_integration` (`test
   integration::job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it
   ... ok`) before being cancelled; the macOS job was killed mid-`cargo
   build` (`Terminate orphan process: pid (37808) (rustc)` in its final
   lines) and never reached the test phase at all. Positive evidence, not
   absence of it, for these two — see Symptom below for the same check
   applied structurally across the other 27 sampled runs.

None of this changes the pass's bottom line (branch-owned, not a CI health
issue, no match to any tracked signature), but the earlier "zero hits"
framing, the sample-size count, and (in a prior commit on this same PR) an
unverified "fixed by the next push" claim were all wrong as originally
stated.

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
then — per the correction above — also checked job-level conclusions for the
28 most-recently-created of the 45 cancelled runs (62%, best-effort sample;
exact IDs in the correction above).

**For every one of those 28 runs except 34774043482, every `Test`-shaped job
(`Test (${{ matrix.os }})`, `Test ${{ matrix.lane }}`, `Test (Docker)`,
`Trybuild ${{ matrix.shard }}`, `Windows Tier 1 journey`, `Loom`, `Test
suite`, `Coverage (${{ matrix.lane }})`) shows the *unexpanded* matrix
template name as its job name, conclusion `cancelled`, and
created/started/completed timestamps within 1-2 seconds of each other** —
GitHub Actions only substitutes a matrix job's real name once it's assigned
a runner and starts; an unexpanded template name with near-simultaneous
timestamps is direct evidence the job was cancelled before it ever started,
not mid-run, so it cannot have hidden a tracked-signature panic. Only
34774043482 broke this pattern (its Test jobs show real elapsed time and
expanded names) — detailed next.

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
  synced the other side when this commit ran.
  **Correction (post-review, via a further Codex review comment on PR
  #2786): whether the branch's next push actually fixed it is unverified,
  not established.** An earlier version of this entry claimed the very next
  push (run 34777703864, ~13 minutes later) "evidently fixed it" because the
  signature didn't recur in the 26 sampled cancelled runs afterward — but
  that run's own `Test (${{ matrix.os }})` / `Test (${{ matrix.lane }})`
  jobs all show the *unexpanded* matrix template name with conclusion
  `cancelled` (near-simultaneous created/started/completed timestamps,
  consistent with being cancelled before the matrix job even started, not
  after running the test) — so there is no completed test-job conclusion
  from that run to point to either way. The absence of a repeat signature in
  a sample of cancelled runs whose own Test jobs were themselves cancelled
  before completing is not evidence the test passed anywhere; it's just
  absence of observation. Corrected: this failure is not observed again in
  the runs sampled this pass, full stop — whether or how it was actually
  fixed is unverified. Not a CI health issue, and not a match to any tracked
  signature, regardless.
- **The same run's two long-running cancelled jobs, checked directly (not
  inferred from absence)**: `Test (Docker)` ran 2026-09-13T18:48:04Z–19:26:50Z
  (~38 min) before being cancelled; its full log shows
  `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`
  actually ran and passed (`... ok`) — a positive pass, not just an absent
  failure — with no panic or `FAILED` marker anywhere in the log for any of
  the four tracked signatures. `Test (macos-latest)` ran 18:48:07Z–19:28:02Z
  (~40 min) before being cancelled; its log ends mid-`cargo build` (`Terminate
  orphan process: pid (37808) (rustc)`), meaning it never reached the test
  phase at all — it was killed at the toolchain/build step, not during test
  execution.
- **Zero hits on any of the four actively-tracked flaky tests** —
  `live_upgrade` (three signatures), `cache_stampede`, `sim_fault_plan`,
  `job_tracking_stores_integration` (six signatures total) — across
  everything checked this pass: both run-level failures, job-level
  conclusions across the 28 sampled cancelled runs, and full-log inspection
  of the two long-running cancelled jobs inside 34774043482. The remaining
  17 cancelled runs in the window were not checked at all, so this stays a
  best-effort sample, not a proven-exhaustive one, on the cancelled-run half.

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
`examples/cms` in the other) — also working as intended. No test-vs-product
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
  author. Whether or how the third was actually fixed is unverified (see the
  correction in Symptom above); it just isn't observed again in this pass's
  sample.

## 📊 Measurement

No rerun campaign this pass — organic sampling only.

**Correction (post-review, via two further Codex review comments on PR
#2786): the "54 runs, 0 hits" framing below was wrong on two counts, not
just under-scoped.** (1) *Unanchored*: the 24 `success` run IDs behind that
count were never recorded, so — same moving-page problem this ledger
already documents — the population can't be reproduced against a live,
still-growing table. Recorded now, for full anchoring of all 71 runs in the
window (2 `failure` + 45 `cancelled`, both already listed above; the 24
`success`): 34749174494, 34752926346, 34753030227, 34754803183, 34757098823,
34765515241, 34765746780, 34766479411, 34768032048, 34768621010, 34769751539,
34770704250, 34771756276, 34778795964, 34778901499, 34779465564, 34779560168,
34779894202, 34788398417, 34791102646, 34798792725, 34818648526, 34820370400,
34820504735. (2) *Miscalibrated, and the more important error*: 27 of the 28
checked cancelled runs never started a Test-shaped job at all (established
above), and this pass never verified whether the two `failure` runs' test
binaries reached the tracked tests before failing either — so treating all
54 as zero-hit trials manufactures a denominator this pass didn't earn.
Corrected below: no rate or count is asserted. This is organic-sample
triage — matching how every prior daily pass in this ledger has described
its own zero-hit findings — not a rerun-rate measurement; the only trial
count this role's own evidentiary bar would accept comes from the
still-undispatched `manual-macos-contention-check.yml` harness.**

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` (3 signatures) | No occurrence in any log or job conclusion actually inspected this pass (not a rate — see correction above) | Unchanged; harness still undispatched, 6th pass |
| `cache_stampede` | No occurrence in any log or job conclusion actually inspected this pass | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence in any log or job conclusion actually inspected this pass | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | No occurrence in any log or job conclusion actually inspected this pass; one positive pass confirmed (34774043482's Docker job, still n=1 total historically) | Unchanged, undiagnosed |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 6th consecutive idle pass, ~137h |
| `dependabot/cargo/validator-0.21.0` lockfile+compile break | Triaged, branch-owned | Not a CI health issue |
| `claude/happy-edison-fstb1z` Windows edge-conformance self-check | Triaged, branch-owned | Not a CI health issue |
| `claude/epic-meitner-vkej1i` hidden job failure inside cancelled run | Triaged, branch-owned; not observed again (fix unverified) | Not a CI health issue |
| Cancelled-run job-level check | 28/45 sampled (62%), 1 hidden failure found (above) | Best-effort, not exhaustive |
| Cancelled-run long-running-job log check (the 2 jobs that ran >30 min) | 2/2 checked, 0 tracked-signature hits, 1 positive pass confirmed | Complete for this run |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at in [2026-09-13T09:02Z, 2026-09-14T08:00:19Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(run_id, failed_only=true, return_content=true)
# each cancelled run's job-level conclusions via list_workflow_jobs(run_id, filter=latest)
#   → grep for "conclusion":"failure" (run 34774043482 is the one hit found)
#   → the 28 run IDs checked are listed in the correction note at the top of this report
# for any cancelled job with non-trivial (started_at vs completed_at) elapsed time,
# fetch its full log and grep for the tracked signature names + panic/FAILED markers:
get_job_logs(job_id=<Test (Docker) in 34774043482>, return_content=true)
get_job_logs(job_id=<Test (macos-latest) in 34774043482>, return_content=true)
# → grep -E "live_upgrade|cache_stampede|sim_fault_plan|job_tracking_stores_integration|panicked at|test result: FAILED"
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
