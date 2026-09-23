# 🚦 Semaphore: CI health follow-up — `sqlite_jobs_scheduler_e2e` flake reproduced outside CI for the first time; the "concurrency required" framing was wrong

Follow-up to `docs/reports/2026-09-22-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass had
working network and toolchain access in its own sandbox (as the 2026-09-22
pass did), and used it to follow through on that pass's own next step for
`sqlite_jobs_scheduler_e2e::sqlite_job_backend_tracks_job_status_durably`: run
the whole test binary at default parallelism, not filtered to one test. That
reproduced the flake — **3/100** — for the first time outside CI. A third
organic hit also turned up this pass's own sampling window, in a job shape
(`Coverage (sandbox-sqlite)`) not previously checked for this signature,
raising the organic count from n=2 to n=3.

This report's diagnosis went through six review corrections in total, the
last of which overturns its own headline claim rather than just refining
it. A same-day **0/20** fully-serial control looked clean and was reported
as "this test never fails serially" — a Codex review comment on PR #2922
correctly pointed out that n=20 has only a 46% chance of catching a true
3% rate (54% chance of a clean run even if the bug is present as
frequently as the concurrent sample suggested), so that control was never
powered to support the claim. Rerun at a properly powered **n=100**: the
serial control **also failed, 1/100, the identical signature** — the
"fails only under concurrency" boundary this report and the ledger both
asserted is false. Combined with a further realization this pass missed
initially — `--test-threads=1` only prevents different *test functions*
from running concurrently; it does not stop async tasks *within* one
test's own tokio runtime (this target's own `queue_depth_survey_loop`
included) from interleaving with that test's main body — the working
hypothesis shifts from "requires concurrent sibling test functions" to
"requires whole-binary execution context" (whether serial or concurrent),
with the specific mechanism still unidentified. No fix opens this pass.
No named mechanism survives review; this role's own hard gate requires
one, plus a correct category, before a fix PR. No new hits on any other
tracked signature.

This report also went through five earlier corrections diagnosing the
intra-test candidate, in order: overstating "concurrency confirmed" as a
category rather than a correlation; misidentifying the concurrent actor as
a `worker_loop` racing the enqueue (this test passes `run_workers: false`,
so none is ever spawned — the real actor is `queue_depth_survey_loop`);
dismissing that actor as "read-only, so weaker" (its `wait_ready()` call
can still win the race to run the schema-creating DDL, which is a write);
proposing that DDL race explains the panic via WAL-snapshot staleness on
the enqueue's connection — ruled out by `enqueue_job_at`'s own source
order (it awaits schema-readiness *before* acquiring its connection, so
that connection cannot predate the DDL); and finally the serial-control
power issue above. Kept here for the record, since the fix history itself
is part of what the next pass needs to not repeat.

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

**`sqlite_job_backend_tracks_job_status_durably` — a controlled local
reproduction is *suggestive of* sensitivity to whole-binary *execution
context*, not statistically confirmed; concurrency specifically, the
root-cause category, and the specific resource all remain unconfirmed
too.**

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

**Sixth review correction: that 20-run control was never powered to
support the conclusion drawn from it, and the properly-powered rerun
overturns it.** A Codex review comment on PR #2922 pointed out that at a
true 3% rate, `n=20` has a 54% chance of showing zero failures by chance
alone — so "0/20" was consistent with the bug still being present at the
same rate as the concurrent sample, not evidence it requires concurrency.
Verified the math (`(1-0.03)^20 ≈ 0.544`) and reran the same control at
`n=100` — enough for only a ~5% chance of a clean run at a true 3% rate.
**Result: 1/100 failed, iteration 88, the identical signature and line.**
The "fails only under concurrency" claim is false.

This also surfaced a mechanical point about what `--test-threads=1`
actually controls: it serializes different *test functions* against each
other, but does nothing to the tokio runtime *within* one test — this
target's own `queue_depth_survey_loop` still runs as a separately
scheduled async task alongside the test's main body regardless of the
`--test-threads` value. So neither the 20-run nor the 100-run "serial"
control was ever a true no-concurrency condition at the level the panic
could plausibly originate from; they only removed concurrency *between
test functions*, which this new evidence suggests was never the necessary
condition anyway.

Combined with the existing isolated-single-test results from 2026-09-22
(0/100 local + 0/50 CI-native, both `--test-threads=1` and filtered to
just this test), the pattern across five samples is: **isolated single-test
execution stays clean (0/150); whole-binary execution fails at a low rate
whether or not different test functions run concurrently (1/100 serial,
3/100 concurrent, not statistically distinguishable from each other at
this N).**

**Seventh review correction, same pass: this pattern is suggestive, not
statistically confirmed, and an earlier version of this section overstated
it as such.** A Codex review comment on PR #2922 checked the isolated-vs-
whole-binary comparison the same way it checked the earlier serial
control: at a true 1% rate, `0/150` still has a ~22% chance of occurring
by chance (`0.99^150 ≈ 0.221`); a one-sided exact (Fisher) test of 3/100
against 0/150 gives `p ≈ 0.063` — suggestive but short of conventional
significance, and the weaker 1/100-vs-0/150 comparison is less significant
still. Verified both figures independently. So "requires whole-binary
execution context" is the working hypothesis this pass leaves behind, not
an established finding — something about running alongside 26 sibling
tests, not specifically about libtest-level thread concurrency between
them, remains the best-supported reading of the data so far, but a larger
matched-sample campaign (a next step in its own right) would be needed to
actually confirm it statistically.

**Test-vs-product verdict: not rendered, and not leaning either way — after
two review corrections, not one.** This report originally leaned
"presumptively test-side," reasoning that a real deployment does not run 27
concurrent test functions against one SQLite file. A second Codex comment on
PR #2922 correctly pointed out that reasoning doesn't survive the pool-size
correction just below.

That correction's own replacement claim — a `worker_loop` racing
`enqueue_tracked` within this one test — was itself wrong, caught by a
*third* Codex comment: this target test calls `job::start_runtime(...,
false)` (`autumn/tests/sqlite_jobs_scheduler_e2e.rs:1285-1294`), and
`start_runtime`'s `run_workers: bool` parameter gates the worker-spawning
loop behind an early return (`autumn/src/job/sqlite.rs:1469-1471`,
`if !run_workers { return Ok(()); }`) — confirmed by direct read, matching
the test's own comment ("Enqueue-only... the web half of a split"). No
`worker_loop` exists in this test. The one task `start_runtime` spawns
unconditionally for every role, including this one, is
`queue_depth_survey_loop`, which shares the same pool and schema gate.

**A fourth Codex comment then caught a second mistake in that same
paragraph**: calling the survey loop "read-only, so weaker" discarded a
concrete mechanism rather than ruling it out. Its periodic `SELECT` is
read-only, but its `wait_ready()` call races the enqueue's `ready()` on the
same `OnceCell`, and *whichever caller wins runs `ensure_schema`* — a write
that creates the partial unique index the failing `ON CONFLICT` targets —
on *its own* pooled connection, while the enqueue later gets a *different*
connection from the same pool. Confirmed via `autumn/src/db.rs`: every
non-read-only pooled connection in this backend runs `PRAGMA journal_mode =
WAL`, which raised a WAL-snapshot-staleness mechanism as live.

**A fifth Codex comment, same review round, ruled that specific mechanism
out rather than leaving it open.** `enqueue_job_at` calls
`queue_handle.ready().await?` — which resolves only once `ensure_schema`'s
future has completed — *before* `pool.get().await` for its own connection
(`autumn/src/job/sqlite.rs:392-393`, confirmed: `ready()` on line 392,
`pool.get()` on line 393, in that order). Rust drops `ensure_schema`'s own
connection guard at the end of its function body, before the async fn
returns, which is before the `OnceCell` reports ready, which is before
`enqueue_job_at` even requests its own connection. There is no window for
the enqueue's connection to hold a stale snapshot predating the DDL — it
doesn't exist as a connection yet when the DDL commits. The specific
staleness story is dead; what survives is only that the survey loop's
`wait_ready()` can still be the one to run `ensure_schema`, without that
explaining this failure via the route just proposed. **No named mechanism
survives this pass's review.** Both directions — an inter-test shared
resource, and some intra-test interaction not yet identified — stay open,
with no live specific candidate for either.

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
`start_runtime` call's spawned background task(s) can still be running after
`shutdown.cancel()` and after the owning test's
`global_job_runtime_test_lock()` guard is dropped, into a window where a
different, non-lock-holding sibling (or the next lock-holder) is active. For
this specific target test that spawned task is `queue_depth_survey_loop`
(no `worker_loop` here, `run_workers` is `false`) — its `wait_ready()` call
can still be the one to run `ensure_schema`, but the WAL-staleness route
from that observation to the panic is now ruled out (above), so this angle
has no live named mechanism. Whether that spawned task can still be
running after `shutdown.cancel()`/guard-drop, in a window a different test
is active, remains a distinct, not-yet-checked question on its own terms.
Other tests in this file that call `start_runtime` with
`run_workers: true` would spawn a real `worker_loop` too, a separate,
not-yet-checked instance of the same class of gap. Both audits remain
worth doing, but the n=100 serial-control result above (1/100, identical
signature) means neither can be *the whole* explanation on its own: both
were framed around a *different*, concurrently-running test interfering —
a condition serial execution doesn't provide. Whatever explains the
serial-mode failure has to work with only one test function running at a
time; a next pass should start there (what, within a single test's own
execution plus ambient process state left by 18-26 prior serial tests,
could make this test's own intra-process behavior nondeterministic) before
returning to the inter-test angles as a possible *additional* contributor
to the higher concurrent rate.

## 🔧 Treatment

No fix this pass. Per this role's hard gate, a fix PR needs the root-cause
category *and* the specific defect; this pass ends with *less* certainty
about the category than its own earlier drafts claimed, not more — the
properly-powered serial control shows concurrency between test functions
is not the necessary condition, so "inter-test shared resource vs.
intra-test race widened by sibling load" was itself the wrong framing.
Working out what "whole-binary execution context" actually means
mechanically, then naming the specific defect, is next pass's work.

Added two jobs to `.github/workflows/manual-sqlite-jobs-rerun-check.yml`,
alongside the existing filtered/serial `rerun` job: `rerun_default_parallelism`
builds the binary once and runs it *whole* (no filter, no `--test-threads`
override) N times — the CI-native form of the local default-parallelism
repro (3/100). `rerun_serial_whole_binary`, added the same pass once the
0/20-was-underpowered finding landed, mirrors it with `--test-threads=1` —
the CI-native form of the properly-powered serial repro (1/100). Both
upload each iteration's full log, so a future pass (or CI itself) can
confirm both figures without needing a local sandbox with outbound network
access. Neither is dispatchable this pass: `workflow_dispatch` only accepts
a workflow already on the repository's default branch (`trunk-dev`), the
same gotcha every harness in this ledger has hit on its own introduction
pass. Once merged, the next step is to dispatch both — they already exist,
not something to reimplement.

## 📊 Measurement

| Protocol | Result |
|---|---|
| Whole binary, default parallelism, 100 same-commit reruns (this pass, local) | **3/100 failed** (3%) — iterations 43, 78, 95, identical signature/line each time |
| Whole binary, fully serial (`--test-threads=1`), 20 same-commit reruns, same day (control) | 0/20 failed — **underpowered, see the n=100 rerun below** |
| Whole binary, fully serial (`--test-threads=1`), 100 same-commit reruns, same day (properly-powered control) | **1/100 failed** (iteration 88), identical signature/line |
| Isolated single test, `--test-threads=1`, 100 local reruns (2026-09-22) | 0/100 failed |
| Isolated single test, `--test-threads=1`, 50 CI-native reruns (2026-09-22) | 0/50 failed |

No revert check applies — no fix was proposed this pass to revert.

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `sqlite_job_backend_tracks_job_status_durably` | n=2 organic, isolated-shape 0/100+0/50, category unconfirmed | n=3 organic (3rd hit in `Coverage (sandbox-sqlite)`); whole-binary Tier 1 baselines 3/100 (default parallelism) and 1/100 (properly-powered serial) both fail, isolated single-test stays clean at 0/150 — "requires concurrency" is falsified; "requires whole-binary execution context" is the working hypothesis but not statistically confirmed (3/100-vs-0/150 one-sided exact p≈0.063); root-cause category and specific defect both unidentified; test-vs-product verdict open; CI-native whole-binary harness (both concurrent and serial) added | Under active investigation, escalated |
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

for i in $(seq 1 100); do
  cargo test -p autumn-web --features "sqlite,test-support,storage" \
    --test sqlite_jobs_scheduler_e2e -- --test-threads=1 > "ctrl-$i.log" 2>&1
done
# -> 1/100 FAILED (iteration 88), identical signature — an earlier n=20 run
#    of this same control showed 0/20, which a Codex review comment on
#    PR #2922 correctly flagged as underpowered ((1-0.03)^20 ≈ 54% chance
#    of a clean run at a true 3% rate); rerun at n=100 for ~5% miss chance.
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
