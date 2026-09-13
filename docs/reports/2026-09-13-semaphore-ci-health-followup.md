# 🚦 Semaphore: CI health follow-up — an escape from four independent MinIO fixes colliding at merge, harness idle a 5th day

**Correction (post-review, via a Codex review comment on this report's own
PR #2768): the original version of this report misattributed the cleanup
commits and understated the collision's scope.** It named #2749/#2750/#2751/
#2752 as the fix commits and framed this as a two-PR (#2740/#2743) collision.
Checked directly against each commit's own diff (not its title, and not any
commit's own self-description — even the commit that removed the dead helper
misattributes what added it): #2749, #2750, and #2751 are empty merges (no
file changes — `trunk-dev` already carried equivalent content from a
different, concurrently-merging branch by the time each landed), and #2752
never touched either MinIO test file. The real chronology involves **at
least six independent PRs**, not two, several of which reached this fix as a
side-effect of an unrelated feature branch's own "drive red CI to green"
work. Corrected throughout below; see the ledger's own corrected entry for
the full per-commit accounting.

**Second correction (post-review, via three further Codex review comments on
PR #2768):** (1) the lint-fallout window actually ends at #2756
(2026-09-13T02:09:18Z), not #2729 (02:30:05Z) — the tree at #2756 is already
confirmed dead-code-free, so #2729 twenty-one minutes later is a subsequent,
unrelated refactor of an already-clean tree, not part of resolving the
fallout; the measured window is therefore ~8.4 hours, not ~8.7. (2) Several
timestamps below combined an already-UTC hour with a redundant `-05:00`
suffix (e.g. writing `17:03:51-05:00` when the source commit's own local
time was `12:03:51-05:00` = `17:03:51Z`) — corrected to plain `Z` throughout.
(3) The "100+ runs each" sample-size claim below was impossible against a
`perPage=100` query (each page is capped at exactly 100) and overstated the
population; corrected to the actual combined count.

**Third correction (post-review, via two further Codex review comments on
PR #2768):** (1) "six independent fixes" conflated genuine independent
outage diagnoses with reactive cleanup commits — #2756 only deletes an
unused helper (not a fresh diagnosis), and #2725's own sub-commit message
says it repairs dead code "the merge from trunk-dev introduced," i.e. it
is reacting to the collision, not independently discovering the outage.
Corrected to 4 independent diagnoses (#2740, #2743, #2722, #2720) plus 2
reconciliation commits (#2725, #2756). (2) The "at least 6 distinct,
unrelated WIP branches" claim named only 5 (counting
`brave-goldberg-gyr60j`'s two hits as one branch) and had folded in two
branches (`fix/reddit-clone-minio-*`, `fix/minio-quay-registry`) that are
actually other sessions' own outage-fix attempts, not dead-code victims;
corrected to the 5 branches actually confirmed by job-log inspection.

**Fourth correction (post-review, via two further Codex review comments on
PR #2768):** (1) the lint fallout is actually **two disjoint intervals**,
not one continuous window: `avatar_s3_integration.rs`'s dead-code seed was
fixed by #2725 at 23:19:07Z, and `offsite_backup.rs`'s competing
`minio_image()` helper was not introduced until #2720 at 23:22:06Z — so the
tree was briefly, fully dead-code-free for about 3 minutes in between.
Corrected to two intervals (17:46:17Z–23:19:07Z, ~5h33m, and
23:22:06Z–02:09:18Z, ~2h47m) totaling ~8.3h, not one continuous ~8.4h span.
(2) PR #2740's green `Test (Docker)` run verifies the Quay redirect only for
the two files reached by this repo's Docker sweeps
(`offsite_backup.rs`, `sqlite_replication_s3.rs`); `avatar_s3_integration.rs`'s
own test is outside both sweeps (confirmed against `AGENTS.md` and a
repo-wide search of `ci.yml` — no match) and was never actually run by that
job, so calling the whole fix "CI-natively verified" overstated it. (3) The
"four actively-tracked flaky signatures" phrase below is ambiguous: there
are four tracked *tests*, but `live_upgrade` alone carries three distinct
signatures, for six signatures total — reworded below.

Follow-up to `docs/reports/2026-09-11-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — the one substantive finding (a merge-time collision among several
independently-authored fixes for the same outage) had already fully resolved
itself on `trunk-dev` before this pass began; what remains is recording it
accurately as escape-analysis evidence per this role's own evidentiary tiers.
The other finding is confirmation, not a new defect (with a scope caveat):
PR #2740's own `Test (Docker)` run (previously "pending") completed
successfully, CI-natively verifying the fix for the two files reached by
this repo's Docker sweeps — not the third file, which no CI job actually
runs (see the fourth correction above).

## 🎯 Verdict path

Unchanged from the 2026-09-11 pass: `trunk-dev` is green, and the required
gate developers wait on is `Test suite` (`test-gate`), fed by
`[test, trybuild, test-features, test-docker]`. `coverage` remains a separate,
non-gating lane. `manual-macos-contention-check.yml` remains dispatch-only —
**still zero `workflow_dispatch` runs**, the same number reported on
2026-09-08 through 2026-09-11, now a 5th consecutive idle day (~90 hours since
it became dispatchable at 2026-09-08T15:07:44Z).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from roughly
2026-09-12T13:58Z to 2026-09-13T09:02Z (~19 hours, two `perPage=100` pages of
the workflow-run list — ~130+ runs combined, not "100+ each"). Triaged every
non-cancelled failure by job/log inspection rather than by branch name:

- **A pre-existing, already-tracked outage accounts for the earliest
  failures**: `blissful-rubin-dzer7y` at 2026-09-12T15:48:35Z (before #2740
  merged at 17:03:52Z) failed `Test (Docker)` at
  `sqlite_replication_s3::replicates_to_and_restores_from_a_real_s3_endpoint`
  with the original Docker-Hub-repository-gone panic — the same outage
  #2740 fixed 75 minutes later. Not new.
- **A merge-time collision among four independent fixes for that same
  outage produced ~8.3 hours of spurious `Lint`/`Clippy` dead-code
  failures, across two disjoint intervals, on 5 unrelated branches,
  needing two further reconciliation
  commits to fully clean up.** The same universally-visible failure
  (every `Test (Docker)` run 404ing on the dead `minio/minio` Docker Hub
  repository, regardless of a PR's own diff) was independently
  re-diagnosed and fixed, each from its own PR's own red CI, inside four
  separate PRs within a ~6.3 hour window: #2740 (this ledger's own fix,
  2026-09-12T17:03:51Z, inlined the redirect at all four call sites, no
  helper function); #2743 (independent, 17:46:17Z, added an unwired
  `MINIO_IMAGE` constant — the first dead-code seed); #2722 (an unrelated
  replay-guard test PR, sub-commit "fix: point MinIO testcontainers at
  quay.io (Docker Hub repo pulled)" — its own message independently
  re-derives the outage from its own PR's CI failure, 23:21:15Z):
  introduced a new `start_minio()` helper; #2720 (an unrelated
  `SeqKey`-ordering PR, sub-commit "fix: MinIO Docker tests point at
  quay.io, not the dead Docker Hub repo," likewise independently
  re-derived, 23:22:06Z, one minute after #2722): introduced a
  *competing*, uncalled `minio_image()` helper — the second dead-code
  seed. Two further commits then **reconciled** the collision rather than
  independently diagnosing anything: #2725 (an unrelated `autumn upgrade`
  codemod PR, sub-commit explicitly titled "fix: use the MINIO_IMAGE
  const **the merge from trunk-dev introduced**," 23:19:07Z) wired
  #2743's constant into its call site — landing *before* #2722/#2720, so
  the tree was briefly, fully dead-code-free for about 3 minutes
  (23:19:07Z–23:22:06Z: `avatar_s3_integration.rs` already fixed, and
  `offsite_backup.rs` had neither helper yet); #2756
  (2026-09-13T02:09:18Z, 9h05 after #2740) deleted #2720's uncalled
  helper — its diff touches nothing else, and this is where the tree is
  clean for good. #2729 (a large, unrelated wire-contracts feature
  PR whose long-lived branch had merged `trunk-dev` three times over the
  same window and picked up a MinIO fix each time) merged 21 minutes
  later, at 02:30:05Z, but that is a subsequent refactor of the
  already-clean tree left by #2756, not part of resolving the fallout —
  it reintroduces and re-collapses its own branch's separate
  `minio_image()` copy entirely within its own single squashed commit
  (`trunk-dev` itself never saw that intermediate duplicate), and lands
  one clean improvement as a side effect: a new regression test,
  `minio_image_pulls_from_the_public_registry`. None of
  #2725/#2722/#2720/#2729 has "MinIO" in its own PR title — each reached
  this fix only as a side-effect of its own unrelated work hitting the
  same red CI. Confirmed via job-log inspection on 5 distinct, unrelated
  WIP branches between 17:46:17Z and 02:09:18Z:
  `claude/friendly-ritchie-uw76a2`,
  `claude/tender-galileo-6f3dr7`, `claude/epic-clarke-8nbaes`,
  `claude/brave-goldberg-gyr60j` (both signatures), `claude/busy-cerf-9zos9k`
  — each failing `Lint` with either `` error: function `minio_image` is
  never used `` (`autumn-cli/tests/integration/offsite_backup.rs:216`) or
  `` error: constant `MINIO_IMAGE` is never used ``
  (`examples/reddit-clone/tests/avatar_s3_integration.rs:22`). (The branch
  names `fix/reddit-clone-minio-*` and `fix/minio-quay-registry` were also
  visible in the same window; those are other sessions' own outage-fix
  attempts, not dead-code victims, so they are not counted in the 5.)
  `trunk-dev`'s current tip (`6e71bfb`, post-#2729's later refactor) is
  clean and consolidated onto one `minio_image()` helper with a fast
  Docker-free regression test guarding it — confirmed directly against
  the file.
- **Everything else in the window is WIP-branch-owned, not a CI health
  issue**: a `dependabot/cargo/rust-toolchain-1.120.0` bump broke
  `autumn-cli`'s own `semver_script_checks_optional_features_with_pinned_rustdoc_toolchain`
  test (the bump moved the pinned toolchain the test asserts against); a
  `vesper/bugbash-2499-capture-min-length` branch failed its own new
  `semver` hygiene check on `macos-latest`; the previously-reported
  `claude/happy-edison-fstb1z` `Clippy` churn (9 runs, one person iterating
  on not-yet-fixed lint issues) continued. None of these are cross-branch
  or environment-dependent.
- **Zero hits on any of the four actively-tracked flaky tests** —
  `live_upgrade` (three distinct signatures), `cache_stampede`,
  `sim_fault_plan`, `job_tracking_stores_integration` (six signatures
  total) — in the sampled window.

## 🔍 Diagnosis

The MinIO-collision fallout is **neither a test defect nor a product
defect — a process/coordination gap**. Four PRs independently and correctly
diagnosed the same 100%-reproducible external-registry outage (MinIO's
Docker Hub repository having been removed) and independently wrote a correct
fix; two more then had to reconcile the dead code those four collectively
left behind. Nothing in `ci.yml` or the repo's own tooling surfaces "another
open PR already targets this exact failure" before merge, and nothing flags
"this branch's own dead code came from a trunk-dev merge, not its own diff"
either — so a failure visible to literally every open PR at once drew
repeated, uncoordinated fixes, including from PRs whose own subject matter
had nothing to do with MinIO. This is Tier 1
escape-analysis evidence per this role's own evidentiary tiers — a measured
gap, now closed (with a regression test added as a side benefit), not a new
quarantine candidate. No test-vs-product verdict is needed because no test's
own correctness was ever in question: the failures were exclusively
`-D warnings`/`-D dead-code` lint gate hits, not runtime behavior.

PR #2740's own `Test (Docker)` check (job 103543584136, workflow run
34688787858) completed `success` at 2026-09-12T11:55:09Z, which the
2026-09-12 ledger entry had flagged as still-pending. That closes the loop
for `offsite_backup.rs` and `sqlite_replication_s3.rs` — both reached by this
repo's Docker sweeps, so the Quay redirect is now CI-natively verified for
those two, not merely `clippy`-clean locally. `avatar_s3_integration.rs`'s
own test sits outside both sweeps and outside any other `ci.yml` job
(confirmed by a repo-wide grep and against `AGENTS.md`'s own description of
sweep scope), so its registry pull remains compile-and-lint-verified only.

## 🔧 Treatment

No fix PR — there was nothing left to fix. The escape resolved itself via
six commits (four independent diagnoses plus two reconciliation commits)
from other sessions before this pass began; the only action here is
recording it accurately, per this role's own instruction that an escape
found via triage is filed/recorded rather than silently noted and moved
past.

- `docs/ci-health/quarantine-ledger.md` updated:
  - Closed the MinIO/Quay entry's pending CI-native verification, then
    scope-corrected it to name exactly the two files that verification
    actually covers.
  - Added a new "Escape" entry documenting the four-PR collision and its
    two reconciliation commits, the measured ~8.3h blast radius across
    two disjoint intervals (5 confirmed branches, 2 failure signatures),
    the actual per-commit resolution chronology, and the coordination-gap
    mechanism classification — then corrected that same entry four times,
    same day, after Codex review comments caught first the misattributed
    commits, then the window's wrong end-commit, then the conflation of
    independent diagnoses with reactive cleanups and an overstated branch
    count, then the single-continuous-window overstatement and the
    verification-scope overstatement (see the correction notes at the top
    of this report).
  - Added a 2026-09-13 dated update to the `live_upgrade` entry (5th
    consecutive pass, harness still at 0 dispatches, ~90h idle, zero new
    hits on any tracked signature in this pass's window) and to the
    `job_tracking_stores_integration` entry (no repeat, still n=1).
- **Recommendation for a human, unchanged from the last four passes**:
  dispatch `manual-macos-contention-check.yml` (`samples: "20"`) against a
  `trunk-dev` commit at or after `8fae8af`. ~90 hours idle since it became
  dispatchable is now most of a week of not even the partial evidence it
  could be producing for the `live_upgrade`/`cache_stampede`/
  `sim_fault_plan` investigation.
- **No action needed on the WIP failures** (dependabot toolchain bump,
  `capture_min_length`, `happy-edison-fstb1z`'s lint churn) — each belongs
  to its own branch's author, not to CI health.

## 📊 Measurement

No rerun campaign this pass — organic sampling plus one escape write-up.

| Item | This pass | Status |
|---|---|---|
| MinIO/Quay fix (#2740) | CI-native `Test (Docker)` run confirmed success | Closed — verified for the 2 files reached by the Docker sweeps; `avatar_s3_integration.rs` remains compile/lint-only |
| MinIO-fix collision escape (4 diagnoses + 2 reconciliations) | ~8.3h across 2 disjoint intervals, 5 branches, 2 signatures, all `Lint`-only | Closed — resolved before this pass began; ledger attribution, window structure, verification scope, and diagnosis/cleanup split all corrected same-day per Codex review |
| `live_upgrade` (3 signatures) | 0 new hits in ~19h window | Unchanged; harness still undispatched, 5th day |
| `cache_stampede` | 0 new hits | Unchanged, undiagnosed |
| `sim_fault_plan` | 0 new hits | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | 0 new hits (still n=1 total) | Unchanged, undiagnosed |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 5th consecutive idle pass, ~90h |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1..2)
# → filtered to created_at in [2026-09-12T13:58Z, 2026-09-13T09:02Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(run_id, failed_only=true, return_content=true)
```

The escape's two signatures:

```
get_job_logs(run_id=34726714162, failed_only=true, return_content=true)
# → error: function `minio_image` is never used
#   --> autumn-cli/tests/integration/offsite_backup.rs:216:4

get_job_logs(run_id=34710860744, failed_only=true, return_content=true)
# → error: constant `MINIO_IMAGE` is never used
#   --> examples/reddit-clone/tests/avatar_s3_integration.rs:22:7
```

PR #2740's CI-native confirmation:

```
pull_request_read(get_check_runs, pullNumber=2740)
# → "Test (Docker)" job 103543584136, conclusion: success, completed 2026-09-12T11:55:09Z
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
