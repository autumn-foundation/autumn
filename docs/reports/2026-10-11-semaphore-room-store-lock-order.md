# Semaphore: `room_store_db` lock-order flake (2026-10-11)

## 🎯 Verdict path

`Test (Docker)` in `ci.yml` sweeps the `#[ignore]`d testcontainer tests, among them
`autumn-media-plugin/tests/room_store_db.rs`. Run 38109993643 (PR #3240, which touches only
upgrade code) failed there on
`reaper_racing_a_join_that_got_the_row_first_keeps_the_room_and_its_cap`
(`join gets the seat: RoomNotFound`). `Test (macos-latest)` also failed in that run;
its log was not retrievable (only the cache-save tail). The run `38124217567` failure
is a PR-local clippy error and not part of this report.

## 🌡️ Symptom

Same-binary rerun protocol (4 vCPU sandbox, local Docker, `postgres:16-alpine`,
whole file run with the 22 tests in parallel, as CI does):

- isolated test, 30 runs: 0/30 failures. It only fails under suite load.
- whole file, unmodified: **5/25** runs failed. A further instrumented loop saw 6 more
  failures in about 29 runs. Three tests fail with the same shape:
  `reaper_racing_a_join_that_got_the_row_first…`,
  `join_racing_the_reaper_never_returns_a_seat_that_is_gone`,
  `join_racing_the_last_leave_never_returns_a_seat_that_is_gone`.
  Signatures: `join gets the seat: RoomNotFound`; `rooms_reaped` `left: 1 right: 2`;
  `the join must see the room as gone`.

## 🔍 Diagnosis

Category: order dependence on a lock queue the test does not control. **Test, not product.**

The tests hold the room row with a transaction, let two writers queue behind it
(`settle` waits for N blocked sessions), then `COMMIT` the holder and assert that the
writer that queued first wins. A `pg_stat_activity` dump taken at failure shows the
queue was right (first waiter on `transactionid`, second on `tuple`), yet the second
waiter won. After a holder **commits**, each waiter sees the row as updated and re-locks
the new version through EvalPlanQual, re-queueing behind the other, so Postgres does not
promise FIFO. After a **rollback** there is no updated version, and the waiters take the
row in arrival order.

Product verdict: both interleavings are handled correctly. In every failure the
product invariants (no seat returned that the cascade deletes; cap restored) held and
only the test's ordering assertion tripped.

## 🔧 Treatment

`release()` rolls the holder back instead of committing. Its `UPDATE` is a no-op, so
nothing is lost. No sleep, retry or widened assertion.

## 📊 Measurement

- before: 5/25 whole-file runs failed
- after: **0/50** whole-file runs failed (same binary, same host)
- revert check: removing the reaper's restore step fails
  `reaper_racing_a_join…` 3/3 (`the claim is restored`, `left: 0 right: 6`); making
  `lock_room` a plain `SELECT` fails 4 tests 3/3 of 3 runs. Product source restored
  afterwards.
- ledger: no entry needed (never quarantined).

## 🔬 Reproduce

```
cargo test -p autumn-media-plugin --test room_store_db --no-run
for i in $(seq 1 50); do <printed binary> --ignored || echo FAIL $i; done
```
