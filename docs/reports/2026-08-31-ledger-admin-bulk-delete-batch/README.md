# 🗃️ Ledger: batch TokenAdminModel bulk delete (statements/action N→1)

## 🎯 Workload

`POST /admin/{slug}/actions` (`autumn-admin-plugin/src/routes.rs`,
`model_action`) is the admin panel's bulk-action endpoint: it parses a form
body carrying `action=<name>` plus a **repeated, uncapped** `ids=<id>` field
(one entry per row an operator selected in the list view), then calls
`model.execute_action(&pool, &action, ids)`.

[`AdminModel::execute_action`](../../../autumn-admin-plugin/src/traits.rs)
(the default every model gets unless it overrides the method) dispatches
`"delete"`/`"restore"`/`"purge"` by looping over `ids` and calling the
model's single-row `delete`/`restore`/`purge` once per id:

```rust
"delete" => {
    let mut count: u64 = 0;
    for id in ids {
        self.delete(&pool, id).await?;
        count += 1;
    }
    Ok(count)
}
```

[`TokenAdminModel`](../../../autumn-admin-plugin/src/tokens.rs) (the
built-in admin model for scoped API tokens, `/admin/api-tokens/`) doesn't
override `execute_action`, so it inherits this loop. Its `delete()` is a
full `pool.get()` + single-row `UPDATE ... WHERE id = $1` round trip
(tokens.rs) — so an operator selecting hundreds of stale or compromised
service tokens in the admin list and clicking "Delete selected" cost one
statement, and one connection checkout, **per token**, not per click.

**Fixture**: a 50,000-row `api_tokens` table (the real schema from
`autumn-admin-plugin/tests/token_admin_db.rs`'s `CREATE_TABLE_SQL`, included
verbatim so the fixture can't drift from what the admin UI actually
manages). Skewed `principal_id` cardinality (80% of rows land on 400 repeat
principals — services with many issued tokens — 20% are one-off long-tail
principals), 12% pre-revoked, 35% NULL `last_used_at`, and real dead tuples
from a follow-up `UPDATE` before `ANALYZE`. The bulk-delete selection is
2,000 ids — a plausible one-shift "revoke every stale service token"
operator action — scattered every 25th id across the table (not a
contiguous head block), deliberately including 200 ids that are already
revoked (must stay a no-op, not double-count) and 50 ids past the table's
range that don't exist at all (must stay a no-op, not error) — 2,050 ids
submitted in total.

**Reproduce**:
```bash
cargo test -p autumn-admin-plugin --test token_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). This crate has no consolidated
`tests/integration/mod.rs` (unlike `autumn`/`autumn-cli`), so this is a
plain `#[ignore]`d test in its own binary — CI's Docker sweep
(`-- --ignored`, see `CLAUDE.md`) picks it up with no workflow edit.

## 📈 Profile

This harness drives exactly one workload — the bulk-delete action — so
there's no cross-statement ranking to build: the revoke `UPDATE` (whichever
shape it takes) is the entire measured cost. It is not a small slice of a
bigger request; it **is** the request. The relevant "profile" here is the
`calls` count against a single, well-known statement shape, which is
exactly the signal the Ledger process calls out as invisible in a buffer
ranking but dominant in `pg_stat_statements.calls`: "individually trivial,
collectively dominant."

## 🧭 Plan

Same access method either side — a primary-key point lookup via
`api_tokens_pkey`, no seq scan, no sort:

**Before** (`baseline/output.txt`, `calls=2050`):
```
UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE $2 WHERE id = $1 AND revoked_at IS NULL
```
```
Update on public.api_tokens  (cost=0.29..8.31 rows=0 width=0)
  Buffers: shared hit=3
  ->  Index Scan using api_tokens_pkey on public.api_tokens  (cost=0.29..8.31 rows=1 width=14)
        Index Cond: (api_tokens.id = 25)
        Filter: (api_tokens.revoked_at IS NULL)
        Buffers: shared hit=3
```

**After** (`after/output.txt`, `calls=1`):
```
UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE $2 WHERE id = ANY($1) AND revoked_at IS NULL
```

Every id-scoped `UPDATE`, `restore`/`purge`, and the loop's own `pool.get()`
call collapses into one round trip carrying one bound `bigint[]` array
instead of 2,050 separately-prepared, separately-executed statements.

## 💡 Hypothesis

