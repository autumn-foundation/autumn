### Fixed

- **cli:** on Windows, `autumn dev` no longer stalls about a minute per edit
  when the project's `autumn-web` is older than the CLI (issue #2458). The app
  now lists a `shutdown-signal` capability in its ready file. If the file has
  no such line, the CLI stops the app at once and tells you to run
  `cargo update -p autumn-web`.
