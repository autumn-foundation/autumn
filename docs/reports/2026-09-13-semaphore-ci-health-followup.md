# 🚦 Semaphore: CI health follow-up — an escape from at least six independent MinIO fixes colliding at merge, harness idle a 5th day

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

Follow-up to `docs/reports/2026-09-11-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — the one substantive finding (a merge-time collision among several
independently-authored fixes for the same outage) had already fully resolved
itself on `trunk-dev` before this pass began; what remains is recording it
accurately as escape-analysis evidence per this role's own evidentiary tiers.
The other finding is confirmation, not a new defect: PR #2740's own
`Test (Docker)` run (previously "pending") completed successfully, so that
fix is now CI-natively verified rather than merely clippy-clean.

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
- **A merge-time collision among at least six independent fixes for that
  same outage produced ~8.4 hours of spurious `Lint`/`Clippy` dead-code
  failures on unrelated branches.** The same universally-visible failure
  (every `Test (Docker)` run 404ing on the dead `minio/minio` Docker Hub
  repository, regardless of a PR's own diff) was independently fixed
  inside six separate PRs within a ~6.5 hour window: #2740 (this ledger's
  own fix, 2026-09-12T17:03:51Z, inlined the redirect at all four call
  sites, no helper function); #2743 (independent, 17:46:17Z, added an
  unwired `MINIO_IMAGE` constant — the first dead-code seed); #2725 (an
  unrelated `autumn upgrade` codemod PR, one of whose sub-commits wired
  that constant in, 23:19:07Z); #2722 (an unrelated replay-guard test PR,
  sub-commit introducing a new `start_minio()` helper, 23:21:15Z); #2720
  (an unrelated `SeqKey`-ordering PR, sub-commit introducing a
  *competing*, uncalled `minio_image()` helper one minute later,
  23:22:06Z — the second dead-code seed); and #2756 (removed the uncalled
  helper, 2026-09-13T02:09:18Z — **the tree is confirmed dead-code-free at
  this commit**, ending the fallout window here). #2729 (a large,
  unrelated wire-contracts feature PR whose long-lived branch had merged
  `trunk-dev` three times over the same window and picked up a MinIO fix
  each time) merged 21 minutes later, at 02:30:05Z, but that is a
  subsequent refactor of the already-clean tree left by #2756, not part
  of resolving the fallout — it reintroduces and re-collapses its own
  branch's separate `minio_image()` copy entirely within its own single
  squashed commit (`trunk-dev` itself never saw that intermediate
  duplicate), and lands one clean improvement as a side effect: a new
  regression test, `minio_image_pulls_from_the_public_registry`. None of
  #2725/#2722/#2720/#2729 has "MinIO" in its own PR title — each reached
  this fix only as a side-effect of its own unrelated work hitting the
  same red CI. Confirmed via job-log inspection on at least 6 distinct,
  unrelated WIP branches between 17:46:17Z and 02:09:18Z:
  `claude/friendly-ritchie-uw76a2`,
  `claude/tender-galileo-6f3dr7`, `claude/epic-clarke-8nbaes`,
  `claude/brave-goldberg-gyr60j` (both signatures), `claude/busy-cerf-9zos9k`,
  each failing `Lint` with either `` error: function `minio_image` is
  never used `` (`autumn-cli/tests/integration/offsite_backup.rs:216`) or
  `` error: constant `MINIO_IMAGE` is never used ``
  (`examples/reddit-clone/tests/avatar_s3_integration.rs:22`). `trunk-dev`'s
  current tip (`6e71bfb`, post-#2729's later refactor) is clean and
  consolidated onto one `minio_image()` helper with a fast Docker-free
  regression test guarding it — confirmed directly against the file.
- **Everything else in the window is WIP-branch-owned, not a CI health
  issue**: a `dependabot/cargo/rust-toolchain-1.120.0` bump broke
  `autumn-cli`'s own `semver_script_checks_optional_features_with_pinned_rustdoc_toolchain`
  test (the bump moved the pinned toolchain the test asserts against); a
  `vesper/bugbash-2499-capture-min-length` branch failed its own new
  `semver` hygiene check on `macos-latest`; the previously-reported
  `claude/happy-edison-fstb1z` `Clippy` churn (9 runs, one person iterating
  on not-yet-fixed lint issues) continued. None of these are cross-branch
  or environment-dependent.
- **Zero hits on any of the four actively-tracked flaky signatures**
  (`live_upgrade`'s three signatures, `cache_stampede`, `sim_fault_plan`,
  `job_tracking_stores_integration`) in the sampled window.

## 🔍 Diagnosis

The MinIO-collision fallout is **neither a test defect nor a product
defect — a process/coordination gap, and a six-way one**. Every PR involved
independently and correctly diagnosed the same 100%-reproducible
external-registry outage (MinIO's Docker Hub repository having been removed)
and independently wrote a correct fix; nothing in `ci.yml` or the repo's own
tooling surfaces "another open PR already targets this exact failure" before
merge, and nothing flags "this long-lived branch's last trunk-dev merge
already picked up a fix for this" either — so a failure visible to literally
every open PR at once drew repeated, uncoordinated fixes, including from PRs
whose own subject matter had nothing to do with MinIO. This is Tier 1
escape-analysis evidence per this role's own evidentiary tiers — a measured
gap, now closed (with a regression test added as a side benefit), not a new
quarantine candidate. No test-vs-product verdict is needed because no test's
own correctness was ever in question: the failures were exclusively
`-D warnings`/`-D dead-code` lint gate hits, not runtime behavior.

PR #2740's own `Test (Docker)` check (job 103543584136, workflow run
34688787858) completed `success` at 2026-09-12T11:55:09Z, which the
2026-09-12 ledger entry had flagged as still-pending. That closes the loop:
the Quay redirect is now CI-natively verified, not merely `clippy`-clean
locally.

## 🔧 Treatment

No fix PR — there was nothing left to fix. The escape resolved itself via
six PRs from other sessions before this pass began; the only action here is
recording it accurately, per this role's own instruction that an escape
found via triage is filed/recorded rather than silently noted and moved
past.

- `docs/ci-health/quarantine-ledger.md` updated:
  - Closed the MinIO/Quay entry's pending CI-native verification (confirmed
    success).
  - Added a new "Escape" entry documenting the six-PR collision, its
    measured ~8.4h blast radius (6 confirmed branches, 2 failure
    signatures), the actual per-commit resolution chronology, and the
    coordination-gap mechanism classification — then corrected that same
    entry twice, same-day, after Codex review comments caught the initial
    misattribution and then the window's wrong end-commit (see the
    correction notes at the top of this report).
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
| MinIO/Quay fix (#2740) | CI-native `Test (Docker)` run confirmed success | Closed, fully verified |
| Six-PR MinIO-fix collision escape | ~8.4h window, 6 branches, 2 signatures, all `Lint`-only | Closed — resolved by six independent PRs before this pass began; ledger attribution and window end-point both corrected same-day per Codex review |
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