"`execute_action`'s default `"delete"` branch is a `for id in ids { self.delete(&pool, id).await?; }`
loop (traits.rs) — one DB round trip *and* one connection checkout per id.
`TokenAdminModel::delete` (tokens.rs) is a single-row, idempotent
`UPDATE ... WHERE id = $1 AND revoked_at IS NULL`. The fix is mechanical:
override `execute_action` on `TokenAdminModel` for the `"delete"` action to
issue the same idempotent predicate once, batched over the whole id list
via `WHERE id = ANY($1)`."

## 🔧 Change

`autumn-admin-plugin/src/tokens.rs`: `TokenAdminModel` now overrides
`execute_action`. The `"delete"` branch issues one
`UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' WHERE id = ANY($1) AND revoked_at IS NULL`
bound to the full `ids: Vec<i64>`, and returns `ids.len()` — matching the
loop's own counting behavior exactly (see Equivalence). `TokenAdminModel`
never declares soft delete (`supports_soft_delete()` is the trait default,
`false`), so `actions()` only ever offers `"delete"`; the `"restore"`/`"purge"`/
unhandled-action branches are kept as unchanged copies of the trait
default's per-id loop, purely so a direct or out-of-band call to
`execute_action("restore", …)`/`execute_action("purge", …)` still gets the
exact same "does not support soft delete" error it always did. There is no
batching concern there: `self.restore`/`self.purge` are the trait's default
methods, which return `Err` on the very first id regardless of loop shape.

No migration — the table and its `api_tokens_pkey` index are unchanged;
this only changes how many round trips one bulk action costs. This is a
single, scoped override (`TokenAdminModel` only, one file) rather than a
change to `AdminModel::execute_action`'s default, which would touch every
model — a plugin-wide default is a broader design surface than "smallest
change that moves the counter" covers, and is left for whoever adds the
next model that needs it to decide with the same shape in hand.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before the run. Full statement dumps in `baseline/output.txt`
(captured against the pre-fix per-id loop, its own commit) and
`after/output.txt` (captured against the fix, same fixture, same session).

| | before | after |
|---|---:|---:|
| revoke statement calls | 2,050 | **1** |
| revoke statement buffers | 11,970 | 15,236 |
| ids submitted (for reference) | 2,050 | 2,050 |

Statement count drops from **one per id to one per bulk action** — the
admissible-on-its-own N+1 floor ("statement count per request drops from
O(n) to O(1)... needs no other justification"). Buffers touched are **not**
reduced (in fact higher, 11,970 → 15,236) — folding 2,050 single-row
point-UPDATEs into one `id = ANY($1)` array-match does the same
index-lookup-and-heap-update work per id, plus the array itself has to be
read and matched against, so the total work touched by Postgres is not
smaller. This is an explicit, disclosed round-trip-count win, not a buffer
reduction, and it is not represented as one — the same shape the prior
`write_documents` batching PR (#2308) reported.

No `temp_blks_written` at any point (no spill, either side, confirmed in
both `output.txt` dumps). No index was added or dropped, so there is no
write-amplification/WAL-tax tradeoff to measure — this changes *how many
statements* carry the same writes, not what they write or what indexes
maintain them.

## ✅ Equivalence

The harness dumps `(id, revoked_at IS NULL)` for every one of the 2,050
submitted ids, sorted by `id` (no ties), at the end of both the baseline and
after runs. **The two dumps are byte-for-byte identical**
(`diff <(sed baseline dump) <(sed after dump)` — empty). This covers:

- the 1,800 ids that were NULL (`revoked_at`) before the action and must
  transition to non-NULL
- the 200 ids forced already-revoked before the action, which must stay
  revoked and not error or get touched twice
- the 50 nonexistent ids past the table's range, which must not appear in
  the dump and must not error the whole batch
- the returned `count` from `execute_action`, asserted equal to
  `ids.len()` (2,050) in both directions — matching the pre-fix loop's own
  behavior of counting every submitted id as "applied" regardless of
  whether its `UPDATE` actually matched a row (a duplicate or already-
  revoked id was, and still is, counted)

No existing test's expectations were edited. The existing
`autumn-admin-plugin` Docker-backed suite (`token_admin_db.rs`,
`impersonation_admin.rs`) passes unchanged against the fix.

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement applies
— see Measurement above; this is a round-trip-count change on an existing
`UPDATE`, not a new write pattern or a new index to maintain.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit):
cargo test -p autumn-admin-plugin --test token_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1

# Full existing admin-plugin Docker suite:
cargo test -p autumn-admin-plugin -- --ignored
```
