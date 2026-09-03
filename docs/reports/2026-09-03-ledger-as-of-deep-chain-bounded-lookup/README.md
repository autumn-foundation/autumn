# 🗃️ Ledger — bounded lookup for `ledger_as_of`/`ledger_diff`

**Date:** 2026-09-03
**Target:** `ledger_as_of_at`/`ledger_diff_at`, the generated ledgered-repository
read path (`autumn-macros/src/repository.rs`)
**Outcome:** optimization for the realistic query pattern (near-head as-of:
buffers −82% to −94%, flat regardless of chain depth), with a disclosed
regression for the pathological worst case (a query about a record's very
first revision, on a deep, physically-scattered chain: +619% buffers on this
fixture)

---

## 🎯 Workload

`#[repository(ledgered = true)]` (issue #1699) gives an entity a bitemporal,
tamper-evident revision ledger: every write appends a full snapshot, and
`ledger_as_of`/`ledger_diff` answer "what did this record look like at instant
X" / "what changed between X and Y". This is exactly the feature a financial
account, an invoice, or a contract uses — something adjusted repeatedly over a
long operational life and later audited by a support agent or a compliance
reviewer asking about **recent** history far more often than about the
record's very first state.

Both generated methods are pure functions
([`snapshot_as_of`](../../autumn/src/ledger.rs)) over whatever
`ledger_revisions(record_id)` returns, and `ledger_revisions` has exactly one
SQL shape:

```sql
SELECT id, table_name, tenant_id, record_id, seq, op, actor, request_id,
       snapshot, valid_from, recorded_at, prev_hash, hash
FROM _autumn_ledger_revisions
WHERE table_name = $1 AND record_id = $2 AND ($3::text IS NULL OR tenant_id = $3)
ORDER BY seq ASC
```

No `LIMIT`, no reference to `as_of` at all. It reads **every** stored revision
of the record — every row, every `snapshot` TEXT column (the largest column in
the table) — to answer a question that has exactly one answer, regardless of
whether `as_of` asks about right now or the record's very first revision.

**Fixture** (`autumn/tests/integration/ledger_as_of_deep_chain_profile.rs`,
testcontainer Postgres 16, `pg_stat_statements` preloaded): three "hot"
ledgered accounts written through the real `save`/`update` path at three chain
depths — 300 / 700 / 1,200 revisions, real operational history rather than one
huge outlier — plus a long tail of 150 accounts touched only 6 times each (the
same skewed-cardinality shape the export/reaper Ledger fixtures use, expressed
here as chain depth rather than row count). Every hot account's live row is
itself updated hundreds of times, so its heap carries real dead tuples by the
time `ANALYZE` runs (no `VACUUM`), and the three hot chains are seeded
**concurrently**, so their revisions land physically interleaved on disk —
the same layout concurrent writes to different records produce in a live
deployment.

**Reproduce:**

```bash
cargo test -p autumn-web --features "test-support" --test integration_tests \
  -- --ignored ledger_as_of_deep_chain_profile --nocapture --test-threads=1
```

Requires Docker. Wired into CI's Docker-dependent sweep automatically (a
house-pattern `#[ignore]`d testcontainer test in `tests/integration/`, per
CLAUDE.md — no workflow edit needed).

---

## 📈 Profile

This harness drives exactly one workload, so the ranking below is the whole of
it: every `_autumn_ledger_revisions` statement this run issues is either the
`ledger_revisions` full-chain read or (after the fix) the new bounded lookup —
100% of the harness's buffers and calls, by construction.

---

## 🧭 Plan

**Before** (`baseline/output.txt`) — `EXPLAIN (ANALYZE, BUFFERS, VERBOSE,
SETTINGS)` on the exact statement `ledger_as_of`/`ledger_diff` issue today, at
the 1,200-revision chain:

```
Sort  (cost=236.87..239.87 rows=1200 width=347) (actual rows=1200 loops=1)
  Buffers: shared hit=129
  ->  Seq Scan on public._autumn_ledger_revisions (actual rows=1200 loops=1)
        Filter: (table_name = 'ledger_deep_chain_accounts' AND record_id = 3)
        Rows Removed by Filter: 1900
        Buffers: shared hit=129
```

**actual rows=1200** at the scan node — every revision of the record, every
time, whether `as_of` asks about the instant just recorded or the very first
one.

**After** (`after/output.txt`) — the same record, asking "what did it look
like 5 postings ago" (the realistic near-head case):

