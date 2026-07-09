# Server-Timing response header

Autumn can emit a standards-conformant
[`Server-Timing`](https://www.w3.org/TR/server-timing/) response header on
every served request when the `[observability]` config section opts in. The
header surfaces the same wall time that the access log records as
`duration_ms`, plus a per-request database roll-up so N+1 queries show up
directly in the browser's DevTools Network → Timing pane.

## Sample header

```
Server-Timing: total;dur=42.180, db;dur=21.500;desc="14 queries"
```

- `total;dur=…` — whole-request wall time, in milliseconds, measured with
  the same `Instant`-based formula as the access log's `duration_ms`.
- `db;dur=…;desc="N queries"` — cumulative time spent inside `Db`-tracked
  Postgres queries, plus the query **count** in `desc`. Only present when
  at least one instrumented query ran during the request.

Both `dur` values are `f64` milliseconds rounded to three decimals.

## Viewing it in the browser

In Chromium DevTools open the Network tab, click any request, and go to
the **Timing** sub-tab. The `Server-Timing` metrics appear at the bottom
under **Server Timing**, one row per metric, with the `desc` string shown
as the row label. Firefox surfaces the same metrics in its Network → Timings
panel.

## Enabling it

Add to `autumn.toml`:

```toml
[observability]
server_timing = true
```

or via environment variable:

```
AUTUMN_OBSERVABILITY__SERVER_TIMING=true
```

### Profile defaults

The default follows the active profile:

| Profile               | Default `server_timing` |
| --------------------- | ----------------------- |
| `dev` / `development` | **on**                  |
| everything else       | **off**                 |

This means production apps never leak internal timings to anonymous clients
by default — you have to opt in explicitly, or set the env override in
whichever profile you want it on. Explicit `server_timing = true` or
`server_timing = false` always wins over the profile default.

## Units and clock

The `total` metric uses
`start.elapsed().as_secs_f64() * 1000.0` — the identical formula the access
log uses for its `duration_ms` field. This middleware is applied outer to
the access log, so its `total` brackets the access-log `duration_ms`. For
any one request the two values agree to within a few microseconds — the
access log does path-matching work between the two `Instant` captures, so
expect a sub-microsecond-to-microsecond difference rather than exact
equality — and both are subject to the same monotonic-clock guarantees on
the host.

The `db` metric aggregates elapsed times from `run_instrumented` and the
`Db` drop hook. Queries that run outside a request (background jobs,
startup work) don't count — they can't, because the middleware only scopes
the accumulator for the request future.

## Streaming responses (SSE)

Responses whose `Content-Type` starts with `text/event-stream` receive a
`total`-only header — the `db` metric is omitted even if queries ran
during header assembly. The `total` value reflects **time to first byte**
(when the response headers flush) rather than the full stream lifetime,
which is unbounded for a long-lived SSE feed. Handlers that care about the
duration of a specific streamed body operation should emit a purpose-built
metric via their own tracing span rather than rely on the header.

Non-SSE streaming bodies (chunked JSON, files, WebSocket upgrade
responses) get the full metric set as if they were unary responses — the
middleware measures until the response head is ready, not until the body
drains.

## Failure modes

The header is a best-effort observability surface. If the assembled string
somehow fails `HeaderValue::from_str`, the middleware silently drops the
header rather than aborting the response. Absent timing data (e.g. no
queries ran) never turns into an error path.
