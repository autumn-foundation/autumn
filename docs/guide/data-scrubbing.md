# Data Scrubbing — Refreshing Staging From Production

The most common reason a team copies a production database is to reproduce a bug
or rehearse a migration. With [`autumn db backup`](daemon.md#database-backups)
that copy is one command away — PII and all — onto laptops and shared staging
boxes.

`autumn db scrub` closes that gap. It rewrites every PII-classified column with
deterministic, constraint-valid fake values, so the copy still behaves like the
real database (same row counts, same foreign keys, same uniqueness) while
carrying none of the real values.

The classification is **fail-closed and schema-driven**: the column universe
comes from introspecting the live database, not from a config file, so a column
added yesterday cannot be missing from it. A column that is neither
PII-classified nor explicitly declared safe aborts the scrub.

> **Postgres only.** Scrubbing targets the same databases `autumn migrate`
> resolves — the control database plus every configured shard.

---

## The drill: production → staging in one pass

```sh
# 1. On the production host: take a backup.
AUTUMN_ENV=prod autumn db backup --keep 7

# 2. Move the run directory to the staging host (scp, rsync, or
#    `autumn db backup --upload` + `autumn db restore offsite:prod/latest`).

# 3. On the staging host: restore it and scrub it in one command.
AUTUMN_ENV=staging autumn db scrub \
    --artifact backups/prod/20260101T020000Z --force
```

Step 3 restores the artifact into the database the `staging` profile resolves,
then anonymizes it. `--force` is required because `staging` is not `dev`/`test`
— the same production guard as `autumn db drop`.

To hand a teammate an artifact that is *already* scrubbed, add `--output`:

```sh
AUTUMN_ENV=staging autumn db scrub \
    --artifact backups/prod/20260101T020000Z \
    --output backups/scrubbed --force
```

That writes a fresh, self-describing backup run under `backups/scrubbed/` taken
from the scrubbed database — safe to copy onto a laptop.

You can also scrub a database in place, with no artifact involved:

```sh
autumn db scrub                 # scrub the resolved dev database
autumn db scrub --check         # classify only; write nothing (CI)
autumn db scrub --dry-run       # print the exact SQL; write nothing
```

---

## The classification workflow

Autumn classifies columns from three sources, in precedence order.

### 1. What the framework already knows

Two sources need **no declaration at all**:

- **`#[encrypted]` model columns.** An at-rest-encrypted column is PII by
  construction, so it is always scrubbed — and it is **re-encrypted**, not
  overwritten with a plain string: the replacement is a valid AEAD envelope
  produced row by row under the target database's own key, in the same
  (deterministic or randomized) mode the model declared. Writing plaintext there
  would make every later repository read of that row fail as malformed
  ciphertext, so a `safe` declaration may not override the classification and a
  plaintext strategy is refused outright. `null` is accepted on a nullable
  encrypted column. The scrub needs the target's `active_record_encryption`
  credentials for this and refuses before writing anything if they are missing.
  It also refuses if it cannot tell *which* columns are encrypted — a host with
  the CLI and `scrub.toml` but no `src/models` must name them, with their mode,
  under `[tables.<t>.encrypted]`:

  ```toml
  [tables.users.encrypted]
  api_token = "randomized"
  email = "deterministic"   # the mode matters: re-encrypting a deterministic
                            # column in randomized mode leaves ciphertext the
                            # app can no longer equality-query
  ```
- **Tables registered with the GDPR anonymize strategy** — a
  `GdprRegistry::register(ModelRegistration::anonymize("comments"))` call in your
  app classifies every non-key column of `comments` as PII. Because that is a
  table-level signal, a `safe` declaration **may** narrow it. The scanner reads
  the registration statically, so it needs a string-literal table name; a
  computed one is reported rather than silently ignored.

Primary- and foreign-key columns are never claimed by the table-level
inference: rewriting a key would break referential integrity. Neither are
generated columns, which Postgres refuses to `UPDATE` and which a scrub of their
source columns already covers.

`[defaults] safe_columns` is a cross-table convenience, not a per-column review,
so it does **not** narrow a GDPR anonymize registration — only an explicit
`[tables.<t>] safe` entry does.

### 2. What you declare

Everything else lives in `scrub.toml` at the project root (override with
`--config`):

```toml
# scrub.toml

# Columns that are safe in EVERY table — the one-line escape from repeating
# `id` / `created_at` / `updated_at` in every stanza.
[defaults]
safe_columns = ["id", "created_at", "updated_at"]

[tables.users]
# Reviewed and deliberately kept verbatim.
safe = ["role", "locale", "is_active"]

[tables.users.pii]
email = "email"
full_name = "name"
phone = "phone"
bio = "redact"
last_login_ip = "redact"

[tables.invoices]
safe = ["amount_cents", "currency", "status"]

[tables.invoices.pii]
billing_address = "redact"
```

### 3. Adopting it on an existing schema

You do not have to write that file by hand. Run:

```sh
autumn db scrub --check
```

On a schema with undeclared columns it exits non-zero, lists them, and prints a
paste-ready stanza:

```text
✗ 4 column(s) are neither PII-classified nor declared safe, so the scrub cannot
  prove they carry no real data:
    - users.bio
    - users.email
    - users.full_name
    - users.role
  Declare each one in scrub.toml — under [tables.<table>.pii] to replace it, or
  in `safe` to keep it verbatim. Run `autumn db scrub --check` for a paste-ready
  starting point.

# Paste into scrub.toml, then replace `auto` with an explicit strategy
# (email/name/phone/redact/null/uuid/bytes/json/zero/epoch), or move the
# column into that table's `safe = [...]` list if it holds no PII.

[tables.users.pii]
bio = "auto"
email = "auto"
full_name = "auto"
role = "auto"
```

Paste it, move the genuinely-safe columns into `safe = [...]`, and re-run.

### Keep it honest in CI

```yaml
- name: PII classification is complete
  run: |
    autumn db create
    autumn migrate
    autumn db scrub --check
```

`--check` reads the schema from a live database — the same one `autumn migrate`
resolves — so run it after the migration step your CI already has. It writes
nothing and exits non-zero when any column is unclassified, so a pull request
that adds a column without declaring it fails before it merges.
That is the property no third-party scrubber can offer: the classification is
checked against the real schema, not against yesterday's config file.

### 4. Framework-owned tables

Introspection deliberately skips `autumn_*` / `_autumn*` tables — exactly as
`autumn db pull` and `autumn schema pull` do — so their columns are not part of
the classified universe. Some of them nonetheless carry **app-supplied**
payloads: a queued job's arguments, an offline-sync row buffer, an experiment
assignment.

A scrub therefore *warns* about the ones it finds and tells you which they are.
`api_tokens` is on that list too — production API tokens inherited by a staging
copy are a live credential leak, not merely a PII one.

To have them emptied as part of the scrub, opt in:

```toml
[framework]
purge = ["api_tokens", "autumn_jobs", "autumn_job_tracking", "autumn_sync_pending", "autumn_sync_rows"]
```

Those `DELETE`s run inside the same transaction as the column rewrites. `purge`
only accepts framework-owned names (`autumn_*` / `_autumn*`, plus the
framework's unprefixed tables such as `api_tokens`) — a user table
listed there is an error, because emptying one outright is never something a
scrub should do behind a one-word config key. Schema bookkeeping
(`autumn_migration_checksums`, `_autumn_shard_map`) is never offered.

---

## Sampling: a laptop-sized subset

A scrubbed copy of a 400 GB production database is still 400 GB. `--sample`
emits a **referentially-intact subset** in the same pass, so what you carry away
is small *and* anonymized:

```sh
# 1% of users, plus everything those users relate to, scrubbed.
autumn db scrub --sample users=1%

# An absolute count instead, and two roots in one run.
autumn db scrub --sample users=500 --sample orders=2000
```

The amount applies **per target**: with shards configured, `--sample orders=500`
selects up to 500 rows from each database, not 500 across the topology.

Sampling is a phase of the scrub, never a command of its own: the subset and the
rewrites commit in one transaction, so there is no flag combination that emits
sampled-but-unscrubbed rows.

### How rows are chosen

You name the **roots**. Everything else follows the foreign key graph the
database itself reports:

- **Descend** — rows that reference a selected row are selected too, and they
  carry their own children in turn. That is what "1% of users plus all their
  data" means.
- **Ascend** — rows a selected row references are selected, so every foreign key
  resolves. Those rows are *not* descended from, which is what stops one shared
  parent (an org, a plan) from dragging its whole subtree back in. The other
  children of that org are therefore unreachable, and the run says so rather
  than emptying them.

### Per-table rules

Two rules live alongside the PII declaration, in the same `scrub.toml`:

```toml
[sample]
# Reference data: copied whole, and never descended from.
always_include = ["countries", "currencies", "plans"]
# Excluded entirely — the subset is not the place for an audit trail.
never_include = ["audit_logs", "request_logs"]
```

### Deterministic

Row selection is ordered by a hash of `--seed` and the row's primary key — not
by physical order, and not by `random()`. The same seed against the same source
data selects the identical rows, so a teammate can rebuild the exact subset that
exhibits a bug:

```sh
autumn db scrub --sample users=1% --seed 20260101
```

The seed defaults to `0`, so a run is reproducible whether or not you pass one.

### Fail-closed, same as the classification

Sampling refuses before it deletes anything when it cannot prove the result:

- a table **no root can reach** through the graph (it would be emptied without
  saying so) — name it as a root, or declare it `always_include` /
  `never_include`. Being connected to a root is not enough: the walk descends
  only out of a root and out of the tables it descended into, so a table hanging
  off one it merely *ascended* into (that shared org), or off an
  `always_include` lookup table, is not reachable;
- a foreign key pointing **into** a `never_include` table, which would dangle;
- a table with **no primary key**, which has no row identity to select on;
- a **reference cycle** between tables the sample removes rows from, where those
  removals have no order that keeps every constraint satisfied (copying one of
  them whole with `always_include` takes it out of the removals and breaks the
  cycle);
- a **framework-owned table referencing a sampled one** — those rows are outside
  the sample, so empty them in the same run with `[framework] purge`;
- a **retained table referencing a purged one** — the mirror image. A purge of a
  framework table normally runs before the sample, so that the case above holds;
  when a table the sample *empties* references it, the purge waits until after
  the sample instead. But when a table whose rows the sample *keeps* references
  it, no order works and the run refuses: stop purging that table, or drop the
  referencing one with `never_include`;
- a **purge needed both before and after the sample**: one framework table that
  references a subsetted table (so its purge must run first) *and* is referenced
  by a table the sample empties (so its purge must run last). The only valid
  order interleaves the sample's own deletes around the purge, which one atomic
  sample between two purge passes cannot express;
- a **foreign key declared on a partition** rather than on its partitioned
  parent. The sample plays a partition's rows through that parent, whose rows
  span every partition, so it cannot honour a key binding one partition alone.
  Declare the key on the partitioned parent — Postgres then clones it to each
  partition, and the clone is followed through the parent as usual.

`autumn db scrub --check --sample users=1%` proves the plan is complete and
writes nothing — run it in CI next to the classification check. The foreign key
re-count below is an apply-time check, so `--check` does not run it.

### What it reports

Every run prints per-table row counts, the total against the source, and the
size after the subsetted tables are compacted:

```text
  ℹ Sampling control from users 1%, seed 0 — the same seed against the same source selects the identical rows.
  ── control: sampled rows ──
    users: 1000000 → 10000 row(s) (1.0%, root)
    comments: 4820113 → 48221 row(s) (1.0%, related)
    countries: 249 → 249 row(s) (100.0%, always-include)
    audit_logs: 91002881 → 0 row(s) (0.0%, never-include)
    Total: 96823243 → 58470 row(s) (0.1% of the source), settled in 3 pass(es).
  ✓ 14 foreign key(s) re-verified — every reference in the subset resolves.
    Table size: 402.1 GB → 1.4 GB (0.3% of the source).
```

`[framework] purge` empties its tables **again after the column rewrites**, and
that final pass is the one the guarantee rests on. Earlier passes exist only to
make the sample's deletes possible; a trigger on a scrubbed table — an audit or
history trigger copying `OLD` values — can write fresh rows carrying the original
PII into a purged table while the rewrites run, long after those. (The scrub
still warns when a table it writes to carries user-defined triggers: emptying the
destination cannot help a trigger that writes somewhere the purge list does not
name.)

The foreign key re-check runs **inside** the scrub's transaction, so a violation
rolls the whole run back rather than handing you a broken copy. It counts orphans
per constraint under that constraint's own NULL rule: a partly-NULL composite
reference satisfies the default `MATCH SIMPLE` and is skipped, but violates
`MATCH FULL` and is counted — which matters because the one thing this re-check
adds over Postgres itself is catching a constraint a migration left `NOT VALID`,
where the server never revisits the rows that predate it. Afterwards each
subsetted table is rewritten with `VACUUM (FULL, ANALYZE)` — deleting rows on its
own frees no disk, and the point of a sample is the disk.

Pair it with `--output` to hand a teammate a small, scrubbed artifact:

```sh
AUTUMN_ENV=staging autumn db scrub \
    --artifact backups/prod/20260101T020000Z \
    --sample users=1% --output backups/laptop --force
```

---

## Replacement strategies

| Strategy | Produces | Unique-safe |
|---|---|---|
| `auto` | Derived from the column type (the default for automatically-classified columns) | depends |
| `email` | `scrubbed+<token>@example.invalid` — syntactically valid, permanently undeliverable | ✅ |
| `name` | `Scrubbed <token>` | ✅ |
| `phone` | `+1555<digits>` | ❌ |
| `redact` | `[scrubbed:<token>]` | ✅ |
| `null` | `NULL` (refused on a `NOT NULL` column) | ✅ |
| `uuid` | A deterministic replacement UUID | ✅ |
| `bytes` | Deterministic replacement bytes | ✅ |
| `json` | `{"scrubbed": true}` | ❌ |
| `zero` | `0` / `false` | ❌ |
| `epoch` | `1970-01-01T00:00:00Z` | ❌ |
| `encrypted` | A valid ciphertext envelope (chosen automatically for `#[encrypted]` columns; not declarable) | ✅ |

`<token>` is a hash over the row's primary key, salted with the column name —
one `md5` normally, and two independently-salted ones concatenated for a column
that must stay unique, so a length-bounded `varchar(n)` still has room for
enough entropy.
That makes every replacement:

- **deterministic** — the same row scrubs to the same value on every run, so
  re-running a scrub is idempotent;
- **unique per row** — a `UNIQUE` column stays unique; and
- **distinct per column** — two PII columns of one row never collide.

A table with **no** primary key falls back to the physical `ctid`: still unique
within the statement (so constraints hold), but not stable between runs.

`auto` derives a strategy from the column's type: text columns whose name
contains `email` get an address, other text is redacted, `uuid`/`bytea`/`jsonb`
get type-matched fakes, numbers (including `smallint`, `numeric` and `money`)
become `0`, booleans `false`, and timestamps — plus `date`, `time` and `timetz` —
the epoch. It refuses to guess for a Postgres type outside that set rather than
emit a statement that will fail.

Every strategy is also checked against the column *before* anything is written:
a text-shaped strategy on an `integer` or an `inet`, a `uuid` on a `text`
column, or a `null` on a `NOT NULL` column is a plan-time refusal, never an
apply-time error.

---

## What the scrub guarantees

- **Nothing runs against production by accident.** Writing refuses outside the
  `dev`/`test` profile without `--force`, the same protocol as `autumn db drop`.
  As defence in depth, a scrub also refuses when the write target is the
  database that **any** non-dev/test profile's *config file* declares — every
  `autumn-<profile>.toml` in the project is checked, not just the one an
  artifact's manifest names, so a bare `.dump` with no manifest is not treated
  as permission to continue. That guard has its own waiver,
  `--allow-source-overwrite`, rather than riding on `--force`: the staging drill
  always passes `--force`, so a guard `--force` waived would be inert in exactly
  the workflow it exists for.
- **A refusal writes nothing.** Classification completes before a single row is
  touched. The one exception is `--artifact`: the restore has to run before the
  scrub can read the schema it created, so a refusal *after* a restore leaves
  unscrubbed data in the target. The command says so in the loudest possible
  terms, and `autumn db scrub --check` catches an incomplete declaration before
  any restore happens — run it in CI.
- **Constraints survive.** PII is refused outright on a primary key, on **either
  side** of any foreign key (including every component of a composite one — so a
  natural key another table references, like `users.email` ← `orders.user_email`,
  is protected too), and on a `CHECK`-constrained column, where no fabricated
  value can be proven to satisfy the predicate. `null` is refused on a `NOT NULL`
  column and on a `NULLS NOT DISTINCT` unique index; a constant replacement is
  refused on any column covered by a unique index — composite, partial, and the
  input columns of a unique *expression* index (`(lower(email))`), whose
  uniqueness a per-row token cannot preserve through an arbitrary expression,
  and the *predicate* columns of a partial unique index, where a rewrite changes
  which rows the index covers; and a `varchar(n)` bound narrows the generated token or refuses,
  rather than truncating into collisions.
- **Writes cannot be redirected.** Every statement is `public`-qualified and the
  transaction pins `search_path`, so a role- or database-level tenant
  `search_path` cannot send an `UPDATE` to a table nothing classified. The
  planned tables — and the ones `[framework] purge` empties — are locked for the
  duration, so a row inserted between the classification snapshot and the
  rewrite cannot slip through.
- **`NULL`s stay `NULL`.** A scrub anonymizes values; it never invents them.
- **It is atomic.** Every statement for one database runs in a single
  transaction, so a failure can never leave a half-scrubbed database behind —
  and with shards configured, *every* database is classified before *any* of
  them is written, so an undeclared column on one shard cannot leave the rest of
  the topology half anonymized.
- **Credentials never appear.** No message ever prints a resolved URL.

---

## Limits

- **Postgres only**, and the `public` schema only. A database with base tables
  in another non-system schema is **refused** rather than reported clean over a
  universe the classifier never looked at — and a schema holding only a
  *materialized view* counts, since it keeps its own copy of whatever it
  selected from `public`. The same applies to a table the
  connecting role cannot see: the scrub compares `pg_class` against what it
  could actually read — tables **and** columns, since privileges are granted per
  column too — and refuses on any difference, so a privilege gap can never look
  like a clean bill of health. Foreign tables count as part of that universe: one
  left pointing at production would otherwise be classified by nothing.
- **Row-level security is refused**, for the tables the scrub rewrites *and* the
  ones `[framework] purge` empties. A role that does not bypass RLS would touch
  only the rows its policies expose and report success — a silent partial scrub.
  Connect as the table owner or a `BYPASSRLS` role.
- **Triggers are warned about, not disabled.** An audit or history trigger can
  copy the pre-scrub row into another table as the rewrite runs. The scrub names
  the tables carrying user triggers; check or disable them on the copy.
- **Materialized views are refreshed** (in dependency order, inside the scrub's
  own transaction) since they hold their own copy of whatever they selected — so
  a refresh the role is not allowed to run rolls the rewrites back rather than
  committing base tables a stale view contradicts.
- **A key column holding PII can only be declared `safe`.** A natural key
  (`patients(ssn PRIMARY KEY)`) cannot be anonymized in place without rewriting
  every row that references it, which this command does not do — so it is kept
  verbatim and listed in the report. Restructure the schema or scrub it by hand.
- **A sample is valid, not representative.** `--sample` guarantees the subset is
  referentially intact and PII-free; it does not preserve distributions, and it
  never fabricates rows to pad a small table (that is `autumn seed`'s job).
- **Values are fake, not statistically faithful.** Replacements only need to be
  constraint-valid; there is no synthetic-data modelling or differential
  privacy.
- **Framework-owned tables are not column-classified**, exactly as `autumn db
  pull` and `autumn schema pull` skip them. The scrub warns about the ones that
  carry app-supplied payloads and can empty them on request — see
  [Framework-owned tables](#4-framework-owned-tables).
- **The artifact itself is still unscrubbed.** `autumn db scrub --artifact`
  anonymizes the *database*, not the dump it restored from. Delete the source
  artifact from the staging host once the scrub succeeds, or hand teammates the
  `--output` artifact instead.

## See also

- [Database backups](daemon.md#database-backups) — `autumn db backup` /
  `autumn db restore`, the other half of the drill.
- [Attribute encryption](attribute-encryption.md) — `#[encrypted]`, one of the
  two automatic classification sources.
- [Logging & PII](logging-pii.md) — the log-side scrubber, which is a different
  thing entirely.
- [Seeding](seeding.md) — synthetic data when you do not need production shapes.
