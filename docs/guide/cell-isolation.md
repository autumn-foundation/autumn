# Cell and Shuffle-Shard Isolation

One noisy tenant must not slow down the other tenants. This page shows four
tools. Each tool limits the effect of one tenant, one shard or one cell:

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
the tenancy middleware finds the tenant. Public paths and probes are not
counted.

The tenancy middleware runs before the server-wide
[admission limit](resilience.md). A request holds a tenant permit and an
admission permit. Thus one tenant can fill at most
`max_concurrent_requests` of the admission limit. Set it below
`server.max_concurrent_requests`.

`max_db_connections` caps the database connections that one tenant's
requests hold at the same time. Each `Db` or shard checkout takes one
permit. A checkout over the cap fails with `503`. It does not wait. The
permit goes back when the `Db` drops. Set the cap to at least the largest
number of connections that one handler holds at the same time.

The `autumn_tenant_bulkhead_rejections_total{kind="request"}` and
`{kind="db"}` counters count the rejections. They have no tenant label.

The caps count work in the request handler. They do not count:

- a connection that a streaming body or a spawned task takes;
- a connection that does not come from `Db` or a shard extractor.

With `tenancy.source = "header"` or `"subdomain"`, the client selects the
tenant id. Then the cap is per tenant id, not per client. A client can use
many ids, or fill the cap of a different tenant with slow requests. Use an
authenticated source (`session` or `jwt`) and a request timeout with these
caps.

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

A job's tenant is the tenant of the request that enqueued it. When
`max_concurrent` or `lanes` is more than `0`, the `local` backend does three
things:

- **Fair order.** Each queue serves its tenants round-robin. A quiet tenant
  waits for one job of each other tenant, not for the full backlog of a
  noisy tenant.
- **Slots.** A tenant runs at most `max_concurrent` jobs at the same time.
- **Lanes.** Worker `i` serves lane `i % lanes`. Each tenant gets
  `lanes_per_tenant` lanes from a stable hash of its id (shuffle sharding).
  With 8 lanes and 2 lanes per tenant, there are 28 lane pairs. A noisy
  tenant fills at most 2 of 8 lanes. About 1 tenant in 28 has the same pair.

The lane count is at most `jobs.workers`. If you set more lanes, the runtime
uses `jobs.workers` lanes and logs a warning. Thus each lane has a worker.

Jobs without a tenant are not limited. They run on every lane. All of them
share one place in the round-robin order.

These settings apply to the `local` backend. The Postgres, Redis and `SQLite`
backends do not have a tenant column. On Postgres, use
[shard-local jobs](#shard-local-jobs) to keep the jobs of each shard apart.

| Variable | Field |
| --- | --- |
| `AUTUMN_JOBS__TENANTS__MAX_CONCURRENT` | `jobs.tenants.max_concurrent` |
| `AUTUMN_JOBS__TENANTS__LANES` | `jobs.tenants.lanes` |
| `AUTUMN_JOBS__TENANTS__LANES_PER_TENANT` | `jobs.tenants.lanes_per_tenant` |

`autumn_web::bulkhead::shuffle_shard` gives the lanes of a key. Use it to get
the same assignment in your own code.

## Shard-local jobs

```toml
[jobs]
backend = "postgres"

[jobs.postgres]
shard_local = true
```

With `shard_local`, the Postgres job runtime:

- makes the `autumn_jobs` table on each shard primary, from the same
  migration files as the control database;
- runs `jobs.workers` workers and one maintenance loop for each shard.

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
still writes to the control database.

Know these limits before you turn it on:

- The runtime makes the shard tables in the background after boot. An
  `enqueue_in_tx` before that fails. The app's database role needs the
  `CREATE` privilege on each shard.
- Each shard has its own queue slots. Up to `jobs.workers` × (shards + 1)
  jobs run at the same time, and each holds a connection. Size each pool for
  it.
- `#[job]` concurrency limits and uniqueness keys apply in each shard's
  table, not across shards.
- The job dashboard, the job metrics, the queue-depth gauges and the
  retention sweep cover the control database only. Shard runs do not change
  them.
- An in-transaction enqueue does not use the `job_queue` circuit breaker.

See [ADR 0018](../adr/0018-shard-local-framework-state.md) for the table of
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
[sharding](sharding.md). Thus a cell can own whole shards. It needs the `db`
feature (on by default).

```rust
use autumn_web::cell_router::{CellRouter, CellSpec};
use autumn_web::config::SlotSpec;

let router = CellRouter::new(vec![
    CellSpec::new(
        "cell-1",
        "http://cell-1:3000",
        vec![SlotSpec::Range("0-8191".into())],
    ),
    CellSpec::new(
        "cell-2",
        "http://cell-2:3000",
        vec![SlotSpec::Range("8192-16383".into())],
    ),
])
.expect("every slot has one cell");

let url = router.url_for("acme", "/orders?page=2");
```

Give empty `slots` to every cell to split the slots evenly in order. Use
`for_tenant`, `for_key` or `for_slot` to get the `CellSpec`. The router holds
no data. Thus you can run many copies of it.

Rules for cells:

- Deploy each cell from the same build and the same configuration. Only the
  URLs change.
- Give each cell its own control database. Then a control-database fault
  stops one cell only.
- To move a tenant to a different cell:
  1. Copy the data.
  2. Verify the copy.
  3. Change the slot map.
  4. Delete the old data.

## Multi-region

Autumn does not replicate between regions. Use one active region and one
passive region:

- Run cells in the active region only.
- Keep a replica of each control database and each shard in the passive
  region (streaming replication).

To fail over:

1. Promote the replicas.
2. Start the cells in the passive region.
3. Point the router or DNS at the passive region.

These features are region-local. They do not see the other region:

- the job queue, the scheduler locks and the outbox relay;
- the in-memory idempotency store, the rate-limit buckets and the bulkheads;
- the cache (Moka or a Redis in the same region);
- sessions in memory or in a regional Redis.

Do not run two active regions on one database. The lease locks and the job
claims assume one primary.
