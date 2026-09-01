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
    snapshot    TEXT        NOT NULL DEFAULT '{}',   -- FULL row state after the write,
                                                     -- the exact canonical bytes hashed
    valid_from  TIMESTAMPTZ NOT NULL,   -- valid time
    recorded_at TIMESTAMPTZ NOT NULL,   -- transaction time
    prev_hash   TEXT,                   -- NULL at seq = 1
    hash        TEXT        NOT NULL
);

CREATE UNIQUE INDEX idx_autumn_ledger_revisions_chain
    ON _autumn_ledger_revisions (table_name, COALESCE(tenant_id, ''), record_id, seq);
```

A second framework table holds each record's **high-water mark** — the highest
sequence number its chain has ever reached, kept where deleting a revision
cannot reach it:

```sql
CREATE TABLE _autumn_ledger_high_water (
    table_name  TEXT        NOT NULL,
    tenant_key  TEXT        NOT NULL,   -- COALESCE(tenant_id, '')
    record_id   BIGINT      NOT NULL,
    high_seq    BIGINT      NOT NULL,   -- never decreases
    head_hash   TEXT        NOT NULL,   -- hash of the revision at high_seq
    recorded_at TIMESTAMPTZ NOT NULL,   -- and its transaction time
    PRIMARY KEY (table_name, tenant_key, record_id)
);
```

Its migration backfills a mark for every chain that already exists, so adopting
it is a plain `autumn migrate` with nothing to reconcile afterwards. On Postgres
the backfill takes `LOCK TABLE _autumn_ledger_revisions IN SHARE MODE` for the
length of one sorted pass over the revisions — that is what closes the race with
a concurrent append, and it means ledgered writes block until the migration
commits. Size that before running it against a very large ledger.

Deploy order matters: apply the migration **before** rolling out the new binary.
Code that expects the table and does not find it fails every ledgered write. See
[Tamper evidence](#tamper-evidence) for what the table buys.

`snapshot` is `TEXT`, not `JSONB`, on purpose. `JSONB` parses each number into
`numeric` and re-renders it on output, so a float serde writes as `1e16` comes
back as `10000000000000000`. The revision hash covers the exact bytes that were
written, so a re-rendering store would make `ledger_verify` report tampering on
an untouched chain.

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

// The out-of-band high-water mark this record's chain has reached.
let mark = repo.ledger_high_water(id).await?;
```

`ledger_as_of` returns `None` when the record did not exist yet. Because a
ledgered entity is `soft_delete`, a deleted record still resolves: the
reconstructed model carries the `deleted_at` a live query would have shown, so
live-only callers check it exactly as they would against the table.

### Fidelity

Reconstruction is **byte-for-byte identical** to what a plain query would have
returned at that instant — the snapshot is a full row image, not a replayed
diff. Autumn's test suite pins this against an oracle recorded live at each
intermediate instant, on both storage tiers.

Snapshots go through the model's durable per-field codec rather than
`serde_json::to_value`, so they carry every column — including `#[private]` and
[`#[encrypted]`](attribute-encryption.md) fields, which serde omits from a
model's public JSON. Encrypted columns are stored as recoverable ciphertext (the
ledger table never holds plaintext the model chose to protect) and come back
decrypted.

Three consequences worth knowing:

- Declaring `#[version_history(sensitive = [...])]` columns on a ledgered
  repository is a **compile error** — see below.
- `ledger_diff` compares the *reconstructed models*, not the stored bytes: an
  encrypted column carries a fresh nonce per write, so raw snapshots would report
  it as changed on every revision. A column the model hides from serialization
  therefore does not appear in a diff, though it is fully preserved in an as-of
  reconstruction and fully covered by the hash.
- The live-row cross-check in `ledger_verify` (below) compares the durable codec
  projection of both sides, **decrypted**. Raw ciphertext is never comparable
  between a stored snapshot and a freshly encoded live row (a fresh nonce per
  write in randomized mode; a re-encryption under the new key after a rotation),
  but the plaintext underneath is — so `#[private]` and `#[encrypted]` columns
  are covered there too. Key rotation is handled by the envelope's `key_id`, so a
  retired key still decrypts; only a column whose key is gone entirely drops out
  of the comparison, and the revision hash still covers it.

## Bitemporality

