# 🗃️ Ledger: batch cms `ensure_unique_slug` collision probe (statements up to 199→2, common case unchanged at 1)

## 🎯 Workload

`examples/cms/src/content.rs`'s `ensure_unique_slug` picks a URL slug that is
free among the rows minting the same shape of URL — the WordPress `/about`,
`/about-2`, `/about-3`, … numbering scheme. It is called from two real, public
write paths, not a test helper:

- `routes/admin/posts.rs:1718` — every admin post/page **create**, and every
  **edit that changes the slug** (re-allocated inside the same transaction as
  the update).
- `routes/site.rs:444` (`save_post_with_unique_slug`) — every **public-facing
  content creation** (the importer, and any other caller that saves through
  the shared helper), in a retry loop that can call it up to 5 times on a lost
  race.

Before the fix, the function walks candidate suffixes (`desired`,
`desired-2`, `desired-3`, …) one at a time in a `for` loop, issuing a
separate `SELECT COUNT(*)` round trip per candidate until it finds one that
is free — up to 199 sequential statements for a single slug allocation. A
title with many prior collisions is not an exotic case: a recurring title
("Weekly Update"), a date-stamped post, or a bulk/scripted import naturally
produces exactly this shape. `posts.slug` is already indexed
(`idx_posts_type_slug`, `idx_pages_parent_slug`, `idx_posts_bare_path_slug` —
see `examples/cms/migrations/20260908005714_create_content_schema/up.sql`),
so each individual `COUNT` is cheap in buffers; this is a pure **statement-count
N+1**, not a missing-index problem.

**Fixture** (`examples/cms/tests/ensure_unique_slug_batch_profile.rs`): 300
unrelated background `post`-type rows, so `idx_posts_type_slug` /
`idx_posts_bare_path_slug` have a realistic size instead of trivially fitting
on one page, plus 60 pre-existing collisions on the profiled slug itself
(`some-title`, `some-title-2`, … `some-title-60`) — the shape a recurring
title or a scripted import naturally produces. A follow-up `UPDATE` before
`ANALYZE`, no `VACUUM`, gives the table real dead tuples, the same technique
every other Ledger fixture in this repo uses. Two calls are profiled, both
directly against the real function (not a hand-rolled duplicate query, no
HTTP round trip needed since it takes a plain `AsyncPgConnection`):
`ensure_unique_slug(conn, "post", "some-title", None, None)` (the 60-collision
case) and `ensure_unique_slug(conn, "post", "brand-new-unused-title", None,
None)` (zero collisions — the overwhelmingly common case in production,
added after review — see "Revision" below).

**Reproduce**:
```bash
cargo test -p cms --test ensure_unique_slug_batch_profile \
  -- --ignored --nocapture --test-threads=1
```
Requires Docker (`postgres:16-alpine` testcontainer, `pg_stat_statements`
preloaded).

## 📈 Profile

Single workload, single statement family — the same shape as every other
per-row-loop finding in this repo's history (`ledger_dd_comments`,
`recount_terms`, `set_post_terms`, `mark_dead`, the wiki
`collection_links` insert loop, …): the loop *is* the measured cost, and it
is invisible in a buffer ranking because each individual `COUNT` is cheap.
By `calls`, the unbatched `SELECT COUNT(*)` statement is 100% of the
`posts`-touching statements this call issues before the fix (61 of 61).

The *call-frequency* profile matters as much as the buffer profile here: the
61-collision case is the expensive tail, but the zero-collision case is the
overwhelming majority of real calls (most titles are not reused). A fix
graded only on the collision-heavy scenario can look free while actually
taxing the common path — see "Revision" below, where exactly that was
caught in review.

## 🧭 Plan (before/after `EXPLAIN`)

