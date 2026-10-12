### Added

- **plugin-sandbox:** mount and unmount a sandboxed plugin on a running app
  (issue #2433). Give the app a `PluginMounts` with `AppBuilder::plugin_mounts`.
  Call `mount` and `unmount` at any time. The prefix answers after a mount and
  returns 404 after an unmount. A running request finishes and keeps its
  permit. A static route matches first, so ordinary routes have no added cost.
  A framework prefix is refused. `list` and `routes` show what is mounted.