Every revision carries two instants:

- `recorded_at` — **transaction time**: when the database learned the fact. Set
  by the framework from the **database's** clock (`clock_timestamp()` on
  Postgres, `strftime(…, 'now')` on SQLite), read at the point the append has
  already read the record's chain head — so it reflects database ordering rather
  than the writing host's clock, which across a multi-node deployment can skew or
  step. It is additionally clamped so it never precedes the revision before it,
  which makes transaction time **non-decreasing along a chain by construction**;
  `ledger_verify` reports a chain where it is not.
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

Both bounds **filter**; the answer is always the newest revision that survives.
A revision is a full snapshot, so a later one replaces an earlier one outright
rather than sitting beside it on a timeline — valid time says when a revision's
statement *starts* being true. A future-dated revision is invisible until its
instant arrives; a back-dated correction is visible from the instant it claims,
and supersedes what it corrects from then on. Nothing is ever written back to an
existing revision, so the chain stays append-only.

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
| A sequence number is at `i64::MAX` and cannot be followed | `UnusableSeq` |
| The newest revision does not describe the live row | `LiveStateMismatch` |
| The newest revision was deleted (before *or* after the next write) | `MissingRevision` at the absent sequence number |
| The whole chain was erased | `MissingRevision` at sequence 1 |
| Revisions exist but the high-water mark row was deleted | `HighWaterMissing` |
| The high-water mark was rolled back, or revisions appended without it | `HighWaterBehind` |
| The high-water mark names a different revision at the same sequence | `HighWaterMismatch` |
| Transaction time moves backwards along the chain | `RecordedAtRegression` |
| A live row with no chain **and no mark** at all | *Not* a break — see below |

An intact report carries `head_hash`; a broken one carries none.

### Why the live row is read too

A hash chain proves the revisions that are *present* were not edited. It cannot
prove that none is missing from the **end**: lopping the last two revisions off
leaves something internally perfect. Nor can it see a write that reached the
table without appending a revision at all.

So `ledger_verify` also reads the live row and compares it against the head
revision. That closes both gaps, and with them every remaining write path that
can move a ledgered row without the repository's knowledge — a raw `UPDATE`, a
[counter-cache](counter-cache.md) bump on a ledgered parent, a `dependent(...,
on_delete = delete_all)` cascade declared on some *other* repository. None of
those can be refused at compile time (they are declared elsewhere, or maintained
by the framework), but none of them can hide either.

The reads are taken twice: a write landing between them would look exactly like a
divergence, and this routine exists to produce trustworthy accusations. If the
chain head moves under it, the live comparison is skipped rather than reported —
a concurrent write is not tampering.

### Threat model — read this

The chain is **tamper-evident, not tamper-proof**. It detects any mutation,
insertion, deletion or reordering that does not also re-derive every subsequent
hash, plus — via the live-row cross-check — a truncated tail and any write that
bypassed the ledger.

What it cannot see is a *consistent* rewrite. The hashing rule is open source, so
an adversary with write access to the ledger tables can re-derive a whole chain,
adjust the row to match, and re-establish the high-water mark. Nothing stored
inside the same database can prevent that.

One state is deliberately **not** reported: a live row with no revisions *and no
high-water mark*. That is what every existing row looks like on the day you adopt
`ledgered` — ledgering is not retroactive — so accusing it would put a false
positive in front of every adopting deployment. `revisions_checked == 0` on the
report is what makes the empty case visible to a caller that cares.

### The high-water mark

A revision's sequence number used to be allocated purely from the rows that
survive in `_autumn_ledger_revisions`. That left a window an attacker could close
themselves: delete the newest revision, then wait for a *normal* application
write. The append read `N-1`, re-allocated `N`, chained cleanly onto its
predecessor and matched the live row — so both the chain and the cross-check
reported intact, and the deleted state left no trace.

`_autumn_ledger_high_water` closes it. Every append allocates one past the
greater of the chain head and the mark, and the mark lives outside the deletable
revision rows and never decreases. The same attack now allocates `N+1` and leaves
a **permanent gap** at `N` that `ledger_verify` reports as `MissingRevision` —
whenever it runs, not only in the window before the next write. A *wholly* erased
chain is caught the same way: the mark outlives the rows, so an empty chain
beside a surviving mark is reported rather than mistaken for a row that predates
ledgering.

