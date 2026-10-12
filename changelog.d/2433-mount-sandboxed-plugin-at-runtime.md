### Added

- **plugin-sandbox:** mount and unmount a sandboxed plugin on a running app
  (issue #2433). Give the app a `PluginMounts` with `AppBuilder::plugin_mounts`.
  Call `mount` and `unmount` at any time. The plugin serves its prefix at once,
  and the prefix returns 404 after an unmount. A request that is already
  running finishes and keeps its permit. A static route matches first, so
  ordinary routes cost nothing extra. `list` and `routes` show what is mounted.
