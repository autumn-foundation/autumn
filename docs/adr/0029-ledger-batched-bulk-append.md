# ADR 0029: Ledgered Bulk Writes Append In Batches

- Status: Accepted
- Date: 2026-10-30
- Deciders: Autumn maintainers
- Tags: ledger, version-history, performance

## Context

A `ledgered = true` repository writes one version-history row and one ledger
revision for each changed row. The bulk paths (`save_many`, `update_many`,
`delete_many`, `save_many_skip_invalid`, `upsert_many`) did this one row at a
time (issue #3345). Each row cost 5 statements:

1. the version-history `INSERT`;
2. the reload `SELECT` of the written row;
3. the chain-state read (clock, head, high-water mark);
4. the revision `INSERT`;
5. the high-water upsert.

So `save_many(100)` sent 503 statements. The data write was 1 of them.

The #2323 guards in `append_revision` are the tamper-evidence contract. A
batch must keep each guard, the error text, and the chain that the per-row
path makes.

## Options

1. Keep the per-row path. Document the cost.
2. Batch only the version-history `INSERT`. The cost stays O(n).
3. Batch all five steps. Move the guards into one pure function that the
   single and the batch path both call.
4. Move the append into a database trigger. Each backend needs its own hash
   code, and the trigger cannot read the model codec.

## Decision

Option 3.

- `ledger::plan_revision` is a pure function. It holds every #2323 guard, the
  sequence allocation, the clock clamp and the hash. `append_revision` and
  `append_revisions` both call it.
- `ledger::append_revisions` reads the chain state of all keys in one
  statement. Then it plans each append in input order. Then it writes all
  revisions in one statement and raises all marks in one statement.
- A key that occurs two or more times in one batch has one chain. The second
  append starts from the state that the first append leaves: `seq + 1`, the
  previous hash, and the previous instant as the clock floor. This is the
  chain that two per-row appends make. The two revisions can have the same
  `recorded_at`. `snapshot_as_of` picks the highest `seq`, and
  `ledger_verify` reports only a time that moves back.
- When one row fails a check, the batch writes no revision. The
  transaction rolls back, as it did before. Among the model checks and the
  chain guards, the error names the first row that fails, in input order,
  as before (`ledger::first_batch_error`).
- `version_history::append_version_history_many` writes all history rows in
  one statement.
- The generated code reloads the written rows with one `id IN (...)` query
  for each 1,000 ids. It runs the model checks (non-finite floats, tenant
  change) for each row in the old order.
- Postgres uses `unnest` over arrays. The SQL text does not change with the
  batch size, so one prepared statement serves all sizes. There is no bind
  limit.
- `SQLite` uses a multi-row `VALUES` list. The rows go in chunks below
  `MAX_BIND_PARAMS`. Each value binds with the same SQL type as the per-row
  path, so the stored bytes do not change.
- Single-row writes (`save`, `update`, `delete_by_id`, `restore`) keep
  `append_revision`.
- Each versioned repository has one generated `__autumn_history_flush`
  method. Each bulk site collects its rows and calls it. One copy keeps the
  expansion inside its size budget.
- The hooked `update_many` wrote no history row and no revision. It now
  calls the flush too.

A bulk chunk holds 1,000 rows or fewer. Its history and ledger statements
go from `5n` to 5.

## Planning notes

**Brainstorming.** Batch each step with `unnest`; batch with multi-row
`VALUES`; a `json_each` key list on `SQLite`; a trigger that appends the
revision; a `COPY` into a temporary table; reuse the `RETURNING` row and skip
the reload; one chain-state read with `LATERAL`; plan in Rust, write in SQL;
reject duplicate ids in a batch; chain duplicate ids in input order.

**Reverse brainstorming** (how can a batch break the chain?):

| Failure | Answer |
| --- | --- |
| Two appends for one id read the same head and fork the chain | The plan folds: the second append reads the state the first one leaves |
| The upsert touches one mark two times (Postgres refuses) | Write only the last plan for each key |
| A guard runs on stale state for a duplicate | The folded state is the state the database holds after the first append |
| One bad row; the other rows write revisions | All guards run before any revision write; the transaction rolls back |
| The error names another row than before | Rows are checked in input order; the model checks keep their order |
| Batch size passes the bind limit | Postgres binds arrays; `SQLite` chunks |
| Different stored bytes for one value | Same SQL types as the per-row path; snapshot stays `TEXT` |
| The `RETURNING` row misses an `AFTER` trigger change | Keep the reload (#2326); only batch it |
| The reload misses a row | Same refusal as before: the row cannot be read back |
| A tenant moves between the read and the write | The write runs under the same transaction and row locks as before |
| A plan cache entry for each batch size | Postgres SQL text is fixed |

**Six thinking hats.**

- White (facts): for `save_many(100)`, the 5 statements for each row are
  99.6% of calls. The data write is 0.2%. The chain-head read is already an
  index scan, so the cost is the statement count.
- Red (feeling): the guards are the tamper-evidence contract. A second copy
  of them can become different. One pure function must hold them.
- Black (risks): forked chains for duplicate ids, the upsert conflict on one
  key, a changed error, a bind limit, other stored bytes. See the table.
- Yellow (benefits): the statement count is O(chunks), not O(rows). The
  guards become a pure function, so unit tests can drive every guard with no
  database.
- Green (ideas): a later step can drop the reload when a model has no
  trigger. Out of scope.
- Blue (process): red tests first (statement count on `SQLite`, duplicate
  chaining, guard parity). Then the pure planner, then the batch writes, then
  the macro. A Postgres testcontainer test runs in the Docker sweep.

## Consequences

- A bulk write sends a constant number of ledger statements for each chunk.
- A batch reads the clock one time for each record. Two revisions of one
  record in one batch can have the same `recorded_at`.
- `append_revisions` and `append_version_history_many` are `#[doc(hidden)]`
  entry points for generated code, as `append_revision` is.
