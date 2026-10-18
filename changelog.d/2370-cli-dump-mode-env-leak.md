### Fixed

- **cli:** every `autumn` subcommand that spawns the app binary in a one-shot
  dump mode (`routes`, `routes audit`, `openapi export`, `cache audit`,
  `data-flow`, `agents manifest`, `graph`, `jobs manifest`, `plugin-check`,
  `task`, the capacity ladder's route probe) now clears every *other* dump
  mode from the child's environment, not just the ones each call site
  happened to list by hand. An ambient `AUTUMN_DUMP_OPENAPI=1` (or
  `AUTUMN_BUILD_STATIC=1`, left over from an earlier `autumn build` in the
  same shell) could silently win over the mode a command actually asked for
  and hand it the wrong manifest instead of failing loudly (issue #2370).
