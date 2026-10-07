# ADR 0017: Multi-Replica Deterministic Simulation

- Status: Accepted
- Date: 2026-10-07
- Deciders: Autumn maintainers
- Tags: simulation-testing, distributed-systems, determinism

## Context

A `Sim` held one app. A second `mount` replaced the first, so two replicas
could not share a clock. The P0 bugs of #3050 (job double-run, cron double-fire,
missing fences) need two nodes, lease loss or clock skew to show (issue #3067).

Two replicas on one `SQLite` database also broke determinism. A `SQLite` query
runs on a tokio blocking thread. Without control, a query can end while the
runtime thread runs other tasks. The task that waits for it then sees the
result at a different point in different runs.

## Decision

1. `Sim::mount_replica(name, app)` mounts named apps next to each other. Each
   replica has its own state, job runtime and scheduled tasks.
2. Each replica reads a `NodeClock`: sim elapsed time, scaled by a drift rate,
   plus an offset. `Sim::step_replica_clock` jumps it, as an NTP step does.
   `Replica::seeded_clock` draws the offset and drift from the seed.
3. Each replica gets its own one-slot pool on the shared database through a
   `DbLink`. The link drops the session (pool hooks), and faults writes with
   `TEMP` triggers on its own connection: `RAISE(ABORT)` before the write is a
   mid-query error, `RAISE(FAIL)` after it is commit ambiguity.
4. `sim::runtime()` is current-thread and paused, with one blocking thread and
   a gate. A gated database operation starts only in a park of the runtime
   thread that began after the last gated operation ended. So each operation
   runs while every task waits, and its result is seen at the same point of
   every run. `#[sim_test]` uses this runtime.
5. `Sim::run_for` moves time by tokio auto-advance only. Tokio moves its
   paused clock to the next timer only when all tasks wait and no blocking
   work is queued, such as a `SQLite` query. `Sim::advance` and
   `Sim::run_to_idle` keep their old behaviour: gated work does not wait while
   they run.
6. `Sim::kill_replica` aborts the tasks the app registered through
   `spawn_app_task`, as a process death does.
7. `sim::trace::capture` records framework events with sim time. The
   `sim-sweep` bin runs the jobs, scheduler and lock scenarios for each seed,
   and runs the first seeds twice to compare traces.
8. Framework code that a sim drives must be replayable. The trace check found
   three sources, and this change fixes them:
   - `SQLite` `random()` in stale-claim recovery (now a hash of an entropy
     draw and the row id);
   - unbiased `tokio::select!` in the job, scheduler, lock and cost loops and
     the job claim heartbeat (now `biased;`, shutdown first);
   - the run-unique substrate database name in a log line (aliased in traces).
9. The sweep also found a bug in the job claim heartbeat (#3051). When the
   heartbeat gave up or stopped with a renewal in flight, `select!` dropped
   the renewal, and diesel-async panicked before the heartbeat stopped the
   run. A peer then ran the job again. The heartbeat now lets a renewal in
   flight finish on its own task.

## Consequences

- Two- and three-replica tests of the framework's own coordination run in
  milliseconds, and replay from a seed.
- The CI seed sweep tests the framework, not only a toy scenario.
- When the runtime drops its tasks, diesel-async can wait on the runtime
  thread for a query in flight. The gate lets such a query run after 2 s and
  counts it. A fleet run with a count above zero fails.
- `#[sim_test]` has one blocking thread. A blocking task that waits for a
  second blocking task never ends.
- Process-wide state is not per replica: the cache, the event bus and the
  global job client belong to the replica mounted last.
- Not in scope, for follow-up work:
  - a Redis fault lane (latency, errors, partition, lost pub/sub messages).
    Redis has no in-process seam, so a lane needs one first. The #3055 and
    #3056 regressions need this lane.
  - Postgres lanes. `LeaseLock` (#3053) and the tick table (#3052) use the
    database clock, so the sim cannot drive them. Their testcontainer tests
    cover them.
  - handler queries dropped mid-flight. On a lost claim or a timeout, the job
    worker drops the handler. A `SQLite` query in flight then panics on a
    current-thread runtime. The fleet handlers do not query, so the sweep does
    not cover this.
  - an idempotency-under-crash scenario. The idempotency store is per process
    (memory) or Redis, and the record is not atomic with handler writes
    (#3061). A shared store across replicas needs the Redis lane or #3061.

## Alternatives considered

- **One shared pool for all replicas.** Simpler, but per-replica faults need
  per-replica connections, and the blocking race stays.
- **Seed tokio's select RNG.** `Builder::rng_seed` needs `tokio_unstable`.
- **Gate with a real-time delay.** Flaky under CI load.
