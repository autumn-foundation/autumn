# 🚦 Semaphore: CI health follow-up — first Tier 1 baseline for job_tracking (1/50), mechanism confirmed, deterministic fix

Follow-up to `docs/reports/2026-09-18-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass
clears the hard gate for a Determinism PR on
`job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`:
a Tier 1 same-commit rerun-rate baseline (1/50), a named mechanism confirmed
by reading the store's own source (not left as a hypothesis), and a rendered
test-vs-product verdict. The after-measurement is still outstanding — see
Measurement and Treatment — so this entry is not yet closed in the ledger.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **12th consecutive
idle pass** (~283 hours, past 11.5 days, since it became dispatchable
2026-09-08T15:07:44Z). Not dispatched this pass: new macOS CI spend needs a
human sign-off per this role's own rules, and no such sign-off is available
in this unattended run.

## 🌡️ Symptom

**The `job_tracking_stores_integration` harness result** (the headline of
this pass): `.github/workflows/manual-job-tracking-rerun-check.yml`, built by
the 2026-09-18 pass and merged the same day (#2845), was dispatched twice
against `trunk-dev`'s tip on 2026-09-18, before this pass started and not yet
folded into the ledger. Run 35364903427 failed at checkout (a malformed
`sha` input, 41 hex characters instead of 40 — zero iterations executed, not
a data point). Run 35365077413, redispatched two minutes later with a
corrected `sha`, ran to completion:

```
RESULT: 1/50 failed, 49/50 passed
```

The one failure, iteration 26 (`get_job_logs` on job 105665441315), panics
with the identical tracked signature — `"record should be past its
configured TTL"` at
`autumn/tests/integration/job_tracking_stores_integration.rs:264:5` — inside
a fresh testcontainers Postgres container built for that iteration alone.
This is a 2% same-commit rerun rate, the low-rate side of this role's own
≥20/≥50 sample-size split, and the harness delivered exactly the ≥50 samples
that bar calls for.

**Organic-hit sampling for the rest of the ledger's tracked corpus**
(2026-09-18T07:33:38Z exclusive to 2026-09-20T07:33:19Z, ~72h, two
`perPage=100` `pull_request`-event pages, 200 runs: 143 cancelled / 47
success / 10 failure) found **zero new hits** on any of `live_upgrade`'s
three signatures, `cache_stampede`, or `sim_fault_plan`. Of the 10 run-level
failures: 2 predate this window (already counted in the 2026-09-18 report);
2 are `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0`'s own
action-pin bump breaking `MSRV (1.88.0)` and all three `Test (${{
matrix.os }})` jobs on that branch — that PR's own subject matter, unmerged,
so not affecting any other PR's CI; 2 are ordinary branch-owned `Clippy`
failures (`vesper/bugbash-2828-intentional-root`,
`claude/project-thread-bk4ejy` — the latter also fails `SQLite runtime`'s own
clippy step); 1 is `dependabot/cargo/validator-0.21.0` repeating its
already-documented `fuzz/Cargo.lock` staleness; 1 is
`vesper/macro-crate-split` repeating its already-documented
in-progress-refactor multi-job break. **Gap, noted rather than silently
closed**: `claude/stop-changelog-conflicts-0xp5f8` and
`claude/intelligent-wright-ebjkn4` were not individually triaged at job level
this pass — time-boxed in favor of following the job_tracking result through
to a fix — so they are not confirmed branch-owned, only unconfirmed against
the four tracked signatures by omission.

## 🔍 Diagnosis

**Mechanism, confirmed by source, not inferred.**
`PgJobTrackingStore::update` (`autumn/src/job_tracking.rs`) unconditionally
executes `UPDATE autumn_job_tracking SET record = ..., updated_at = $3,
expires_at = $4 WHERE key = $1` with `expires_at = now + ttl_secs` on
**every** call. Both lifecycle writes the running job triggers —
`mark_running` (once the job runtime picks the enqueued job up) and
`settle_success` (on completion) — route through this same `update`
unconditionally (confirmed via `run_job_handler_inner` in
`autumn/src/job.rs`). The test enqueues a job with `ttl_secs: 1`, reads the
record once, then sleeps a fixed 1200ms before asserting `expires_at <=
NOW()`. If either lifecycle write lands inside that 1200ms window — ordinary
job-dispatch latency, not contention or clock skew — it pushes `expires_at`
back out past the check point, and the assertion fires on a record that is
about to expire but hasn't yet.

