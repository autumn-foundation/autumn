### Changed

- **ci:** the advisory gate now generates every `autumn new` flavor and audits
  the real app with its own `deny.toml` (issue #2466). It sees the app's own
  manifest, a fresh lockfile and dev-dependencies.
