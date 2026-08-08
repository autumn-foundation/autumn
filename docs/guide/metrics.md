# App Metrics

Autumn's `/actuator/prometheus` and `/actuator/metrics` endpoints already expose
the framework's own `autumn_http_*` families. The `autumn_web::metrics` facade
lets **application code** add its own counters, gauges and timers at the point
where the interesting thing happens — one line at the call site, no trait to
implement, no type to define, nothing to register with `AppBuilder`.

Everything recorded through the facade appears automatically on the **same**
scrape endpoint as the built-in families.

> Contributing families from a **plugin or subsystem** — with your own storage,
> collected on demand at scrape time — is the `MetricsSource` trait instead.
> See [Plugin Metrics Sources](metrics-sources.md), and the
> [comparison table](#facade-vs-metricssource-which-one-do-i-want) below.

---

## Quick start

### 1. Record from a handler

```rust
use autumn_web::metrics;
use autumn_web::get;

#[get("/checkout")]
async fn checkout() -> &'static str {
    metrics::counter("checkout_completed_total")
        .with_label("status", "paid")
        .increment(1);
    "ok"
}
```

That is the whole integration. No `AppBuilder` call, no app-defined struct: the
first `counter("checkout_completed_total")` call registers the instrument in a
process-global registry and every later call finds it again.

### 2. Scrape it

```
$ curl http://localhost:3000/actuator/prometheus
# HELP autumn_http_requests_total Total number of HTTP requests
# TYPE autumn_http_requests_total counter
autumn_http_requests_total{version="stable"} 1234
...
# TYPE checkout_completed_total counter
checkout_completed_total{status="paid"} 3
```

### 3. Describe it (optional)

Without a description there is no `# HELP` line. Call `describe_counter` once at
startup to add one:

```rust
autumn_web::metrics::describe_counter(
    "checkout_completed_total",
    "Checkouts that reached a terminal state",
);
```

```
# HELP checkout_completed_total Checkouts that reached a terminal state
# TYPE checkout_completed_total counter
checkout_completed_total{status="paid"} 3
```

`describe_gauge` and `describe_histogram` do the same for the other kinds.

Describing a metric does **not** register it — the description is held until the
first `counter(...)`/`gauge(...)`/`histogram(...)` call creates the instrument.
So the two calls may come in either order, and a metric that is described but
never recorded stays out of the scrape output entirely. (It also means
`describe_histogram` cannot accidentally freeze a histogram's bucket bounds; see
[Buckets](#buckets).)

Help text is stripped of control characters — a `# HELP` line is one line — and
truncated to 512 characters.

---

## Which instrument do I want?

| Instrument                       | Answers                            | Convention   |
| -------------------------------- | ---------------------------------- | ------------ |
| `counter`                        | *how many times did X happen?*     | `*_total`    |
| `gauge`                          | *how many X are there right now?*  | plain noun   |
| `timer` (and `histogram`)        | *how long did X take?*             | `*_seconds`  |

A counter only ever goes up, so PromQL `rate()` works on it. A gauge moves in
both directions. A timer is a histogram of seconds, which is what
`histogram_quantile()` needs to give you a p99.

```rust
use autumn_web::metrics;

// Counter: increments only.
metrics::counter("emails_sent_total").increment(1);

// Gauge: set, increment, decrement.
metrics::gauge("worker_queue_depth").set(queue.len()); // usize, u64, i64, f64…
metrics::gauge("worker_queue_depth").increment(1);
metrics::gauge("worker_queue_depth").decrement(1);

// Histogram: any non-negative observation (not just seconds).
metrics::histogram("upload_size_bytes").record(2_048);
```

Gauges and histograms take any primitive number, including `usize`, `u64` and
`i64` — so `set(queue.len())` compiles without a cast. Values above 2^53 are
rounded to the nearest `f64`, which is all Prometheus can carry anyway.

Counters saturate at `u64::MAX` instead of wrapping: a wrapped total is
indistinguishable from a counter reset and would give `rate()` an enormous
phantom spike. Non-finite values (`NaN`, `±Inf`) are rejected on gauges and
histograms alike, so the scrape and the JSON view can never disagree.

---

## Timing with `timer`

The guard returned by `start()` records the elapsed time when it **drops**, so
every exit path is covered — including an early `?` return and an unwinding
panic:

```rust
use autumn_web::metrics;

async fn charge_card(amount: u64) -> Result<(), PaymentError> {
    let _timing = metrics::timer("payment_duration_seconds").start();

    let receipt = gateway::charge(amount).await?; // early return: still recorded
    ledger::write(receipt).await?;
    Ok(())
}
```

Bind the guard to a **named** variable (`_timing`, not `_`): `let _ = ...` drops
it immediately and records a duration of roughly zero.

Three more ways to record, when a guard does not fit:

```rust
use std::time::Duration;
use autumn_web::metrics;

let timer = metrics::timer("payment_duration_seconds");

// Wrap a closure, or a future.
let receipt = timer.time(|| gateway::charge_blocking(amount));
let receipt = timer.time_async(gateway::charge(amount)).await;

// Or record a duration you already measured.
timer.record(Duration::from_millis(120));
```

A guard can also be resolved early. `stop()` records and returns the elapsed
duration; `cancel()` throws the measurement away and records nothing:

```rust
let timing = metrics::timer("payment_duration_seconds").start();
let elapsed = timing.stop();          // records once, returns the duration
// let timing = ...; timing.cancel(); // records nothing
```

A guard records exactly once, whether it is stopped explicitly or dropped.

### Buckets

Timers and histograms use these default upper bounds, in seconds:

```
0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10
```

Override them for one instrument at startup, **before** it is first used —
bounds are frozen at registration so they cannot move under a running scrape
target:

```rust
use autumn_web::metrics;

// Either order works: describing a metric does not register it.
metrics::describe_histogram("payment_duration_seconds", "Card charge latency");
metrics::set_histogram_buckets(
    "payment_duration_seconds",
    &[0.05, 0.1, 0.5, 1.0, 5.0, 30.0],
);
```

Bounds must be 1..=20 finite, positive, strictly ascending values; anything else
is ignored with a warning and the defaults are kept. A call that arrives after
the histogram has been registered is ignored with a warning too.

Bounds are rendered into `le` exactly as `client_golang` renders them, so a
recording rule that matches `le` as a string keeps working: plain decimal inside
`[1e-4, 1e21)` and exponential outside it (`5e-05`, `1e+21`).

A timer renders as a standard Prometheus histogram — cumulative `_bucket` lines
ending at `le="+Inf"`, which always equals `_count`:

```
# TYPE payment_duration_seconds histogram
payment_duration_seconds_bucket{le="0.05"} 0
payment_duration_seconds_bucket{le="0.1"} 2
payment_duration_seconds_bucket{le="0.5"} 3
payment_duration_seconds_bucket{le="1"} 3
payment_duration_seconds_bucket{le="5"} 4
payment_duration_seconds_bucket{le="30"} 4
payment_duration_seconds_bucket{le="+Inf"} 4
payment_duration_seconds_sum 1.732
payment_duration_seconds_count 4
```

Non-finite and negative observations are rejected with a warning, so `_sum` can
never become `NaN`. `_sum` is still an `f64` accumulator that is never reset, so
a long-lived process recording astronomically large observations can eventually
saturate it to `+Inf` — permanently. Recording seconds (what `timer` does) keeps
you many orders of magnitude away from that.

---

## Labels and cardinality

`with_label` attaches a label to the series a handle records into. Labels are
canonicalized for you: sorted by key, deduplicated first-wins, values stripped of
control characters (a stray `\n` or ANSI escape cannot split or forge an
exposition line) and truncated to 128 characters. An invalid, over-long or
reserved label name (`le`, `quantile`, anything starting `__`) drops that
**label**, never the sample.

When a handle carries more than eight usable labels, the ones kept are those with
the lexicographically smallest names — never the first eight you happened to
attach — so the same label set always lands in the same series.

```rust
metrics::counter("checkout_completed_total")
    .with_label("status", "paid")
    .with_label("currency", "eur")
    .increment(1);
```

> **Label values must come from a small, closed set the code controls.** Never
> label with user input, email addresses, account or order IDs, or anything else
> unbounded. Every distinct combination of label values is a separate time
> series that lives for the life of the process — and a label value is written
> in plaintext to whatever scrapes you. The same rule (and the same reasoning)
> as structured log fields: see [Logging and PII](logging-pii.md).

The facade enforces hard caps so a mistake degrades the metric instead of the
process:

| Limit                             | Value          | On overflow                                      |
| --------------------------------- | -------------- | ------------------------------------------------ |
| **Labeled** series per instrument | 100            | Samples with a new label set dropped and counted |
| Instruments in the registry       | 256            | Further new names get an inert handle            |
| Labels per series                 | 8              | Extra labels dropped, sample still recorded      |
| Label value length                | 128 characters | Truncated                                        |
| Metric name length                | 128 bytes      | **Rejected** — inert handle, never truncated     |
| Label name length                 | 128 bytes      | Label dropped, sample still recorded             |
| Help text length                  | 512 characters | Truncated                                        |

The unlabeled series — what a handle with no `with_label` call records into — is
separate and does not count against the 100. Names are rejected rather than
truncated, because two names sharing a 128-byte prefix would otherwise silently
become one metric.

Hitting the series cap logs **one** warning per instrument and is visible in the
scrape itself, so you can alert on it:

```
# HELP autumn_metrics_series_dropped_total App metric samples dropped because the metric had already hit its series cardinality cap
# TYPE autumn_metrics_series_dropped_total counter
autumn_metrics_series_dropped_total{metric="checkout_completed_total"} 57
```

That counts **samples**, not distinct label sets: a hot call site hammering one
over-cap label set is exactly what you need to see, and counting distinct sets
would mean remembering the very label sets the cap exists to stop remembering.

Series are never evicted once retained — evicting a counter would reset it and
break `rate()`.

---

## Reserved names

- The **`autumn_` prefix** belongs to the framework's built-in families, as do
  the built-in names themselves. Registering one is rejected.
- `:` is rejected too — Prometheus reserves it for recording rules.
- A name that is not a valid Prometheus metric name is rejected.
- A histogram (or timer) also reserves its derived `_bucket`, `_sum` and
  `_count` names. Registering an instrument that collides with another
  instrument's base or derived names is rejected, in either direction.
- The first registration of a name fixes its kind. Asking for the same name as a
  different kind is rejected, so there is always exactly one `# TYPE` line per
  family.

Every rejection logs one warning and returns an **inert handle** that records
nothing. The facade never panics and never poisons the scrape output. Rejected
names are escaped and truncated before they reach the log, so a name carrying
newlines or ANSI escapes cannot forge log records.

> **Registration order can lock you out, and the only signal is a log line.**
> The derived-name reservation runs in both directions and first registration
> wins, so a gauge named `payment_duration_seconds_sum` registered anywhere in
> the process permanently blocks the histogram `payment_duration_seconds` — and
> the block is silent apart from one warning at the losing call site. Pick
> `_sum`/`_count`/`_bucket`-suffixed names only when you mean the derived
> families of a histogram you own.

Unlike the built-in families, app metrics carry **no implicit `version` label**.
The label set belongs entirely to your call site, so the framework cannot
collide with a `version` label you chose yourself.

---

## JSON endpoint (`/actuator/metrics`)

The same data appears under a new top-level `app` key, alongside the existing
`http` and `database` keys (which are unchanged):

```json
{
  "http": { "requests_total": 1234, ... },
  "app": [
    {
      "name": "checkout_completed_total",
      "help": "Checkouts that reached a terminal state",
      "kind": "counter",
      "series": [
        { "labels": { "status": "paid" }, "value": { "value": 3 } }
      ],
      "dropped_series": 0
    }
  ]
}
```

The key is absent — rather than an empty array — when the app has recorded
nothing. Histogram series carry `count`, `sum` and cumulative `buckets` instead
of a single `value`.

`autumn_web::metrics::snapshot()` returns the same view as Rust values, if you
want to assert on recorded metrics from a test.

---

## Actuator exposure config

**Recording always works. Only exposure is gated.**

```toml
[actuator]
prometheus = true    # /actuator/prometheus scrape endpoint — on by default
```

With `prometheus = false` the scrape endpoint is not mounted at all (`404`), for
app metrics exactly as for built-in ones — there is no bypass. Only the
**Prometheus scrape format** is gated: your call sites keep recording, the
router says so once at startup, and `/actuator/metrics` still shows the `app`
key, under the same visibility rules as the built-in families. See
[Plugin Metrics Sources](metrics-sources.md#actuator-exposure-config) for how
this interacts with `actuator.sensitive`.

---

## Facade vs `MetricsSource`: which one do I want?

| | `autumn_web::metrics` facade | `MetricsSource` trait |
| --- | --- | --- |
| **Use when** | app code should record an event as it happens | a subsystem already owns the numbers |
| **Model** | push: you call `increment`/`set`/`record` | pull: `collect()` runs at scrape time |
| **Setup** | none — call it | implement the trait, register with `AppBuilder` |
| **Storage** | the facade's process-global registry | yours |
| **Histograms / timers** | yes | no — counters and gauges only |
| **Registration** | automatic on first use | explicit, named |
| **JSON key** | `app` | `sources.<name>` |
| **Scope** | process-global | per app instance |
| **Typical caller** | handlers, services, jobs | plugins, framework subsystems |

Both render into the same `/actuator/prometheus` output. Name ownership runs
built-in families first, then facade metrics, then plugin sources: a plugin
family colliding with a name already emitted is skipped with a warning.

---

## Testing

The registry is process-global and tests run concurrently, so give each test its
own instrument names rather than clearing shared state:

```rust
// Behind the `test-support` feature.
let name = autumn_web::metrics::testing::unique_name("checkout_completed_total");
autumn_web::metrics::counter(&name).increment(1);
```

Assert with `contains()` on those unique names — never on whole-body equality or
line counts of the scrape output, which also carries built-in families and
anything a concurrent test recorded.

Those names are never reclaimed and they share the process-wide 256-instrument
budget with every other test in the same binary. A handful per test is fine; a
loop that registers hundreds will exhaust the registry for whatever runs after
it. Cap the loop, or reuse one name with different labels — that cap is
per-instrument.
