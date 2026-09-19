# Error Reporting

When an Autumn handler **panics** or returns a **5xx**, the failure should go
*somewhere* you'll actually see it — Sentry, Honeycomb, Slack, a custom sink —
not just scroll past in the logs. Autumn's error-reporting pipeline is the
"configure once, errors go somewhere" seam, modeled on Rails'
[`Rails.error.report`](https://guides.rubyonrails.org/error_reporting.html).

Two things happen automatically, out of the box:

1. **Handler panics are caught at the HTTP layer.** A panic inside a handler
   becomes a clean `500` [Problem Details](./tutorial/09-errors.md#problem-details-json-error-contract)
   response instead of aborting the worker task. The panic payload never leaks
   to the client.
2. **Panics and 5xx responses are reported.** Each produces exactly one
   structured `ErrorEvent` delivered to every registered reporter. The default
   [`LogReporter`] writes it through `tracing`, so the feature is useful with
   zero configuration.

> This slice covers **panics + server errors only**. Client (`4xx`) errors and
> validation-error aggregation are out of scope.

---

## Quick start

A reporter is one trait with one method. Wiring it up is a single builder call:

```rust,no_run
use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};

struct SlackReporter {
    webhook_url: String,
}

impl ErrorReporter for SlackReporter {
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
        Box::pin(async move {
            // POST `event` to Slack; swallow transport errors.
            let _ = (&self.webhook_url, event.status, &event.message);
        })
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_error_reporter(SlackReporter { webhook_url: std::env::var("SLACK_URL").unwrap() })
        .routes(routes![/* ... */])
        .run()
        .await;
}
```

A runnable version lives in [`examples/error-reporting`](https://github.com/autumn-foundation/autumn/tree/main/examples/error-reporting).

---

## The `ErrorEvent`

Every report carries enough context to locate the failure:

| Field          | Description                                                        |
| -------------- | ------------------------------------------------------------------ |
| `status`       | HTTP status of the failing response (always `5xx`).                |
| `message`      | The error message (panic payload, or the underlying error).        |
| `problem_type` | The Problem Details `type` URI, when the error carried one.        |
| `request_id`   | The `X-Request-Id` of the failing request.                         |
| `route`        | The matched route template, e.g. `/users/{id}`.                    |
| `method`       | The HTTP method, e.g. `GET`.                                       |
| `panic`        | `Some(PanicInfo)` for caught panics: the payload + a backtrace.    |
| `capsule`      | `Some(CapsuleRef)` when a [failure capsule](#the-replay-capsule) was written. |

For panics, `panic.backtrace` is populated only when `RUST_BACKTRACE` is set
(`RUST_BACKTRACE=1`), matching the standard library's backtrace gate.

### The replay capsule

With `[failure_capture] enabled = true`, the same panic or 5xx that produces
this event also writes a **failure capsule**: one JSON file holding the redacted
request, the database traffic it produced and the clock readings it took, which
`autumn replay <capsule>` re-runs offline against your application. `capsule`
carries its `id` and `path`, and the file is already on disk by the time your
reporter is invoked — persistence runs first, ahead of the reporters and ahead
of the `enabled`/`sample_rate` gate — so a reporter can attach the path, upload
the file, or read it inline. It is `None` when capture is off or the write
failed. A capsule holds real request data and real database rows; see the
[Failure Capsules guide](./failure-capsules.md) before enabling it.

---

## Multiple reporters

Call `.with_error_reporter(..)` more than once to fan out — every reporter
receives every event:

```rust,ignore
autumn_web::app()
    .with_error_reporter(SentryReporter::from_env())
    .with_error_reporter(SlackReporter { webhook_url })
    .routes(routes![/* ... */])
    .run()
    .await;
```

When you register **no** reporter, the built-in [`LogReporter`] is used so
panics and server errors still surface in your logs.

---

## Reporting never breaks a request

Reporting runs on a **detached task**, so a slow or failing reporter can't delay
or break the client response:

- The client always gets its clean `500` (or other `5xx`) response immediately.
- If a reporter **panics or errors**, the failure is swallowed and logged; other
  reporters still run.

---

## Configuration

The `[reporting]` section of `autumn.toml` controls delivery:

```toml
[reporting]
enabled = true       # deliver events to reporters (default: true)
sample_rate = 0.25   # report ~25% of events (default: 1.0 = all)
```

- **`enabled = false`** suppresses *delivery* only. Handler panics are **still
  caught** and turned into clean 500s regardless of this setting.
- **`sample_rate`** is a fraction in `[0.0, 1.0]`. Use it to cap reporter volume
  (and cost) on high-traffic services.

---

## How it fits the middleware stack

The reporting + panic-catch layer sits inside
[`RequestIdLayer`](./middleware.md#middleware-ordering) (so the request id is
available when a handler panics) and outside the route handler (so handler
panics are caught). The resulting `500` still flows out through the
[exception-filter](./middleware.md) chain, so HTML error-page negotiation and
Problem Details rendering work exactly as they do for any other error.

This is the panic-aware promotion of the older
[`ExceptionFilter`](./middleware.md) concept: `ExceptionFilter` transforms the
*response*; `ErrorReporter` ships the *event*. They compose — keep your
filters, add reporters.

### Scope: which 5xx are observed

Because the layer sits *inside* `RequestIdLayer` (so every event — panics
included — carries the request id), 5xx responses produced by middleware
*outside* it are not observed. In practice the only such case is a `503` from
the **session layer** when a session store (e.g. Redis) is unavailable. That is
a deliberate trade-off: a session-store outage is an infrastructure failure
already surfaced by readiness/health probes, and moving reporting outside the
session layer would also move it outside `RequestIdLayer`, dropping the request
id from *every* event. Handler panics and handler/inner-middleware server errors
— the failures you're expected to act on — are reported with full context.

---

## Shipping to a concrete backend

A concrete Sentry/Honeycomb/Datadog backend lives in its own crate (mirroring
how `autumn-storage-s3` lives outside core). To build one, implement
`ErrorReporter` over the backend's client and publish it as a small adapter
crate; users add it as a dependency and wire it with one
`.with_error_reporter(..)` call.

---

## See also

- [Errors tutorial](./tutorial/09-errors.md) — `AutumnError`, Problem Details,
  HTML error pages.
- [Middleware guide](./middleware.md) — the layer ordering and exception
  filters.
- [Failure Capsules](./failure-capsules.md) — recording a failing request and
  replaying it offline with `autumn replay`.

[`LogReporter`]: https://docs.rs/autumn-web/latest/autumn_web/reporting/struct.LogReporter.html
