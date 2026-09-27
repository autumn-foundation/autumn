### Fixed

- **docs:** `autumn generate policy --help` and the generator's module rustdoc
  both claimed that, with no owner column on the model, `can_update`/
  `can_delete` "default-deny … safe-by-default until then". They have not since
  issue #1830: with no owner column to key a rule on, both fall back to an
  authentication check, so any signed-in user may update or delete any row. The
  generated file said so all along, under a `SECURITY TODO` marker; the two
  places a reader looks *before* generating said the opposite, and said it in
  the reassuring direction. Both now describe what is emitted.

### Documentation

- **docs:** document `autumn generate policy` in the
  [code generators guide](docs/guide/generators.md). The command that writes
  the `Policy`/`Scope` pair appeared nowhere in the guide, so readers of the
  authorization guide hand-wrote what a generator emits. The new section covers
  both owner-column cases, and `docs/guide/authorization.md` now links to it.
