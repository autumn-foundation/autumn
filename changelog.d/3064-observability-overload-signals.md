### Breaking Changes

- **Breaking:** `autumn_http_request_duration_seconds` is now a histogram ([migration guide](docs/migrations/next.md)).
  It has `method`, `route` and `status_class` labels (issue #3064). The old
  quantile lines move to the deprecated
  `autumn_http_request_duration_quantiles_seconds` summary.
- **Breaking:** `TelemetryConfig` and `OtlpTraceRuntime` have a new `sample_ratio` field ([migration guide](docs/migrations/next.md)).
  A struct literal must set it or use `..Default::default()`.

### Deprecated

- **observability:** `autumn_http_request_duration_quantiles_seconds`. Use the
  `autumn_http_request_duration_seconds` histogram. A later release removes it.

### Added

- **observability:** `/actuator/prometheus` exports the job queue depth, the
  age of the oldest ready job, dead letters per job, the DB pool size,
  available and waiting counts, and a DB pool wait histogram (issue #3064).
- **observability:** a trusted proxy can set `X-Request-Id`. The app keeps a
  well-formed id. `http_client` sends the request id on outbound calls;
  `RequestBuilder::without_request_id` turns this off.
- **telemetry:** `telemetry.sample_ratio` sets a parent-based ratio sampler.
  `http_client` opens one CLIENT span per attempt. Request logs carry
  `trace_id` and `span_id` when an OpenTelemetry span is active.

### Changed

- **observability:** a non-standard HTTP method is counted as `_other` in the
  metrics, also in the `/actuator/metrics` JSON route keys. This bounds the
  memory that the per-route metrics use.