**Before** (single-candidate shape, one call per suffix tried — shown for
the 30th candidate probed):
```
SELECT count(*) FROM posts WHERE slug = 'some-title-30' AND post_type = ANY(ARRAY['post', 'page']) AND (post_type = 'post' OR parent_id IS NULL)
Aggregate  (cost=12.58..12.59 rows=1 width=8) (actual time=0.061..0.062 rows=1 loops=1)
  Output: count(*)
  Buffers: shared hit=8
  ->  Index Scan using idx_posts_type_slug on public.posts  (cost=0.27..12.58 rows=1 width=0) (actual time=0.058..0.059 rows=1 loops=1)
        Index Cond: ((posts.post_type = ANY ('{post,page}'::text[])) AND (posts.slug = 'some-title-30'::text))
        Filter: ((posts.post_type = 'post'::text) OR (posts.parent_id IS NULL))
        Buffers: shared hit=8
Execution Time: 0.114 ms
```
This shape repeats **61 times** in sequence — one per candidate, from
`some-title` through `some-title-61` — before the first free one is found.
(After the fix, this exact shape still runs, but **only once** — as the
fast-path probe of `candidates[0]`, shown as `buffers=10` above with the
real fixture's row count, vs. `shared hit=8` in this smaller illustrative
`EXPLAIN`.)

**After, fallback path** (batched shape, one call for the **full remaining
198-candidate list** — `some-title-2` through `some-title-199` — reached
only because the fast-path probe above found `some-title` taken; the real
function always sends the full remaining list here, not just enough to
cover the fixture's 60 collisions, since it can't know the collision count
in advance):
```
SELECT slug FROM posts WHERE slug = ANY(ARRAY['some-title-2','some-title-3',...,'some-title-199']) AND post_type = ANY(ARRAY['post', 'page']) AND (post_type = 'post' OR parent_id IS NULL)
Seq Scan on public.posts  (cost=0.49..21.70 rows=198 width=12) (actual time=0.101..0.132 rows=59 loops=1)
  Output: slug
  Filter: ((posts.post_type = ANY ('{post,page}'::text[])) AND ((posts.post_type = 'post'::text) OR (posts.parent_id IS NULL)) AND (posts.slug = ANY (...)))
  Rows Removed by Filter: 301
  Buffers: shared hit=14
Execution Time: 0.154 ms
```
The planner picks a sequential scan over the index here (361-row fixture,
198-value `ANY()` list — cheaper than up to 198 index probes), which is a
planner choice, not something the fix mandates; on a table with many more
rows and a more selective type filter it may choose the index instead.
Buffers are unchanged from the 61-item illustration above (`shared hit=14`
either way — a wider `ANY()` array over the same small fixture doesn't cost
more once the planner has already committed to a sequential scan), and the
`pg_stat_statements` snapshot in the Measurement table below is the real
number either way, captured from the actual function call, not this
illustrative query. Either way it
is **one** round trip, carrying the whole candidate list instead of paying
network/parse/plan overhead per candidate. Full output in
`baseline/output.txt` and `after/output.txt`.

## 💡 Hypothesis

The function issues one `SELECT COUNT(*)` per candidate suffix instead of
one batched `SELECT ... WHERE slug = ANY(...)` — the textbook N+1-on-read
pattern this repo's own `CLAUDE.md`/Ledger doctrine calls out ("Going from
O(n) statements to O(1) is the single highest-value change available in a
Diesel codebase"). The loop's job — find the first candidate, in a fixed
enumeration order, that no existing row (under the same scoping filters)
already holds — does not require asking the database one candidate at a
time: the *set* of already-taken candidates can be fetched in one query, and
"first candidate not in that set" is then a plain Rust walk over an ordered
list.

## 🔧 Change

`examples/cms/src/content.rs`'s `ensure_unique_slug` now builds the exact
ordered candidate list the original loop would have enumerated — same
starting point (`desired`, or `desired-2` when `shadowed_by_a_route`), same
`2..=199` suffix bound, same off-by-one boundary (`desired-200` is never
itself queried) — then resolves it in **at most two** queries instead of up
to 199:

1. **Fast-path probe**: the same single-value, index-backed
   `SELECT COUNT(*) WHERE slug = $1` shape the original loop's very first
   iteration used, checking only `candidates[0]`. If free (the common case),
   return it immediately — one cheap statement, identical in shape and cost
   to what the unfixed code already paid for this case.
