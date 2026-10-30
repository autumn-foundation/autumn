### Performance

- **router:** a request through the default ingress stack allocates less: 98
  heap blocks and 15,666 bytes, not 133 blocks and 28,270 bytes (issue #2830).
  The clone that each outer `Route` level makes for each request now stops at
  a shared pointer, above the operator layers and the inner layer group. The log context, trusted-proxy, idempotency-replay and
  transaction-timeout layers no longer box a future. The method-override,
  submit-token and error-page layers box a future only for a form body that
  they must read first. The session layer boxes only to load a session from a
  cookie or to save a changed session.

### Breaking Changes

- **Breaking:** the `Service::Future` types of `LogContextService`,
  `SessionService`, `MethodOverrideService`, `IdempotencyReplayService`, and
  the services of `TrustedProxiesLayer` and `SubmitTokenLayer` are no longer
  `Pin<Box<dyn Future>>`. The futures of `LogContextService` and
  `SessionService` can be `!Unpin`
  ([migration guide](docs/migrations/next.md)).
