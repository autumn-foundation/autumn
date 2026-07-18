# Horizontal Sharding

A single Postgres primary — even with read replicas — eventually becomes
the write bottleneck. Autumn's sharding story routes tenant data across
multiple independent Postgres databases while keeping the application
code shard-unaware: routing is declared in `autumn.toml`, and handlers
use the `ShardedDb` / `Shards` extractors from the prelude.

Sharding composes with the primary/replica story: **each shard is a full
primary + optional replica topology** of its own, with the same
`replica_fallback` semantics you already use on the control role.

## Keys, slots, and shards

```
routing key (tenant id) ──hash──▶ logical slot (0..16384) ──config map──▶ shard
```

Keys never map to physical shards directly. They hash onto a fixed set
of **16384 logical slots**, and each slot is owned by exactly one shard
per the configuration. Two properties fall out of this split:

- **The key→slot hash is a permanent contract.** It is deterministic
  across processes, replicas, and Autumn versions (FNV-1a/splitmix64 —
  never the process-randomized std hasher), so every web replica routes
  the same tenant to the same shard, forever. Golden-vector tests in the
  framework pin it.
- **The slot→shard map is just configuration.** Resharding means moving
  whole slots: copy a slot's rows to the new shard, flip its `slots`
  entry, deploy. Keys are never rehashed.

The slot count is **fixed at 16384** — the same constant Redis Cluster
and Valkey use — and is not configurable. Slots are pure routing-table
entries (no pools or per-slot resources), so the fixed count costs
nothing, and it removes the classic "chose too few partitions on day
one" failure mode: there is no number to pick, nothing to outgrow short
of 16384 physical shards, and every Autumn deployment routes the same
key to the same slot.

## Configuration

```toml
[database]
# The CONTROL role. Framework state lives here and is never sharded.
primary_url = "postgres://db-control/app"

[[database.shards]]
name = "shard0"                              # stable identity for logs/metrics/health/CLI
primary_url = "postgres://db-shard0/app"
slots = ["0-8191"]                           # indices and/or "A-B" inclusive ranges

[[database.shards]]
name = "shard1"
primary_url = "postgres://db-shard1/app"
slots = ["8192-16383"]
replica_url = "postgres://db-shard1-ro/app"  # optional: full topology per shard
replica_fallback = "primary"                 # falls back to [database] defaults
primary_pool_size = 4

[tenancy]
enabled = true
source = "header"        # the tenant id doubles as the shard routing key
```

`slots` is all-or-none: either every shard declares it (the map must
cover `0..16384` exactly once; an explicit empty list marks a drained
shard being decommissioned) or none does, in which case Autumn
auto-splits the slot space into contiguous even ranges **by declaration
order**. The auto-split is convenient to start with, but reordering
entries then moves data — pin explicit `slots` before any topology
change.

Environment overrides address shards positionally:

| Variable | Field |
|----------|-------|
| `AUTUMN_DATABASE__SHARDS__{i}__NAME` | `database.shards[i].name` |
| `AUTUMN_DATABASE__SHARDS__{i}__PRIMARY_URL` | `database.shards[i].primary_url` |
| `AUTUMN_DATABASE__SHARDS__{i}__SLOTS` | CSV, e.g. `"0-8191,16000,16382-16383"` |
| `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_URL` | `database.shards[i].replica_url` |
| `AUTUMN_DATABASE__SHARDS__{i}__PRIMARY_POOL_SIZE` | `database.shards[i].primary_pool_size` |
| `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_POOL_SIZE` | `database.shards[i].replica_pool_size` |
| `AUTUMN_DATABASE__SHARDS__{i}__REPLICA_FALLBACK` | `database.shards[i].replica_fallback` |

Pool sizes multiply across shards (N shards × pool_size + control +
replicas). Startup logs the aggregate as `total_max_connections` —
check it against your Postgres `max_connections` budget.

## Handlers

