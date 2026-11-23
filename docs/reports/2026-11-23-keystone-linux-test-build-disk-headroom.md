# 🏛️ Keystone [findings]: the Linux test build is running out of disk (free at end of a green build 38 GB → 9.9 GB in 11 days)

Date: 2026-11-23. Outcome: findings memo, not an RFC. The fix is a two-way
door (a CI config change, well under 2 engineer-weeks to make or undo), so the
team that makes it decides the details in its PR.

## 🎯 Scope

`Build tests (ubuntu-latest)` (`test-build` in `.github/workflows/ci.yml`),
the one Linux job that builds every test binary. Every Linux `test` slice,
every `trybuild` shard and every coverage lane waits on it. When it fails,
they are skipped and `Test suite` goes red.

Window: the 50 most recent completed `ci.yml` push runs on `trunk-dev`,
2026-11-12 to 2026-11-23. Source: the GitHub Actions runs and jobs API and
the raw job logs. See 🔬 Reproduce.

## 📈 Evidence (tier 2: CI record)

**trunk-dev push runs, 2026-11-12 → 11-23.** 19 runs ran to the end (the
rest were cancelled by a newer push). 9 passed and 10 failed. Failures by the
first job that failed:

| Cause | Runs | Note |
| --- | --- | --- |
| `Supply chain (cargo-deny)` advisory gate | 4 | New RUSTSEC advisories. Comes with the domain, and Ballast closes them. |
| **`Build tests (ubuntu-latest)`: runner killed** | **2** (02adfcad 11-20, b9beb562 11-21) | Exit 143 and "The runner has received a shutdown signal" at about 24–25 min, during the compile |
| macOS `test` slice | 2 | One test failure (be896ef6), one `download-artifact` failure (31652505) |
| `Coverage (workspace)` | 1 | 833fa41a |
| Not determined | 1 | 2fb25745. The jobs API returned 502, then no failed job on the first page. |

The Linux build is 2 of 10 failures (20%), and 2 of the 6 that do not come
from advisories.

**Free disk on that job, from its own `df -h` steps.** Same runner image
(`ubuntu-26.04`, 145 GB `/dev/root`, 117 GB free after the disk-reclaim
step), same build (≈760 `Compiling` lines, 279 test binaries, a 7.1–7.2 GB
archive), kache v2.0.0 throughout:

| Green run | Free at end of build | Largest kache store logged | Store cap kache logs |
| --- | --- | --- | --- |
| 2026-11-12 (job 131313186550) | 38 GB (75% used) | 30.8 GB | 7.0 GiB (7,516,192,768 B) |
| 2026-11-18 (job 134552850477) | 22 GB (85%) | 51.0 GB | 7.0 GiB |
| 2026-11-23 (job 137481319386) | **9.9 GB (94%)** | **59.2 GB** | 7.0 GiB |
| 2026-11-21, failed (job 136943131034) | killed at 24m48s | 41.7 GB, 10 min before the kill | 7.0 GiB |

Where it sits relative to the rest: the build and the archive did not grow,
and the base image did not change. The growth is all in the local kache store.
It ends a green build at 4–8 times its own cap. Each `INFO kache::wrapper:
…-gc: cache or disk pressure, triggering background GC … handed the sweep to
the daemon` line in the log reports a larger store than the line before it.

## 💡 Hypothesis (mechanism)

During a build the local kache store holds a second copy of each artifact it
compiles, next to `target/`. The action comment says the same about macOS:
"kache adds a daemon and a second copy of every artifact in its store beside
target/". On top of that copy, the store holds the prefetched set. The prefetch
is a hint for 902 crates, and the first GC lines record 8.5–25.5 GB in the
store within 1–2 minutes of starting, before most compiles. The 7 GiB cap is
not enforced while the build runs. The background GC is handed off, and the
store keeps growing.

At the end of the 2026-11-23 run, 107 GB was in use above the base image:
about 59 GB of kache store, a 7.4 GB archive, and roughly 40 GB of `target/`.
The 59 GB store is larger than the build it caches.

Why it grows from run to run is **not established** by this memo. The likely
candidate is the prefetch manifest and shared S3 store growing with every
protected push. ADR 0038 says the store "needs its own expiry". The
implementing team should confirm it from the kache store and manifest, not from
this memo.

