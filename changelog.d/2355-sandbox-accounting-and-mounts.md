### Fixed

- **plugin-sandbox:** close the open findings of issue #2355.
  - A request body is now base64-encoded straight into the frame line. The
    per-request footprint counts the body 4 times, not 5.
  - A declared route on the configured OpenAPI, Swagger UI or MCP path is a
    `RouterBuildError` at build time. It no longer panics at `Router::nest`.
  - `autumn plugin inspect` now checks the HEAD that each declared GET also serves.
  - `fd_seek` on an unknown descriptor returns `EBADF` and is recorded as a
    denial.
