### Changed

- **Contributor workflow (non-breaking):** a release note is written as its own
  file under `changelog.d/`, not as a bullet in `CHANGELOG.md`. Every open PR
  wrote to the top of the `## [Unreleased]` section, so every PR conflicted with
  every other PR over text that was never the point of either. One file per PR,
  and two PRs never edit one file. `scripts/check-changelog-fragments.sh` gates
  the shape and fails a PR that edits `CHANGELOG.md`;
  `scripts/update-changelog.sh` folds the files into the changelog at release
  time. The migration-guide and plugin-freshness gates read the fragments
  together with the changelog, so nothing they used to catch in review is
  caught later.
