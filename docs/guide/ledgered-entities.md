# Ledgered Entities (Time Travel & Tamper Evidence)

Autumn's [version history](version-history.md) answers _"who changed this row,
when, and to what?"_ — but a column-level diff log cannot be queried **as
state**. You cannot ask what a record looked like last Tuesday, diff it across
two instants, or prove the stored history was not rewritten.

A **ledgered** entity closes that gap. One marker makes an entity bitemporal by
construction: every write appends an immutable, hash-chained revision carrying a
full row snapshot in your own Postgres or SQLite, so you can query any record
*as of* any past instant, diff it across time, and verify the history was never
tampered with — with no separate event store.

## When to use this vs. version history vs. audit logging

| Concern | Tool |
|---------|------|
| "What did invoice 42 look like on the day we approved it?" | **Ledger** (this guide) |
| "Prove nobody edited that history afterwards." | **Ledger** (this guide) |
| "Who changed row 42's `plan_tier`, and what was the previous value?" | [Version history](version-history.md) |
| "Which admin exported user data at 14:32?" | [`autumn::audit`](audit-logging.md) |

The ledger is version history *promoted to queryable, provable state*. It does
not replace version history — `ledgered = true` implies `versioned = true`, so a
ledgered entity keeps `version_history()` and everything built on it.

## Opting in

```rust
#[repository(Invoice, soft_delete, ledgered = true)]
pub trait InvoiceRepository {}
```

That marker is the **only per-model change required**. Every write path version
history already covers — hand-written handlers, `#[repository(api = "…")]`
endpoints, `#[job]` and `#[mailer]` paths, bulk saves, upserts, dependent
cascades — appends a revision automatically.

