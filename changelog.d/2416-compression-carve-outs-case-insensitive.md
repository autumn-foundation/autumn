### Fixed

- **compression:** the already-compressed type carve-outs (`audio/`, `video/`,
  archives, WOFF fonts) now match in any letter case (issue #2416). Before,
  `Audio/MPEG` was compressed again.
