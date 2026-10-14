### Fixed

- **agent authority:** the `#[agent_operable]` analyser now refuses more
  escape shapes (issue #2451): futures assigned after declaration or shadowed
  in a block, opaque macros with no handle, named futures given to `timeout`,
  a spoofed `commit`, `sleep` or `cache`, callees that are not paths, diesel
  writes through an aliased or unresolved table, and branch-dependent
  assignments. A data-changing CTE records every table it writes. A declared
  effect stays `declared` when a proof matches it. Qualified job paths and
  function-item aliases no longer give false positives.
- **agent authority:** the manifest keeps the first route per `operation_id`,
  as the MCP server does. A skipped route shows `not-exposed` with the
  `duplicate-operation-id` caveat, and no longer trips the `--check` gate.
- **Migration:** bare `sleep(..)`, `session.x()`, `cache.x()` and
  `tx.commit()` on untyped values are now refused. Use `tokio::time::sleep`,
  a typed parameter, or `#[agent_effect(...)]`.