`ShardedDb` resolves the routing key automatically (a `ShardKeyOverride`
request extension, then the tenancy task-local, then direct tenant
extraction per `[tenancy]`), routes it, and checks out a connection to
the owning shard's primary. It dereferences to `AsyncPgConnection`
exactly like `Db`, and `tx()` has the same semantics — on that one
shard:

```rust
use autumn_web::prelude::*;

#[get("/bookmarks")]
async fn list(tenant: Tenant, mut db: ShardedDb) -> AutumnResult<Json<Vec<Bookmark>>> {
    // Several tenants share a shard: still filter by tenant.
    let rows = bookmarks::table
        .filter(bookmarks::tenant_id.eq(&tenant.0))
        .load::<Bookmark>(&mut *db)
        .await?;
    Ok(Json(rows))
}
```

`Shards` is the explicit API — extract once, route per call:

```rust
#[get("/users/{user_id}/report")]
async fn report(shards: Shards, Path(user_id): Path<i64>) -> AutumnResult<String> {
    let mut db = shards.db_for(user_id).await?;      // primary of the owning shard
    let mut ro = shards.read_for(user_id).await?;    // replica-aware read connection
    let mut admin = shards.db_on("shard0").await?;   // by name (admin paths)
    // ...
    Ok("ok".into())
}
```

Cross-shard fan-out runs concurrently (bounded) and collects per-shard
results in declaration order instead of short-circuiting, so one down
shard degrades an aggregate endpoint to a partial answer:

```rust
let counts = shards
    .each_shard(|shard, mut db| {
        let name = shard.name().to_owned();
        async move {
            let count = bookmarks::table.count().get_result::<i64>(&mut *db).await?;
            Ok((name, count))
        }
    })
    .await; // Vec<(ShardId, Result<(String, i64)>)>
```

### Cross-shard reads from a generated repository (`CrossShard`)

A `#[repository(tenant_scoped, sharded)]` repository fans out across every
shard when you call `repo.across_tenants().find_all()` / `count()` and the
derived reads. The normal repository extractor resolves a tenant to route to
one shard, so a **cross-tenant admin** endpoint — which has no tenant header
or task-local — would be rejected during extraction. Extract `CrossShard<R>`
instead: it loads the full shard set without a tenant and hands you a
repository already in `across_tenants()` mode.

```rust
#[get("/admin/bookmarks")]
async fn admin_list(
    CrossShard(repo): CrossShard<PgBookmarkRepository>,
) -> AutumnResult<Json<Vec<Bookmark>>> {
    // Fans out across all shards; per-shard DB metrics/slow-query logs are
    // attributed to the shard that runs each query.
    Ok(Json(repo.find_all().await?))
}
```

Reads fan out; **writes are rejected** (there are no cross-shard writes — see
below). Gate these endpoints with your admin authorization.

Repositories generated by `#[repository]` extract over the **control**
pool. To run one against a shard, use the generated `from_shard`
constructor — it takes a [`ShardedDb`] extractor and preserves the full
request instrumentation (statement timeout, slow-query threshold, route
metric label):

```rust
#[post("/bookmarks")]
async fn create(db: ShardedDb, Json(body): Json<Body>) -> AutumnResult<Json<Bookmark>> {
    let repo = PgBookmarkRepository::from_shard(&db);
    let bookmark = repo.save(&body.into()).await?;
    Ok(Json(bookmark))
}
```

A `from_shard` repository routes its read-only methods (`find_*`, `count`,
`paginate`, …) to the shard's **read replica** automatically when one is
configured and healthy — the same transparent read scale-out as the
control replica, now per shard. Mutating methods always run on the shard
primary. The decision honors that shard's `replica_fallback` policy and
replica readiness, so adding a `replica_url` to a shard doubles its read
capacity with no handler changes. Pin a read-after-write-sensitive
repository to the shard primary with `#[repository(Model, primary_reads)]`,
or a single call chain with `repo.on_primary()`.