The mark is never *authoritative*, only cross-checked — on the read path **and**
on the write path. `ledger_verify` compares it with the chain in both directions
and reports either side disagreeing; and an append that finds the two in a state
no framework code path can produce **refuses** rather than overwriting the
evidence. That second half matters: without it, deleting a revision and the mark
together and then waiting for ordinary traffic would have the append quietly
re-create both, which is the very laundering this feature exists to stop.

```rust
// Both are cheap, and come from one statement; pin them together.
let head = repo.ledger_head(id).await?;
let mark = repo.ledger_high_water(id).await?;
```

| Tamper with the mark | Reported | Next ordinary write |
|---|---|---|
| Roll `high_seq` back so a truncation looks clean | `HighWaterBehind` | **Allowed** — this is also what a pre-#2323 node in a mixed-version fleet leaves, so the write heals it |
| Rewrite `head_hash` at the head's own sequence number | `HighWaterMismatch` | **Refused** — appending would settle the disagreement in the writer's favour |
| Delete the mark row so the old attack works again | `HighWaterMissing` | **Refused** — the record's writes fail until an operator has looked |
| Push `recorded_at` far into the future | `HighWaterMismatch` | **Refused** past an hour's skew; and while the head revision is present its own hashed instant is the floor, so the mark's cannot steer transaction time at all |

A refusal is a real write outage for that record, and it is meant to be: the
alternative is a silent repair that destroys evidence. The recovery is to run
`ledger_verify`, decide what happened, and — if the mark was lost rather than
attacked — re-apply the high-water migration's backfill, which is idempotent.

What none of this does is close the class. An attacker who can `DELETE` from the
revisions table *and* `UPDATE` the mark to agree with what survives leaves a
state that is internally consistent, and ordinary traffic then continues from it
— the ledger cannot see that from inside the database. It raises the bar rather
than removing the ceiling.

So the ceiling is unchanged: pin the head hash somewhere the database cannot
reach. Treat that as required for an audit posture, not optional.

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
| Calling `purge(id)` | Not generated. `purge` is soft-delete's hard-delete escape hatch — a raw `DELETE FROM` that writes no history at all. `delete_by_id` and `restore` — both of which record a revision — are the whole delete surface. |
| A `dependent(..., on_delete = destroy)` cascade from a **soft**-deleting parent | The ledgered child is soft-deleted and records a revision, like any other delete. |
| The same cascade from a **hard**-deleting parent | Refused at runtime with a typed `LedgerError::HardDeleteCascade`. Neither outcome is available: erasing the child destroys the record its ledger reconstructs, and soft-deleting it leaves a live foreign key pointing at a parent row about to disappear, which the database rejects. The parent's macro cannot see that the child is ledgered — they are separate `#[repository]` invocations — so this is the one guard the first slice cannot make a compile error. Make the parent `soft_delete`, or use `on_delete = nullify`. |
| `#[version_history(sensitive = [...])]` | Rejected: a redacted column cannot be reconstructed, so byte-for-byte as-of fidelity would be unprovable. |
| `no_versioned_record_impl` | Rejected: the ledger snapshots through the generated `VersionedRecord` impl, and a hand-written one is not guaranteed to serialize every column. |
| `retention(...)` / `position(...)` | Already rejected for `versioned = true`: both mutate rows outside the history-writing paths. |

## Multi-tenancy and sharding

A `tenant_scoped` ledgered repository stamps `tenant_id` on every revision and
scopes every ledger read to the active tenant — a read as tenant B never sees
tenant A's revisions, and `ledger_as_of` fails closed to `None`.

`across_tenants()` ledger reads are **rejected**, not widened. A chain is per
`(tenant, record)`, and two tenants' rows may share a record id, so an unscoped
read would interleave their chains into one sequence — 1, 1, 2, 2 — which
`verify` would correctly call `DuplicateSeq` on history nobody touched. Read the
ledger inside a tenant scope instead.

Cross-shard ledger reads are rejected for the same class of reason: per-shard
record ids are ambiguous, so a naive merge would be wrong. Query a specific shard
instead.

## Cost

