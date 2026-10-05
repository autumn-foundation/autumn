# Overload signals

Autumn exports the signals that show overload: request latency, job backlog,
database pool pressure, and request correlation. This page tells you what the
framework exports and how to use it. It does not tell you how to scrape it;
see [Metrics](../metrics.md) for the scrape endpoint.

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

A histogram aggregates across replicas. To get the fleet p99 per route:

```promql
histogram_quantile(0.99,
  sum by (le, route) (rate(autumn_http_request_duration_seconds_bucket[5m])))
```

To get the fraction of requests slower than 250 ms (an SLO burn rate):

```promql
1 - sum(rate(autumn_http_request_duration_seconds_bucket{le="0.25"}[5m]))
  / sum(rate(autumn_http_request_duration_seconds_count[5m]))
```

### Deprecated summary

`autumn_http_request_duration_quantiles_seconds` keeps the old p50, p95 and
p99 lines. It is a summary over the last 10,000 requests of one replica. Do
not aggregate it. A later release removes it. See the
[migration guide](../../migrations/next.md).

## Job queue metrics

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `autumn_jobs_queue_depth` | gauge | `queue` | Jobs that are ready to run and wait. |
| `autumn_jobs_oldest_age_seconds` | gauge | `queue` | Wait time of the oldest ready job. `0` when the queue is empty. |
| `autumn_jobs_dead_letter_total` | counter | `job` | Executions moved to the dead-letter queue. |

On the Postgres and Redis backends, the queue gauges come from a survey of
the shared store. Every replica reports the same backlog.

A rising `autumn_jobs_oldest_age_seconds` shows that the workers cannot keep
up, before the depth looks large.

## Database pool metrics

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `autumn_db_pool_max_size` | gauge | `pool` | Maximum connections. |
| `autumn_db_pool_size` | gauge | `pool` | Open connections. |
| `autumn_db_pool_available` | gauge | `pool` | Idle connections. |
| `autumn_db_pool_waiting` | gauge | `pool` | Tasks that wait for a connection. |
| `autumn_db_pool_wait_seconds` | histogram | none | Time a `Db` checkout waits for a connection. |

The `pool` label is `primary`, or `shard:<name>:primary` and
`shard:<name>:replica` for a sharded app. The gauges are present only when
the app has a pool.

The wait histogram counts each `Db` extractor checkout, also a checkout that
fails. A checkout timeout is the wait that matters most. Background jobs that
use the pool directly are not counted.

## Request IDs

Every response has an `X-Request-Id` header. Every log event of the request
has the same `request_id`.

The app makes a new UUID for each request, with one exception. The app keeps
an inbound `X-Request-Id` when both conditions are true:

1. `[security.trusted_proxies]` trusts the peer. See
   [Client identity behind a proxy](../extractors.md#client-identity-behind-a-proxy)
   for the trust rules.
2. The value is a UUID in the hyphenated form (36 characters) or the simple
   form (32 hex digits).

The app keeps the text as sent, so your logs match the proxy logs. The app
ignores all other values, and it ignores the header from an untrusted peer.

```toml
[security.trusted_proxies]
trust_forwarded_headers = true
ranges = ["10.0.0.0/8"]
```

`http_client` sends the current request id as `x-request-id` on each
outbound call. A header that you set on the request builder wins.

## Traces and logs

These features need the `telemetry-otlp` feature and `telemetry.enabled`.

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
trace is never cut in half. A value out of range is clamped to `[0, 1]`.

### Outbound spans

`http_client` opens one `http.client.request` span (kind `CLIENT`) for each
attempt. A retry is a new span with `http.request.resend_count` set. The
`traceparent` header of each attempt names that attempt's span. The span
holds the URL path, not the query, because a query can hold secrets.

### Trace IDs in logs

When an OpenTelemetry span is active, the request span records `trace_id` and
`span_id`. Every log event of the request carries them, in the JSON output
and in the `/actuator/logfile` buffer. Use them to go from a log line to its
trace.
