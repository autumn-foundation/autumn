### Fixed

- **cli:** on Windows, `autumn dev` no longer stalls about a minute per edit
  when the project's `autumn-web` is older than the CLI (issue #2458). The app
  now writes a `capability shutdown-signal` line to its ready file. If an app
  writes a budget-only ready file, the CLI stops it at once and tells you to
  run `cargo update -p autumn-web`.
