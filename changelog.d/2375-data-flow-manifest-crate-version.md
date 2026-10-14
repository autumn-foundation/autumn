### Breaking Changes

- **Breaking:** `model_path` in the data-flow manifest now includes the
  compatible version of the defining crate, for example
  `shop::Customer@2::Customer`
  ([migration guide](docs/migrations/next.md)). Make the manifest again. Commit
  it.

### Fixed

- **data-flow:** two incompatible versions of one crate in one binary no longer
  merge into one manifest row (issue #2375). A patch release does not change
  the key, except for a crate before 0.1
  and a prerelease.
