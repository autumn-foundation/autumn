### Breaking Changes

- **Breaking:** **media:** `RoomStore::heartbeat` takes a `max_session`
  argument (issue #1974, [migration guide](docs/migrations/next.md)). A
  heartbeat never renews the room token past `joined_at + max_session`; after
  that it returns the same opaque `404`, and the client must join again. Set it
  with `[media] room_session_max_seconds` (default 12 hours, at least
  `room_token_ttl_seconds`). `MediaConfig` has two new public fields.

### Added

- **media:** `[media] room_rate_limit_per_minute` limits each client IP on
  each room route (default `0`, off). It uses the core `#[throttle]` limiter,
  so `[security.trusted_proxies]` decides the client IP. An over-limit request
  gets `429` before its body is read (issue #1974).
- **deploy:** `autumn deploy up` installs MediaMTX 1.19.3 at
  `[media.mediamtx] binary_path` before cutover when nothing is there. It
  copies the binary out of the digest-pinned `bluenviron/mediamtx` image and
  checks its version. It never replaces a file that is there. Set
  `install_binary = false` to install it yourself (issue #1974).
- **doctor:** with `[media.mediamtx] enabled = true`, `autumn doctor` runs the
  two pure media checks, and with `--online` all six over SSH (issue #1974).
