### Breaking Changes

- **Breaking:** **media:** `RoomStore::heartbeat` takes a `session_max`
  argument, and `RoomStore::roster` does too (issue #1974,
  [migration guide](docs/migrations/next.md)). A heartbeat never renews the
  room token past `joined_at + session_max`. After that, heartbeat and roster
  return the same opaque `404`; the client leaves, then joins again. Set it with
  `[media] room_session_max_seconds` (default 12 hours, at least
  `room_token_ttl_seconds`). `MediaConfig` has two new public fields.

### Added

- **media:** `[media] room_rate_limit_per_minute` limits each client IP on
  each room route (default `0`, off). It uses the core `#[throttle]` limiter,
  so `[security.trusted_proxies]` decides the client IP. An over-limit request
  gets `429` before its body is read. On create and join, the session check
  runs first, so an anonymous caller gets `401` (issue #1974).
- **deploy:** `autumn deploy up` installs MediaMTX 1.19.3 at
  `[media.mediamtx] binary_path` before cutover when nothing is there. It
  copies the binary out of the digest-pinned `bluenviron/mediamtx` image and
  checks its version. It installs `docker.io` with apt when Docker is missing.
  It never replaces a file that is there. Set `install_binary = false` to
  install it yourself (issue #1974).
- **doctor:** when `[deploy]` is set and `[media.mediamtx] enabled = true`,
  `autumn doctor` runs the two config-only media checks, and with `--online`
  all six over SSH (issue #1974).

### Fixed

- **deploy:** a failed kamal-proxy install no longer leaves a partial binary at
  `/usr/local/bin/kamal-proxy`. Before, the install shell ignored errors, so a
  failed copy was moved into place, and every later deploy refused to replace
  it. apt now waits up to 5 minutes for its lock (issue #1974).
