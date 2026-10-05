### Fixed

- **media:** `MediaPlugin` now installs only the primitives you enable
  (issue #1974). Before, it installed storage, the encode jobs and the
  retention sweep with no primitive enabled, and `with_broadcast()` did
  nothing. Now `with_broadcast()` installs `MediaMtxClient` and `MediaUrls`
  extensions, and a plugin with no primitive installs nothing and logs a
  warning. If your app queues media jobs, enable a primitive.
