# Embedded Clustering

Two instances of the same Autumn binary can find each other, agree on who is
running, and share one distributed primitive — a cluster-wide counter — with
**no external coordination service**. No Redis, no Postgres, no etcd, no
ZooKeeper, and no new entry in your dependency tree: the substrate is compiled
into the framework and switched on by a config section.

The properties, stated plainly up front: this is an **opt-in** subsystem
(`[cluster] enabled = false` by default), it is **eventually consistent**
(nodes converge on a shared view; they never agree instantly), it is
**convergent (CRDT)** by construction rather than by timing, and its traffic is
**authenticated (HMAC-SHA256) but unencrypted**. This first slice is a
deliberate **2-node slice**: designed, tested, and documented for exactly two
nodes and exactly one shared primitive.

> **This is not database sharding.** [Horizontal Sharding](sharding.md) also
> uses the word "cluster" — a Redis-Cluster-style 16384-slot map routing
> tenants across `[[database.shards]]` Postgres databases. That is data
> placement across databases. This guide is about *application processes*
> finding each other over the network. The two features share no configuration
> keys, no code, and nothing but the unfortunate word.

## What you get

- **A member view.** Each node knows which nodes it currently believes are
  running, with their advertised address, status, and incarnation.
- **A cluster-wide counter.** `increment()` on one node is observable via
  `get()` on the other, typically within one push interval.
- **A health component.** `cluster:membership` reports the local view on
  `/actuator/health`, and the substrate's own metrics land on
  `/actuator/metrics`.

## What you do not get

- **Not mutual exclusion, not leader election.** For work that must run on one
  replica only, use [Distributed Locks](distributed-locks.md) or the
  [multi-replica scheduler](scheduled-multi-replica.md). The cluster counter
  cannot fence anything.
