# `ExperimentService::assign` on `PgExperimentStore`: a new session per store call (findings)

This is a Ledger run on the request path of A/B experiments. A handler calls
`Experiments::assign` / `ExperimentService::assign`. The experiment config
comes from the in-memory snapshot (#3262). Every other store call on the path
runs `Shared::connect` in `autumn/src/experiments.rs`:
`diesel::PgConnection::establish` and then `SET statement_timeout`. That
covers `get_override`, `get_assignment`, `has_assignment_in_group` and
`record_assignment`. So each call opens a new Postgres session, and drops it
when it returns.

**Findings only. No code changed.** The fix gives the store a pool or a
reused connection, which changes how many connections each replica holds
open. That is a maintainer decision (see "What a fix needs" below).

## Workload and fixture

`autumn/tests/integration/experiments_assign_connection_profile.rs` sets up
the fixture and drives `ExperimentService::assign` on a real
`PgExperimentStore`. It is the public entry point the `Experiments`
extractor calls.

* 40 experiments: 8 running (3 of them in the `checkout` exclusion group), 3
  concluded, 25 archived, 4 drafts.
* Sticky assignments, Zipf-skewed over the 36 non-draft experiments. Actors
  are 3/5 `user:<id>` and 2/5 anonymous session ids, skewed towards frequent
  visitors. 1% of rows are overrides, `assigned_at` covers a year, and an
  actor holds at most one experiment of the group. Three sizes:
  163,383 / 818,476 / 2,456,435 rows (59k / 295k / 887k distinct actors).
* 40 QA overrides. 3% of assignments are updated before `ANALYZE` with no
  `VACUUM` (5,941 / 29,772 / 88,902 dead tuples).
* 1,000 `assign` calls in a fixed order: 800 returning actors, 150 new actors
  (spread over all eight running experiments), 20 QA-override actors, and 30
  calls on concluded experiments. The snapshot TTL is 1 h and the snapshot is
  loaded before the counters reset, so no refresh falls in the window.
* For comparison, the 800 returning-actor reads run again on **one held
  connection**: the same two statements with the same text and parameters.

The server was Postgres 16.15 with `pg_stat_statements.track = all` and
`track_planning = on`. On 16, the `shared_blks_*` columns then include the
planner's buffers. The harness prints them under `exec_buf`, and its
`plan_buf` column is the 17+ split, so it reads 0 here. Autovacuum was off.

## Profile

Counters are from `pg_stat_statements` (statements, buffers),
`pg_stat_database.sessions` (sessions opened) and `pg_stat_io` (every
shared buffer a client backend touched, including backend start-up, which
`pg_stat_statements` cannot see). They come out the same at all three sizes;
the full output is in `baseline-*.txt`.

| per `assign` call | sessions opened | statements | statement buffers | backend buffers (`pg_stat_io`) |
|:--|--:|--:|--:|--:|
| returning actor (800) | **2.00** | 8.00 | 277.1 | 573.2 |
| new actor (150) | **3.37** | 17.83 | 701.2 | 1,231.3 |
| QA override (20) | 2.00 | 11.00 | 255.0 | 524.5 |
| concluded experiment (30) | 0 | 0 | 0 | 1.7 |
| **all 1,000** | **2.15** | **9.29** | **331.9** | **653.8** |
| returning-actor reads on one held connection | 0.001 (1 for 800) | 2.00 | **5.3** | **5.6** |

A returning actor, the common case, costs:

* **2 sessions and 8 statements.** Six of the 8 are connection set-up:
  diesel's `SET CLIENT_ENCODING` and `SET TIME ZONE`, then the store's
  `SET statement_timeout`, once per session. That is 75% of the workload's
  statements (4,800 of 6,400).
* **277 buffers, where a held connection needs 5.3** (-98.1%). The two reads
  execute in 4 + 1 buffers. The other ~270 are the planner filling a cold
  catalog cache: every statement is the first one its backend plans.
* **573 backend buffers, where a held connection needs 5.6** (-99.0%), once
  backend start-up is counted.

A new actor opens 3 to 4 sessions (`get_override`, `get_assignment`, then
`record_assignment` in its own transaction; a group experiment adds
`has_assignment_in_group`). `has_assignment_in_group` costs 296 buffers a call
on a cold session.

## Plan

`explain.sh` runs `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` on each
read twice: first as the opening statement of a new session, then again on
the same session. Output at 2.46M rows is in `explain-3000000.txt`:

| statement | plan | execution buffers | planning buffers, new session | planning buffers, warm session |
|:--|:--|--:|--:|--:|
| `get_override` | Seq Scan, 40-row table | 1 | 105 | 0 |
| `get_assignment` | Index Scan `…_experiment_actor_key` | 4 | 167 | 0 |
| `has_assignment_in_group` | Index Scan `idx_autumn_exp_assignments_actor` ⋈ Seq Scan `autumn_experiments` | 6 | 350 | 2 |

The plans are right. Each lookup is a unique-key probe, and none of them
scales with the table. The cost is all in how the session is set up.

## Hypothesis

`Shared::connect` opens a new `PgConnection` for every store call. Each new
backend starts with an empty relcache and catcache, so the first plan of
every statement reads 100 to 350 catalog buffers. Each connection also pays
three set-up statements and a backend fork. Reusing a connection removes all
three costs: the held-connection row above is that counterfactual, measured.

No other statement on this path clears the floor. The execution side is 5
buffers per returning actor. There are no temp blocks and no N+1, and
`calls` grows linearly with requests.

## What a fix needs (a maintainer decision)

Any fix holds connections open per replica:

1. **A small idle list in `Shared`**, for example
   `Mutex<Vec<PgConnection>>` with a cap. Check out a connection, and return
   it only after a clean call. `refresh_conn` already keeps one connection
   this way for the snapshot reload. It needs no new crate.
2. **Diesel's `r2d2` feature.** That is a new dependency feature.
3. **Run the store on the app's async pool.** That changes the store's
   constructor and its sync API.

Each changes connection-pool behaviour: how many idle backends a replica
keeps, and how a broken connection is spotted and dropped. A reused
connection also keeps session state between calls. The store sets only
`statement_timeout`, and `record_assignment`'s advisory lock is
transaction-scoped, so nothing leaks today. The rule has to hold for later
changes too.

`PgFlagStore` and `PgConfigStore` have the same `connect()`. Their reads come
from snapshots, so they connect only on writes and refreshes. This finding
covers the experiment assign path only.

## Reproduce

```sh
# A Postgres 16 server that preloads pg_stat_statements (track_planning on), or
# leave AUTUMN_LEDGER_PG_URL unset and the harness starts a testcontainer.
export AUTUMN_LEDGER_PG_URL=postgres://postgres@localhost:5544/ledger
for n in 200000 1000000 3000000; do
  AUTUMN_LEDGER_ASSIGNMENTS=$n cargo test -p autumn-web --test integration_tests -- \
    --ignored experiments_assign_connection_profile --nocapture --test-threads=1 \
    > docs/perf/experiment-assign/baseline-$n.txt
done
docs/perf/experiment-assign/explain.sh "$AUTUMN_LEDGER_PG_URL" > docs/perf/experiment-assign/explain-3000000.txt
```

The fixture rows are drawn with `setseed`, so they are fixed for a given
size. Wait events were not sampled: no run had more than one client in
flight, so there was no contention to sample.
