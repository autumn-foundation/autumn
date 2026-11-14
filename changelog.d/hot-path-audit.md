### Performance

- **Request path:** a request does less work that only an error or a
  Markdown response needs. A `GET` no longer parses `Accept` to learn that
  no range names `text/markdown`. The request headers are copied for the
  error page in dev only, and `Accept` is read for an error response only.
  The error reporter keeps the method, route and request id typed until it
  reports. Measured with callgrind on `throttle_check`, `captcha_check` and
  `csrf_verify`: 8% to 10% fewer instructions and 8 to 10 fewer
  allocations for each request.
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
- **Adaptive admission:** a sample no longer converts a 128-bit integer to
  a float, and the Vegas limit computes its `log10` step by comparison for
  limits below 100 000.
- **Pagination:** a `Page<T>` response builds its `Link` header in one
  buffer: two allocations, not seven.