- **Not durable.** Counter values live in process memory for the lifetime of
  the process (see [Failure semantics](#failure-semantics)).
- **Not encrypted.** Authenticated (HMAC) only — deploy on a trusted network.
- **Not a replacement** for the Redis or Postgres backends behind sessions,
  jobs, channels, or the scheduler. It is an additional option that needs no
  datastore, not a substitute for one.
- **Not tested past two nodes.** The protocol pushes full state to every known
  peer on every interval; three or more nodes is out of scope for this slice.

## Enable it

```toml
# autumn.toml
[cluster]
enabled = true
secret = "…"                          # prefer the env var below
cluster_name = "autumn"
bind_addr = "0.0.0.0:7946"
advertise_addr = "10.0.1.7:7946"
seed_peers = ["10.0.1.8:7946"]
node_id = "web-1"
push_interval_ms = 500
suspicion_timeout_ms = 2500
```

Every key has an environment override, which is how you supply the secret and
the per-instance addresses on a container platform:

```bash
AUTUMN_CLUSTER__ENABLED=true
AUTUMN_CLUSTER__SECRET=…                       # shared by every node
AUTUMN_CLUSTER__CLUSTER_NAME=autumn
AUTUMN_CLUSTER__BIND_ADDR=0.0.0.0:7946
AUTUMN_CLUSTER__ADVERTISE_ADDR=10.0.1.7:7946
AUTUMN_CLUSTER__SEED_PEERS=10.0.1.8:7946       # comma-separated
AUTUMN_CLUSTER__NODE_ID=web-1
AUTUMN_CLUSTER__PUSH_INTERVAL_MS=500
AUTUMN_CLUSTER__SUSPICION_TIMEOUT_MS=2500
```

### Configuration keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch. When false nothing binds, no task spawns, no extension installs. |
| `secret` | secret string | none | Shared HMAC key, at least 16 bytes. A `SecretString`, never logged. |
| `cluster_name` | string | `"autumn"` | Logical cluster name. Covered by the MAC, so two clusters cannot mix. |
| `bind_addr` | socket addr | `"127.0.0.1:0"` | Where the cluster listener binds. Port `0` picks an ephemeral port. |
| `advertise_addr` | socket addr | none | The address peers should dial. Defaults to the bound address. |
| `seed_peers` | list of socket addrs | `[]` | Addresses to dial on startup. CSV in the env form. |
| `node_id` | string | none | Stable node identity. Defaults to a per-boot random id. |
| `push_interval_ms` | u64 | `500` | Base period of the state push, carrying ±20% per-node jitter. |
| `suspicion_timeout_ms` | u64 | `2500` | Silence after which a peer is locally down and leaves the view. |

### Validation

`[cluster]` is validated at boot and a bad section is a startup error, not a
warning:

- `enabled = true` requires `secret`, and the secret must be at least 16 bytes.
  There is no lenient unauthenticated mode.
- `suspicion_timeout_ms` must be at least `3 × push_interval_ms`. Below that
  ratio a single delayed push evicts a healthy peer and the view flaps.
- `push_interval_ms` must be at least `10`.
- `bind_addr`, `advertise_addr`, and every entry of `seed_peers` must parse as
  a `SocketAddr` (`host:port`, IP literal — hostnames are not resolved).
- `node_id` and `cluster_name`, when set, must be non-empty and at most 64
  bytes. Both travel in every frame and are covered by the MAC.
- An unknown key under `[cluster]` is a config error, like every other section.

### Choosing addresses

`bind_addr` is a local bind; `advertise_addr` is what other nodes dial. They
differ whenever the bind is a wildcard or the node sits behind NAT:

- Binding `0.0.0.0:7946` **requires** an explicit `advertise_addr` — `0.0.0.0`
  is not a dialable address, and a peer that learns it can never reach you.
- Leaving `bind_addr` at the default ephemeral port is fine for the node that
  dials out, but a node nobody can dial can still be reached over the
  connection it opened. Give at least one node a fixed port so the other has
  something to seed with.
- `seed_peers` is a dial list, not a membership list. It is used to make first
  contact; after that each node learns peers (and their advertised addresses)
  from the pushed state. Seeding one direction is enough — node B pointed at
  node A produces a two-member view on both.

## Using the counter

The cluster installs a `ClusterHandle` as an app extension when it is enabled,
so the lookup is an `Option` and your handler decides what a disabled cluster
means:

```rust
use autumn_web::cluster::ClusterHandle;
use autumn_web::extract::State;
use autumn_web::prelude::*;

#[post("/sightings")]
async fn record_sighting(State(state): State<AppState>) -> AutumnResult<Json<serde_json::Value>> {
    let cluster = state
        .extension::<ClusterHandle>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("cluster is not enabled"))?;

    // Grow-only, cluster-wide. Applied locally right away, pushed to peers on
    // the next interval.
    let sightings = cluster.counter("boids_sighted");
    sightings.increment();

    Ok(Json(serde_json::json!({
        "node": cluster.node_id(),
        "members": cluster.members().len(),
        "boids_sighted": sightings.get(),
    })))
}
```

| Method | Returns | Notes |
| --- | --- | --- |
| `ClusterHandle::node_id()` | `&str` | This node's id — configured, or the per-boot random one. |
| `ClusterHandle::local_addr()` | `SocketAddr` | The address actually bound (resolved port). |
| `ClusterHandle::members()` | `Vec<ClusterMemberInfo>` | The **local** view, including this node. |
| `ClusterHandle::counter(name)` | `ClusterCounter` | Cheap handle; call it per request. |
| `ClusterCounter::increment()` | `()` | Adds 1 to this node's own entry. Synchronous, never fails. |
| `ClusterCounter::get()` | `u64` | Saturating sum of every entry this node has merged. |

`ClusterMemberInfo` carries `{ id, addr, status, incarnation }`, where `status`
is `Alive` or `Suspect` — a member this node considers down is not in the view
at all.

`get()` may **jump upward** between two calls with no local increment in
between: that is a merge from a peer landing. It never moves downward. Treat it
as a lower bound on the true cluster-wide total, never as a limit to enforce.

## Verify with two terminals

One binary, two processes, one shared secret. Both terminals need the same
secret, so export it first in each:

```bash
export AUTUMN_CLUSTER__ENABLED=true
export AUTUMN_CLUSTER__SECRET="dev-only-cluster-secret-change-me"
```

Terminal A — the node that will be seeded against:

```bash
AUTUMN_SERVER__PORT=3000 \
AUTUMN_CLUSTER__NODE_ID=node-a \
AUTUMN_CLUSTER__BIND_ADDR=127.0.0.1:7946 \
cargo run
```

Terminal B — same binary, different ports, seeded at A:

```bash
AUTUMN_SERVER__PORT=3001 \
AUTUMN_CLUSTER__NODE_ID=node-b \
AUTUMN_CLUSTER__BIND_ADDR=127.0.0.1:7947 \
AUTUMN_CLUSTER__SEED_PEERS=127.0.0.1:7946 \
cargo run
```

Within a couple of push intervals both nodes report the same two members:

```bash
curl -s localhost:3000/actuator/health | jq '.components["cluster:membership"]'
curl -s localhost:3001/actuator/health | jq '.components["cluster:membership"]'
```

```json
{
  "status": "UP",
  "details": {
    "node_id": "node-a",
    "cluster": "autumn",
    "local_addr": "127.0.0.1:7946",
    "member_count": 2,
    "members": [
      { "id": "node-a", "addr": "127.0.0.1:7946", "status": "alive", "incarnation": 1 },
      { "id": "node-b", "addr": "127.0.0.1:7947", "status": "alive", "incarnation": 1 }
    ]
  }
}
```

Increment on A, read on B:

```bash
curl -s -X POST localhost:3000/sightings     # {"boids_sighted":1,…} on node-a
curl -s localhost:3001/sightings             # 1 on node-b within a push interval
```

Now stop node B and watch node A converge to a one-member view. `Ctrl-C` takes
the clean path (B sends a leave, A drops it inside a few hundred milliseconds);
`kill -9` takes the failure path (A drops B after `suspicion_timeout_ms`).
Either way:

```bash
curl -s localhost:3000/actuator/health \
  | jq '.components["cluster:membership"] | {status, count: .details.member_count}'
# { "status": "UP", "count": 1 }
```

A one-member view is **healthy**. The indicator registers in the `HealthOnly`
group (see [Health Indicators](health-indicators.md)), so it never gates
`/ready` and never reports `DOWN` because a peer went away — an orchestrator
cannot be tricked into restarting the survivor.

`/actuator/metrics` exposes the same facts as counters, which is what you
alert on:

| Metric | Kind | Notes |
| --- | --- | --- |
| `autumn_cluster_members` | gauge | Size of the local view. |
| `autumn_cluster_pushes_sent_total` | counter | State pushes written. |
| `autumn_cluster_pushes_received_total` | counter | Frames accepted after verification. |
| `autumn_cluster_merges_applied_total` | counter | Merges that changed local state. |
| `autumn_cluster_frames_rejected_total` | counter | Labelled `reason` — see the receive path below. |

A steady `frames_rejected_total{reason="mac"}` means somebody is talking to
your port with the wrong secret.

## How it works

### The wire format

One TCP listener per node. Connections are byte pipes and nothing more:
**connection state carries zero liveness meaning**. A live connection with no
recent state push is a silent peer; a dropped connection that reconnects
before the suspicion timeout is a non-event.

Each frame is a 4-byte big-endian length prefix followed by exactly that many
bytes of JSON:

```
+-------------------+--------------------------------------+
| u32 big-endian N  | JSON envelope, exactly N bytes       |
+-------------------+--------------------------------------+
```

`N` is capped at **65536 (64 KiB)** and the cap is checked **before any
allocation**. A prefix of `0` or greater than the cap closes that connection
without reading further. The sender applies the same cap and refuses to emit an
oversized frame rather than sending something the peer must reject.

The envelope:

| Field | Type | Meaning |
| --- | --- | --- |
| `v` | u8 | Envelope version. `1` in this slice; any other value is dropped. |
| `key_id` | u8 | Which signing key made the MAC. Always `0`; reserved for key rotation. Anything else is dropped. |
| `cluster` | string | Sender's `cluster_name`. A mismatch with the local name is dropped. |
| `sender` | string | Sender's node id — the *authenticated* identity. The source address is never an identity. |
| `incarnation` | u64 | Sender's incarnation when the frame was produced. |
| `seq` | u64 | Per-sender counter, incremented per frame, reset to `0` when the incarnation increases. |
| `payload` | string | The inner message as a JSON string. Opaque until the MAC verifies. |
| `mac` | string | Lowercase hex HMAC-SHA256, 64 characters. |

The MAC is computed over a **length-delimited** concatenation, so no field
value can be shifted into another field:

```
signing_input = L(v) ‖ L(cluster) ‖ L(sender) ‖ L(incarnation) ‖ L(seq) ‖ L(payload)

L(x) = 8-byte big-endian byte length of x, followed by x
  v            → 1 byte, the u8 value
  cluster      → UTF-8 bytes
  sender       → UTF-8 bytes
  incarnation  → 8 bytes, big-endian
  seq          → 8 bytes, big-endian
  payload      → UTF-8 bytes of the payload string
```

`key_id` is deliberately **outside** the signing input: it selects the key
rather than being protected by it. Flipping it selects a key that does not
exist, the MAC then fails, and the frame is dropped — a tampered `key_id` can
cost an attacker a dropped frame, never a forged one.

### Receive path

The order is normative. Every step that drops does so by logging and
continuing: it never panics, never ends the read loop, and always increments
`frames_rejected_total` with the named reason.

1. **Length prefix** — `N == 0` or `N > 65536` closes the connection with no
   allocation (`reason="oversize"`).
2. **Envelope parse** — malformed JSON, or an envelope missing a field, is
   dropped (`reason="malformed"`). Only the fixed envelope is parsed here.
3. **Header checks** — `v != 1` (`reason="version"`), `key_id != 0`
   (`reason="key_id"`), `cluster != cluster_name` (`reason="cluster"`).
4. **MAC** — recompute over the signing input and compare in **constant time**.
   A mismatch drops the frame (`reason="mac"`). Nothing below this line runs on
   unauthenticated bytes.
5. **Self-origin** — `sender == local node id` is dropped
   (`reason="self_origin"`). This is decided on the authenticated field, never
   on the peer address, so a seed list containing your own address and a
   reflected frame are both handled by the same rule.
6. **Replay watermark** — per sender, the node remembers the highest
   incarnation it has accepted and the highest `seq` accepted at that
   incarnation. A frame is dropped (`reason="replay"`) when its `incarnation`
   is lower than the recorded one, or equal to it with `seq` not greater than
   the recorded `seq`. A higher `incarnation` adopts the new incarnation and
   resets the sequence watermark.
7. **Payload parse** — only now is `payload` deserialized. An unknown message
   type or malformed body is dropped (`reason="payload"`).
8. **Merge** — the message is applied, and receipt is recorded against the
   sender for the liveness overlay.

### Messages

Two message types share the envelope. They are internally tagged so an unknown
future variant is a clean drop rather than a parse failure:

```json
{"type":"state_push","state":{
  "members": {
    "node-a": { "addr": "127.0.0.1:7946", "incarnation": 1, "status": "alive" },
    "node-b": { "addr": "127.0.0.1:7947", "incarnation": 1, "status": "alive" }
  },
  "counters": {
    "boids_sighted": { "node-a": 3, "node-b": 2 }
  }
}}
```

```json
{"type":"leave"}
```

`state_push` carries the entire replicated document and doubles as the
heartbeat — there is no separate ping. `leave` carries no fields at all: it
applies to the `(sender, incarnation)` pair in the authenticated envelope, so a
captured leave can never be replayed against a newer incarnation of that node.

Wire structs tolerate unknown fields and default missing ones, so a newer node
can add fields without breaking an older peer. (This is the opposite of the
config layer, where an unknown key is an error.)

### Membership

Membership is two things that are easy to confuse, so they are separated in the
types:

**Replicated status** is part of the shared document and converges. A
`MemberRecord` is `{ addr, incarnation, status }` where `status` is only
`Alive` or `Left`. Records merge pairwise:

1. Higher `incarnation` wins.
2. At equal incarnation, `Left` beats `Alive` — a leave is never undone by an
   in-flight older push at the same incarnation.
3. At equal incarnation and equal status, the lexicographically greater `addr`
   wins. This tie-break exists only to keep the merge commutative; it should
   never fire in practice.

**Refutation** keeps a live node from being buried. A node that receives a
document marking *itself* `Left` sets its incarnation to one above the highest
it has seen for itself, marks itself `Alive`, and pushes immediately. Because
rule 1 outranks rule 2, that refutation wins everywhere.

**Local liveness** is *not* replicated. Each node times the silence since it
last accepted a frame from each peer, using the injected monotonic clock:

```
   accepted a frame                accepted a frame
        │                                │
        ▼                                ▼
   ┌─────────┐  silence > 2×push   ┌─────────┐  silence > suspicion  ┌──────┐
   │  Alive  │────────────────────▶│ Suspect │──────────────────────▶│ Down │
   └─────────┘                     └─────────┘                       └──────┘
     in view                        in view                        not in view
```

`Suspect` at twice the push interval is a warning, not an eviction: with ±20%
jitter a healthy peer's gap never approaches it, but two missed pushes are
worth showing an operator. `Down` at `suspicion_timeout_ms` removes the member
from the view. Because validation forces `suspicion_timeout_ms ≥ 3 ×
push_interval_ms`, `Suspect` always strictly precedes `Down`.

The view is therefore **local**: replicated `Alive` records, minus peers this
node currently considers down, plus this node itself. Two nodes can briefly
disagree about the view — that is exactly what eventually consistent means
here, and it is why the counter's correctness does not depend on the view.

**Leaving** has a fast path and a correctness path, and only the second one is
a contract. On shutdown a node marks its own record `Left` at its current
incarnation and sends a `leave` over the connections it already has, bounded to
**250 ms** so it always fits inside the shutdown drain budget. That is the fast
path, and it is best-effort. If the process is killed, the network eats the
frame, or the peer is mid-reconnect, the peer still converges — by silence, at
the suspicion timeout. Never build anything on a leave arriving.

Records for departed members stay in the document as tombstones so `Left`
propagates, and are pruned locally after ten suspicion timeouts.

### The counter

`ClusterCounter` is a grow-only CRDT counter, the simplest structure that makes
"observable on the other node" a property of the data rather than of the
network's timing:

- Each counter name maps to a map of `node id → u64`. A node writes **only its
  own entry**, so no two nodes ever write the same cell.
- `increment()` adds 1 to this node's entry, immediately and locally, then
  nudges the push task. Local `get()` reflects it at once.
- Merge is **per-entry maximum**. That is commutative, associative, and
  idempotent, so pushes may arrive out of order, be duplicated, or be dropped
  entirely, and the result is the same once any later push lands.
- `get()` is the **saturating sum** of every entry.

There is no `decrement`. A grow-only counter is what makes the merge total and
the arithmetic auditable; a decrement map is a possible later addition and the
wire format leaves room for it.

**Saturation, not wrapping.** A per-node entry saturates at `u64::MAX` instead
of wrapping, and `get()` saturates the sum. An implausible count clamps; it
never wraps to zero and never panics.

> **Three rules, three mechanisms.** A boid flock keeps formation with
> alignment, cohesion, and separation, and this substrate is built from the
> same three moves under different names. *Alignment* is `ClusterState::merge`
> — each node steers its view toward the view of the neighbours it can see.
> *Cohesion* is the periodic state push to every known peer, which is the only
> thing keeping the flock from drifting apart. *Separation* is the ±20% jitter
> on each node's push interval, which stops identically configured nodes from
> flying in lockstep and colliding on the wire. The real flocking math, with
> actual vectors, lives in [`examples/island-flock`](../../examples/island-flock).

## Failure semantics

**Partition.** Both sides keep accepting increments and keep serving their own
view; neither side is fenced and no write is rejected. During the split each
side reports a total missing the other side's contributions. When the partition
heals, the next push in each direction merges both documents and both nodes
converge on the full sum. Nothing is lost, because the counter only grows.

**Replay.** A captured frame replayed later is dropped by the per-sender
sequence watermark, and a replayed `leave` can only ever name the incarnation
it was signed at. If it names a stale incarnation, it loses the merge; if it
somehow lands, the live node refutes at a higher incarnation and re-appears.
Frames are authenticated but not timestamped, so replay protection is
per-sender-sequence, not clock-based.

**Authenticated (HMAC), unencrypted.** Every frame is authenticated with
HMAC-SHA256 over the shared secret, and unauthenticated bytes never reach a
payload parse. Frames are **not encrypted**: anyone who can read the wire can
read your node ids, advertised addresses, counter names, and counts, and anyone
who can reach the port can make you compute one HMAC per frame. Run the cluster
port on a trusted network — a private VPC subnet, a WireGuard or Tailscale
interface, a container network — and never expose it to the internet. Do not
put anything sensitive in a counter name. Key rotation is not in this slice:
`key_id` is reserved for it, and changing the secret today means restarting
both nodes.

**Leave is advisory, suspicion is the contract.** The 250 ms leave is a
latency optimization for clean shutdowns. Correctness comes from the suspicion
timeout, which handles the kill, the crash, the pulled cable, and the lost
leave identically.

**Counter volatility.** Counters live in process memory for the lifetime of the
process. There is no persistence. A restarted node returns with an empty entry
of its own and re-learns the other entries from its peer, so a rolling restart
of one node at a time preserves the total — but if every node is down at once,
the count is gone. If you need a durable count, write it to the database (see
[Counter Caches](counter-cache.md)) and use this for ephemeral, cluster-wide
tallies.

**State growth.** The document holds one member record and one counter entry
per distinct node id ever seen, plus one map per distinct counter name, and each
node keeps one replay-watermark row per distinct sender it has accepted a frame
from. Member records are pruned after their tombstone window; **counter entries
and watermark rows are not** —
dropping a counter entry would make `get()` move downward, and dropping a
watermark row would re-open a replay window. The consequence is operational:
with no configured `node_id`, every restart mints a fresh random id and leaves
an entry behind forever. Set an explicit, stable `node_id` on any long-lived
deployment, and keep counter names a small fixed set from your code rather than
anything derived from user input. Entry garbage collection is out of scope for
this slice.

**A one-member view is healthy.** `cluster:membership` reports `UP` with one
member, exactly as with two. It is a `HealthOnly` indicator: it never gates
`/ready`, and it never reports `DOWN` because a peer disappeared. A surviving
node keeps serving its counter and its traffic, and a liveness probe must never
be able to kill it for being alone.

**Two nodes.** Every push carries the full document to every known peer, there
are no indirect probes, and there is no quorum anywhere. That is a sound design
at two nodes and an unproven one beyond that. Treat larger fleets as future
work, not as a supported configuration.
