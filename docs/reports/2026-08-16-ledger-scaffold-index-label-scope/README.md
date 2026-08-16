# 🗃️ Ledger: page-scope scaffold index/nested reference-label loads (rows read 500,000→20, buffers -98.8% at 500k parent rows)

## 🎯 Workload

Every scaffolded resource with a `belongs_to`/`references` field whose parent
has a displayable string column (issue #1146) generates a `GET /{plural}`
`index` handler that, on **every single page view**, builds a
`{name}_labels: HashMap<String, String>` so the list can show `Post: "My
First Post"` instead of a raw `post_id`. `autumn-cli`'s
`render_index_reference_label_loads` (autumn-cli/src/generate/scaffold.rs)
built that map by calling the SAME `{name}_select_options` loader the
create/edit form uses to populate its `<select>` — a full, unfiltered
`SELECT id, col FROM table ORDER BY id` with no `LIMIT`. A form dropdown
genuinely needs every possible option; the index only ever displays labels
for the ~20 rows on the current page (`DEFAULT_PAGE_SIZE` =
`autumn_web::pagination::DEFAULT_PAGE_SIZE` = 20), so this re-scanned the
**entire referenced table** regardless of its size, on every index view. The
identical mechanism exists a second time in the `--belongs-to` nested list
(`children_section_with`, the child list rendered inline on the parent's
`show` page).

**Reproduce the codegen** (confirms the exact generated query — see 🔧
Change):
```bash
cargo build -p autumn-cli --bin autumn
cd /tmp && ./autumn new bench-app && cd bench-app
../autumn generate scaffold Post title:String
../autumn generate scaffold Comment body:Text post:references
grep -A6 'pub async fn index' src/routes/comments.rs | head -10
```

**Fixture**: `posts` (the referenced table, display column `title`) /
`comments` (the paginated child, `post_id BIGINT NOT NULL REFERENCES
posts(id)`, auto-indexed — schema pinned by
`autumn-cli`'s`scaffold_references_field_emits_fk_column_constraint_and_index`
test, which this reproduces exactly). Three sizes — a small app, a growing
SaaS, and a mature multi-year app:

| size   | posts (parent) | comments (child) |
|--------|----------------:|------------------:|
| small  | 5,000           | 20,000            |
| medium | 50,000          | 200,000           |
| large  | 500,000         | 2,000,000         |

Comments are skewed 80/20 toward the most recent 20% of posts (a realistic
"new posts get more engagement" shape for a blog/forum with a long tail of
quiet old posts), seeded deterministically (`setseed(0.4241)`).
`VACUUM ANALYZE` after load.

**Reproduce** (repeat per size — small / medium / large):
```bash
createdb ledger_bench
psql -d ledger_bench -c "CREATE EXTENSION pg_stat_statements;"
# shared_preload_libraries = 'pg_stat_statements'
psql -d ledger_bench -f fixture/schema.sql

psql -d ledger_bench -v posts=500000 -v comments=2000000 -f fixture/seed.sql

# EXPLAIN captures
psql -d ledger_bench -f fixture/explain_before.sql
psql -d ledger_bench -f fixture/explain_after.sql

# pg_stat_statements profile, BEFORE
psql -d ledger_bench -c "SELECT pg_stat_statements_reset();"
psql -d ledger_bench -f fixture/workload_before.sql
psql -d ledger_bench -f fixture/profile.sql

# pg_stat_statements profile, AFTER — the fixed query needs the page's own
# FK values (the real Rust handler already holds these in memory from the
# `Vec<Comment>` it just deserialized; a bare psql script has to fetch them
# separately, OUTSIDE the measured window, to hand them to workload_after.sql):
PAGE_IDS=$(psql -d ledger_bench -tA -c \
  "SELECT array_agg(DISTINCT post_id ORDER BY post_id)::text
   FROM (SELECT post_id FROM comments ORDER BY id DESC LIMIT 20 OFFSET 0) page;")
psql -d ledger_bench -c "SELECT pg_stat_statements_reset();"
psql -d ledger_bench -v page_ids="$PAGE_IDS" -f fixture/workload_after.sql
psql -d ledger_bench -f fixture/profile.sql

# Result equivalence
psql -d ledger_bench -f fixture/equivalence.sql
psql -d ledger_bench -f fixture/equivalence_edge.sql
```

## 📈 Profile

Each simulated `GET /comments` request issues exactly three statements —
the `page()`-generated `COUNT(*)`, the `LIMIT/OFFSET` page fetch, and the
label load — so within this one-request workload the label-load statement's
share of total buffers is (before the fix):

| size   | label-load buffers | COUNT buffers | page-fetch buffers | label load's share of workload buffers |
|--------|--------------------:|---------------:|---------------------:|----------------------------------------:|
| small  | 72                  | 36              | 3                     | 64.9%                                    |
| medium | 707                 | 315             | 4                     | 68.9%                                    |
| large  | 7,051               | 3,061           | 4                     | 69.7%                                    |

Well clear of the 5%-of-workload floor at every size — this single
statement dominates the request's cost, and it dominates it *for a reason
that has nothing to do with how many comments exist*: `COUNT(*)`/page-fetch
buffers scale with the `comments` table, but the label load's buffers scale
with the **entire `posts` table**, no matter the page size or which comment
is being viewed. On a resource where the parent table happens to be small,
this is a rounding error; on a `users`/`categories`/`authors`-style
reference table in a real app — which is exactly the kind of table that
tends to grow into the hundreds of thousands of rows — it is paid in full on
every single index page load.

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` per size. Full output in
`baseline/explain_before_*.txt` / `after/explain_after_*.txt`. Same node
type either side (`Index Scan using posts_pkey`) — this is not a plan-shape
change, it is the same access method reading a different number of rows:

**Before** (large, 500,000 posts):
```
Index Scan using posts_pkey on public.posts
  (actual time=0.036..59.540 rows=500000 loops=1)
  Output: id, title
  Buffers: shared hit=7051
```

**After** (large, same fixture, scoped to the page's 20 distinct `post_id`s):
```
Index Scan using posts_pkey on public.posts
  (actual time=0.039..0.279 rows=20 loops=1)
  Output: id, title
  Index Cond: (posts.id = ANY ('{123562,224658,...20 ids...}'::bigint[]))
  Buffers: shared hit=83
```

Same story at every size — `small`/`medium` in `baseline/`/`after/`.

## 💡 Hypothesis

"The handler issues a full-table `SELECT` to label a paginated page of ~20
rows." `render_index_reference_label_loads`'s generated `index` handler (and
`children_section_with`'s nested-list twin) built their #1146 parent-label
map by calling `{name}_select_options(&mut db)` — a loader whose OTHER
caller is the create/edit form, which genuinely needs every possible parent
row for its `<select>`. Reusing it for the index conflated two different
needs: "every row, for a dropdown" and "just this page's rows, for a label
lookup." The mechanism is structural, not a matter of missing an index — the
existing `posts_pkey` index already backs both queries perfectly; the
defect is that the "before" query never gives it a `WHERE` clause to use.

## 🔧 Change

One change in autumn-cli's scaffold codegen
(`autumn-cli/src/generate/scaffold.rs`), touching two functions that shared
the identical over-fetch pattern:

- `render_index_reference_label_loads` (flat `GET /{plural}` index, all 6
  handler variants that splice its output — plain, owner-scoped, sharded,
  and the `/search` fragment handlers, tenant-scoped and not) now builds
  `{name}_ids: Vec<i64>` from `page_data.content` — the page the handler
  *already fetched* — and queries `{table}::table.filter({table}::id.eq_any({name}_ids))`
  instead of calling the form's full-table `{name}_select_options` loader.
- `children_section_with`'s nested-list label-load loop (the `--belongs-to`
  child list rendered on the parent's `show` page) gets the same fix, using
  `&mut **db` to match its existing double-deref connection convention
  (it receives `db: &mut Db`, not the owned `Db` the flat index handlers
  take).
- Both keep the exact same `{name}_labels: HashMap<String, String>` output
  contract (keyed by `row.{name}.to_string()`, with the existing `"—"`
  fallback for a missing/`None` key) — `render_columns_vec`'s label-lookup
  closures are untouched, so this is purely a change in how the map gets
  populated, not what it contains or how it's read.
- The form's own `{name}_select_options` loader is untouched and still
  called, unconditionally, by the create/edit form handlers — it still needs
  every possible option for its `<select>`, so no unused-function risk was
  introduced.
- Updated two stale comments and one codegen assertion
  (`sharded_index_renders_parent_label_via_loaded_map` in
  `scaffold_belongs_to.rs`, which asserted the sharded index called
  `post_id_select_options` — it no longer does) to match.

No migration, no index (the FK is already indexed — see 🎯 Workload), no
change to the Diesel schema.

## 📊 Measurement

pg_stat_statements + `EXPLAIN (ANALYZE, BUFFERS)`, one simulated `GET
/comments` request per size (page 1, `DEFAULT_PAGE_SIZE` = 20):

| size   | before rows read | after rows read | rows Δ    | before buffers | after buffers | buffers Δ |
|--------|-------------------:|-------------------:|----------:|----------------:|---------------:|----------:|
| small  | 5,000               | 20                  | -99.60%   | 72               | 54              | -25.0%    |
| medium | 50,000              | 20                  | -99.96%   | 707              | 61              | -91.4%    |
| large  | 500,000             | 20                  | -99.996%  | 7,051            | 83              | -98.8%    |

Statement count per request is unchanged (3 before, 3 after — this is a
rows-read/buffers fix, not an N+1 elimination: the label load was always one
statement, it just read far more than it needed to). Clears the impact
floor two ways at every size: **rows read at the scan node drops ≥99.6%**
(floor: ≥50%), and **buffers drop ≥20%** on a statement that is ≥65% of the
workload's buffers (floor: ≥20% reduction on a ≥5%-of-workload statement) —
comfortably at medium/large, right at the line at small (a 5,000-row table
mostly fits in a handful of pages either way, so the buffer win is modest
there even though the rows-read win is not). No temp blocks either side, no
plan-shape change (same `Index Scan using posts_pkey` both sides — see 🧭
Plan), no WAL impact (read-only workload).

## ✅ Equivalence

`fixture/equivalence.sql`: for the same page, `SELECT id, title FROM posts
WHERE id = ANY(page_ids) EXCEPT SELECT id, title FROM posts` returns 0 rows
— every `(id, title)` pair the scoped query returns is byte-identical to
what the full-table query would have returned for that id, and the scoped
query returns exactly one row per distinct page id (no fan-out). This has to
hold: both queries `SELECT` the same two columns from the same table with no
concurrent writes between them, so equivalence here is a structural
guarantee of the rewrite, not a coincidence of this fixture — the check
exists to prove the rewrite didn't introduce a typo (wrong column, wrong
table) that would make it stop holding.

`fixture/equivalence_edge.sql` (all inside a rolled-back transaction):
- **Duplicate FK values on one page** (several comments on the same post):
  `id = ANY(ARRAY[1,1,1,1,1])` against the primary key returns exactly 1
  row, not 5 — no fan-out from repeated array elements, so a page with
  several comments on the same post still produces one map entry, not
  duplicates.
- **Empty page** (0 comments, e.g. a resource with no rows yet, or a page
  past the end): `id = ANY(ARRAY[]::bigint[])` returns 0 rows without
  erroring — the label map is empty, matching what the "before" loader's
  (unused, since there's nothing to render) map would have been.
- **FK with no matching parent row**: `id = ANY(ARRAY[-1])` and `id = -1`
  both return 0 rows — the scoped query and a full-table scan agree there is
  no such id, so the existing `"—"` fallback in `render_columns_vec`'s
  label-lookup closure applies identically either way. (Diesel's `NOT NULL
  ... REFERENCES` FK constraint means this can't happen for a live row in
  practice, short of the parent being deleted through raw SQL outside the
  framework — but the lookup already has to tolerate it structurally, since
  the same `HashMap::get` fallback also covers a genuinely-`None` nullable
  FK.)

Bi-temporal / as-of semantics: not applicable — `posts`/`comments` carry no
validity-interval or as-of columns; this is plain current-state pagination.

## 💸 Write cost

None. No index was added (the FK's `idx_comments_post_id` already existed —
see 🎯 Workload) and no write path was touched; this is a read-only query
change on the index handler.

## 🔬 Reproduce

See the `psql` sequence under 🎯 Workload. Codegen verified two ways beyond
the SQL harness above:
- `cargo test -p autumn-cli --bin autumn generate::scaffold::` (314 inline
  codegen tests) and `cargo test -p autumn-cli --test cli_tests
  scaffold_belongs_to`/`scaffold_nested_resources` (17 integration tests) —
  all pass unchanged except the one updated assertion noted in 🔧 Change.
- `cargo test -p autumn-cli --test cli_tests scaffold_belongs_to -- --ignored`
  (`belongs_to_scaffold_cargo_checks`, patches `autumn-web` to this
  checkout's path and runs `cargo check --bins` on a freshly generated flat
  scaffold) passes. The nested (`--belongs-to`) path has no equivalent
  `--ignored` test in the suite, so it was verified manually: generated a
  project with `Post` (parent, `title:String`), `Author` (a second
  reference target), and `Comment body:Text post:references
  author:references --belongs-to post` (exercising both the flat index's
  and the nested list's label loads, including a reference field that is
  *not* the nesting FK, which is the case the nested loop's `if f.name ==
  fk { continue; }` guard would otherwise skip entirely), patched
  `autumn-web` to the local path, and ran `cargo check --bins` — exit 0, no
  errors, only pre-existing warnings unrelated to this change.
