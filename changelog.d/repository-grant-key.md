### Added

- **repository:** `#[repository(.., mcp, grant = X)]` governs the generated MCP
  tools with an authority grant (issue #2451). Each tool carries the grant. The
  build fails if the grant does not list the model in `writes`.
