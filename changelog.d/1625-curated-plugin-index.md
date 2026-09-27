### Added

- **plugin index (#1625):** a curated plugin index,
  `autumn-cli/plugin-index/index.toml`, seeded with the six first-party
  plugins. Each listing records the supported `autumn-web` range, the last
  `autumn plugin-check` result, the trust class (`full trust: native code` or
  a sandboxed capability manifest), and the #1601 API tier.
  `autumn plugin list` and `autumn plugin add` read it before crates.io. A
  crates.io result with no listing is marked `[unlisted: not verified]`. An
  experimental listing is marked `[EXPERIMENTAL API]`. `plugin add` prints
  the trust review before it changes a file, installs a listed community crate
  at its verified version (also with `--offline`), refuses a listing that
  failed re-verification, and does not wire a sandboxed listing (exit 2).
  `AUTUMN_PLUGIN_INDEX=<path>` selects another index file.
- **`autumn plugin index check` / `record` (#1625):** `check` is the listing
  gate: admission rules, and re-verification against the current release.
  `record` writes `plugin-check --format json` reports into listings: a pass
  lists, a fail flags `incompatible`, a second fail on a later release
  delists. CI re-verifies every listing (`plugin_index_reverify_listings`).
  Authors submit a listing by pull request; see `docs/plugins.md`.

### Changed

- **`autumn plugin-check` (#1625):** `route-attribution` skips, not fails,
  when the plugin mounts no routes and its contract proves it is registered
  under `--plugin-name` (a cache or search plugin). The JSON report carries
  the declared `contract`.