Each write adds one indexed read — a single statement that fetches the database
clock, the chain head (`ORDER BY seq DESC LIMIT 1`) and the high-water mark
together — plus the revision `INSERT` and the mark's upsert, inside the
transaction the write already opened; a ledgered **delete** adds a fourth
statement, reading back the `deleted_at` the update wrote so the revision
snapshots the post-delete row exactly. Bulk paths (`save_many`,
`upsert_many`, `delete_many`) pay this per row, inside the transaction that
already holds their row locks — a `delete_many` over 1000 ids goes from a couple
of statements to a few thousand. Chunk accordingly, or keep bulk writes off
ledgered entities where throughput matters.

`ledger_head` and `ledger_high_water` come from one statement over two indexed
rows, so pinning both on a schedule is cheap. The mark table takes an in-place
`UPDATE` per ledgered write — the ledger's only non-append-only write — so it
produces one dead tuple per revision in a table whose primary key is its only
index; nothing unusual for autovacuum, but worth knowing at high write rates.
`ledger_revisions`, `ledger_as_of`, `ledger_diff` and `ledger_verify` read a
record's whole chain (there is no pagination in this slice), and `ledger_verify`
additionally reads the live row, the high-water mark, and re-reads the head — the
last of those is the stability gate that keeps a concurrent write from being
mistaken for tampering.

Snapshots store the full row, so a ledgered table's history grows with row width,
not just with the size of each change — the price of O(1) as-of reconstruction
and provable fidelity. Retention and compaction of old revisions are not part of
this slice.

## Limits of this slice

- Single-entity only. Cross-entity "as of" queries that join several ledgered
  entities at one consistent past instant are not supported yet.
- API-level only — no time-slider or history-viewer UI.
- No retention, compaction, or archival of old revisions.
- No distributed or multi-node ledger consensus.
- Postgres and SQLite only.
- No pagination on `ledger_revisions` — a record's whole chain is read at once.
- Writes that reach a ledgered table from outside its own repository — a
  [counter-cache](counter-cache.md) column maintained on a ledgered parent, a
  `dependent(..., on_delete = delete_all | nullify)` cascade declared on another
  repository, a hand-written `UPDATE` — do not append a revision. They cannot be
  refused at compile time (they are declared elsewhere), but `ledger_verify`
  reports each as a `LiveStateMismatch`.
- Transaction time is read from the database *before* the write commits, so a
  revision can still become visible a commit's worth of time after the instant it
  carries. Closing that would need a commit timestamp, which cannot be read
  before the insert that has to hash it.
- The high-water mark raises the bar on a consistent rewrite rather than closing
  it: an attacker who can rewrite the revisions table can usually rewrite the
  mark table too. Anchoring a chain outside the database — a notary, an
  append-only object store — is still the answer, and pinning `ledger_head` is
  the supported form of it.
- A revision written by a **pre-#2323 writer** during a rolling deploy does not
  raise the mark, so `ledger_verify` reports `HighWaterBehind` until that
  record's next write re-establishes it. Migrate, then finish the deploy. A
  record *created* by such a writer is worse: it has a chain and no mark, so the
  next new-code write refuses until the migration's backfill is re-applied.
- A record whose transaction-time floor sits more than an hour ahead of the
  database's own clock refuses further writes rather than clamping up to it.
  Writing behind such a floor would hash a far-future instant into the chain —
  correctly, so `ledger_verify` could not object — while every as-of query
  quietly stopped returning the revision. The bound turns an irreversible silent
  corruption into a loud stop.
- Emptying `_autumn_ledger_revisions` without `_autumn_ledger_high_water` (or the
  reverse) leaves every record reporting tampering. `autumn db scrub` refuses a
  `[framework] purge` naming one without the other.
- The mark table is *forgeable* by anyone who can `INSERT` into it: a mark
  planted on a record that has no revisions makes `ledger_verify` report an
  erased chain that never existed, and the record's first real write then starts
  above sequence 1. That is the same access level that already allows forging
  revisions outright, so it adds no new privilege — but it is a way to
  manufacture a *false* accusation, not only to hide a true one.

## See also

- [Version history](version-history.md) — the column-level change log the ledger
  builds on
- [Audit logging](audit-logging.md) — named business actions
- [Attribute encryption](attribute-encryption.md) — how encrypted columns appear
  in snapshots
- [Soft deletes](soft-delete.md) — the delete surface a ledgered entity keeps
