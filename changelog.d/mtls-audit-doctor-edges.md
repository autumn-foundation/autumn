### Fixed

- **tls:** Startup now refuses a `[server.tls.client_auth]` `required_paths`
  entry that can never match a request path, such as `/users/{id}` or
  `/internal%2F` (issue #2706).
- **cli:** With `mode = "required"`, the posture manifest now shows
  `mtls_required: true` for every route (issue #2706). The posture diff no longer reports a
  `required_paths` change as widening when the new mode is `required`.
- **cli:** `autumn doctor` now fails on a `required_paths`, `crl_path` or
  `reload_interval_secs` value of the wrong type, and on a `required_paths`
  entry that startup would refuse (issue #2706).