The runner kill is consistent with a full disk, which GitHub-hosted runners
report as a lost or shut-down runner rather than as ENOSPC. Neither failed
log reaches a final `df`, so this is inferred from the trend, not observed.

## 🧭 Do nothing / decide later

Free disk at the end of a green build fell about 2.6 GB a day (38 → 9.9 GB
over 11 days). At that rate a green build has no headroom left within about
4 days of 2026-11-23. The variance between runs is already larger than the
9.9 GB left: 2 of the last 7 Linux builds were killed. If nothing changes,
most trunk-dev pushes go red within a week, with every Linux test slice,
trybuild shard and coverage lane skipped. PR runs build on the same runner
size, and with the same prefetch, so PRs go red too. This blocks every merge
that requires `Test suite`, and it is not a hypothetical cost.

## 🔧 Decision: findings plus a recommendation, decided in a PR

- **Door class:** two-way. Every option below is a change to
  `.github/actions/kache` or the `test-build` job. Making one or undoing it
  takes less than 0.5 engineer-week. ADR 0038 stays: the S3 store and the
  per-compile cache are not in question.
- Recommendation, in order of how cheap and reversible each step is:
  1. Pass `warm: false` to `.github/actions/kache` in `test-build`. The macOS
     archive already does this, for the same reason. This removes the 8–25 GB
     prefetched before the compile starts. It costs prefetch hits on that one
     job. Compiles still read the S3 store on demand.
  2. Enforce a bound on the local store during the build, if kache v2.0.0
     offers one: synchronous eviction, a lower cap, or not keeping a local
     copy of what has been uploaded. This needs the kache docs, which this
     memo did not read.
  3. If steps 1 and 2 do not bring the end-of-build free space back above
     about 30 GB, move `test-build` to `Swatinem/rust-cache`, as #3663 did for
     macOS.

## ⚖️ Alternatives

1. **Defer.** Not viable: see the baseline above. The disk is projected to run
   out within days.
2. **The boring one: a larger runner** through `meta.heavy_runs_on`. This works,
   but it pays every month for a store that is four to eight times its own cap,
   and growth would use up the extra disk too.
3. **Delete more of the runner image** in the disk-reclaim step. The step
   already frees 117 GB. Whatever is left to delete is smaller than the 2.6 GB a
   day the store adds.

## 🗺️ Path

1. **Abort-safe first step:** `warm: false` on `test-build`, plus a job-summary
   line that records free disk and kache store size after the archive step.
   This ships alone and changes no test result. If it does not help, revert one
   line.
2. Check the store bound in kache's docs (option 2) and apply it if one exists.
3. Only if 1 and 2 fall short: `rust-cache` for `test-build` (option 3).

## 📊 Success metric

- Metric: free disk on `/dev/root` at the end of a green `Build tests
  (ubuntu-latest)` on trunk-dev.
- Current: 9.9 GB (2026-11-23). Earlier: 22 GB on 11-18, 38 GB on 11-12.
- Target: ≥ 30 GB, flat across 10 consecutive trunk-dev runs, and 0 runner
  kills on that job.
- Review date: **2027-01-18**.
- Fitness function: the step-1 summary line, made a warning annotation when
  free disk is below 20 GB. It warns, it does not fail, so a full disk shows up
  as a trend before it shows up as a red build.

## 🔬 Reproduce

```bash
# 1. trunk-dev push runs and their conclusions
gh api "repos/autumn-foundation/autumn/actions/workflows/ci.yml/runs?branch=trunk-dev&event=push&status=completed&per_page=50" \
  --jq '.workflow_runs[] | [.id,.created_at,.conclusion,.head_sha[0:8]] | @tsv'
# 2. the failed jobs and steps of one run
gh api "repos/autumn-foundation/autumn/actions/runs/<run_id>/jobs?per_page=100" \
  --jq '.jobs[] | select(.conclusion=="failure") | [.name, ([.steps[]|select(.conclusion=="failure")|.name]|join(";"))] | @tsv'
# 3. disk and kache store size from one Build tests (ubuntu-latest) job log
gh api "repos/autumn-foundation/autumn/actions/jobs/<job_id>/logs" \
  | sed 's/\x1b\[[0-9;]*m//g' | grep -E '/dev/root|gc: cache or disk pressure|workspace-tests.tar.zst'
```

Jobs used: 131313186550 (11-12), 134552850477 (11-18), 136615970610 (11-20,
killed), 136943131034 (11-21, killed), 137481319386 (11-23).
