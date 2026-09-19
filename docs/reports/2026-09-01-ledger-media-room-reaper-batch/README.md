# 🗃️ Ledger — media-room reaper phase-2 batching

**Date:** 2026-09-01
**Target:** `DbRoomStore::reap_stale`, phase 2 (`autumn-media-plugin/src/rooms_db.rs`)
**Outcome:** optimization — N+1 eliminated (15,504 → 1 statement/tick), phase-2 buffers −47.6%

---

## 🎯 Workload

The background media-room reaper. `spawn_room_reaper_loop`
(`autumn-media-plugin/src/rooms.rs`) is started from the plugin's startup hook
whenever the room feature is wired in, and ticks every
`ROOM_REAPER_INTERVAL_SECONDS` (**60s by default**). Each tick calls
`RoomStore::reap_stale(now, idle_ttl)` once. Under
`room_store_backend = "db"` that is `DbRoomStore::reap_stale`, which runs two
phases:

- **Phase 1** — one `DELETE` evicting every participant whose `last_seen_at`
  is older than `now - idle_ttl`. One statement, unchanged by this work.
- **Phase 2** — drop every now-empty room older than the same cutoff.

Phase 2 was the target. It loaded every candidate room in one query, then, for
**each** candidate, issued a separate `SELECT COUNT(*)` against
`media_room_participants` and — when that count was zero — a separate `DELETE`.

This is not an operator-triggered batch action: it runs unconditionally, once a
minute, in every process, and `n` scales with how many mesh rooms went stale
since the previous tick. A deployment's own traffic sets the cost.

**Fixture** (built by the committed harness, seeded deterministically from a
fixed reference clock — no RNG, no wall-clock dependence): 8,704 rooms across
40 namespaces with skewed cardinality (8 "busy" namespaces hold ~70% of rooms,
32 long-tail namespaces share the rest) and 6,304 participants, in the five
states the reaper's contract must tell apart:

| category | rooms | `created_at` | participants | expected |
|---|---:|---|---|---|
| `stale_empty` | 6,000 | `< cutoff` | none | reaped |
| `stale_all_participants_stale` | 1,500 | `< cutoff` | 3 each, all stale | phase 1 empties, phase 2 reaps |
| `stale_with_fresh_participant` | 500 | `< cutoff` | 1 stale + 1 fresh | kept (occupied) |
| `fresh_empty` | 300 | `>= cutoff` | none | kept (create→first-join window) |
| `fresh_with_participants` | 400 | `>= cutoff` | 2 each, mixed | kept (not a candidate) |
| boundary/edge rows | 4 | see below | see below | see below |

Dead-tuple realism: every `stale_all_participants_stale` participant row takes a
second heartbeat-style `UPDATE` after the initial seed, and `ANALYZE` runs with
**no** intervening `VACUUM`, so planner statistics see the dead tuples.

**8,002 of the 8,704 rooms are phase-2 candidates** (`created_at < cutoff`), and
7,501 of those are empty at phase-2 time and must be reaped.

**Reproduce:**

```bash
cargo test -p autumn-media-plugin --test room_reaper_batch_profile \
  -- --ignored --nocapture --test-threads=1
```

Requires Docker (testcontainers spins Postgres 16 with `pg_stat_statements`
preloaded). Now also wired into CI's "Run Docker-dependent tests" step.

---

## 📈 Profile

This harness drives exactly one workload, so the reaper tick *is* the total —
the ranking below is the whole of it, not a slice. The signal is
`pg_stat_statements.calls` on two statement shapes that are individually
trivial (both are composite-primary-key point operations, no seq scan, no sort)
and collectively dominant. They would never surface in a ranking sorted by
buffers-per-statement; they surface by `calls`.

Baseline `pg_stat_statements` for one tick, ordered by calls
(`baseline/output.txt`):

