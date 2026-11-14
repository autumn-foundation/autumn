### Performance

- **Request path:** a request does less work that only an error, a form or
  a Markdown response needs. Measured with callgrind and dhat on
  `throttle_check`, `captcha_check` and `csrf_verify`: 10% to 14% fewer
  instructions and 11% to 14% fewer heap allocations for each request. The
  ingress allocation gate measures 88 blocks for a request, down from 98.
  - A `GET` no longer parses `Accept` to learn that no range names
    `text/markdown`.
  - The request headers are copied for the error page in dev only, and
    `Accept` is read for an error response only.
  - The error reporter keeps the method, route and request id typed until
    it reports.
  - The submit token is drawn when a form reads it. A request that renders
    no form makes no random draw for it.
  - A session id minted for a request with no cookie stays a `Uuid` until
    something reads it. A request with a session cookie no longer copies
    the id into an idempotency extension, and a signed cookie is parsed by
    borrow.
  - CSRF, bot protection and the rate limiter call the inner service in
    place for a request they only forward, without cloning the stack below.
  - An access-log-excluded path (`/health`, `/static`, …) allocates no
    access-log state and does not read the clock.
  - Asset handlers borrow the request headers; whether a `/static` path is
    fingerprinted is one set lookup.
- **Request path, many cores:** reading the clock and checking maintenance
  mode no longer write to memory that every worker shares. The clock wrote
  a process-wide mark five times for each request, and maintenance mode
  took a read lock.
- **Logging:** the request span is created with its request id. Recording
  the id afterwards was a second pass through every subscriber layer, and
  the JSON log format parsed the span's fields again for it.
- **App metrics:** `counter(NAME).with_label(..).increment(1)` takes 23%
  fewer instructions. The registry hashes names and label sets with
  `foldhash`, seeded for each process, and checks names by byte.
- **Signed cookies:** an HMAC verify compares a buffer on the stack. It
  allocated a hex `String` for each check.
- **`#[throttle]`:** a check keys from the request's parts by borrow,
  without copying its headers: 1 allocation for a token-keyed check, down
  from 3. The global rate limiter's quota headers take one allocation each.
- **Adaptive admission:** a limit update takes 19% fewer instructions. A
  sample no longer converts a 128-bit integer to a float, and the Vegas
  limit computes its `log10` step by comparison for limits below 100 000.
- **Pagination:** a `Page<T>` response builds its `Link` header in one
  buffer: two allocations, not seven.
