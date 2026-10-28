# Request Cost and Carbon-Aware Deferral

Autumn can measure what each request, job run and scheduled tick costs. It
adds the cost to the total of the tenant that caused it. It can also delay
background work while a cost signal (carbon intensity or price) is high. It
measures how much of that work moved out of the *window*: the time while the
signal is above `defer_threshold`.

This guide covers the first two slices of issue #1720.

---

## What it measures

For each request, the `CostLayer` records:

| Value | How it is measured |
|---|---|
| CPU time | The thread CPU clock, read before and after each poll of the request. Linux, Android, macOS, iOS and FreeBSD use `CLOCK_THREAD_CPUTIME_ID`. Other targets use the wall time of the poll. |
| Allocated bytes | An `AllocationProbe` that you supply. Without a probe, the value is `0`. |
| DB queries | The same query count as the `Server-Timing` `db` metric. |
| Tenant | The tenant that the tenancy middleware resolved. A request with no tenant goes to `_none`. |

A request that the server drops before it completes (the client went away)
is recorded with the cost that the layer measured until then.

The layer does not count:

- work that the handler moves to another task (`tokio::spawn`, `spawn_blocking`),
- the time to stream the response body,
- the layers outside it (compression, session, metrics).

### Background work

With metering on, the runtime also measures each job run (on all jobs
backends) and each scheduled tick, with the same method. A retry is a new run.

| Work | Tenant key |
|---|---|
| Job on the `local` backend | The tenant of the request that enqueued the job. |
| Job on `postgres`, `redis` or `sqlite` | `_none`. The tenant is not stored in the queue. |
| Scheduled tick | `_none`. |

Autumn uses the tenant only for cost. The job handler does not run in that
tenant's scope. A job that a request enqueues after its transaction commits
also goes to that tenant.

If a run stops early (lease timeout, shutdown), the runtime records the cost
that it measured until then. The cost of a run includes the framework work
around the handler (job tracking, failure capsules).

---

## Turn on cost measurement

```toml
[cost]
enabled = true
max_tenants = 1000   # more tenants go into "_other"
```

Or set `AUTUMN_COST__ENABLED=true`.

Cost measurement is off by default. When it is on, each request reads the
thread CPU clock two times per poll.

To check the overhead on your machine, run the p99 probe:

```bash
cargo test -p autumn-web --release --test cost_reconcile -- --ignored
```

It sends requests that each use 1 ms of CPU, with and without metering. It
fails when metering adds more than 2% to p99.

### Tenant keys

The tenant key is the tenant id. With `[tenancy] source = "header"` or
`"subdomain"`, the client chooses the id. To keep memory bounded, these ids go
into `_other`:

- an id longer than 64 bytes,
- the reserved ids `_none` and `_other`,
- a new id when the accountant already has `max_tenants` tenant keys.

`_none` and `_other` do not count toward `max_tenants`, so requests with no
tenant always go to `_none`. The accountant does not remove a key until the app
restarts.

A response that the router serves before tenancy runs, such as a static-first
(SSG/ISR) page or a startup `503`, is counted under `_none`.

---

## Read the cost

### `Server-Timing`

When `[observability] server_timing` is on, each response gets these metrics:

```text
Server-Timing: cost-cpu;dur=1.234, cost-db;desc="3 queries"
```

The layer adds `cost-alloc;desc="4096 bytes"` when a probe is set. The header
follows the `server_timing` setting, so cost data does not go to clients in
production unless you turn that setting on. `cost-cpu` is a precise timing
signal. Do not show it to anonymous clients.

### Metrics

The accountant is the `autumn.cost` metrics source. `/actuator/prometheus` and
`/actuator/metrics` show these counters:

- `autumn_cost_requests_total`
- `autumn_cost_cpu_seconds_total`
- `autumn_cost_allocated_bytes_total`
- `autumn_cost_db_queries_total`

For job runs and scheduled ticks, these counters have a `kind` label (`job` or
`task`):

- `autumn_cost_work_runs_total`
- `autumn_cost_work_cpu_seconds_total`
- `autumn_cost_work_allocated_bytes_total`
- `autumn_cost_work_db_queries_total`

