### Fixed

- **agent authority:** a raw `TRUNCATE a, b` or `DROP TABLE a, b` now records
  every table, so a grant for one table no longer allows the other (issue #2451).
- **agent authority:** the MCP outcome audit event names the principal a route
  guard resolved. It no longer says `agent:anonymous` for `#[secured]` tools.
  The attempt event still does, because it is written before dispatch.