If you need a repository over an explicit pool **without** request context
(e.g. a background job that resolved a shard pool from state), use
`with_pool_untracked`. Statement timeout, slow-query threshold, and route
labels are reset to framework defaults — use this only when there is no
`ShardedDb` available:

```rust
let shard = shards.set().route(&tenant_id).await?;
let repo = PgBookmarkRepository::with_pool_untracked(shard.primary_pool().clone());
```

A `#[repository(tenant_scoped, sharded)]` repository routes automatically:
the extractor resolves the tenant and connects to the owning shard, while
`across_tenants()` (or the [`CrossShard`](#cross-shard-reads-from-a-generated-repository-crossshard)
extractor for tenant-free admin endpoints) fans the reads out across every
shard.

## There are no cross-shard transactions

Full stop. A transaction spans exactly one shard. A job that writes to
two tenants' shards can fail halfway; `each_shard` reads can observe
torn aggregates while writers are active. This is the design, not a gap
to fill later — giving up multi-object atomicity across partition
boundaries is what buys independent failure domains and linear write
scaling. Design workflows so each unit of work touches one shard, and
treat cross-shard aggregates as approximate.

## The control database

Framework state is **never sharded**. The `autumn_jobs` queue, Postgres
scheduler advisory locks, sessions, feature flags, and idempotency keys
all live on the control topology (`database.primary_url`/`url`), and the
plain `Db` extractor still points there. Startup fails fast if you
configure shards plus a Postgres-backed jobs/scheduler backend without a
control role.

Jobs that operate on sharded data should carry the shard key in their
payload and resolve the shard from state:

```rust
#[job]
async fn reindex_tenant(state: State<AppState>, payload: Json<Reindex>) -> AutumnResult<()> {
    let shard = state.shards().expect("sharded app").route(&payload.tenant_id).await?;
    let pool = shard.primary_pool().clone();
    // ...
    Ok(())
}
```

## Migrations

Startup auto-migrate (dev) and `autumn migrate` (prod) apply your
migrations to the control database first, then every shard in
declaration order, **failing fast** with a `target=` label on every log
line — a half-migrated fleet that boots is worse than a crashed deploy,
and already-migrated targets are skipped idempotently on retry.

```bash
autumn migrate                  # control + every shard, per-target summary
autumn migrate --shard shard1   # one shard
autumn migrate --control-only   # pre-shards behavior
autumn migrate status           # per-target pending/applied table
```

## Health, readiness, and metrics

Each shard registers a `db:shard:<name>` component in `/ready` and
`/actuator/health`. On every readiness probe it live-checks replica
connectivity and re-runs the migration parity comparison, gating that
shard's replica reads exactly like the control replica:

- replica unready + `replica_fallback = "fail_readiness"` → component
  `Down`, `/ready` fails, `read_for()` errors;
- replica unready + `replica_fallback = "primary"` → component stays
  `Up`, reads degrade to the shard's primary, replica state in details.

`/actuator/metrics` exposes a `database_shards` block with per-shard
primary/replica pool stats and slot counts, and shard-routed checkouts
tag their route metrics with `shard=<name>` so per-shard latency
separates.

## Custom routing and whale tenants

Hash routing balances **key count, not load** — and at tenant
granularity, a single hot "whale" tenant cannot be split. This is the
most common way multi-tenant sharding breaks in practice. Two escape
hatches:

1. Give the whale's slot range its own physical shard (slots are the
   unit of placement).
2. Install a directory router that pins specific tenants and falls back
   to the hash for everyone else:

```rust
use autumn_web::sharding::{ShardId, ShardKey, ShardRouter, ShardSet};

struct PinnedRouter;

impl ShardRouter for PinnedRouter {
    fn route<'a>(
        &'a self,
        key: ShardKey<'a>,
        shards: &'a ShardSet,
    ) -> futures::future::BoxFuture<'a, Result<ShardId, autumn_web::AutumnError>> {
        Box::pin(async move {
            if let ShardKey::Str("whale-corp") = key {
                // Dedicated shard for the hot tenant.
                return Ok(shards.by_name("whale").expect("configured").id());
            }
            let slot = shards.slot_for_key(key);
            Ok(shards.shard_for_slot(slot).expect("slot map covers all slots").id())
        })
    }
}

// autumn_web::app().with_shard_router(PinnedRouter)
```

Routing is async, so a directory router may consult a cache or the
control database. Custom pool construction per shard goes through
`DatabasePoolProvider::create_shard_topology`.

### Built-in directory router

You usually don't need to hand-write the router above. The framework ships
`DirectoryShardRouter`, which reads a control-plane `_autumn_shard_directory`
table (`tenant_key → shard_name`), caches results with a TTL, and falls back
to the hash router for any tenant without a row:

```rust
// Apply the framework migrations to the control DB first (creates
// _autumn_shard_directory), then opt in:
autumn_web::app().with_directory_shard_router()
```

Pin a tenant by inserting (or updating) a row:

```sql
INSERT INTO _autumn_shard_directory (tenant_key, shard_name)
VALUES ('whale-corp', 'whale')
ON CONFLICT (tenant_key) DO UPDATE SET shard_name = EXCLUDED.shard_name;
```

You don't need to invalidate the cache by hand. A trigger on the directory
table fires `NOTIFY` on every change (including this raw SQL write), and the
built-in path spawns a background listener that `LISTEN`s on that channel and
evicts the affected tenant's cached mapping on **every** replica. Because
Postgres delivers `NOTIFY` at commit, the eviction lands the moment the new
mapping becomes visible — a re-pin during a slot move takes effect immediately,
well before the cache TTL (default 30s) would expire it, and a slow-committing
transaction can't be missed. The TTL remains the backstop if the control DB /
LISTEN connection is briefly unreachable.

Building your own `DirectoryShardRouter` and installing it via
`with_shard_router` instead? Wrap it in an `Arc` **once** and share clones, so
the listener and the installed router invalidate the *same* cache
(`Arc<DirectoryShardRouter>` implements `ShardRouter`):

```rust
let router = std::sync::Arc::new(DirectoryShardRouter::new(control_pool));
// Listener and router share one cache:
DirectoryShardRouter::spawn_invalidation_listener(
    std::sync::Arc::clone(&router),
    control_url,
    sweep_interval,
);
app.with_shard_router(std::sync::Arc::clone(&router));
```

Spawning the listener on a *separate* `DirectoryShardRouter` only invalidates
that instance's cache, leaving the installed router's routing stale until the
TTL. If you skip the listener entirely, the TTL is your only refresh.

The directory is the routing complement to the slot-move runbook below: move
the data, pin the tenant to its new shard, and unpinned tenants keep hashing
as before. (You can also enable it via `database.directory_shard_router =
true`; an explicit `with_shard_router` always takes precedence.)

> **Control pool sizing.** Directory routing resolves the tenant→shard key by
> checking out a second control connection during extraction. A handler that
> already holds a control connection (e.g. extracts `Db` and then `ShardedDb`
> or a sharded repository) would deadlock on a control pool sized to **1** — the
> first checkout can't be released until the handler runs. Startup therefore
> requires the control pool to allow **at least 2** connections when directory
> routing is enabled and fails fast with a clear error otherwise. Size
> `database.pool.max_size` accordingly.

## Read-your-own-writes, × N

Each shard inherits the replica story's read-your-own-writes gap:
`read_for(key)` immediately after `db_for(key)` is the textbook
stale-read sequence, now on every shard independently. Use the shard's
primary (`db_for`) for read-after-write paths, exactly as you would with
the control replica.

## Resharding runbook