`soft_delete` is required, not optional: see
[What is refused, and why](#what-is-refused-and-why).

## Migration

Run `autumn migrate` after opting a model in. Autumn applies the framework
migration that creates `_autumn_ledger_revisions`:

```sql
CREATE TABLE _autumn_ledger_revisions (
    id          BIGSERIAL   PRIMARY KEY,
    table_name  TEXT        NOT NULL,
    tenant_id   TEXT,
    record_id   BIGINT      NOT NULL,
    seq         BIGINT      NOT NULL,   -- 1-based position in this record's chain
    op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    actor       TEXT        NOT NULL DEFAULT 'system',
    request_id  TEXT,
    snapshot    JSONB       NOT NULL DEFAULT '{}',   -- FULL row state after the write
    valid_from  TIMESTAMPTZ NOT NULL,   -- valid time
    recorded_at TIMESTAMPTZ NOT NULL,   -- transaction time
    prev_hash   TEXT,                   -- NULL at seq = 1
    hash        TEXT        NOT NULL
);

CREATE UNIQUE INDEX idx_autumn_ledger_revisions_chain
    ON _autumn_ledger_revisions (table_name, COALESCE(tenant_id, ''), record_id, seq);
```

SQLite gets an equivalent fork (`INTEGER PRIMARY KEY`, JSON as `TEXT`) under the
same migration version, exactly as version history does.

Ledgering a model **after launch** is non-destructive but not retroactive: the
chain starts at the first write after you opt in. Existing rows are not
backfilled, because their past is unknowable.

## Querying the past

```rust
// Exact state at a past transaction instant.
let then: Option<Invoice> = repo.ledger_as_of(id, last_tuesday).await?;

// Field-level delta between two instants.
let delta = repo.ledger_diff(id, last_tuesday, now).await?;
for change in &delta.changes {
    println!("{}: {:?} -> {:?}", change.column, change.before, change.after);
}

// Prove the stored history was never rewritten.
let report = repo.ledger_verify(id).await?;
if let Some(broken) = &report.broken {
    tracing::error!(seq = broken.seq, kind = %broken.kind, "{}", broken.detail);
}

// The raw chain, oldest first.
let revisions = repo.ledger_revisions(id).await?;

// The head hash, for pinning outside the database.
let head = repo.ledger_head(id).await?;
```

`ledger_as_of` returns `None` when the record did not exist yet. Because a
ledgered entity is `soft_delete`, a deleted record still resolves: the
reconstructed model carries the `deleted_at` a live query would have shown, so
live-only callers check it exactly as they would against the table.

### Fidelity

Reconstruction is **byte-for-byte identical** to what a plain query would have
returned at that instant — the snapshot is the model's own serialized column
values, not a replayed diff. Autumn's test suite pins this against an oracle
recorded live at each intermediate instant.

Two documented boundaries:

- Columns opted into [at-rest encryption](attribute-encryption.md)
  (`versioned_ciphertext`) are snapshotted as ciphertext, exactly as version
  history stores them. As-of reconstruction of such a column yields ciphertext.
- Declaring `#[version_history(sensitive = [...])]` columns on a ledgered
  repository is a **compile error** — see below.

## Bitemporality

Every revision carries two instants:

- `recorded_at` — **transaction time**: when the database learned the fact.
  Always set by the framework from the write's own clock read.
- `valid_from` — **valid time**: when the fact became true in the business
  domain. Defaults to `recorded_at`.

Read valid time from your own column when the domain has one:

```rust
#[repository(Invoice, soft_delete, ledgered(valid_time = "effective_at"))]
pub trait InvoiceRepository {}
```

The column may be `DateTime<Utc>`, `NaiveDateTime`, or an `Option` of either.

Both axes are queryable:

```rust
use autumn_web::ledger::LedgerAsOf;

// What the database held at this instant.
repo.ledger_as_of_at(id, LedgerAsOf::transaction(t)).await?;

// What was true at this instant, per everything the database knows now.
repo.ledger_as_of_at(id, LedgerAsOf::valid(t)).await?;

// What the database believed *then* about *then* — the auditor's question.
repo.ledger_as_of_at(id, LedgerAsOf::bitemporal(known_at, true_at)).await?;
```

A revision's valid interval is `[valid_from, next_revision.valid_from)`, derived
at read time rather than stored, so no revision is ever updated after it is
written and the chain stays append-only.

## Tamper evidence

Each revision embeds the hash of its predecessor, forming a per-record chain.
`ledger_verify` walks the chain and reports the **first broken link**:

| What was done to the stored history | `LedgerBreak` reported |
|---|---|
| A row was edited in place | `HashMismatch` at that revision |
| A row was edited *and* re-hashed | `PrevHashMismatch` at the next revision |
| A revision was deleted | `MissingRevision` at the absent sequence number |
| A revision was inserted | `DuplicateSeq`, or `HashMismatch` on an appended forgery |
| The chain no longer starts at seq 1 | `BrokenChainStart` / `MissingRevision` |

An intact report carries `head_hash`; a broken one carries none.

### Threat model — read this

The chain is **tamper-evident, not tamper-proof**. It detects any mutation,
insertion, deletion or reordering that does not also re-derive every subsequent
hash. An adversary with write access to the ledger table *and* knowledge of the
hashing rule — which is open source — can rewrite a whole chain consistently.
Nothing stored inside the same database can prevent that.

To close that gap, pin the head hash somewhere the database cannot reach:

```rust
if let Some(head) = repo.ledger_head(id).await? {
    notary.pin(id, head.seq, &head.hash).await?;   // append-only store, notary, …
}
```

A wholesale rewrite then produces a head hash that disagrees with the pin.

The database provides one hard guarantee on top of detection: the
`(table_name, COALESCE(tenant_id, ''), record_id, seq)` unique index makes a
duplicated or forked revision a write error rather than silent corruption.

## What is refused, and why

A ledgered entity's history *is* the record, so every way of erasing or redacting
it is refused at the repository seam — at compile time, not at runtime:

| Configuration | Diagnostic |
|---|---|
| `ledgered = true` without `soft_delete` | Rejected: a hard `DELETE` erases the row the ledger reconstructs, so an as-of query would return state whose record no longer exists and `verify` could not tell erasure from tampering. |
| Calling `purge(id)` | Not generated. `purge` is soft-delete's hard-delete escape hatch — a raw `DELETE FROM` that writes no history at all. `delete_by_id` (which records a delete revision) and `restore` are the whole delete surface. |
| `#[version_history(sensitive = [...])]` | Rejected: a redacted column cannot be reconstructed, so byte-for-byte as-of fidelity would be unprovable. |
| `no_versioned_record_impl` | Rejected: the ledger snapshots through the generated `VersionedRecord` impl, and a hand-written one is not guaranteed to serialize every column. |
| `retention(...)` / `position(...)` | Already rejected for `versioned = true`: both mutate rows outside the history-writing paths. |

## Multi-tenancy and sharding

A `tenant_scoped` ledgered repository stamps `tenant_id` on every revision and
scopes every ledger read to the active tenant — a read as tenant B never sees
tenant A's revisions, and `ledger_as_of` fails closed to `None`.

Cross-shard ledger reads are rejected: per-shard record ids are ambiguous, so a
naive merge would be wrong. Query a specific shard instead.

## Cost

Each write adds one indexed `SELECT … ORDER BY seq DESC LIMIT 1` and one
`INSERT`, inside the transaction the write already opened. Bulk paths
(`save_many`, `upsert_many`, `delete_many`) do this per row. Snapshots store the
full row, so a ledgered table's history grows with row width, not just with the
size of each change — the price of O(1) as-of reconstruction and provable
fidelity. Retention and compaction of old revisions are not part of this slice.

## Limits of this slice

- Single-entity only. Cross-entity "as of" queries that join several ledgered
  entities at one consistent past instant are not supported yet.
- API-level only — no time-slider or history-viewer UI.
- No retention, compaction, or archival of old revisions.
- No distributed or multi-node ledger consensus.
- Postgres and SQLite only.

## See also

- [Version history](version-history.md) — the column-level change log the ledger
  builds on
- [Audit logging](audit-logging.md) — named business actions
- [Attribute encryption](attribute-encryption.md) — how encrypted columns appear
  in snapshots
- [Soft deletes](soft-delete.md) — the delete surface a ledgered entity keeps
