### Fixed

- **plugin-sandbox:** all sandboxed plugins together can now run at most 128
  guests at once (issue #2355). Before, one plugin with a high
  `max_concurrency` could fill Tokio's blocking pool and delay host work such
  as password hashing. A request over the limit gets 503 with `Retry-After`.
  Change the limit with `plugin_sandbox::set_guest_slots`, called once at boot.