2. **Batched fallback**: only reached when the first candidate collides.
   Issues **one** query — `posts::slug.eq_any(&candidates[1..])` combined
   with the exact same scope filters that already existed
   (`competing_types`, nested-page/bare-path parent scoping, `exclude_id`),
   selecting just `posts::slug`. The returned rows collect into a
   `HashSet<String>` of taken candidates, and the first remaining candidate
   *not* in that set wins — the same "first free wins" rule the loop
   applied one probe at a time. If every candidate is taken, the same
   `AutumnError::unprocessable_msg("Too many posts share this slug; choose
   a different one")` is returned.

(See "Revision" below for why this is two queries rather than the one a
first pass at this fix used.)

One behavioral wrinkle is deliberately **not** reproduced: in the
`shadowed_by_a_route` case, the original loop rechecks `desired-2` a second
time via its carried-over candidate (a bug of the check-then-advance
structure, not a semantic requirement). That recheck is idempotent — the
same string can't become "more taken" the second time it's checked — so
dropping it changes nothing about which slug is returned or when the error
fires; the *set* of candidates searched (`desired-2` through `desired-199`,
in that order) is unchanged.

No schema change, no new index, no migration — `posts.slug` was already
indexed on every filter shape this query uses.

## 📊 Measurement

| Scenario | Metric | Before | After | Tool |
|---|---|---:|---:|---|
| `ensure_unique_slug` (60 pre-existing collisions, "post") | statements | 61 | **2** | `pg_stat_statements.calls` |
| same | buffers (hit+read) | 325 | 24 (10 probe + 14 batch) | `pg_stat_statements` |
| `ensure_unique_slug` (0 collisions — the common case) | statements | 1 | **1** (unchanged) | `pg_stat_statements.calls` |
| same | buffers (hit+read) | 4 | 4 (unchanged) | `pg_stat_statements` |

The collision-heavy case goes from O(n) round trips to a constant 2 — still
clears **"Elimination of an N+1 — statement count per request drops from
O(n) to O(1)"** independent of the buffer delta. The common, zero-collision
case — the overwhelming majority of real calls — is **unchanged**: same one
statement, same buffer count, because the fast-path probe reproduces
exactly the query shape the original loop's first iteration already paid
for. That equality (not just "no regression") is what the added common-case
profiled scenario asserts.

## ✅ Equivalence

All of the following are asserted in the same profiling test run
(`ensure_unique_slug_batch_profile.rs`), against the real Postgres fixture,
calling the real function directly — and all pass identically against both
the pre-fix and post-fix code (see `baseline/output.txt` and
`after/output.txt`, both ending "All equivalence checks passed."):

- **No collision**: an unused desired slug is returned immediately
  (`solo-title` → `solo-title`).
- **One collision**: `desired` taken, `desired-2` free
  (`duet-title` → `duet-title-2`).
- **The exact 198/199 boundary**: with candidates `boundary` through
  `boundary-198` taken (198 rows), the call still succeeds with
  `boundary-199` — the last candidate the original loop would ever reach.
  Taking `boundary-199` too (199 taken candidates) makes the call fail with
  the unchanged error message, rather than silently reaching for
  `boundary-200`.
- **`shadowed_by_a_route`**: a bare-path-type post desiring `search` (a
  literal reserved application route) starts its candidate search at
  `search-2`, not at the bare word.
- **`exclude_id`**: editing a post's own slug back to its current value does
  not collide with itself (`edit-me`, excluding its own id, returns
  `edit-me`).
- **Nested-page scoping**: a page named `team` under one parent does not
  collide with a page named `team` under a *different* parent (both resolve
  to the plain `team`).
- **Bare-path scoping across types**: a top-level `post` and a top-level
  `page` sharing a bare URL path *do* compete — a `page` desiring a slug a
  `post` already holds is renamed (`showcase` → `showcase-2`), matching
  `BARE_PATH_TYPES`.

The main profiled scenario itself is also an equivalence check: with 60
pre-existing collisions, both the pre-fix and post-fix code return
`some-title-61`, asserted directly in the test.