```
Limit  (cost=0.28..0.57 rows=1 width=347) (actual rows=1 loops=1)
  Buffers: shared hit=11
  ->  Index Scan Backward using idx_autumn_ledger_revisions_record
        (actual rows=1 loops=1)
        Filter: (recorded_at <= '...')
        Rows Removed by Filter: 5
        Buffers: shared hit=11
```

**actual rows=1** at the `Limit` node, **6 rows examined** at the scan node
(5 rejected + 1 returned) instead of 1,200. Same index that already existed
(`idx_autumn_ledger_revisions_record`, `(table_name, record_id, seq)`) — no
migration, no new index.

**Disclosed worst case** — the same record, asking about its very first
revision:

```
Limit  (cost=0.28..173.56 rows=1 width=347) (actual rows=1 loops=1)
  Buffers: shared hit=949
  ->  Index Scan Backward using idx_autumn_ledger_revisions_record
        (actual rows=1 loops=1)
        Filter: (recorded_at <= '...')
        Rows Removed by Filter: 1199
        Buffers: shared hit=949
```

1,199 of the chain's 1,200 rows still get examined here — asking about the
oldest instant is the one case an `ORDER BY seq DESC LIMIT 1` scan cannot
short-circuit, because the qualifying row sits at the *far* end of the scan
direction. See "Disclosed trade-off" below for why this is worse in **buffers**
than the old plan's 129, and why that isn't the number that should decide this
change.

---

## 💡 Hypothesis