| calls | buffers | statement |
|---:|---:|---|
| 8,002 | 18,412 | `SELECT COUNT(*) FROM media_room_participants WHERE namespace = $1 AND room_id = $2` |
| 7,501 | 60,018 | `DELETE FROM media_rooms WHERE namespace = $1 AND room_id = $2` |
| 7,501 | 15,002 | `DELETE FROM ONLY public.media_room_participants …` (the FK's `ON DELETE CASCADE` RI statement, fired internally per deleted room) |
| 1 | 5,512 | `DELETE FROM media_room_participants WHERE last_seen_at < $1` (phase 1) |
| 1 | 82 | `SELECT namespace, room_id FROM media_rooms WHERE created_at < $1` (candidate scan) |

Phase 2's share of the tick: **15,504 of 15,505 client statements (99.99%)** and
**78,512 of 84,024 client-statement buffers (93.4%)**. It is the workload.

---

## 🧭 Plan

Both plans are in `baseline/output.txt` and `after/output.txt`, captured with
`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` before the sweep mutates
anything.

**Before** — the per-candidate occupancy check. The plan itself is fine; there
are just 8,002 of them:

```
Aggregate  (cost=8.30..8.31 rows=1 width=8) (actual rows=1 loops=1)
  Buffers: shared hit=2
  ->  Index Only Scan using media_room_participants_pkey  (actual rows=0 loops=1)
        Index Cond: ((namespace = 'tenant-busy-0') AND (room_id = 'stale-empty-0'))
        Heap Fetches: 0
        Buffers: shared hit=2
```

**After** — the changed node: the 8,002 index-only scans collapse into one
`Hash Right Anti Join`, which is where the whole emptiness decision now happens:

```
Hash Right Anti Join  (cost=310.83..544.59 rows=5794) (actual rows=6001 loops=1)
  Inner Unique: true
  Hash Cond: ((p.namespace = media_rooms.namespace) AND (p.room_id = media_rooms.room_id))
  Buffers: shared hit=194
  ->  Seq Scan on media_room_participants p  (actual rows=6302 loops=1)
  ->  Hash  (actual rows=8002 loops=1)
        ->  Seq Scan on media_rooms  (actual rows=8002 loops=1)
              Filter: (created_at < '2026-06-01 11:30:00')
              Rows Removed by Filter: 702
```

(The seq scans are correct here: 8,002 of 8,704 rooms match the cutoff — 92%
selectivity. An index on `created_at` exists and the planner is right not to
use it at that selectivity. No index was added or dropped by this change.)

---

## 💡 Hypothesis

The phase-2 loop asks the database "is this one room empty?" once per candidate
and then tells it "delete this one room" once per empty candidate, when the
whole decision is expressible as one predicate. The emptiness test is exactly a
correlated `NOT EXISTS` against `media_room_participants`' composite primary
key — the same key the per-candidate `COUNT(*)` already probes — so the loop can
become a single anti-join delete with no change to which rows are chosen.

Mechanism, specifically: **the handler issues one `SELECT` (and one `DELETE`)
per parent row instead of one batched statement.** Statement count per tick is
O(n) in stale-room candidates and drops to O(1).

---

## 🔧 Change

One change, in `autumn-media-plugin/src/rooms_db.rs`: phase 2's candidate load
+ per-candidate `COUNT(*)` + per-candidate `DELETE` becomes a single

```sql
DELETE FROM media_rooms
WHERE created_at < $1
  AND NOT EXISTS (SELECT … FROM media_room_participants
                  WHERE media_room_participants.namespace = media_rooms.namespace
                    AND media_room_participants.room_id  = media_rooms.room_id)
```

expressed through Diesel's backend-portable query builder
(`diesel::dsl::not(diesel::dsl::exists(…))`), matching the module's stated
pg + sqlite portability constraint — no raw SQL, no Postgres-only fragment.

`stats.rooms_reaped` now comes from the single statement's affected-row count
rather than a per-iteration sum.

**No migration. No new index. No dropped index. No schema change**, so no
migration lock to declare and no write-amplification tax to measure. The
statement takes only the row locks its own `DELETE` takes, exactly as the
per-room deletes did.

---

## 📊 Measurement

All counters from `pg_stat_statements` in the same session, same fixture, same
harness, before and after (`baseline/output.txt` vs `after/output.txt`).

| counter (per reaper tick) | before | after | delta | tool |
|---|---:|---:|---|---|
| **phase-2 client statements** | **15,504** | **1** | **O(n) → O(1)** | `pg_stat_statements.calls` |
| ↳ candidate scan | 1 | 0 | −1 | `pg_stat_statements.calls` |
| ↳ `COUNT(*)` occupancy checks | 8,002 | 0 | −8,002 | `pg_stat_statements.calls` |
| ↳ `media_rooms` DELETE | 7,501 | 1 | −7,500 | `pg_stat_statements.calls` |
| **phase-2 buffers** | **78,512** | **41,109** | **−47.6%** | `shared_blks_hit + shared_blks_read` |
| temp blocks written | 0 | 0 | none either side | `pg_stat_statements.temp_blks_written` |
| rooms reaped | 7,501 | 7,501 | identical | `ReapStats` |
| participants reaped (phase 1) | 5,400 | 5,400 | identical | `ReapStats` |
| phase-1 sweep | 1 call, 5,512 buffers | 1 call, 5,512 buffers | unchanged | `pg_stat_statements` |
| FK `ON DELETE CASCADE` (internal) | 7,501 calls, 15,002 buffers | 7,501 calls, **18,402** buffers | calls unchanged, **buffers +22.7%** | `pg_stat_statements` |

Both impact-floor criteria are cleared independently: the N+1 elimination
(statements per unit of work O(n) → O(1)) on its own, and the ≥20% buffer
reduction on the statement that is 93.4% of the workload's buffers.

**Disclosed, not hidden:** the FK's referential-integrity cascade still fires
once per deleted room (7,501 times — that is a per-row trigger, and folding the
client statements together cannot change it) and it touches **more** buffers
than before: 15,002 → 18,402, +3,400. Counting it in, total phase-2 buffers go
93,514 → 59,511, still **−36.4%**. This pass did not isolate why the cascade's
own buffer count rises; the plausible cause is page-access ordering (the
batched delete visits rooms in anti-join output order rather than
candidate-scan order, so the cascade's index probes have different locality),
but that is a hypothesis, not a measurement, and it is not claimed as one.

Wall-clock is not cited anywhere above, per the Ledger evidence rules.

---

## ✅ Equivalence

The harness runs **unchanged** before and after — only the implementation under
it differs — and asserts, in both worlds:

- `rooms_reaped` equals the analytically-derived reap set (7,501) — identical either side.
- The surviving room count equals the analytically-derived keep set (1,203) — identical either side.
- **Boundary, `created_at`:** a room whose `created_at` sits *exactly* on the
  cutoff survives (the predicate is `.lt`, not `.le`).
- **Boundary, `last_seen_at`:** a stale room whose only participant's
  `last_seen_at` sits *exactly* on the cutoff survives — phase 1 must not evict
  that participant, so the room must not read as empty.
- **Tenant isolation:** the same `room_id` ("general") exists in two namespaces
  in opposite states; the stale/empty one is reaped and the fresh/occupied one
  survives, so the composite `(namespace, room_id)` key never lets one tenant's
  reap decision leak into another's.
- The create→first-join window is preserved: fresh empty rooms are kept.

The pre-existing Docker suite `autumn-media-plugin/tests/room_store_db.rs`
passes **unchanged** — all 7 tests, including the four that cover this exact
sweep (`reap_evicts_stale_participant_and_drops_emptied_room`,
`reap_drops_created_never_joined_room_but_keeps_a_fresh_empty_room`,
`reap_never_crosses_namespaces`, `reap_on_a_clean_store_is_a_zero_count_no_op`).
No test's expectations were edited.

**Isolation/visibility:** unchanged, and marginally strengthened. Both versions
run outside an explicit transaction, so each statement is its own. The old code
had a per-candidate window between the `COUNT(*)` and the `DELETE` in which a
joiner could take a seat in a room already judged empty; the single statement
evaluates the predicate and deletes atomically, so that window is gone. The
documented last-write-wins concurrency contract still holds: the sweep remains
an unconditional delete keyed only on the injected clock, so concurrent reapers
in other processes still converge (a second reaper's delete simply affects zero
rows).

No ANN/vector index and no bi-temporal validity semantics are touched, so
recall@k and as-of resolution do not apply.

---

## 💸 Write cost

No index added, dropped, or altered, so there is no write-amplification
trade to measure. The change removes 15,503 statements per tick and their
round trips; it adds none.

---

## 🔬 Reproduce

```bash
# The profiling harness (before/after numbers above)
cargo test -p autumn-media-plugin --test room_reaper_batch_profile \
  -- --ignored --nocapture --test-threads=1

# The pre-existing reaper semantics suite, which must pass unchanged
cargo test -p autumn-media-plugin --test room_store_db -- --ignored --test-threads=1
```

Artifacts: `baseline/output.txt` (captured at commit `810e9c8`, before any
source change) and `after/output.txt`.
