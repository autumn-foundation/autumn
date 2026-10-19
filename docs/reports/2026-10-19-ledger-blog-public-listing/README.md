# Ledger: blog admin list stops detoasting every post body

PR title: `🗃️ Ledger: blog admin list without body (all-posts statement buffers 802,450→2,452, -99.7%)`

## 🎯 Workload
`examples/blog` (release build) against PostgreSQL 16, `SQL_ASCII`, default
config, 100,009 posts: 95% published, ~7.9 KB bodies (pglz-compressed, out-of-line
TOAST), `created_at` spread over 8 years, 9 boundary rows. Driven through HTTP:
300 post views + 2× each of `/en`, `/feed.xml`, `/api/posts`, `/admin`.
The 2-per-endpoint mix is my construction, not production traffic. The
buffer share per request does not depend on it; the share of the *workload* does.

## 📈 Profile (baseline, `baseline/`)
| statement (all `FROM posts`) | calls | % calls | buffers | % buffers | temp written |
|---|---|---|---|---|---|
| `WHERE published ORDER BY created_at DESC` (home, feed, API) | 6 | 1.9 | 2,291,274 | 74.0 | 0 |
| `ORDER BY created_at DESC` (**admin list**, target) | 2 | 0.6 | 802,450 | 25.9 | 1,994 |
| `WHERE slug = $1 AND published` | 300 | 97.4 | 2,340 | 0.1 | 0 |

`pg_statio_user_tables`: 770,285 of ~785,000 block touches on `posts` are TOAST (98%).
Target ≥5% of buffers ✔ (25.9%), <5% of calls.

## 🧭 Plan
`baseline/explain_admin_all.txt` / `after/explain_admin_all.txt`: **`EXPLAIN (ANALYZE, BUFFERS)` does not show the
cost**: it never detoasts columns it doesn't output, so both plans read the same
~1,225 heap buffers (Seq Scan, external merge sort). The TOAST reads only
appear in `pg_stat_statements`/`pg_statio`. The plan shape is unchanged
(`after/` was generated with the narrow column list).

## 💡 Hypothesis
`/admin` selects `Post::as_select()` (including `body`) but renders only
id/title/slug/published/created_at. Every row's TOASTed body is fetched and
decompressed and discarded.

## 🔧 Change
`Post::all` → `Post::all_listings`, selecting those five columns into `PostListing`.
No migration, no index, no schema change, no lock.

## 📊 Measurement (`pg_stat_statements`, same harness, same fixture, same session)
| counter | before | after | Δ |
|---|---|---|---|
| admin statement buffers | 802,450 | 2,452 | **-99.7%** |
| admin `shared_blks_read` | 204,272 | 2,069 | -99.0% |
| admin `temp_blks_written` | 1,994 | 1,230 | -38% (still spills: not eliminated) |
| workload total buffers | 3,096,130 | 2,296,130 | -25.8% |
| posts TOAST block touches (`pg_statio`) | 770,285 | 570,285 | -26% |
| statements / calls | 624 | 624 | unchanged |
| rows read | 200,018 | 200,018 | unchanged (same rows, fewer columns) |
| WAL bytes | 0 | 0 | read path |

Clears the floor: ≥20% buffer reduction on a statement ≥5% of the workload.

## ✅ Equivalence
`/en`, `/admin`, `/api/posts`, `/feed.xml` fetched before and after the change
and compared with `cmp`: all four byte-identical (65 MB, 109 MB, 767 MB, 780 MB)
over the full fixture, including the empty, 199/200/201-byte, multibyte and
draft boundary rows. Created-at values are distinct so ordering is tie-free.
Only `/admin` changed code. The others are the control.
Existing tests unchanged.

## ❌ Negative result: the home-page card
`/en` renders a 200-char preview, so it needs only the start of `body`. I tried
`substr(body,1,200)` + `octet_length(body)`. Two findings (`negative-result/`):
1. `substr` over this pglz-compressed TOAST value is **not** sliced: 381,254
   buffers vs 1,226 for `octet_length(body)` alone, i.e. the same as selecting
   the whole body. Slicing needs `STORAGE EXTERNAL` (a table rewrite, ask-first).
2. The database is `SQL_ASCII`, so `substr` counts bytes, and the page diverged
   on the 4-byte-character boundary row. A per-encoding query is a correctness risk.
Reverted; not in this PR.

## 💸 Write cost
None (no index, no write-path change).

## 🧱 Not done, needs a human decision
Home, feed and `/api/posts` are 99.8% of the remaining buffers: they load every
published post, unbounded. The fix is pagination/keyset (public API change) or a
`STORAGE EXTERNAL` migration; both are "ask before".

## 🔬 Reproduce
```
initdb; start with shared_preload_libraries=pg_stat_statements; create db blog + extension
cargo build -p blog --release   # app runs migrations on boot
psql -v n=100000 -f fixture.sql blog
./explain.sh baseline && ./profile.sh baseline 100000   # on the parent commit
./explain.sh after card && ./profile.sh after 100000
```
Not run: migration rollback (no migration), insert-throughput (no index), `cargo test --all-features` workspace-wide (blog crate only).