1. Pin explicit `slots` on every shard (if you started with the
   auto-split) and deploy — this changes nothing, it just makes the map
   explicit.
2. Stand up the new shard's database and add its `[[database.shards]]`
   entry with `slots = []` (a valid drained shard). Deploy; run
   `autumn migrate --shard <new>`.
3. For each slot to move: copy that slot's rows (`WHERE` on your tenant
   key filtered through `ShardSet::slot_for_key`, or dual-write at the
   application layer), then move the slot index from the old shard's
   `slots` to the new shard's and deploy. Writes for that slot now land
   on the new shard.
4. Delete the moved rows from the old shard once traffic confirms.

Moving a slot moves only that slot's keys — the framework's tests pin
this property (`moving_a_slot_in_config_moves_only_that_slot`).

### Worked example: moving a tenant's data

[`examples/bookmarks-sharded/src/bin/move_slot.rs`](../../examples/bookmarks-sharded/src/bin/move_slot.rs)
is a runnable, commented implementation of step 3's data copy. For a set of
tenant keys it:

1. copies their rows source → destination inside a single transaction on the
   destination (the shard-local `BIGSERIAL` id is not copied, so re-runs never
   collide on the primary key),
2. verifies the move — row counts **and** an id-independent content checksum
   must match on both shards,
3. deletes the source rows only with `--confirm`, and only after verification
   passes.

```bash
# Copy + verify only (source rows kept) — inspect both shards first:
cargo run --bin move_slot -- \
  --from postgres://autumn:autumn@localhost:5443/bookmarks_shard0 \
  --to   postgres://autumn:autumn@localhost:5444/bookmarks_shard1 \
  --tenant acme

# Re-route acme to the destination, then delete the stale source rows:
cargo run --bin move_slot -- --from … --to … --tenant acme --confirm
```

The script never edits routing — copy and verify, re-route the tenant, then
delete.

> **⚠️ A hash slot is shared by every key that hashes to it.** This tool copies
> only the rows of the tenant key(s) you name. If you move a *single* tenant and
> then remap its slot in `autumn.toml`, every co-tenant in that slot is rerouted
> to the destination too — but their rows were never copied, so their data
> effectively disappears. To move one tenant, pin it with the
> [`DirectoryShardRouter`](#custom-routing-and-whale-tenants)
> (`database.directory_shard_router = true`) instead of remapping a slot. Only
> remap a hash slot when you have copied **every** key in that slot.

### `autumn shard move-slot`

The same copy → verify → `--confirm` delete flow ships as a framework command
that resolves `--from` / `--to` by their configured shard names (honoring
`--profile` and env, like `autumn migrate`) and works on any table:

```bash
# Copy + verify only (source rows kept):
autumn shard move-slot --from shard0 --to shard1 \
  --table bookmarks --tenant acme

# Re-route acme to shard1 (pin it in the directory router) and deploy, then
# delete the stale source rows:
autumn shard move-slot --from shard0 --to shard1 \
  --table bookmarks --tenant acme --confirm
```

It copies every column (so references to a moved row stay valid), verifies row
counts and a `to_jsonb` content checksum on both shards, advances the
destination's PK sequence after a verified copy (configurable with
`--id-column`, default `id`) so the next insert there won't collide with a
copied id, and only deletes from the source with `--confirm`. Like the example,
it never edits routing — and the same hash-slot caveat applies: to move a single
tenant, pin it with the `DirectoryShardRouter` rather than remapping a shared
slot. Requires the `psql` client on `PATH`.

## Testing

Deadpool builds pools lazily, so sharded states are constructible in
tests without N running databases. The transactional test harness wraps
only the control pool today: sharded checkouts in tests run against
real (non-rolled-back) connections.

See [`examples/bookmarks-sharded`](../../examples/bookmarks-sharded)
for a runnable Docker Compose stack: two shards, a control database,
two web replicas behind nginx, and a one-shot multi-target migrator.
