# 🚦 Semaphore: CI health follow-up — `sqlite_jobs_scheduler_e2e` flake reproduced outside CI for the first time (3/100 at default parallelism, 0/20 serial control)

Follow-up to `docs/reports/2026-09-22-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass had
working network and toolchain access in its own sandbox (as the 2026-09-22
pass did), and used it to follow through on that pass's own next step for
`sqlite_jobs_scheduler_e2e::sqlite_job_backend_tracks_job_status_durably`: run
the whole test binary at default parallelism, not filtered to one test. That
reproduced the flake — **3/100** — for the first time outside CI, against a
same-day **0/20** fully-serial control on the identical binary. A third
organic hit also turned up this pass's own sampling window, in a job shape
(`Coverage (sandbox-sqlite)`) not previously checked for this signature,
raising the organic count from n=2 to n=3. No fix opens this pass: the
root-cause *category* (concurrency/shared-state between tests in the same
binary) is now confirmed, but the *specific* shared resource is not, and this
role's own hard gate requires both before a fix PR. No new hits on any other
tracked signature.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **15th consecutive
idle pass** (~352.9 hours, past 14.7 days, since it became dispatchable
2026-09-08T15:07:44Z). Not dispatched this pass: new macOS CI spend needs a
human sign-off per this role's own rules, unavailable in this unattended run.

## 🌡️ Symptom

**Organic-hit sampling**, 2026-09-22T06:24:50Z (exclusive) to
2026-09-23T07:37:08Z (~25.2h): the `event=pull_request` filter on
`list_workflow_runs` returned stale/miscounted pages this pass (`total_count`
swinging by hundreds between near-identical calls, page 1 anchored ~three
weeks in the past) — worked around by querying with no event filter
(`status=completed` only, which paginates correctly, newest-first) and
filtering to `event: "pull_request"` client-side. One `perPage=100`/page=1
query, span 2026-09-21T18:16:27Z–2026-09-23T07:37:08Z, fully covering the
window with margin — 58 `pull_request` runs in-window: 27 success / 24
cancelled / **7 failure**. All 7 triaged at job/log level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 35806829348 | `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0` | `MSRV`, all three `Test (${{ matrix.os }})` | already-documented **own subject matter** — the action-pin bump, unmerged. |
| 35754864260 | `vesper/bugbash-2312-bootstrap-ingress` | `Lint` (Clippy) | branch-owned WIP. |
| 35725615129 | `vesper/bugbash-2363-cache-audit-profile` | `Lint` (fmt), `MSRV`, `Diesel migration version collisions` | branch-owned WIP, all three on this branch's own in-progress diff. |
| 35725506589 | `vesper/bugbash-2419-doctor-strict-manifest` | `Lint` (Clippy), `MSRV`, `Diesel migration version collisions` | branch-owned WIP. |
| 35725462356 | `vesper/bugbash-2331-csv-required-columns` | `Lint` (Clippy) | branch-owned WIP. |
| 35754765898 | `vesper/bugbash-2311-validate-before-dedup` | `Lint` (Clippy) | branch-owned WIP. |
| 35754806914 | `vesper/bugbash-2310-tombstone-lock-order` (PR "fix(search): lock tombstones…", touches no job/SQLite code) | `Coverage (sandbox-sqlite)` | **new organic hit** — see below, not branch-owned. |

The last row's run showed `conclusion: failure` at the run level while the
default `list_workflow_jobs` page (30 of 34 jobs) reported every job
`success` — investigated rather than written off as a tooling artifact: the
missing 4 jobs were the `Coverage (${{ matrix.lane }})` matrix, on page 2.
`Coverage (sandbox-sqlite)` failed its "Generate coverage (plugin-sandbox +
sqlite)" step with the identical `sqlite_job_backend_tracks_job_status_durably`
panic already tracked in this ledger (same line, same message). **n=2→n=3
organic**, and on a branch whose diff cannot own it (search/ledger-locking
code only), the same reasoning already applied to the first two hits.

None of the 7 match `live_upgrade`, `cache_stampede`, `sim_fault_plan`, or
`job_tracking_stores_integration` (closed).

## 🔍 Diagnosis

**`sqlite_job_backend_tracks_job_status_durably` — root-cause *category* now
confirmed by a controlled local reproduction; the specific shared resource is
not yet identified.**

The third organic hit matters beyond the raw count: `Coverage
(sandbox-sqlite)`'s coverage-generation step invokes the same
`sqlite_jobs_scheduler_e2e` binary with no `--test-threads` flag — the same
default-parallelism shape as `ci.yml`'s ordinary `SQLite runtime
(feature=sqlite)` job (the shape both original organic hits occurred under,
per the 2026-09-22 report), and *not* the shape
`manual-sqlite-jobs-rerun-check.yml`'s existing `rerun` job actually
dispatches (filtered to the one test, `--test-threads=1`). All three organic
hits to date have occurred under default parallelism; none has ever occurred
under the filtered/serial shape the existing harness tests.

That is exactly the variable this pass isolated. With a working local
toolchain, it built `sqlite_jobs_scheduler_e2e` once and ran two same-day,
same-toolchain samples:

- **Whole binary (all 27 tests), no filter, no `--test-threads` override —
  libtest's default parallelism** — 100 iterations: **3/100 failed**
  (iterations 43, 78, 95), identical signature and line each time.
- **Whole binary, fully serial (`--test-threads=1`, no filter)** — 20
  iterations, as a same-day control: **0/20 failed.**

Combined with the existing isolated-single-test results from 2026-09-22
(0/100 local + 0/50 CI-native, both `--test-threads=1` and filtered to just
this test), four independent samples now agree on one boundary: this test
fails only when it runs *concurrently* with its own siblings in the same
binary — never alone, and never when the whole binary runs one test at a
time.

**Test-vs-product verdict: not rendered, and — after a second review
correction below — not leaning either way.** This report originally leaned
"presumptively test-side," reasoning that a real deployment does not run 27
concurrent test functions against one SQLite file. A second Codex comment on
PR #2922 correctly pointed out that reasoning doesn't survive the pool-size
correction just above: the target test's own `worker_loop` (spawned by
`start_runtime`) and its own `enqueue_tracked` call draw connections from the
*same* pool concurrently, entirely within this one test, independent of any
sibling — the same intra-pool multi-connection shape a production deployment
hits whenever a worker loop and a request-path enqueue run against one
SQLite file at once, which is this backend's normal operating mode, not a
test artifact. Concurrent siblings may simply be perturbing scheduling
enough to trigger a race that already lives in that pool usage, in which
case the defect would be product-reachable. Sibling-test concurrency remains
a live, separate candidate too. Both directions stay open until the specific
resource is identified.

**Correction, added post-review (a Codex comment on PR #2922 caught this
before merge):** this report originally claimed `build_sqlite_pool` pins
`pool_size: 1`, ruling out a stale-prepared-statement race across
connections in this test's own pool. That was wrong — it conflated a
*different* test's explicit `pool_size: 1` with `build_sqlite_pool` itself.
Read directly, `build_sqlite_pool` (`autumn/tests/sqlite_jobs_scheduler_e2e.rs:78-88`)
builds a `DatabaseConfig` with no `pool_size` override, so it inherits
`DatabaseConfig::default()`'s value — **10** — and `create_pool` passes that
straight through as the pool's `max_size`. The target test's pool can hold
up to 10 physical connections, so a connection-local stale statement or
schema-visibility race is **not** ruled out and remains an open candidate.
See the ledger entry's own correction for the full detail. The one resource
this test provably shares with the rest of the process is
`job::global_job_client()` — the same
process-global the 2026-09-11 `job_tracking_stores_integration` entry already
established `enqueue_tracked` routes through. The existing "ruled out"
finding for `GLOBAL_JOB_CLIENT` in this entry only checked whether *other
lock-holding* siblings truly interleave with the target test (they cannot —
the lock is held for the whole test); it did not check whether a
`start_runtime` call's spawned worker-loop task can still be running after
`shutdown.cancel()` and after the owning test's
`global_job_runtime_test_lock()` guard is dropped, into a window where a
different, non-lock-holding sibling (or the next lock-holder) is active. That
gap — not a new hypothesis, an unexamined corner of the existing one — is
the named next step.

## 🔧 Treatment

No fix this pass. Per this role's hard gate, a fix PR needs the root-cause
category *and* the specific defect; only the category (resource contention
between concurrently-scheduled tests in the same binary) is confirmed. Naming
the specific defect is next pass's work, not this one's.

Added `rerun_default_parallelism` to
`.github/workflows/manual-sqlite-jobs-rerun-check.yml`: a second job,
alongside the existing filtered/serial `rerun` job, that builds the same
binary once and runs it *whole* (no filter, no `--test-threads` override) N
times, uploading each iteration's full log — the CI-native form of the local
repro above, so a future pass (or CI itself) can confirm the 3/100 figure
without needing a local sandbox with outbound network access. Not
dispatchable this pass: `workflow_dispatch` only accepts a workflow already
on the repository's default branch (`trunk-dev`), the same gotcha every
harness in this ledger has hit on its own introduction pass.

## 📊 Measurement

| Protocol | Result |
|---|---|
| Whole binary, default parallelism, 100 same-commit reruns (this pass, local) | **3/100 failed** (3%) — iterations 43, 78, 95, identical signature/line each time |
| Whole binary, fully serial (`--test-threads=1`), 20 same-commit reruns, same day (control) | **0/20 failed** |
| Isolated single test, `--test-threads=1`, 100 local reruns (2026-09-22) | 0/100 failed |
| Isolated single test, `--test-threads=1`, 50 CI-native reruns (2026-09-22) | 0/50 failed |

No revert check applies — no fix was proposed this pass to revert.

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `sqlite_job_backend_tracks_job_status_durably` | n=2 organic, isolated-shape 0/100+0/50, category unconfirmed | n=3 organic (3rd hit in `Coverage (sandbox-sqlite)`); whole-binary Tier 1 baseline 3/100 vs. 0/20 serial control; category confirmed (concurrency-dependent), specific defect still open; CI-native whole-binary harness added | Under active investigation, escalated |
| `live_upgrade` (3 signatures) | Uncampaigned, 14 idle passes | No new organic hits | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (14 idle passes) | 0 (15th idle pass, ~352.9h) | Needs human sign-off for CI spend |

## 🔬 Reproduce

Confirm the third organic hit:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106870385779,
             return_content=true, tail_lines=150)
# -> sqlite_job_backend_tracks_job_status_durably panicked at
#    autumn/tests/sqlite_jobs_scheduler_e2e.rs:1301:6, identical signature
```

Reproduce the whole-binary default-parallelism baseline and the serial
control:

```
cargo test -p autumn-web --features "sqlite,test-support,storage" \
  --test sqlite_jobs_scheduler_e2e --no-run

for i in $(seq 1 100); do
  cargo test -p autumn-web --features "sqlite,test-support,storage" \
    --test sqlite_jobs_scheduler_e2e > "iter-$i.log" 2>&1
done
# -> 3/100 FAILED, all sqlite_job_backend_tracks_job_status_durably at
#    sqlite_jobs_scheduler_e2e.rs:1301:6

for i in $(seq 1 20); do
  cargo test -p autumn-web --features "sqlite,test-support,storage" \
    --test sqlite_jobs_scheduler_e2e -- --test-threads=1 > "ctrl-$i.log" 2>&1
done
# -> 0/20 FAILED
```

Confirm the seven triaged failures in this pass's window:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287, status=completed, perPage=100, page=1)
# filter client-side to event == "pull_request" and created_at in window
# (the event=pull_request server-side filter mis-paginated this pass)
# -> 58 runs in [2026-09-22T06:24:50Z, 2026-09-23T07:37:08Z], 7 failures,
#    all seven triaged above
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```