## 💸 Write cost

None — this is a read-only allocation probe (no `INSERT`/`UPDATE`/`DELETE`
in the batched query itself). No index added or dropped; no WAL impact.

## 🔬 Reproduce

```bash
# Baseline (checkout the harness-only commit first, or `git stash` the fix):
cargo test -p cms --test ensure_unique_slug_batch_profile \
  -- --ignored --nocapture --test-threads=1

# After (with the fix in content.rs applied):
cargo test -p cms --test ensure_unique_slug_batch_profile \
  -- --ignored --nocapture --test-threads=1

# Verification:
cargo fmt --all -- --check
cargo clippy -p cms --all-targets --all-features -- -D warnings
cargo test -p cms
cargo test -p cms --test integration_test -- --ignored --test-threads=1
```

## 📝 Revision

The PR's first commit batched the *entire* candidate list into one
`eq_any(&candidates)` query unconditionally — every call, collision or not.
Review (Codex, PR #2910) caught that this regresses the common,
zero-collision path: on this fixture's 361-row table the planner resolves a
~199-literal `ANY()` array via a **sequential scan** rather than the single
indexed probe the original loop's first iteration used for the same case,
because scanning 361 rows once is cheaper than the planner's cost model
expects ~199 targeted index probes to be. A bigger production table would
likely keep the index scan, but the underlying point holds regardless of
plan shape: checking up to 199 candidates in one query is strictly more
work than checking 1, and the zero-collision call — not the 60-collision
one — is what nearly every real invocation of this function actually is.

The fix above is the two-phase version: probe `candidates[0]` alone first
(matches the original loop's first iteration exactly), and only reach for
the batched query when that probe finds a collision. The added
zero-collision profiled scenario in the Measurement table is what proves
the common path is now byte-for-byte as cheap as before, not just
"probably fine" — `any calls=0` in that scenario's `pg_stat_statements`
snapshot means the batched query never runs at all when there is nothing to
batch.

### Second round

A follow-up review caught two more real issues in that two-phase commit,
both fixed here:

1. **The fast-path probe still built all ~199 candidate strings before
   checking any of them.** The two-phase version still collected the whole
   ordered `candidates: Vec<String>` up front, `format!`-ing and
   heap-allocating every suffix, then only *queried* the first one. A
   zero-collision save — the common case — paid for ~198 unnecessary
   allocations of strings it never used. Fixed by computing only
   `first_candidate` up front (one allocation, matching what the original
   loop's first iteration allocated) and building the rest of the ordered
   list — now a plain `(first_suffix..=199u32).map(...).collect()` — only
   after that probe reports a collision. No behavior change: the candidate
   order, the `shadowed_by_a_route` starting point, and the 199-candidate
   boundary are identical; this only moves *when* the strings after the
   first are allocated.
2. **The harness's illustrative fallback `EXPLAIN` didn't match what the
   fixed function actually sends.** It hand-built an `ANY()` array sized to
   the fixture's 60 collisions (`some-title` through `some-title-61`, 61
   items) rather than the full remaining candidate space the real fallback
   query always sends regardless of collision count (`some-title-2` through
   `some-title-199`, 198 items, since `some-title` itself is now checked
   separately by the fast-path probe). Array cardinality can change the
   planner's index-vs-sequential-scan decision, so a truncated illustration
   was not evidence about the query the function actually runs. Fixed by
   building the illustrative array with the same bounds
   (`first_suffix..=199`) the real fallback uses. The plan shape and buffer
   count (`shared hit=14`) are unchanged with the corrected 198-item array —
   this fixture is small enough that the wider array doesn't change the
   planner's sequential-scan choice — but the illustration now actually
   proves that instead of assuming it.

Neither changes the Measurement table's numbers (both were re-captured
after these fixes and are unchanged: 2 statements / 24 buffers for the
60-collision case, 1 statement / 4 buffers, unchanged, for the 0-collision
case) or any equivalence assertion — `cargo test -p cms --test
ensure_unique_slug_batch_profile -- --ignored --nocapture --test-threads=1`
still reports "All equivalence checks passed."
