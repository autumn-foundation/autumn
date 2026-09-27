### Fixed

- **deploy:** `autumn deploy status` no longer names a release for a dangling
  `current` symlink, or for a `current` that points outside `releases/`
  (issue #2277). It reports the release as unknown, counts it as state drift,
  and `--strict` exits non-zero.
