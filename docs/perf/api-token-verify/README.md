# `DbApiTokenStore` request path: no `last_used_at` round trip when the stamp is not due

This is a Ledger run on the bearer-token request path:
`RequireApiToken` calls `DbApiTokenStore::verify_scoped`
(`autumn/src/auth.rs`) for every request.

`verify_scoped` looked up the live token, then sent a guarded `UPDATE` to stamp
`last_used_at`. The guard (`last_used_at IS NULL OR last_used_at < now - 5 min`)
keeps the write to once every 5 minutes. But the statement went out on every
request. On a busy token it matched no row almost every time, and each call
still cost a round trip and a primary-key probe.

The lookup now also reads `last_used_at`. When the stamp is not due, the
`UPDATE` is not sent. When it is due, the same guarded `UPDATE` runs as before.

## Workload and fixture

`autumn/tests/integration/api_token_verify_profile.rs` sends real `GET`
requests with `Authorization: Bearer …` through the `RequireApiToken` layer of
a `TestApp`, with a `DbApiTokenStore` on the app's pool.

* `api_tokens` from the shipped migrations (`create_api_tokens` and
  `add_scoped_token_columns`), at 100,000 / 1,000,000 / 3,000,000 rows. About
  three tokens per principal, created over two years. 25% are revoked
  (rotation history), 8% expired, 30% have a future expiry and the rest none.
  Each has 1 to 4 scopes. 20% were never used, and the rest were last used in
  the last 90 days. 3% are updated before `ANALYZE` with no `VACUUM`
  (3,059 / 30,213 / 90,241 dead tuples). Heap 19 / 194 / 582 MB, indexes
  26 / 263 / 791 MB.
* 64 hot live tokens carry the traffic, picked with Zipf-like skew by a
  seeded generator. 2% of requests use an unknown or revoked token and get
  401.
* Two phases of 2,048 requests each. Each phase gets its own app pool,
  warmed by one request before the counters reset and closed before they
  are read, and starts after a `CHECKPOINT`:
  1. `steady`: 48 hot tokens were used in the last 4 minutes, so their stamp
     is not due. 16 were last used an hour ago, so each is stamped once.
  2. `all-due`: all 64 hot tokens are due once.

Postgres 16.15, `pg_stat_statements.track = all`, autovacuum off, one client.

## Profile (baseline, 1M rows, `steady`)

From `pg_stat_statements`. The full output is in `baseline-*.txt`.

| statement | calls | rows | buffers | share of buffers | share of calls |
|:--|--:|--:|--:|--:|--:|
| `last_used_at` `UPDATE` (the target) | 2,007 | **16** | 8,115 | **44.2%** | **32.9%** |
| token lookup `SELECT` | 2,048 | 2,007 | 10,238 | 55.8% | 33.6% |
| `SELECT 1` (pool recycle check) | 2,048 | 2,048 | 0 | 0% | 33.6% |

That is 2.98 statements and 8.96 shared buffers per request. 2,007 `UPDATE`s
wrote 16 rows. No temp blocks anywhere.

## Plan

`explain.sh` writes `explain-before.txt`, run on the 3M fixture with a hot
token that was used a minute ago:

```
--- verify_scoped: stamp last_used_at (not due: matches no row)
 Update on public.api_tokens (actual rows=0 loops=1)
   Buffers: shared hit=7
   ->  Index Scan using api_tokens_pkey on public.api_tokens (actual rows=0 loops=1)
         Index Cond: (api_tokens.id = 19750)
         Filter: ((last_used_at IS NULL) OR (last_used_at < (… - '00:05:00'::interval)))
         Rows Removed by Filter: 1
```

The lookup reads the same 5 buffers with or without the extra
`last_used_at` column (`Index Scan using idx_api_tokens_hash`, shared
hit=5 in both). After the change, the not-due `Update` node is not executed
at all.

## Measurement

Tool: `pg_stat_statements` for statements and buffers, and
`pg_stat_user_tables` for row updates. Each row is 2,048 requests.

| size, phase | statements | buffers | `UPDATE` calls | `UPDATE` buffers | rows updated |
|:--|--:|--:|--:|--:|--:|
| 100k steady, before | 6,103 | 14,306 | 2,007 | 6,113 | 16 |
| 100k steady, after | 4,112 (−32.6%) | 8,301 (**−42.0%**) | 16 | 108 | 16 |
| 1M steady, before | 6,103 | 18,353 | 2,007 | 8,115 | 16 |
| 1M steady, after | 4,112 (−32.6%) | 10,361 (**−43.5%**) | 16 | 123 | 16 |
| 3M steady, before | 6,103 | 18,331 | 2,007 | 8,094 | 16 |
| 3M steady, after | 4,112 (−32.6%) | 10,341 (**−43.6%**) | 16 | 104 | 16 |
| 100k all-due, before | 6,103 | 14,368 | 2,007 | 6,187 | 64 |
| 100k all-due, after | 4,160 | 8,555 (−40.5%) | 64 | 374 | 64 |
| 1M all-due, before | 6,103 | 18,402 | 2,007 | 8,176 | 64 |
| 1M all-due, after | 4,160 | 10,642 (−42.2%) | 64 | 416 | 64 |
| 3M all-due, before | 6,103 | 18,408 | 2,007 | 8,180 | 64 |
| 3M all-due, after | 4,160 | 10,648 (−42.2%) | 64 | 420 | 64 |

