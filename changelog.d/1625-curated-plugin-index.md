### Added

- **plugin index (#1625):** a curated plugin index,
  `autumn-cli/plugin-index/index.toml`, seeded with the six first-party
  plugins. Each listing records the supported `autumn-web` range, the last
  `autumn plugin-check` result, the trust class and the #1601 API tier. The
  trust class is `full trust: native code`, or a sandboxed capability
  manifest.
- **`autumn plugin list` / `add` (#1625):** both read the index before
  crates.io. A crates.io result with no listing is marked
  `[unlisted: not verified]`. An experimental listing is marked
  `[EXPERIMENTAL API]`. `plugin add` shows the trust review before it changes
  a file. It installs a listed community crate at its verified version, pinned
  with `=`, also with `--offline`. It refuses a listing that failed
  re-verification. For a sandboxed listing it changes no file and exits 2.
  `AUTUMN_PLUGIN_INDEX=<path>` selects another index file.
- **`autumn plugin index check` / `record` (#1625):** `check` is the listing
  gate: admission rules, and re-verification against the current release.
  `record` writes `plugin-check --format json` reports into listings. A pass
  lists. A fail flags `incompatible`. A second fail on a different release
  delists. CI re-verifies every listing (`plugin_index_reverify_listings`) and
  uploads the updated index. Authors submit a listing by pull request; see
  `docs/plugins.md`.

### Changed

- **`autumn plugin-check` (#1625):** routes are attributed to
  `Plugin::name()`, by default the type path. When routes carry only the
  contract's registered name, the route checks use that name, and the report
  keeps `--plugin-name`. `route-attribution` skips when no route carries
  either name (a cache or search plugin). The JSON report carries the declared
  `contract`.