For deferrable runs, these counters have a `kind` label and a `window` label
(`shifted` or `in_window`). See [Measure the shift](#measure-the-shift).

- `autumn_cost_deferrable_runs_total`
- `autumn_cost_deferrable_cpu_seconds_total`

These two endpoints are public. With `[actuator] sensitive = false`, each
request counter has one sample. Each work counter has one sample for each
`kind`. With `sensitive = true`, the request and work counters have one sample
for each tenant, with a `tenant` label. The deferrable counters have one sample
for each `kind` and `window`, and never a `tenant` label.

### `GET /actuator/cost`

This endpoint shows the signal and the total for each tenant. It needs
`[actuator] sensitive = true`, because tenant ids are not public.

```json
{
  "enabled": true,
  "signal": { "value": 520.0, "threshold": 400.0, "high": true, "deferrals": 2 },
  "total":  { "requests": 3, "cpu_micros": 9120, "allocated_bytes": 0, "db_queries": 6 },
  "tenants": {
    "acme":   { "requests": 2, "cpu_micros": 6080, "allocated_bytes": 0, "db_queries": 4 },
    "globex": { "requests": 1, "cpu_micros": 3040, "allocated_bytes": 0, "db_queries": 2 }
  },
  "jobs": {
    "total":   { "runs": 4, "cpu_micros": 40100, "allocated_bytes": 0, "db_queries": 8 },
    "tenants": { "acme": { "runs": 4, "cpu_micros": 40100, "allocated_bytes": 0, "db_queries": 8 } },
    "shift": {
      "shifted_runs": 3, "shifted_cpu_micros": 36000,
      "in_window_runs": 0, "in_window_cpu_micros": 0,
      "ratio": 1.0
    }
  },
  "tasks": {
    "total":   { "runs": 0, "cpu_micros": 0, "allocated_bytes": 0, "db_queries": 0 },
    "tenants": {},
    "shift": {
      "shifted_runs": 0, "shifted_cpu_micros": 0,
      "in_window_runs": 0, "in_window_cpu_micros": 0,
      "ratio": null
    }
  }
}
```

`total` and `tenants` are requests. `jobs` and `tasks` are background runs.

### In code

```rust
use autumn_web::cost::CostAccountant;

if let Some(accountant) = state.extension::<CostAccountant>() {
    let acme = accountant.tenant("acme");
    let jobs = accountant.snapshot().jobs;
}
```

The accountant keeps the totals in memory, for each process. It does not
remove them.

---

## Count allocated bytes

The framework does not install a global allocator. To count bytes, install a
counting allocator in your app and give Autumn a probe.

1. Add a counting allocator to your app, for example `allocation-counter`.
2. Write the probe:

   ```rust
   use autumn_web::cost::AllocationProbe;

   struct Counting;

   impl AllocationProbe for Counting {
       fn measure(&self, poll: &mut dyn FnMut()) -> u64 {
           allocation_counter::measure(poll).bytes_total
       }
   }
   ```

3. Insert it in a state initializer:

   ```rust
   use std::sync::Arc;

   app.state_initializer(|state| {
       let probe: Arc<dyn AllocationProbe> = Arc::new(Counting);
       state.insert_extension(probe);
   })
   ```

The probe runs one time for each poll. Make it cheap.

---

## The cost signal

`CostSignal` holds a live value and a threshold. The unit is your choice: grid
carbon intensity in g/kWh, or a price per unit. The signal is *high* when the
value is above the threshold.

The framework always puts a `CostSignal` in the app state. Its threshold comes
from `[cost] defer_threshold`. Without a threshold, the signal is never high.

### Set it from code

```rust
use autumn_web::cost::CostSignal;

if let Some(signal) = state.extension::<CostSignal>() {
    signal.set(520.0);
}
```

Use this to connect your own carbon or price feed.

### Set it without a redeploy

1. Declare the framework key in your runtime-config registry:

   ```rust
   registry.define_cost_signal()?;
   ```

2. Insert the service as an `Arc<RuntimeConfigService>` extension:

   ```rust
   let service = Arc::new(RuntimeConfigService::new(registry, store));
   app.state_initializer(move |state| state.insert_extension(service));
   ```

3. Change the value:

   ```bash
   autumn config set autumn_cost_signal 520 --actor ops@example.com
   ```

The app reads the key every `signal_refresh_secs` (default `5`). The store can
already hold a high value when the app starts, so with a `defer_threshold`,
deferrable work waits until the first read succeeds. `signal.pending` in
`/actuator/cost` is `true` until then.

---

## Defer background work

Mark the work as `deferrable`:

```rust
#[job(deferrable)]
async fn rebuild_search_index(state: AppState, args: RebuildArgs) -> AutumnResult<()> {
    Ok(())
}

#[scheduled(every = "15m", deferrable)]
async fn compact_archives(state: AppState) -> AutumnResult<()> {
    Ok(())
}
```

Set the threshold:

```toml
[cost]
defer_threshold = 400.0
defer_recheck_secs = 30
```

While the signal is high:

- A deferrable job does not start. It holds no worker slot and uses no
  attempt. The runtime never drops it. An operator can cancel it while it
  waits. A job that is not deferrable on the same queue runs.
  - `local`: the job waits in memory. The runtime checks the signal again
    every `defer_recheck_secs`. When the signal falls, the job goes back on
    the queue.
  - `postgres` and `sqlite`: workers do not claim the job. It stays
    `enqueued` in the table. When the signal falls, the next poll claims it.
  - `redis`: a worker that pops the job parks it in the `blocked` set. It
    does not claim it. When the signal falls, a worker puts it back on its
    queue. The concurrency gauges do not count it.
- A deferrable task waits, then runs its tick when the signal falls. Later
  ticks on that replica fold into that one run.
  - On the `postgres` and `sqlite` schedulers the tick claim expires, so the
    tick waits first. Then it takes the claim and checks the signal again.
  - On the in-process scheduler the tick takes its lease, then waits.
- Request handlers and work that is not deferrable run as usual.

`signal.deferrals` in `/actuator/cost` counts each `local` job or tick that
started to wait. A job on a durable backend stays in the queue, so it is not
counted.

### Measure the shift

With metering on, the runtime classifies a deferrable run when it starts:

| Class | Meaning |
|---|---|
| `shifted` | A window held the run, then the run started while the signal was low. |
| `in_window` | The run started while the signal was high. |

A deferrable run that no window held, and that started while the signal was
low, is in no class. The wait for the first runtime-config value is not a
window.

`shift.ratio` in `/actuator/cost` is `shifted_cpu / (shifted_cpu +
in_window_cpu)`. When no CPU time is measured, it uses the run counts. It is
`null` when no run is in a class. A ratio below `1.0` shows work that ran in
a window, for example a job that a worker claimed just before the signal
rose.

A `local` job or a tick was held when it waited in a window. A job on a durable
backend was held when it was ready (its due time) before the last time a
worker saw the window.

For a `JobInfo` or `TaskInfo` that you make by hand, call
`autumn_web::cost::mark_deferrable(WorkKind::Job, "name")`.

### Limits

- A durable worker reads the signal before each claim. A job that a worker
  claimed before the signal rose runs. It counts as `in_window`.
- On `postgres` and `sqlite`, a large backlog of deferred jobs at the head of
  a queue makes each claim scan past those rows.
- A local job that waits only for the first runtime-config value, and then
  waits in a window, is not `shifted`.
- A process knows only the windows that it saw. A durable job that a process
  claims after a restart, when that process did not see the window, is not
  `shifted`. The shifted ratio can then be low.
- On the `local` backend, a deferred job is in memory. A restart loses it, as
  it loses any other queued local job.
- On the `postgres` and `sqlite` schedulers, replicas can resume at
  different times, up to `defer_recheck_secs` plus `signal_refresh_secs`
  apart. Set `lease_ttl_secs` longer than that plus the task run time. Then a
  replica that resumes late finds the tick taken.
- With more than one replica, each replica can hold one waiting tick of a
  task. So after the window, the task can run one time on each replica.
- With metering on, the DB lane installs the query timer on each checked-out
  connection, as `Server-Timing` does. This replaces a diesel default
  instrumentation that your app set.

---

## Configuration

| Key | Env | Default | Meaning |
|---|---|---|---|
| `cost.enabled` | `AUTUMN_COST__ENABLED` | `false` | Meter each request, job run and scheduled tick. |
| `cost.defer_threshold` | `AUTUMN_COST__DEFER_THRESHOLD` | unset | Deferrable work waits while the signal is above this value. Must be `>= 0`. |
| `cost.defer_recheck_secs` | `AUTUMN_COST__DEFER_RECHECK_SECS` | `30` | Seconds between two checks while work waits. Must be at least `1`. |
| `cost.max_tenants` | `AUTUMN_COST__MAX_TENANTS` | `1000` | Most tenant keys. More go into `_other`. |
| `cost.signal_refresh_secs` | `AUTUMN_COST__SIGNAL_REFRESH_SECS` | `5` | Seconds between two reads of `autumn_cost_signal`. Must be at least `1`. |
