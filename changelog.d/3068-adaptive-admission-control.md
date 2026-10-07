### Breaking Changes

- **Breaking:** `Route` has a new public field `criticality`, and
  `config::ServerConfig` and `config::HttpClientConfig` each have a new
  public field (issue #3068). A struct literal must set it or use
  `..Default::default()`. `capsule::schema::HttpErrorKind` has a new variant
  `ThrottledLocally` ([migration guide](docs/migrations/next.md)).

### Added

- **admission:** adaptive concurrency limit. Set
  `[server.admission] mode = "adaptive"` and `algorithm = "gradient2"`
  (default), `"vegas"` or `"aimd"`. The limit follows measured latency
  between `min_limit` and `max_limit`. `max_limit` defaults to the static
  ceiling (issue #3068, ADR 0016).
- **admission:** route criticality. `#[get("/x", criticality = "sheddable")]`
  (or `"critical"`, `"default"`). Under overload, the server rejects
  `sheddable` routes first and `critical` routes last. Set the share of the limit for
  each class under `[server.admission.partitions]` (defaults: `default = 1.0`,
  `sheddable = 0.5`).
- **admission:** the HTTP client sends the inbound request's criticality in
  `X-Autumn-Criticality` when it is not `default`. `RequestBuilder::criticality` sets it for one call.
  A server reads the header only with
  `server.admission.trust_criticality_header = true`.
- **http client:** client-side adaptive throttling (Google SRE), off by
  default. `[http.client.adaptive_throttle] enabled = true`. A rejected
  attempt returns `ClientError::ThrottledLocally`, which maps to `503`.
- **metrics:** `/actuator/prometheus` shows `autumn_admission_limit` and
  `autumn_admission_shed_total{criticality="..."}`.
