# Capacity Contracts

Autumn already protects itself against overload *reactively*: bounded-concurrency
load shedding ([ADR-0009](../adr/0009-adopt-overload-protection-load-shedding.md))
returns `503` once too many requests are in flight. But the ceiling it enforces
has always been a hand-tuned guess in `autumn.toml`, and nothing told you what a
given binary could actually sustain before you deployed it.

A **capacity contract** replaces that guess with a measurement. It is a
committed, versioned artifact — `capacity.lock` — that says:

> On this host class, this build sustains **N req/s at P99 ≤ M ms**; past that
> point it admission-controls rather than degrades.

Four things use it:

| Step | Command / setting | What it does |
|---|---|---|
| **Calibrate** | `autumn calibrate` | Measures the envelope, writes `capacity.lock` |
| **Commit** | `git add capacity.lock` | The contract travels with the build, like `Cargo.lock` |
| **Gate** | `autumn calibrate --check` | Fails CI with a diff when a rebuild leaves the envelope |
| **Enforce** | `[server] capacity_contract` | The runtime admits against the proven envelope |

---

## 1. Calibrate

```sh
autumn calibrate
```

The run:

1. Builds your app **in release mode** — a debug-profile number is not something
   to size infrastructure from.
2. Reads the app's route graph back through the same `AUTUMN_DUMP_ROUTES`
   pipeline `autumn routes` uses, recording each route's statically derived
   resource shape (see [Per-route resource shape](#per-route-resource-shape)).
3. Boots the binary on a reserved port **with admission control switched off**,
   so the run measures the app rather than a ceiling it was already carrying.
4. Warms up, then walks a seeded concurrency ladder (`1,2,4,8,16,32,64` by
   default), holding each rung for two seconds.
5. Records the **saturation knee** — the last rung where more concurrency still
   bought materially more throughput — and writes `capacity.lock`.

`saturation_concurrency` is the knee as measured. `admission_limit` is
deliberately **not** the same number: it carries a 2x headroom factor, floored
at the host's logical CPU count. Both adjustments exist because the concurrency
a loopback driver offers is not the concurrency the runtime counts. A load-shed
slot is held from the moment the layer sees a request until the handler's future
resolves — which in production also spans reading the request body off a real
network — so by Little's law the same throughput needs a strictly larger
in-flight count over a WAN than over `127.0.0.1`. Enforcing the raw knee would
shed traffic the binary can actually serve. The floor covers the other
direction: a ladder whose second rung fails to gain (a CPU-quota'd container, a
noisy runner, one route that serialises on a lock) puts the knee at concurrency
1, and a contract licensing a single in-flight request would brown out the
deploy. Headroom errs toward admitting; the measured envelope is still recorded
unscaled.

```toml
version = 1

[provenance]
autumn_version = "0.7.0"
calibrated_at = "2026-09-01T12:04:11Z"
git_commit = "9f2c1ab"
git_dirty = false
route_graph_digest = "sha256:4c1d…"

[host]
os = "linux"
arch = "x86_64"
logical_cpus = 8
total_memory_mb = 16384

[calibration]
seed = 1733
concurrency = [1, 2, 4, 8, 16, 32, 64]
rung_ms = 2000
warmup_ms = 1000

[envelope]
sustained_rps = 4210.5
p99_latency_ms = 18.42
saturation_concurrency = 64
admission_limit = 128

[[routes]]
method = "GET"
path = "/posts"
handler = "posts::index"
shape = "db-bound"
pools = ["db"]
```

Useful flags: `--concurrency 1,4,16,64,256` to reshape the ladder, `--rung-ms`
and `--warmup-ms` to trade run time for stability, `--seed` to change the
request profile, `-p` / `--bin` to pick a target in a workspace.

The workload is recorded in the contract, and `--check` **replays it** rather
than its own defaults:

```toml
[calibration]
seed = 1733
concurrency = [1, 2, 4, 8, 16, 32, 64]
rung_ms = 2000
warmup_ms = 1000
```

That matters more than it looks. An envelope only means something next to the
workload that produced it: gating a contract measured with `--concurrency
1,8,64 --rung-ms 5000` against a rebuild measured with the default ladder
compares two different experiments, and the verdict would be about the flags
rather than the build. Pass a workload flag explicitly to `--check` and it is
honoured, with a warning saying exactly that.

### What gets driven

A calibration run only drives load against routes where doing so measures the
*application*:

- **`GET` only**, so a run never mutates application state.
- **No path parameters**, because a fabricated id measures the 404 path.
- **Not `gated`**, because an unauthenticated run measures the auth rejection.
- **Application routes only** — framework probes are exempt from load shedding
  anyway and would flatter the envelope.

If your app exposes none of these, `autumn calibrate` says so rather than
inventing a number.

---

## 2. Commit

`capacity.lock` belongs in version control next to `autumn.toml`. It is
canonicalized on write — routes sorted, pool lists sorted and deduplicated — so
re-calibrating an unchanged route graph produces a byte-identical route section
and the diff shows only the numbers that actually moved.

---

## 3. Gate in CI

```sh
autumn calibrate --check
```

`--check` re-measures, compares against the committed contract, and exits
non-zero with a human-readable diff:

```
🍂 autumn calibrate --check

  host              linux/x86_64 8 vCPU (committed: linux/x86_64 8 vCPU)
  sustained req/s   4210.5 → 2940.1  (-30.2%, tolerance -15.0%)
  P99 latency (ms)  18.42 → 19.03    (+3.3%, tolerance +25.0%)

✗ this build no longer meets the committed capacity.lock:
    - sustained throughput regressed 30.2% (4210.5 → 2940.1 req/s), beyond the 15.0% tolerance
```

Tolerances default to **15% for throughput** and **25% for P99**, adjustable
with `--tolerance-rps` / `--tolerance-p99`. They are sized to sit between
observed run-to-run noise on a quiet host and a regression worth paging about.
The P99 band also carries **1 ms of absolute slack** on top of the proportional
one: an app whose handlers return a `&'static str` has a sub-millisecond
loopback P99, where 25% is less headroom than a single context switch on a
shared runner consumes, and a gate that fails no-op rebuilds of the very apps
it is most likely pointed at is worse than no gate.

Exit codes distinguish the two ways `--check` can be red: **1** means this build
regressed, **2** means the gate could not judge it (a different host class, or a
committed contract that records no usable envelope). Only the first is a reason
to go optimise something.

The gate is deliberately narrow. It compares **two numbers**, and only when the
rebuild ran on the same host class as the contract. It never compares
timestamps, git provenance, or the contract digest, because a no-op rebuild
changes all three — and a capacity gate that cries wolf gets switched off, which
is strictly worse than not having one.

Route-graph changes are reported alongside the numbers but **never fail the
gate**:

```
  route graph:
    • route added: POST /posts (db-bound [db])
    • route shape changed: GET /search compute-bound → db-bound [db]
```

They are a reason to read the diff, not to block a build that still meets its
envelope.

### Running it on the right machine

A contract is about a host class, not about your code alone. `--check` on a
different class exits non-zero with a distinct diagnosis:

```
✗ host class differs from the committed capacity.lock, so the two envelopes
  are not comparable.
```

Give the capacity gate a dedicated CI lane on a stable runner class. Bolting it
onto a shared, noisy-neighbour lane produces exactly the false positives the
tolerances are designed to avoid.

---

## 4. Enforce at runtime

```toml
# autumn.toml
[server]
capacity_contract = "capacity.lock"
```

With this set — and `max_concurrent_requests` **unset** — the load-shedding
admission ceiling is sourced from the contract's `admission_limit` instead of a
hand-tuned guess, and the binary sheds past it exactly as ADR-0009 describes —
but against a number someone measured rather than guessed. (`admission_limit`
is the measured knee plus headroom, floored at the host's CPU count; see
[Calibrate](#1-calibrate) for why the two numbers differ.)

Precedence, and the reasoning behind it:

1. **An explicit `[server] max_concurrent_requests` always wins**, including an
   explicit `0` (today's spelling of "shedding off"). An operator who set a
   number by hand outranks a file committed months ago.
2. Otherwise the contract, **but only** when it was measured on this host class
   and records a non-zero limit.
3. Otherwise unlimited — today's default, unchanged.

Every failure along the contract path — missing file, malformed document, a
newer schema version, a contract from a different host class — degrades to
**unlimited with a warning**, never to a ceiling. Failing closed here would mean
a typo'd path or a stale lockfile sheds every request on the way up, turning a
capacity feature into an outage. The same reasoning covers a recorded limit of
`0`: it is read as "no limit was proven", never as "shed everything".

The environment-variable spelling is `AUTUMN_SERVER__CAPACITY_CONTRACT`.

---

## Per-route resource shape

The contract records more than an aggregate number: each route carries the
resource *shape* derived statically from the route graph.

| Shape | Meaning |
|---|---|
| `db-bound` | Holds a database connection for the request; concurrency is bounded by pool checkout, not CPU |
| `io-bound` | Holds a non-database external resource (mail, events, presence, notifications) |
| `compute-bound` | Proves no pool — the honest reading of a handler declaring no resource extractor |

The shape is read off the handler's **declared extractors** at macro-expansion
time, so it is proven rather than guessed:

```rust
#[get("/posts")]
async fn index(db: Db) -> Markup { … }     // db-bound, pools = ["db"]

#[get("/about")]
async fn about() -> Markup { … }           // compute-bound
```

Routes generated by `#[repository(api = "…")]` are `db-bound` by construction —
their handlers own the pool checkout internally, so no extractor appears in a
signature a macro could read.

### What the shape does not claim

It is a **provable subset**, never a complete accounting. A pool reached through
an application-held `State` value, or behind a type alias that hides the
extractor's name, is invisible to it and the route reads as `compute-bound`. A
route with no pools is therefore normal, not a defect — the same "provable"
caveat the security dimensions of `autumn routes audit` carry.

The shape narrows *why* an envelope moved. The envelope itself is measured, not
inferred from the shapes.

---

## The guarantee, and its assumptions

**The guarantee.** For a build whose `capacity.lock` was calibrated on this host
class, the recorded `sustained_rps` and `p99_latency_ms` are what the binary
sustained at the saturation knee under the recorded seeded profile, and the
runtime will admit at most `admission_limit` concurrent requests, shedding the
rest with `503` + `Retry-After` rather than degrading past the envelope.

**The assumptions it rests on.** Every one of these is a way the number can stop
describing your production system:

- **Single binary, single host.** The contract says nothing about a fleet.
  Cluster-wide aggregation and autoscaler integration are explicitly out of
  scope for this slice.
- **The host class must match.** CPU count, architecture, and OS are compared;
  memory is recorded but not gated, because it is unavailable on some platforms
  and varies with container accounting. Noisy neighbours on a shared runner are
  not modelled at all.
- **The measured workload is the calibratable subset.** Unauthenticated,
  parameterless `GET` routes. If your traffic is dominated by authenticated
  writes, the envelope describes a workload you do not run.
- **Loopback is not your network.** Calibration drives `127.0.0.1`, so a
  request's in-flight lifetime is essentially handler time, while in production
  it also spans reading the body off the wire. The 2x headroom on
  `admission_limit` is a blunt correction for this, not a measurement of it.
- **The contract is not re-validated against the running route graph.** The
  runtime checks the host class, not whether the app still has the routes the
  envelope was measured against. A `capacity.lock` left uncommitted-to for
  months, against an app that has since grown forty routes, is enforced
  verbatim. Re-calibrate when the shape of the app changes; the `[[routes]]`
  section and `route_graph_digest` are there so the diff shows you when it has.
- **No external dependencies are calibrated.** A run measures your process. If
  your database saturates before your app does, the contract records your app's
  ceiling, not your system's.
- **Calibration is empirical, not symbolic.** This slice is
  static-shape-derivation plus one calibration run, not a worst-case proof. Rust
  is what makes the envelope *stable enough to be worth recording* — no GC
  pauses, no JIT warmup, visible allocation — but the numbers still come from
  measurement.
- **A dirty tree is recorded, not rejected.** `git_dirty = true` in the
  provenance means the contract describes a build nobody else can reproduce.
  Treat it as a draft.

---

## See also

- [ADR-0009 — Overload protection via load shedding](../adr/0009-adopt-overload-protection-load-shedding.md)
- [Cloud-Native Guide](cloud-native.md) — probes, graceful shutdown, and the rest
  of the production posture
- [Dev-Loop Latency Budget](dev-loop-latency.md) — the sibling budget-and-gate
  story for the edit–refresh loop
