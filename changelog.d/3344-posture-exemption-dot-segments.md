### Fixed

- **posture diff:** a CSRF exemption acknowledgment now binds to a route that
  a request reaches through `..`, `.` or `%2e` segments (issue #3344). Before,
  such a route could change without a new acknowledgment.