The statement has no predicate on `as_of` at all, so it cannot stop early: it
is structurally a full-chain read no matter what instant is asked about. A
bounded lookup — `ORDER BY seq DESC LIMIT 1` with the same bitemporal filter
`snapshot_as_of` already applies in Rust, now pushed into the `WHERE` clause —
lets the database stop at the first qualifying revision. Because `recorded_at`
is monotonic non-decreasing in `seq` (`monotonic_recorded_at`, #2323), a
transaction-time bound — what `ledger_as_of`/`ledger_diff` use — means the scan
only has to walk back past revisions *newer* than the answer, i.e. its cost is
proportional to **how far back the question asks**, not to the chain's total
length. Recent-history questions, the overwhelmingly common audit pattern,
become cheap; only "what was true right at the start" stays expensive — a
much better distribution of cost than "always expensive."

## 🔧 Change

`autumn-macros/src/repository.rs`: adds `__autumn_ledger_revision_at`, a new
generated method that issues the bounded lookup (same guard blocks —
cross-shard, cross-tenant, tenant setup — as every other ledger query; same
row shape as `ledger_revisions`, mapped through a new single-row counterpart of
its row mapper). `ledger_as_of_at` and `ledger_diff_at` now call it instead of
`ledger_revisions` + `snapshot_as_of`. `ledger_revisions` and `ledger_verify`
are untouched — they legitimately need the whole chain (verification walks
every link) and still do.

No migration: the index this query uses
(`idx_autumn_ledger_revisions_record`) already existed for `ledger_revisions`
itself. No schema change, no lock beyond the `SELECT`'s own.

---

## 📊 Measurement

`pg_stat_statements`, one run each, reset before the measured call:

| query | depth | before: calls | before: buffers | after: calls | after: buffers | Δ buffers |
|---|---:|---:|---:|---:|---:|---:|
| near-head as-of ("5 postings ago") | 300 | 1 | 22 | 1 | 4 | **−82%** |
| near-head as-of ("5 postings ago") | 700 | 1 | 129 | 1 | 8 | **−94%** |
| near-head as-of ("5 postings ago") | 1,200 | 1 | 132 | 1 | 8 | **−94%** |
| worst case: as-of at the FIRST revision | 1,200 | 1 | 132 | 1 | 947 | **+618%** (disclosed) |
| `ledger_diff` across the last 10 postings | 1,200 | 1 | 129 | 2 | 16 | **−88%** (statements 1→2, disclosed) |

The before column is flat at ~130 buffers regardless of *which* instant is
asked about — direct evidence the old query ignores `as_of` entirely. The
after column for the near-head case stays flat at 4–8 buffers **across all
three chain depths** (300/700/1,200) — the "admissible at ≥3 sizes" bar for a
plan-shape claim — instead of scaling with depth the way the old plan does
(22 → 129 → 132).

Temp blocks: zero on every statement, either side (no spill). WAL bytes: N/A
— this is a read-only change, no write path touched, no index added.

### Disclosed trade-off

Two costs go up, both disclosed rather than hidden:

- **`ledger_diff` issues 2 statements instead of 1** (one bounded lookup per
  instant, `from` and `to`, instead of one full-chain read shared between
  them). Buffers still fall sharply (129 → 16) because each lookup is now
  short — this is a straightforward win, not a wash.
- **The worst case — asking about a record's very first revision — reads more
  buffers than before on this fixture (949 vs 132)**, not fewer. Mechanism:
  `ORDER BY seq DESC LIMIT 1` can only short-circuit when the qualifying row is
  near the *scan's* end (recent history for a transaction-time bound); asking
  about the oldest instant forces the backward index scan to walk almost the
  whole chain, and — because this fixture seeds three chains **concurrently**,
  so their revisions interleave physically on disk, the same layout concurrent
  writes to different records produce in a live deployment — each step is a
  non-contiguous heap fetch. Postgres counts a buffer hit per access, not per
  distinct page, so revisiting an already-cached page still adds to the count;
  the old plan's sequential `Seq Scan` touches each page in the (here, small)
  table exactly once no matter how many distinct records' rows live on it.
  Note the planner's own **cost** estimate for this query (`0.28..173.56`) is
  *lower* than the near-head query's is high in the old plan's terms and
  nowhere close to predicting the 949-buffer actual — another instance of
  "cost estimates are not evidence."

  This is a real cost, not a rounding artifact, and it does not clear the
  impact floor on its own. It is accepted here because (a) it is bounded by
  the record's own chain depth, never larger, and only reachable by a query
  about a record's *oldest* history; (b) the near-head case — asking about
  recent state, which is what an as-of audit query does in practice — improves
  unconditionally at every depth tested; and (c) for the long-tail majority of
  ledgered records (a handful of revisions, not thousands), there is no
  meaningful "old vs. recent" distinction to begin with — the bounded query is
  strictly better there regardless of position. A workload that specifically
  and frequently queries ancient instants on very deep chains would want a
  covering index (`INCLUDE`) to eliminate the heap fetches entirely; that is a
  schema change (`CREATE INDEX`, write cost, migration lock) out of scope for
  this change and flagged here as a follow-up rather than folded in.

---

## ✅ Equivalence

- **Existing tests pass unchanged.** `tests/sqlite_ledger.rs` (39 tests,
  including the bitemporal `valid_time` axis, tenant fail-closed reads, the
  never-returns-a-revision-recorded-after-the-instant invariant, and the
  golden as-of/diff/verify walk) and `tests/integration/ledger_postgres.rs`
  (byte-for-byte as-of reconstruction against a live oracle, diff, verify,
  restore) needed no edits.
- **This harness self-checks equivalence**: at every depth, the near-head
  as-of reconstructs `balance_cents` equal to the exact expected revision
  position; the worst-case as-of reconstructs the insert (`balance_cents ==
  0`); `ledger_diff` reports exactly the one column that actually changed.
  These are hard-asserted, not printed.
- **`ledger_verify`/`ledger_revisions` untouched** — the hash-chain walk and
  the raw chain listing still read every revision, unconditionally, exactly as
  before. Only the two callers that only ever needed one revision changed how
  they fetch it.
- The SQL predicate change is exactly `snapshot_as_of`'s own selection rule
  (discard revisions that fail the transaction/valid-time bound, keep the
  greatest surviving `seq`) moved into the `WHERE`/`ORDER BY`/`LIMIT` clause —
  `ORDER BY seq DESC LIMIT 1` over a filtered set *is* `max_by_key(seq)` over
  that set, not an approximation of it.

---

## 🔬 Reproduce

```bash
# Requires Docker.
cargo test -p autumn-web --features "test-support" --test integration_tests \
  -- --ignored ledger_as_of_deep_chain_profile --nocapture --test-threads=1

# Full existing correctness suites, unchanged:
cargo test -p autumn-web --test sqlite_ledger --features "test-support,sqlite"
cargo test -p autumn-web --features "test-support" --test integration_tests \
  -- --ignored ledger_records_chains_and_reconstructs_on_postgres \
     restore_records_a_revision_on_postgres \
     verify_detects_a_truncated_tail_on_postgres \
  --nocapture --test-threads=1
```

`baseline/output.txt` was captured with the fix's macro change stashed
(`git stash push -- autumn-macros/src/repository.rs`) and the harness rebuilt;
`after/output.txt` with it restored — same harness, same fixture-generation
code, same session.
