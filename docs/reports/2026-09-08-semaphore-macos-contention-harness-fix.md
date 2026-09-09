# 🚦 Semaphore: fix the macOS contention rerun harness (invalid parse → valid, unverified → actionlint-clean)

Follow-up to `docs/reports/2026-09-05-semaphore-ci-health-followup.md` (PR
#2527, merged) and PR #2548 ("broken rerun harness found in PR #2527",
closed without a fix — it posted the root cause as a review comment on
#2527 while #2527 was still open, but #2527 merged before anyone applied
it). This pass fixes the defect #2548 diagnosed but did not patch.

## 🎯 Verdict path

`trunk-dev` tip (`ac5a7ff`) is green. The branch-protection escape from the
2026-09-05 report (#2488 merging 4 commits stale) is resolved. No new
branch-protection setting is confirmed from this session either — still
flagged, not changed, per this role's ask-before rule.

`manual-macos-contention-check.yml` has **never successfully registered a
dispatch**: `actions_list` shows zero `workflow_dispatch` runs for its
workflow ID (350840451) since it was created on 2026-09-05, only the
`push`-triggered parse-failure runs #2548 found (zero jobs, `conclusion:
failure`). The macOS timing cluster this harness exists to investigate
(`live_upgrade` / `cache_stampede` / `sim_fault_plan`, tracked in
`docs/ci-health/quarantine-ledger.md`) is still sitting on organic-sample
evidence only, three days after the harness that was supposed to move past
it was merged.

## 🌡️ Symptom

Reproduced #2548's finding directly with `actionlint` (rhysd/actionlint
v1.7.7) against the file as it stood on `trunk-dev` before this change:

```
.github/workflows/manual-macos-contention-check.yml:34:37: context "matrix"
is not allowed here. available contexts are "github", "inputs", "needs",
"vars". see https://docs.github.com/en/actions/learn-github-actions/contexts#context-availability
for more details [expression]
   |
34 |     if: fromJSON(inputs.samples) >= matrix.n
   |                                     ^~~~~~~~
```

This matches the exact `Invalid workflow file` error #2548 got back from
GitHub's own parser on the live dispatch attempts (`Line 34, Column 9 —
Unrecognized named-value: 'matrix'`), confirming it deterministically
rather than by memory of that report.

## 🔍 Diagnosis

**Root-cause category**: CI/process tooling defect (harness), not a test or
product defect. `jobs.<job_id>.if` only has access to the `github`,
`inputs`, `needs`, and `vars` contexts — `matrix` isn't resolved yet at the
point a job-level `if` is evaluated, since matrix expansion happens per job
*instance*, and the job-level `if` decides whether the job template runs at
all. Gating "how many of the 20 matrix slots actually run" therefore cannot
live in `jobs.test.if`; it has to shrink the matrix itself before
`strategy.matrix` is evaluated.

Same shape as Law 1, one level up in the stack: a rerun harness that reads
as shipped and dispatchable but silently produces zero jobs is worse than
no harness, because it lets "the campaign is committed" stand in for "the
campaign can run."

## 🔧 Treatment

Added a `plan` job (`ubuntu-latest`, cheap) that computes an `n`-length
array from `inputs.samples` via `seq | jq`, and pointed `test`'s
`strategy.matrix` at `fromJSON(needs.plan.outputs.matrix)` instead of a
fixed 20-entry list plus an invalid job-level gate. Picking "5" now
dispatches exactly 5 macOS VMs, not 20 with 15 short-circuited (the
short-circuit was also never reachable, since the whole file failed to
parse) — this doubles as a cost fix, not just a correctness one, since a
human dispatching "5" expecting 5 VMs of spend is exactly what the
ask-before rule on new CI spend assumes happens.

## 📊 Measurement

**Before**: `actionlint` on the pre-change file → 1 error, `context "matrix"
is not allowed here` (reproducing #2548's live-dispatch failure).

**After**: `actionlint .github/workflows/*.yml` → 0 errors, exit 0, across
every workflow file in the repo (ran the whole directory, not just this
file, while the tool was in hand — no other file has this defect).

Also hand-verified the matrix-generation step for all three allowed
`samples` values:

```
$ for n in 5 10 20; do echo "matrix=$(seq 1 "$n" | jq -R . | jq -cs '{n: .}')"; done
matrix={"n":["1","2","3","4","5"]}
matrix={"n":["1","2","3","4","5","6","7","8","9","10"]}
matrix={"n":["1",...,"20"]}
```

This is a parse/structural fix, not a flake rate, so there is no rerun-rate
before/after to report — the "revert check" equivalent here is that
`actionlint` still flags the exact pre-fix construct when reverted (trivial:
the before/after pair above already shows both sides of that same
one-line change).

**Not done in this pass, still gated on human sign-off** (new CI spend,
this role's own ask-before item): actually dispatching the fixed workflow
against a green `trunk-dev` commit to collect the ≥20-sample Tier-1 evidence
the ledger's three under-investigation entries need to close. #2548 already
banked 13/13 clean organic macOS samples since #2510 merged — directionally
reassuring, still short of this role's own N floor. That gap is unchanged by
this fix; this fix only makes closing it possible.

## 🔬 Reproduce

```
curl -sSL https://github.com/rhysd/actionlint/releases/download/v1.7.7/actionlint_1.7.7_linux_amd64.tar.gz \
  | tar xz -C /tmp/actionlint actionlint
/tmp/actionlint/actionlint .github/workflows/manual-macos-contention-check.yml
```

Dispatch (still requires sign-off): `manual-macos-contention-check.yml`,
`sha` pinned to a green `trunk-dev` commit, `samples: "20"` for the
low-rate-flake evidence bar the ledger entries need.
