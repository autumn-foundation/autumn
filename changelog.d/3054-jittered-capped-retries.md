### Breaking Changes

- **Breaking:** **http client:** `RequestBuilder::retries(n)` now sets only the retry count. It no longer enables retries for `POST` and `PATCH`. Call the new `.retry_non_idempotent()` to retry them. That call also sends an `Idempotency-Key` header (the same value on each attempt) when the request has none. `RetryPolicy`, `HttpClientConfig` and `JobConfig` have new public fields (`max_backoff`, `max_backoff_ms`) (issue #3054, [migration guide](docs/migrations/next.md)).

### Changed

- **http client, jobs:** retries use capped exponential backoff with full jitter, `random(0, min(cap, base * 2^n))`, from the new `autumn_web::backoff` module. Callers that fail together no longer retry at the same instant. This applies to the HTTP client and to the `local`, `postgres`, `redis` and `sqlite` job backends. The `local` backend changes from equal jitter to full jitter. New settings: `[http.client] max_backoff_ms` (default 20 s) and `[jobs] max_backoff_ms` / `AUTUMN_JOBS__MAX_BACKOFF_MS` (default 1 h). The durable job backends had no cap before. A claim recovered after its visibility timeout (a worker that crashed or hung) is also requeued with this jitter, not at once (issue #3054).
- **http client:** a `503` with `Retry-After` now waits for the hint, as a `429` does. The wait is `backoff + min(hint, 5 s)`, so callers that get the same hint still spread out. A hint above 5 s is cut to about 5 s (issue #3054).

### Added

- **backoff:** new public module `autumn_web::backoff` (`full_jitter`, `full_jitter_ms`, `ceiling_ms`, `retry_after_wait`). New `RequestBuilder::retry_non_idempotent()` and `RequestBuilder::max_backoff(d)` (issue #3054).
- **webhooks:** `OutboundWebhookPlugin::with_max_attempts(n)` sets the number of delivery attempts. The default stays 5 (`DEFAULT_WEBHOOK_MAX_ATTEMPTS`). `WebhookOutboundManager` has `with_max_attempts(n)` and `max_attempts()` (issue #3054).
