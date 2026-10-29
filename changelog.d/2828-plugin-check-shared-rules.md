### Added

- **plugins:** `plugin_conformance::is_sensitive_path`,
  `normalize_path_for_collision` and `is_plugin_asset_path` are public.
  `autumn plugin-check` now uses them, so the CLI and the library share
  one rule (issue #2828).