Per request: 2.98 → 2.01 statements, and 8.96 → 5.06 buffers at 1M rows.
`UPDATE` calls now equal the number of stamps that are due, not the number of
requests. The lookup's buffers do not change (10,238 before and after at
1M). Temp blocks are 0 throughout.

WAL: the `UPDATE`s that matched no row wrote no WAL, so the statement WAL
does not move (600,506 bytes before and after at 1M; most of it is the
lookup's first touch of each page after the checkpoint). This PR claims no
WAL change.

### Against the impact floor

The target was 44.2% of the workload's buffers and 32.9% of its calls. Its
buffers drop 98.5% (8,115 → 123), and the workload total drops 42–44% at
every size. That clears "≥20% reduction in total buffers for a statement that
is ≥5% of the workload". No index is added, and the schema does not change.

## Equivalence

* **Request outcomes and writes are identical.** In every phase at every
  size, before and after give the same 2,007 × 200 and 41 × 401, the same
  `n_tup_upd`, and the same list of stamped token ids (the `outcome` lines
  in `baseline-*.txt` and `after-*.txt`; `diff` finds no difference).
* **Why skipping is safe.** The only writer of `api_tokens.last_used_at` is
  this guarded `UPDATE`. It writes `now` only where the old value is earlier
  than `now − 5 min`, so the new value is always later than the old one, even
  across replicas whose clocks disagree. The stamp only moves forward. If
  the lookup read a stamp at or after the threshold, the stamp is still at or
  after it when the `UPDATE` would run, and the `UPDATE` would match no row.
  The `UPDATE`'s result was already ignored (`let _ =`), so skipping it
  changes no error path either.
* **Sub-microsecond threshold.** The threshold is compared in Rust with
  nanoseconds. The column holds microseconds. The Rust check can only call
  the stamp due when Postgres would not, never the other way round. In that
  ≤1 µs window it sends the same guarded `UPDATE`, which matches no row.
* `equivalence.sh` (`equivalence.txt`) runs the old and new protocol on two
  copies of the same rows and compares whole rows. Its cases: never used; 1 µs
  before the threshold; exactly on it; 1 µs after it; used 1 s ago; a stamp
  written by a replica whose clock runs 3 minutes ahead; used 7 weeks ago;
  revoked; expired; a future expiry with a fresh stamp; and another replica
  stamping the row between the lookup and the `UPDATE`. Each case runs two
  requests. 0 rows differ.
* `scoped_tokens_db`'s four Postgres tests (issue, list, revoke and rotate
  round trip; SQL expiry filter; `last_used_at` recorded; legacy-shaped row)
  pass unchanged against Postgres 16, as do the `auth` unit tests.

## Not changed (findings)

* **`idx_api_tokens_hash` duplicates `api_tokens_token_hash_key`.**
  `create_api_tokens` declares `token_hash TEXT NOT NULL UNIQUE`, which
  builds a unique index, and then also creates the plain index
  `idx_api_tokens_hash` on the same column. At 3M rows each is 363 MB. The
  planner uses the plain one (`idx_scan` 4,354 vs 0 on the unique one over
  this run), but the unique index can serve every lookup it serves. Every
  token insert, and every non-HOT update, writes both. Dropping the plain
  index is an index drop: it needs a maintainer's decision and an
  observation window from a real deployment.
* **`list` reads the whole table.** `DbApiTokenStore::list` filters on
  `principal_id`, which has no index. At 3M rows it is a parallel seq scan
  that reads 74,548 buffers and removes 2,999,997 rows to return 3
  (`explain-list-3000000.txt`). That path serves the token admin page and
  the CLI, not requests, so this run does not change it.
* **`SELECT 1` once per request, 33.6% of statements.** This is
  diesel-async's `RecyclingMethod::Verified` check on checkout. It costs no
  buffers, and the idempotency run recorded it already.

## Reproduce

Postgres 16 with `pg_stat_statements` preloaded and autovacuum off:

```
AUTUMN_LEDGER_PG_URL=postgres://postgres@127.0.0.1:5433/ledger \
AUTUMN_LEDGER_API_TOKENS=1000000 \
cargo test -p autumn-web --test integration_tests -- --ignored \
  api_token_verify_profile --nocapture --test-threads=1
```

For the baseline, check out the commit before the `auth.rs` change. Then
run `explain.sh` (PG* env, on the database the harness left) and
`equivalence.sh`.
