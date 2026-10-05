### Fixed

- **media:** `MediaPlugin` now installs only the primitives you enable
  (issue #1974). Before, `build` installed storage, the encode jobs and the
  retention sweep with no primitive enabled, and `with_broadcast()` did
  nothing. Now `with_broadcast()` installs `MediaMtxClient` and `MediaUrls`.
  A plugin with no primitive installs no routes, extensions or jobs, and logs
  a warning. If your app queues media jobs, call `with_broadcast()` or
  `with_rooms()`.
