### Fixed

- **deploy:** `autumn deploy status` does not name a release when the `current`
  symlink is dangling or does not point to a directory directly in `releases/`
  (issue #2277). The status shows the release as unknown and reports state
  drift. `--strict` exits non-zero.
