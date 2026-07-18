# Bookmarks Sharded

Framework-native horizontal sharding: two Postgres shards, one control
database, two stateless web replicas behind nginx — and **zero sharding
code in the application**. Routing is declared in `autumn.toml` and the
handlers use the `ShardedDb` / `Shards` extractors from the prelude.

## How routing works

```
tenant id ──hash──▶ logical slot (0..16384) ──config map──▶ physical shard
```

The slot count is fixed at 16384 (the same constant Redis Cluster and
Valkey use) — there is nothing to choose or outgrow. The key→slot hash
is deterministic across processes and Autumn versions (both web replicas
always agree). The slot→shard map is configuration:

```toml
[database]
primary_url = "postgres://.../bookmarks_control"   # control role: NOT sharded

[[database.shards]]
name = "shard0"
primary_url = "postgres://.../bookmarks_shard0"
slots = ["0-8191"]

[[database.shards]]
name = "shard1"
primary_url = "postgres://.../bookmarks_shard1"
slots = ["8192-16383"]
```

Resharding means moving whole slots: copy a slot's rows to the new
shard, flip its `slots` entry, deploy. Keys are never rehashed.

## Prerequisites

- Docker and Docker Compose (the stack runs four Postgres containers —
  control, two shard primaries, and a streaming read replica of shard 0 —
  plus two web replicas, nginx, and a one-shot migrator; nothing is needed
  on the host besides Docker)

## Quick Start

```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
docker compose -f examples/bookmarks-sharded/docker-compose.yml up -d --build
```

The `bookmarks-migrate` one-shot job applies migrations to the control
database and then every shard before the web replicas start.

## Walkthrough

Create bookmarks for two tenants — each tenant's rows land on the shard
that owns its slot, and the response says which:

```bash
curl -s -X POST http://localhost:3000/api/bookmarks \
  -H 'Content-Type: application/json' -H 'X-Tenant-Id: acme' \
  -d '{"url":"https://rust-lang.org","title":"Rust","tag":"lang"}'

curl -s -H 'X-Tenant-Id: acme' http://localhost:3000/api/bookmarks
# {"shard":"shard0","bookmarks":[...]}   ← always the same shard for acme

curl -s -H 'X-Tenant-Id: globex' http://localhost:3000/api/bookmarks
# {"shard":"shard1","bookmarks":[]}      ← a different tenant, possibly a different shard
```

Repeat the list call: nginx alternates between the two web replicas, but
the shard never changes — the hash is process-independent. Verify on the
database side:

```bash
docker compose -f examples/bookmarks-sharded/docker-compose.yml \
  exec postgres-shard-0 psql -U autumn -d bookmarks_shard0 \
  -c 'SELECT tenant_id, count(*) FROM bookmarks GROUP BY 1;'
```

Cross-shard fan-out (concurrent, partial-failure friendly):

```bash
curl -s http://localhost:3000/api/stats
# {"shard0":{"slots":8192,"bookmarks":3},"shard1":{"slots":8192,"bookmarks":1}}
```

Per-shard health components and pool metrics:

```bash
curl -s http://localhost:3000/health | jq '.components | keys'
# [..., "db:shard:shard0", "db:shard:shard1"]
curl -s http://localhost:3000/actuator/metrics | jq '.database_shards'
```

### Per-shard read replicas

Shard 0 is configured with a streaming read replica
(`postgres-shard-0-replica`); `autumn-docker.toml` sets its `replica_url`.
Each shard is a full primary/replica topology. To actually read from the
replica, use a replica-aware accessor: the `ShardedReadDb` extractor, or a
`#[repository(tenant_scoped, sharded)]` repo whose read route resolves to the
replica (also reachable via `Shards::read_for`). Plain `ShardedDb` always checks
out the shard **primary** — it does not route SELECTs to the replica — so use it
for writes (and read-your-writes), and `ShardedReadDb` for replica-backed reads.
Either way, shard 1, which has no replica, transparently reads from its own
primary. Confirm replication is live:

```bash
# Replica is in recovery (streaming from the primary):
docker compose -f examples/bookmarks-sharded/docker-compose.yml \
  exec postgres-shard-0-replica psql -U autumn -d bookmarks_shard0 \
  -c 'SELECT pg_is_in_recovery();'   # t

# A write to shard 0's primary shows up on the replica moments later:
curl -s -X POST http://localhost:3000/api/bookmarks \
  -H 'Content-Type: application/json' -H 'X-Tenant-Id: acme' \
  -d '{"url":"https://replicated.example","title":"Replicated","tag":""}'
docker compose -f examples/bookmarks-sharded/docker-compose.yml \
  exec postgres-shard-0-replica psql -U autumn -d bookmarks_shard0 \
  -c "SELECT count(*) FROM bookmarks WHERE tenant_id = 'acme';"
```

### Moving a tenant between shards

[`src/bin/move_slot.rs`](src/bin/move_slot.rs) copies a tenant's rows to a new
shard, verifies counts + a content checksum, and deletes the source rows only
with `--confirm` — the data half of the resharding runbook in
[docs/guide/sharding.md](../../docs/guide/sharding.md):

```bash
cargo run --bin move_slot -- --from <SRC_URL> --to <DST_URL> --tenant acme [--confirm]
```

### Readiness

Stop a shard and watch readiness degrade (and recover):

```bash
docker compose -f examples/bookmarks-sharded/docker-compose.yml stop postgres-shard-1
curl -s http://localhost:3000/api/stats     # shard1 reports an error, shard0 still answers
docker compose -f examples/bookmarks-sharded/docker-compose.yml start postgres-shard-1
```

## What stays on the control database

Framework state is **never sharded**: the `autumn_jobs` queue, Postgres
scheduler advisory locks, sessions, and feature flags all live on the
control role (`database.primary_url`). Only tenant data routes across
shards. There are **no cross-shard transactions** — a write spans exactly
one shard, and `/api/stats` aggregates independent per-shard snapshots.

## Things to try

- `autumn migrate status` prints a per-target table (control + each
  shard); `autumn migrate --shard shard1` targets one shard, and
  `autumn migrate --profile docker` resolves URLs through this overlay.
- Give shard 1 a replica too (mirror the `postgres-shard-0-replica`
  service) — each shard is an independent primary/replica topology.
- Enable the built-in directory router (`.with_directory_shard_router()`)
  or a custom `ShardRouter` (`.with_shard_router(...)`) to pin a hot
  "whale" tenant to a dedicated shard — hash routing balances key count,
  not load.
