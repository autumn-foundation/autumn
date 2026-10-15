### Changed

- **ci:** the advisory gate now generates every `autumn new` flavor and audits
  the real app with its own `deny.toml` (issue #2466). The audit covers the
  app's own manifest, a fresh lockfile and its dev-dependencies.
