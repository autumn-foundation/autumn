### Security

- **idempotency:** `.idempotent()` scopes its cache by the bearer principal.
  A request with no cookie-backed session used to carry no principal in its
  storage key, so two token clients that sent the same `Idempotency-Key` and
  body to a route that checks the token in the handler shared one cached
  response, and the second never reached the handler. The key now carries the
  principal `RequireApiToken` verified, or else a SHA-256 digest of the raw
  `Authorization` header. Session-only apps keep the keys they had
  ([migration guide](docs/migrations/next.md)).

### Changed

- **idempotency:** `RequireApiToken` registered with `AppBuilder::layer` or
  `static_gate` no longer forces fail-closed replay. A token client's retry
  replays its own response instead of answering `409`, and keeps replaying
  when the client rotates its token. Token APIs, agents and MCP clients can
  use `.idempotent()` ([idempotency guide](docs/guide/idempotency.md#bearer-token-clients)).

### Added

- **routes:** `#[post("/x", idempotent = false)]` keeps the app-level
  idempotency layer off one route, for a route that dedupes by its own key,
  such as a durable workflow route. A hand-built `Route` uses
  `RouteIdempotency::Off`.

### Breaking Changes

- **Breaking:** `RouteIdempotency` has a new `Off` variant. An exhaustive
  `match` needs an arm for it ([migration guide](docs/migrations/next.md)).
