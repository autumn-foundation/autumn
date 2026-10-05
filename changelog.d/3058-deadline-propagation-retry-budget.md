### Added

- **http:** the request deadline now reaches outbound calls (issue #3058).
  Each attempt of the outbound `Client` uses `min(timeout_secs, time left)`.
  The client does not start a retry or a wait that the time left cannot hold.
  `ClientError::DeadlineExceeded` (`504`) tells you that no time was left. See
  [the guide](../docs/guide/timeouts-and-budgets.md).
- **http:** `autumn_web::deadline::Deadline::current()` gives a handler the
  time left. `Deadline::scope` and `deadline::bounded` carry and apply it to
  other work.
- **http:** the outbound client sends the time left in the
  `x-autumn-deadline-ms` header (`http.client.send_deadline_header`, default
  `true`). `server.timeouts.accept_deadline_header` (default `false`, env
  `AUTUMN_SERVER__TIMEOUTS__ACCEPT_DEADLINE_HEADER`) lets a caller's header
  make the route deadline shorter.
- **http:** a retry budget for each upstream host
  (`[http.client.retry_budget]`, on by default). When an upstream always
  fails, only about 10 % of requests retry. First attempts never wait.
- **server:** a timeout `503` now has a `Retry-After` header of 1-3 seconds,
  with jitter.
- **server:** the `autumn_web::extract::ShutdownToken` extractor. It is
  cancelled when the server stops accepting connections. `AppState::shutdown_token`
  no longer needs the `ws` feature.
- **db:** the `Db` connection wait and the Postgres `statement_timeout` stop at
  the request deadline.

### Changed

- **server:** the `prod` profile sets `server.shutdown_timeout_secs` to 35
  (was 30): the 30 s request timeout plus a 5 s margin. Before, a request that
  started just before shutdown could be stopped by the drain watchdog. Startup
  logs a warning when the drain window is shorter than the request timeout
  plus 5 s. Increase your orchestrator grace period by 5 s.
