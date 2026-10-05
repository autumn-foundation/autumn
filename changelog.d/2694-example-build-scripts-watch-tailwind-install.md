### Fixed

- **examples:** `blog`, `bookmarks`, `bookmarks-distributed`, `reddit-clone`,
  `saas` (and its embedded `saas` starter), `teams`, `todo-app`, and `wiki`
  now watch `autumn setup`'s Tailwind install path and `PATH`/`PATHEXT` from
  their `build.rs`, matching the fix `cms` already had. Without it, a build
  that ran before `autumn setup` installed the Tailwind CLI produced no CSS
  and then never reran once the binary was installed, leaving the example
  permanently unstyled until an unrelated source edit retriggered the script
  (issue #2694).
