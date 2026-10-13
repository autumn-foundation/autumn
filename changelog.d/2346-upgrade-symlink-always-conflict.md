### Changed

- **upgrade:** [no-plugin] `autumn upgrade --check` now reports a symlinked scaffold file
  as a `conflict` even when its target matches the template (issue #2346). The
  verdict no longer changes when a file outside the project changes. To accept
  a deliberate link, run `autumn upgrade --accept <path>`.
