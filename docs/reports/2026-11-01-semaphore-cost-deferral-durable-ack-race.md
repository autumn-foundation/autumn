# Semaphore: `postgres_deferrable_job_waits_for_the_cost_signal` asserts DB state before the ack

Date: 2026-11-01. Outcome: **health report with a located, unverified mechanism. No fix PR** (hard gate unmet, see Measurement).

## Verdict path

`ci.yml` `Test (Docker)` (the `--ignored` sweep of the consolidated `integration_tests` binary). Required through `Test suite`.

## Symptom

- Trunk run 41067458884 (head `3bce2476`, 2026-10-30): `552 passed; 1 failed`. The failure was
  `integration::cost_deferral_durable::postgres::postgres_deferrable_job_waits_for_the_cost_signal`
  at `cost_deferral_durable.rs:188`: `the deferred job used no attempt` (`left: 0, right: 1`).
- Trunk run 41109865103 (head `28190ee2`) failed on a different cause: `query 535b1eb49935973a has no Ledger profile`. `docs/perf/ledger/queries/535b1eb49935973a.toml` exists on trunk now, so that one is resolved.
- Last 30 trunk-dev `ci.yml` runs: 2 failures (both `Test (Docker)`), 6 successes, the rest cancelled by newer pushes. No same-commit rerun of `3bce2476` was recorded, so the rate is **1 occurrence, not k/n**.

## Diagnosis (by reading code; not reproduced)

Category: async race inside the test. **Test-vs-product verdict: test.**

`window_scenario` waits for `accountant.snapshot().jobs.total.runs == 2`. That counter is
bumped when `run_claimed_job_handler` (`autumn/src/job.rs`, around line 3059) returns. By its own
doc comment, that function does not settle the record: the caller applies `PendingSettle`
*after the queue ack succeeds*. The test then immediately runs
`SELECT COUNT(*) ... status = 'completed' AND attempt = 1`. When the query lands
between the metering and the ack's `UPDATE ... SET status = 'completed'`, the count is 0.
Ordering "handler returns, then ack" is intentional (#2583), so the product is not wrong.
The test asserts a durable state it never awaited. The same shape may exist in the
Redis sibling of the test.

## Treatment (proposed, not made)

Poll the row with the file's existing `wait_for` (20 s deadline) until it is `completed`, then assert `attempt = 1`.
That awaits the real condition, so it is not a tolerance change. The `attempt` assertion keeps its power.

## Measurement

Not done. This environment has no Docker daemon, so the ≥50x same-commit rerun, the shuffled run and the revert check
(mutating the claim so a deferred job burns an attempt, which must still turn the test red) could not run. Per the hard gate
no fix was opened. To close it:

```
cargo test -p autumn-web --features "test-support,offline-sync,ws,mail,redis,i18n,collab,ledger-capture" \
  --test integration_tests -- --ignored cost_deferral_durable::postgres --test-threads=1   # x50, count failures
```

Then apply the poll, rerun x50 expecting 0, then run the revert check.

## Ledger

No quarantine entry added: the test was not skipped.
