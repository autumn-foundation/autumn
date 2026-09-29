### Added

- **cli:** `autumn explore` (PR #1580) [no-plugin] — an interactive ratatui TUI for
  browsing and searching the app's route table. Compiles the app and
  introspects its routes like `autumn routes`, then presents them in a
  scrollable, incrementally-searchable table (filter by method, path, handler,
  source, or middleware) with a details panel showing source, middleware,
  API version, and sunset status for the selected route.
