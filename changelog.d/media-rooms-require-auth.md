### Security

- **Breaking:** **media rooms:** `POST {api_prefix}/rooms` and
  `POST {api_prefix}/rooms/{room_id}/join` now require an authenticated session
  (`#[secured]`); an anonymous caller gets `401` instead of creating a room or
  taking a seat. Leave, heartbeat and the roster keep their per-room session
  token. `autumn routes audit` reports both routes as `gated`. API clients must
  send the app's session cookie ([migration
  guide](docs/migrations/next.md#media-rooms-create-and-join-require-an-authenticated-session)).
