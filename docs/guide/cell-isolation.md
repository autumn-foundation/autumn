# Cell and Shuffle-Shard Isolation

One noisy tenant must not slow down the other tenants. This page shows the
four tools that limit the blast radius of one tenant, one shard or one cell:

1. [Per-tenant bulkheads](#per-tenant-bulkheads) for requests and database
   connections.
2. [Tenant job isolation](#tenant-job-isolation): per-tenant worker slots
   and shuffle-sharded lanes.
3. [Shard-local jobs](#shard-local-jobs): a job table on each shard.
4. [A cell router](#cells-and-the-cell-router) for a deployment of
   identical cells.

[Per-Tenant Memory Cells](tenant-cells.md) is a different feature. It
counts memory. It does not isolate faults.

All the settings on this page are off by default.

## Per-tenant bulkheads

```toml
[tenancy]
enabled = true
max_concurrent_requests = 32   # per tenant; 0 = no limit
max_db_connections = 4         # per tenant; 0 = no limit
```

`max_concurrent_requests` caps the requests of one tenant that are in flight
at the same time. A request over the cap gets `503 Service Unavailable` with
`Retry-After: 1`. The other tenants are not affected. The cap applies after
the tenancy middleware finds the tenant, so public paths and probes are not
counted.

The bulkhead sits outside the server-wide
[admission limit](resilience.md). A request holds a tenant permit and an
admission permit. Thus one tenant can fill at most
`max_concurrent_requests` of the admission limit. Set it below
`server.max_concurrent_requests`.

`max_db_connections` caps the database connections that one tenant's
requests hold at the same time. A checkout over the cap fails with `503`. It
does not wait. The permit goes back when the `Db` drops.

| Variable | Field |
| --- | --- |
| `AUTUMN_TENANCY__MAX_CONCURRENT_REQUESTS` | `tenancy.max_concurrent_requests` |
| `AUTUMN_TENANCY__MAX_DB_CONNECTIONS` | `tenancy.max_db_connections` |

## Tenant job isolation

```toml
[jobs]
workers = 8

[jobs.tenants]
max_concurrent = 2     # jobs of one tenant that run at the same time; 0 = no limit
lanes = 8              # shuffle-shard lanes; 0 = off
lanes_per_tenant = 2   # lanes that serve each tenant
```

A job's tenant is the tenant of the request that enqueued it. With any of
these settings, the `local` backend does three things:

- **Fair order.** Each queue serves its tenants round-robin. A quiet tenant
  waits for one job of each other tenant, not for the full backlog of a
  noisy tenant.
- **Slots.** A tenant runs at most `max_concurrent` jobs at the same time.
- **Lanes.** Worker `i` serves lane `i % lanes`. Each tenant gets
  `lanes_per_tenant` lanes from a stable hash of its id (shuffle sharding).
  With 8 lanes and 2 lanes per tenant, there are 28 lane pairs. A noisy
  tenant fills at most 2 of 8 lanes, and about 1 tenant in 28 has the same
  pair.

Jobs without a tenant are not limited and run on every lane.

These settings apply to the `local` backend. The Postgres, Redis and `SQLite`
backends do not have a tenant column. On Postgres, use
[shard-local jobs](#shard-local-jobs) to keep the jobs of each shard apart.

| Variable | Field |
| --- | --- |
| `AUTUMN_JOBS__TENANTS__MAX_CONCURRENT` | `jobs.tenants.max_concurrent` |
| `AUTUMN_JOBS__TENANTS__LANES` | `jobs.tenants.lanes` |
| `AUTUMN_JOBS__TENANTS__LANES_PER_TENANT` | `jobs.tenants.lanes_per_tenant` |

`autumn_web::bulkhead::shuffle_shard` gives the lanes of a key, if you need
the same assignment in your own code.

## Shard-local jobs

```toml
[jobs]
backend = "postgres"

[jobs.postgres]
shard_local = true
```

With `shard_local`, the Postgres job runtime:

- makes the `autumn_jobs` table on each shard primary at boot, from the same
  migration files as the control database;
- runs workers and a maintenance loop for each shard.

Then `enqueue_in_tx` on a shard connection is atomic with the shard's data
write:

```rust,ignore
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn sign_up(mut db: ShardedDb, args: Welcome) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT the account on this shard, with conn ...

        // The job row is in the same shard transaction.
        autumn_web::job::enqueue_in_tx("send_welcome", &args, conn).await?;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

If the transaction rolls back, the job is not enqueued. If the control
database stops, the shard workers continue. A plain `enqueue` (no connection)
still writes to the control database. See
[ADR 0018](../adr/0018-shard-local-framework-state.md) for the full table of
framework state and where it lives.

| Variable | Field |
| --- | --- |
| `AUTUMN_JOBS__POSTGRES__SHARD_LOCAL` | `jobs.postgres.shard_local` |

## Cells and the cell router

A cell is a full, independent copy of the app: its own replicas, database
shards, Redis and job workers. A thin router in front of the cells sends each
tenant to its cell. A fault in one cell stops only the tenants of that cell.

```text
             +-------------+
 client ---> | cell router |  (stateless; maps tenant -> cell)
             +------+------+
        +-----------+-----------+
        v           v           v
     cell-1      cell-2      cell-3   (identical deployments)
```

`CellRouter` maps a tenant to a cell with the same 16,384-slot hash as
[sharding](sharding.md), so a cell can own whole shards:

```rust
use autumn_web::cell_router::{CellRouter, CellSpec};
use autumn_web::config::SlotSpec;

let router = CellRouter::new(vec![
    CellSpec {
        name: "cell-1".into(),
        base_url: "http://cell-1:3000".into(),
        slots: vec![SlotSpec::Range("0-8191".into())],
    },
    CellSpec {
        name: "cell-2".into(),
        base_url: "http://cell-2:3000".into(),
        slots: vec![SlotSpec::Range("8192-16383".into())],
    },
])
.expect("every slot has one cell");

let url = router.url_for("acme", "/orders?page=2");
```

Leave `slots` empty on every cell to split the slots evenly in order. Use
`for_tenant`, `for_key` or `for_slot` to get the `CellSpec`. Keep the router
stateless: it holds no data, so you can run many copies of it.

Rules for cells:

- Deploy each cell from the same build and the same configuration, except
  its URLs.
- Give each cell its own control database. Then a control-database fault
  stops one cell only.
- Move a tenant between cells as you move slots between shards: copy, verify,
  flip the map, then delete.

## Multi-region

Autumn does not replicate between regions. Use one region as active and one
as passive:

- Run cells in the active region only.
- Keep the passive region warm with database replicas (streaming replication
  of each control database and shard).
- To fail over, promote the replicas, start the cells in the passive region,
  and point the router or DNS at it.

These features are region-local. They do not see the other region:

- the job queue, the scheduler locks and the outbox relay;
- the in-memory idempotency store, the rate-limit buckets and the bulkheads;
- the cache (Moka or a Redis in the same region);
- sessions in memory or in a regional Redis.

Do not run two regions as active on one database: the lease locks and the
job claims assume one primary.
