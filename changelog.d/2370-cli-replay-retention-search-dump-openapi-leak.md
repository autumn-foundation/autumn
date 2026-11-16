### Fixed

- **cli:** `autumn replay`, `autumn retention --dry-run`, and `autumn search
  reindex` now clear an inherited `AUTUMN_DUMP_OPENAPI=1` from the child
  environment before spawning the app binary (issue #2370). Each of these
  commands already hand-listed the other one-shot mode vars dispatched
  before its own in `AppBuilder::run`, but the list predated `autumn openapi
  export` and was never updated when it shipped, so a leftover
  `AUTUMN_DUMP_OPENAPI=1` from an earlier `autumn openapi export` in the same
  shell could silently turn one of these three commands into an OpenAPI
  export instead.
