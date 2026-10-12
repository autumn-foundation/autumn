### Fixed

- **compression:** the match for already-compressed types (`audio/`, `video/`,
  archives, WOFF fonts) now ignores letter case (issue #2416). Before, the
  server compressed `Audio/MPEG` again.
