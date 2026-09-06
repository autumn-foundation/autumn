# 🚦 Semaphore: CI health follow-up, 2026-09-06

Follow-up to `docs/reports/2026-09-04-semaphore-ci-health-census.md` (#2504)
and `docs/reports/2026-09-05-semaphore-ci-health-followup.md` (PR #2527, open
at the time of this pass). No fix PR ships from this pass either — the hard
gate for one is still not cleared — but this pass finds a concrete defect in
the harness #2527 is about to ship, and adds two days of fresh organic
signal on the open items.

## 🎯 Verdict path

Unchanged from the prior two reports. Good news first: `trunk-dev`'s tip
(`f920bc8`, 2026-09-05T20:29 UTC) is green — the branch-protection escape the
2026-09-05 report flagged (PR #2488 merging 4 commits behind PR #2484's
base) is resolved; `#2518` landed the fix and `trunk-dev` has been building
and testing cleanly since. PR #2527 itself is still open, unmerged, at the
time of this report.

## 🌡️ Symptom

**Finding 1 — the committed rerun harness cannot be dispatched.**
`docs/reports/2026-09-05-semaphore-ci-health-followup.md` and PR #2527 both
describe `.github/workflows/manual-macos-contention-check.yml` as shipped
and ready to dispatch ("a human can dispatch it with one click"). It is not:
GitHub has never successfully registered this workflow. Evidence, harness
being the GitHub Actions API itself plus a fetch of the rendered error page,
zero new CI spend:

- `actions_list list_workflows` returns this workflow's `name` as its own
  file path (`.github/workflows/manual-macos-contention-check.yml`)
  instead of the `name:` declared inside the file ("Manual macOS contention
  check (Semaphore rerun harness)") — the standard symptom of a workflow
  GitHub could not parse. Compare `ci.yml`, whose registered name is `CI`,
  matching its `name:` field exactly.
- All three pushes to `claude/sleepy-brown-3faebr` that touched this file
  produced a run named after it with **zero jobs** and `conclusion:
  failure` (run ids `33960142406`, `33962504398`, `33982014341` — the
  latter pushed *after* a "trim overexplained comments" edit, so this is
  not a comment-related parse issue; the logic is unchanged across all
  three).
- Fetching the rendered run page confirms the exact error: **"Invalid
  workflow file: `.github/workflows/manual-macos-contention-check.yml` at
  Line 34, Column 9 — Unrecognized named-value: 'matrix'. Located at
  position 29 within expression: `fromJSON(inputs.samples) >= matrix.n`"**.

Nobody caught this in PR #2527's own review activity — its 3 PR comments
are all about the *real* `ci.yml` `Test` matrix (the trunk-dev macro
regression and the `live_upgrade` flake), not about this workflow's own
runs, which never surfaced as PR checks at all (an invalid `workflow_dispatch`
-only file produces no check run on the PR — `get_check_runs` for #2527
returns zero rows tied to this file). The harness would have silently done
nothing the first time someone tried to click "Run workflow."

**Finding 2 — two days of organic post-fix signal on the open items.**
An earlier revision of this section sampled only one page (50 runs,
0450→1004 UTC 2026-09-06) and reported n=4, missing roughly eight hours back
to the actual `#2510` merge boundary. Corrected by paginating back to
2026-09-05T20:25:01Z (the merge timestamp) and taking the full union of
non-cancelled `pull_request` runs across both pages, de-duplicated by run
id: **15 non-cancelled runs** in the complete post-merge window
(2026-09-05T20:25:01Z → 2026-09-06T10:04:30Z).

| Run | Created | Conclusion | macOS `Test` outcome |
|---|---|---|---|
| 33990863920 | 20:41:36Z | failure | **N/A** — `Lint`→`Clippy` failed; `Test` matrix never started |
| 33991297425 | 20:50:36Z | success | success |
| 33991380662 | 20:52:23Z | success | success |
| 33991666414 | 20:58:39Z | failure | success† |
| 33991943884 | 21:04:15Z | success | success |
| 33994441171 | 21:55:47Z | failure | success (ubuntu `Test`→`Run tests` failed instead, unrelated) |
| 33999094160 | 23:35:50Z | failure | **N/A** — `Lint`→`Check formatting` failed; `Test` matrix never started |
| 34000856070 | 00:16:31Z | failure | success (`Coverage`→`Generate coverage` failed instead, unrelated) |
| 34002298881 | 00:49:36Z | success | success |
| 34003050046 | 01:06:58Z | success | success |
| 34008308279 | 03:10:04Z | success | success |
| 34011415218 | 04:24:22Z | failure | success (ubuntu `Test`→`Run Docker-dependent tests` failed, unrelated) |
| 34012120234 | 04:41:03Z | success | success |
| 34014673780 | 05:43:04Z | success | success |
| 34015049004 | 05:52:10Z | failure | success (`Test (windows-latest)` failed instead — Finding 3, below) |

† `33991666414` and `34003050046` run on `claude/test-sharding-ci-performance-*`,
a branch modifying `ci.yml` itself to shard the `Test` job (extra jobs like
`Test system-tests (macos-latest)`, `Test markdown (macos-latest)` alongside
the ordinary `Test (macos-latest)`) — not the mainline workflow structure.
`34003050046`'s `Test (macos-latest)` job passed outright. `33991666414`'s
`Test (macos-latest)` job conclusion is `failure`, but its `Run tests` step
— the step that actually runs `cargo test --workspace` and would contain a
`live_upgrade`/`cache_stampede`/`sim_fault_plan` hit — passed; the failure is
in a custom step this experimental branch added (`Run the simulation suite
single-threaded`) that doesn't exist in mainline `ci.yml` and has nothing to
do with the corpus this census tracks. Counted as a pass on that basis, not
pooled uncritically with the mainline-workflow rows.

Two runs (`33990863920`, `33999094160`) never reached the `Test` matrix at
all — `needs: [lint, meta]` blocked them before any OS leg started — so they
are excluded from the denominator entirely, the same convention the
2026-09-04 census used. That leaves **13 eligible executions, 13 clean
macOS passes, 0 hits on `live_upgrade` / `cache_stampede` / `sim_fault_plan`**
since `#2510` merged — directionally consistent with the fix holding, and a
meaningfully larger sample than the n=4 an earlier revision of this section
reported, but still short of the 20-50 this role requires to close the
ledger entry outright; treat as "no disconfirming evidence yet at n=13," not
confirmation.

**Finding 3 — a new single-occurrence non-Linux timing failure.**
Run `34015049004`'s `Test (windows-latest)` job failed with a *different*
test than anything in the existing cluster:
`integration::request_timeout::timeout_fires_cleanly_during_graceful_drain`
panicked on `.expect()` unwrapping a `tokio::time::error::Elapsed` from a
`JoinError` — i.e. a `tokio::time::timeout(...)` around a joined task fired
when the test didn't expect it to, on `windows-latest`, after a 3250s run.
One occurrence — not yet a repeat signature, and not obviously the same
mechanism as `live_upgrade`/`cache_stampede`/`sim_fault_plan` (different
test, different platform-pairing so far: this is the *first* Windows hit in
this test, where the existing cluster is macOS-only) — but it is the same
*shape* of finding the 2026-09-04 census opened with: a hard, unwrap-based
timing assertion failing once on the more heavily-loaded end of the runner
matrix. Recorded here so a second occurrence is recognized as a repeat
rather than re-discovered from zero.

## 🔍 Diagnosis

**Finding 1 (harness defect)**: root-cause category is **CI/process
tooling defect, not a test or product defect** — `matrix` is not a valid
named-value outside `jobs.<job_id>.strategy` and
`jobs.<job_id>.steps[*]`; referencing it in `jobs.<job_id>.if` (as this
workflow does, to filter a static 20-wide matrix down to the requested
sample count) is rejected by GitHub's workflow parser before any job is
ever created. This is exactly Law 1's shape one level up the stack: a
harness that reads as "shipped and dispatchable" in its own PR description
and ledger entry, but silently does nothing, is worse than no harness —
it would have told the next person the campaign was one click away when it
was actually zero clicks away from a no-op.

**Findings 2 and 3**: no verdict rendered — n is too small either way.
Not treated as evidence for or against the existing macOS cluster's
test-vs-product question, per this role's own rule against reading an
organic trickle as a rerun campaign.

## 🔧 Treatment

No fix PR from this pass — the hard gate isn't cleared for the macOS
cluster (still no rerun-campaign data; the harness that would produce it is
the very thing found broken), and Finding 1 is a defect in someone else's
open, unmerged PR rather than something to fork into a competing fix.
Posted the confirmed root cause and a working replacement (compute the
sample list in a small setup job's output and drive `strategy.matrix` from
`fromJSON(needs.setup.outputs.matrix)`, which never needs `matrix` inside
`if`) as a review comment on PR #2527 so it can be corrected before merge
instead of after someone tries to dispatch it and gets nothing.

## 📊 Measurement

- Finding 1: binary/structural (workflow either registers or it doesn't),
  confirmed via GitHub's own parser error text — not a rate, nothing to
  baseline.
- Findings 2/3: n=13 (13/13 clean) and n=1 respectively, both explicitly
  below any threshold this role treats as evidence. No before/after —
  nothing was changed.

## 🔬 Reproduce

Finding 1 — pinned to the specific already-failed run IDs this report cites,
not a live query against the workflow registry: PR #2527 may fix the file
(a working replacement was posted on it), and once it does, re-querying the
*current* registry state would no longer show the parse failure this report
documents. The historical runs stay a fixed record regardless:
```
gh api repos/autumn-foundation/autumn/actions/runs/33982014341 | \
  jq '{conclusion, event, jobs_url: (.jobs_url // .url)}'
gh api repos/autumn-foundation/autumn/actions/runs/33982014341/jobs | jq '.total_count'
# conclusion: "failure", total_count: 0 -- zero jobs ever created for this run.
# Same for 33960142406 and 33962504398, the other two pushes that hit it.
```
Rendering `https://github.com/autumn-foundation/autumn/actions/runs/33982014341`
in a browser shows the "Invalid workflow file" banner with the exact
line/column and expression quoted in Symptom above — that error text is
attached to this specific run, not to the live registry, so it survives the
file being fixed later.

Findings 2/3: `actions_list list_workflow_runs` for `ci.yml`,
`event=pull_request`, `status=completed`, filtered to non-`cancelled`
conclusions, for the window 2026-09-05T20:25Z (the `#2510` merge) through
2026-09-06T10:04Z.
