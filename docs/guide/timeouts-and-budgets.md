# Timeouts, Deadlines and Retry Budgets

This page tells you how Autumn limits the time of a request and the number of
retries. It covers:

- the request deadline, and how Autumn sends it to outbound calls;
- the deadline header between services;
- the retry budget;
- the `Retry-After` header on a timeout `503`;
- the drain window and the `ShutdownToken` extractor.

## The request deadline

Set a request timeout for all routes, or for one route:

```toml
# autumn.toml
[server.timeouts]
request_timeout_ms = 30000   # the prod profile sets 30000
```

```rust
use autumn_web::get;

#[get("/export", timeout_ms = 120000)]
async fn export() -> &'static str {
    "report"
}
```

When the timeout starts, Autumn sets a `Deadline` for the handler task. Read
it with `autumn_web::deadline::Deadline::current()`:

```rust
use autumn_web::deadline::Deadline;
use autumn_web::get;

#[get("/report")]
async fn report() -> String {
    match Deadline::current() {
        Some(deadline) => format!("{} ms left", deadline.remaining().as_millis()),
        None => "no deadline".to_owned(),
    }
}
```

These framework calls use the deadline:

| Call | What it does with the time left |
|---|---|
| Outbound `Client` | Each attempt uses `min(timeout_secs, time left)`. It does not start a retry, a backoff or a `Retry-After` wait that the time left cannot hold. When the deadline stops the call, it returns `ClientError::DeadlineExceeded` (`504` through `?`). The circuit breaker counts it as a cancelled call, not a failure: nothing, unless the call ran past the slow-call threshold. |
| `Db` extractor | The wait for a pool connection stops at the deadline (`503`). A query that is already running is not cut at the deadline: `database.statement_timeout` (`30s` under `prod`) bounds it. |

When a handler returns `ClientError::DeadlineExceeded` or
`deadline::DeadlineExceeded`, or its own error with one of them as a
`source()`, the `504` is treated like a request the timeout cancelled: the
session layer does not save the session changes the handler made.

A task that you start with `tokio::spawn` does not get the deadline. Give it
one with `Deadline::scope`. To stop any other call at the deadline, for
example a Redis call, use `autumn_web::deadline::bounded`:

```rust
use autumn_web::deadline::{self, Deadline};

async fn work() {
    // Stop the call at the request deadline.
    let _outcome = deadline::bounded(tokio::time::sleep(std::time::Duration::from_secs(1))).await;

    // Give a spawned task the same deadline.
    if let Some(deadline) = Deadline::current() {
        tokio::spawn(deadline.scope(async { /* ... */ }));
    }
}
```

A route with `timeout = "off"` has no deadline.

## The deadline header

The outbound `Client` sends the time left in the `x-autumn-deadline-ms`
header. The value is in milliseconds, relative to now. It is not a timestamp,
so clock skew between hosts has no effect. The client sends it only when a
deadline is set, and it sends it to every host, third-party APIs too.

If you set the header yourself, the client keeps your value when it is
shorter than the time left, and sends the time left otherwise.

Each attempt and each redirect hop gets a new value. Under a deadline the
client follows a redirect itself, not inside the HTTP stack, so the next host
gets the time left at that hop. It follows at most 10 hops, then returns
`ClientError::TooManyRedirects`.

The server can read the header from its callers. This is off by default:

```toml
[server.timeouts]
request_timeout_ms = 30000
accept_deadline_header = true   # AUTUMN_SERVER__TIMEOUTS__ACCEPT_DEADLINE_HEADER
```

The header can only make the route deadline shorter. A value that is not a
whole number is ignored. A route with no deadline ignores the header.

To stop the outbound header:

```toml
[http.client]
send_deadline_header = false
```

## The retry budget

Retries at each layer multiply. Three attempts at each of five layers is
3⁵ = 243 calls to the last service. The retry budget stops this.

The outbound `Client` keeps a token bucket for each upstream host. All
requests of one app share the buckets.

- The bucket starts full, with `capacity` tokens.
- A retry after a `429` costs `throttling_cost` tokens.
- A retry after a `5xx`, a connect error or a timeout costs `transient_cost`
  tokens.
- Each first attempt adds `retry_ratio × transient_cost` tokens.
- A retry that succeeds gives its tokens back.
- A first attempt never waits for tokens.

When the bucket is empty, about `retry_ratio` of requests can retry. The other
requests return their first failure at once.

An app keeps buckets for 1,024 hosts. A host past this limit has no budget.

```toml
[http.client.retry_budget]
enabled = true          # default
capacity = 500          # default
transient_cost = 14     # default
throttling_cost = 5     # default
retry_ratio = 0.1       # default: 10 % of requests can retry
```

The cost values are the AWS SDK values. The 10 % limit is from Google SRE.

Startup rejects a cost of `0` and a `retry_ratio` outside `0.0..=1.0`. With
`enabled = false`, these values are not used, so they are not checked.

## `Retry-After` on a timeout `503`

A request that passes its deadline gets a `503` with a `Retry-After` header.
The value is 1, 2 or 3 seconds, at random. The random value spreads the
retries of many clients, so they do not all come back at the same time. In a
`#[sim_test]` the value comes from the sim seed.

## The drain window and `ShutdownToken`

On shutdown, Autumn waits `shutdown_timeout_secs` for running requests. A
request that starts just before shutdown can run for the full request timeout.
Thus the drain window must be longer than the request timeout.

- The prod profile sets `shutdown_timeout_secs = 35`: the 30 s request timeout
  plus a 5 s margin.
- At startup, Autumn logs a warning when `shutdown_timeout_secs` is shorter
  than the request timeout plus 5 s.

Set your orchestrator grace period to `prestop_grace_secs +
shutdown_timeout_secs` or more. See [Cloud-Native Autumn](cloud-native.md).

A handler can stop long work at shutdown with the
`autumn_web::extract::ShutdownToken` extractor. The token is cancelled when the
server stops accepting connections:

```rust
use autumn_web::extract::ShutdownToken;
use autumn_web::get;

#[get("/poll")]
async fn poll(shutdown: ShutdownToken) -> &'static str {
    tokio::select! {
        () = shutdown.cancelled() => "stopping",
        () = tokio::time::sleep(std::time::Duration::from_secs(20)) => "no news",
    }
}
```

## Test it

The deadline and the budget use tokio time, so a `#[sim_test]` with a
`SimNet` runs them in virtual time. See
[Simulation Testing](simulation-testing.md) and the tests in
`autumn/tests/integration/sim_deadline.rs` and
`autumn/tests/integration/sim_retry_budget.rs`.

## Related

- [Outbound HTTP Client](outbound-http.md)
- [Resilience and Circuit Breakers](resilience.md)
- [Cloud-Native Autumn](cloud-native.md)