This is exactly the "worker-refresh" mechanism this ledger's entry already
named as the better-supported of two candidates (the other, a discrete
clock step, was already demoted in an earlier pass via post-review
correction). Reading `update`'s body directly removes the hypothesis
qualifier: the refresh-on-every-write behavior is not inferred from
plausible code shape, it is what the method does.

**Test-vs-product verdict: test defect, not a product defect.** Refreshing
`expires_at` on every lifecycle write is deliberate, correct store
behavior — a job still being worked on should not expire out from under it.
The defect is in the test's assumption that nothing touches the record
between its initial read and a sleep whose duration it picked without
reference to when the job runtime would actually run the job.

## 🔧 Treatment

**Fix, in this pass's own PR**: `autumn/tests/integration/job_tracking_stores_integration.rs`'s
`postgres_backend_persists_tracked_job_and_expires_it` now polls the tracked
record (50ms interval, 5s deadline) until `status` reaches a terminal value
(`"succeeded"`/`"failed"`) before starting the TTL sleep, instead of sleeping
a fixed 1200ms from the initial enqueue-time read. Once the job is terminal,
`mark_running`/`settle_success` have made their last write for that key
(confirmed structurally: one `mark_running` call, one settle call,
`max_attempts: 1` on the `noop` job this test uses, no retry path) — so the
1200ms TTL sleep that follows is racing nothing. This awaits the actual
condition instead of widening the sleep to outrun an unbounded dispatch
latency, per this role's own standing preference.

**Not touched**: the Redis sibling test (same file, lines 45-117) has the
identical race in principle — its own TTL write goes through the same
refresh-on-every-lifecycle-call shape — but has never produced an organic
hit, so it is left as-is rather than rewritten on no evidence of its own.

**Ledger updated**: `job_tracking_stores_integration` gets the full
2026-09-20 account above (baseline, mechanism, verdict, fix, and why the
entry stays open); `live_upgrade`/`cache_stampede`/`sim_fault_plan` are
unchanged (zero new hits, still uncampaigned pending
`manual-macos-contention-check.yml`'s still-outstanding dispatch).

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `job_tracking_stores_integration` rerun rate | n=2 organic (2026-09-11, 2026-09-15), not campaigned | **1/50 (2%), CI-native, same commit** — Tier 1 baseline established | **Outstanding** — needs a post-fix dispatch (`iterations: "50"`) against `trunk-dev` once this PR merges; 0/50 closes the entry |
| Mechanism | Named as a hypothesis ("better-supported candidate") | Confirmed by reading `PgJobTrackingStore::update` directly | N/A |
| `live_upgrade` (3 signatures) | Uncampaigned | No new organic hits this pass | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits this pass | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits this pass | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (11 idle passes) | 0 (12th idle pass, ~283h) | Needs human sign-off for CI spend |

**Revert check**: not run as a second rerun campaign (that would cost
another 50-iteration dispatch before this PR even merges). Structural
instead: the 1/50 result above already reproduces the exact race the fix
removes (iteration 26 failed while the old fixed-sleep code was live), so
the planned post-fix 0/50 is itself the before/after comparison — a clean
run after the fix, on a harness that has already shown it can catch the
failure, is the equivalent of the mutate-and-confirm-red step for a race
that isn't reproducible on demand by mutating the product (the race is
timing-dependent, not something a source edit can force to fire).

## 🔬 Reproduce

Confirm the harness result:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=105665441315,
             return_content=true, tail_lines=150)
# → "RESULT: 1/50 failed, 49/50 passed"
```

Find the failing iteration and its signature:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=105665441315,
             return_content=true, tail_lines=2000)
# grep for "FAIL" / "panicked" -> iteration 26, job_tracking_stores_integration.rs:264:5
```

Confirm the mechanism against source:

```
grep -n "fn update" autumn/src/job_tracking.rs
# read the method body: expires_at = self.expires_at(now) on every call
grep -n "mark_running\|settle_success" autumn/src/job.rs
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```

Once this PR merges, the post-fix verification dispatch:

```
actions_run_trigger(run_workflow, owner=autumn-foundation, repo=autumn,
                     workflow_id=manual-job-tracking-rerun-check.yml,
                     ref=trunk-dev,
                     inputs={sha: "<trunk-dev tip>", iterations: "50"})
```
