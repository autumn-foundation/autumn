### Added

- **server:** `[server.http]` connection limits (issue #3065):
  `header_read_timeout_ms`, `keep_alive_timeout_ms`, `max_header_bytes`,
  `http2_max_concurrent_streams` and `max_connections`. The server
  disconnects a slowloris client after the header-read timeout. See
  [Server Connection Limits](docs/guide/connection-limits.md).
- **server:** `autumn_web::http_server::serve`, the serve loop that applies
  these limits. `App::run` uses it for every listener.
- **server:** `autumn_web::http_server::KeepTunnel`. A handler on axum's own
  `WebSocketUpgrade` keeps it for the socket's life, so an HTTP/2 WebSocket
  is not closed by `keep_alive_timeout_ms`.
- **ws:** `[realtime]` WebSocket limits: `max_connections` (`503` above the
  cap), `max_message_bytes` (close code `1009`), `ping_interval_ms` and
  `idle_timeout_ms` (close code `1001`). See
  [WebSockets](docs/guide/websockets.md#limits).
- **db:** `database.replica_max_lag_ms`. The app measures replica lag. While
  the lag is over the limit or unknown, reads go to the primary. `/ready`
  reports the lag. See
  [Cloud-Native Autumn](docs/guide/cloud-native.md#lag-aware-reads).
- **db:** a boot warning when a database URL looks like PgBouncer, RDS Proxy,
  Supavisor or the Neon pooler (`database.warn_on_pooler`), and the
  [Running behind PgBouncer / RDS Proxy](docs/guide/connection-poolers.md)
  guide with a compatibility matrix.

### Changed

- **config:** the `prod` profile sets the `[server.http]` limits and the
  `[realtime]` message size, ping and idle limits. An HTTP/1 request head over
  64 KiB gets `431`. A WebSocket message over 1 MiB closes the socket with
  code `1009`. See the
  [migration guide](docs/migrations/next.md#prod-profile-connection-and-websocket-limits-issue-3065).

### Breaking Changes

- **Breaking:** `autumn_web::ws::WebSocket` and
  `autumn_web::ws::WebSocketUpgrade` are Autumn wrappers, not axum
  re-exports. `#[ws]` handlers do not change. Call `into_parts()` for the
  axum type and keep the returned `ConnectionHold` for the socket's life
  ([migration guide](docs/migrations/next.md)).
