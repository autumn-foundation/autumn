# Overload signals

Autumn exports the signals that show overload: request latency, job backlog
and database pool pressure. It also gives each request an ID that you can
follow through logs, traces and other services. To scrape the endpoint, see
[Metrics](../metrics.md).

## HTTP latency histogram

`/actuator/prometheus` exports `autumn_http_request_duration_seconds` as a
histogram. Each series has these labels:

| Label | Values |
|-------|--------|
| `version` | The deploy version (`AUTUMN_DEPLOY_VERSION`). |
| `method` | `GET`, `HEAD`, `POST`, `PUT`, `DELETE`, `PATCH`, `OPTIONS`, `CONNECT`, `TRACE`, or `_other`. |
| `route` | The matched route pattern, for example `/items/{id}`. A request that matches no route is `_unmatched`. |
| `status_class` | `1xx`, `2xx`, `3xx`, `4xx`, `5xx`, or `other`. |

Every label has a bounded set of values. A client cannot add series with
random paths or random methods.

The bucket bounds, in seconds, are `0.001`, `0.005`, `0.01`, `0.025`,
`0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10` and `+Inf`.

You can add the histograms of all replicas. To get the fleet p99 per route:

```promql
histogram_quantile(0.99,
  sum by (le, route) (rate(autumn_http_request_duration_seconds_bucket[5m])))
```

To get the fraction of requests slower than 250 ms (the SLO error ratio):

```promql
1 - sum(rate(autumn_http_request_duration_seconds_bucket{le="0.25"}[5m]))
  / sum(rate(autumn_http_request_duration_seconds_count[5m]))
```

The `/actuator/metrics` JSON uses the same method rule. A request with a
non-standard method is counted under `_other <route>`.

### Deprecated summary

`autumn_http_request_duration_quantiles_seconds` keeps the old p50, p95 and
p99 lines. It is a summary over the last 10,000 requests of one replica. You
cannot add summaries from different replicas. A later release removes it.
See the [migration guide](../../migrations/next.md).

## Job queue metrics

Each metric also has the `version` label.

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `autumn_jobs_queue_depth` | gauge | `queue` | Ready jobs that have not started. |
| `autumn_jobs_oldest_age_seconds` | gauge | `queue` | Wait time of the oldest ready job. `0` when the queue is empty. |
| `autumn_jobs_dead_letter_total` | counter | `job` | Executions that this replica moved to the dead-letter queue. |

On the Postgres and Redis backends, the queue gauges come from a survey of
the shared store. Every replica reports the same backlog. The dead-letter
counter is per replica. Add the replicas to get the total.

If `autumn_jobs_oldest_age_seconds` increases, the workers are too slow.
This shows before the depth is large.

## Database pool metrics

Each metric also has the `version` label.

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `autumn_db_pool_max_size` | gauge | `pool` | Maximum connections. |
| `autumn_db_pool_size` | gauge | `pool` | Open connections. |
| `autumn_db_pool_available` | gauge | `pool` | Idle connections. |
| `autumn_db_pool_waiting` | gauge | `pool` | Tasks that wait for a connection. |
| `autumn_db_pool_wait_seconds` | histogram | none | Time a `Db` checkout takes to get a connection. |

The `pool` label is `primary` or `replica`, or `shard:<name>:primary` and
`shard:<name>:replica` for a sharded app. The gauges are present only when
the app has a pool.

The wait histogram records each `Db` extractor checkout of every pool in one
series. It also records a failed checkout, because a checkout timeout is the
most important signal. The time includes a new connection when the pool must
open one. The histogram does not record a job that uses the pool directly.

## Request IDs

Each response from the request-ID layer has an `X-Request-Id` header. Each
log event of the request has the same `request_id`.

The app makes a new UUID for each request, with one exception. The app keeps
an inbound `X-Request-Id` when all of these conditions are true:

1. `security.trusted_proxies.trust_forwarded_headers` is `true`.
2. The peer is trusted:
   - With `ranges`, the peer IP must be in a range.
   - With `trusted_hops = N`, the `X-Forwarded-For` chain must have more
     than `N` entries.
   - With no `ranges` and no `trusted_hops`, every peer is trusted. Any
     client can then set the ID.
3. The request has one `X-Request-Id` header, not more.
4. The value is a UUID in the hyphenated form (36 characters) or the simple
   form (32 hex digits).

The app keeps the text as sent, so your logs match the proxy logs. The app
ignores all other values.

Configure the proxy to replace or remove the client's `X-Request-Id`. If the
proxy passes it through, a client can choose its own ID.

```toml
[security.trusted_proxies]
trust_forwarded_headers = true
ranges = ["10.0.0.0/8"]
```

`http_client` sends the current request ID as `x-request-id` on each
outbound call, to every host. A header that you set on the request builder
wins. To send no ID to a host, call `.without_request_id()` on the request
builder.

A failure capsule never uses an inbound ID as its capsule ID, because a proxy
retry can send the same ID again.

## Traces and logs

This section needs the `telemetry-otlp` Cargo feature and
`telemetry.enabled = true`.

### Sampler

`telemetry.sample_ratio` sets the fraction of new traces to sample. The
default is `1.0`.

```toml
[telemetry]
enabled = true
otlp_endpoint = "http://otel-collector:4317"
sample_ratio = 0.1
```

The sampler is parent-based. A span with a sampled parent is sampled. A span
with an unsampled parent is not. The ratio decides only for root spans, so a
trace is never cut in half. The app clamps a value out of range to `[0, 1]`.

A client that sends a sampled `traceparent` gets a sampled trace. The ratio
does not apply to it.

### Outbound spans

`http_client` opens one `http.client.request` span (kind `CLIENT`) for each
attempt. Each attempt is a new span. `http.request.resend_count` is set from
the second attempt. The `traceparent` header of each attempt names the span
of that attempt. The span holds the URL path, not the query, because a query
can hold secrets. A path can also hold a secret, for example a webhook token.
Your trace backend receives it.

### Trace IDs in logs

When an OpenTelemetry span is active, the request span records `trace_id` and
`span_id`. Each log event of the request carries them, in the JSON output and
in the `/actuator/logfile` buffer. Use them to find the trace of a log line.
With `sample_ratio` below `1`, a log line can show a trace that the app did
not export.
