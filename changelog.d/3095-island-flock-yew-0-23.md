### Changed

- **examples [no-plugin]:** `island-flock` now uses yew 0.23 (issue #3095). The committed
  `flock` island bundle is rebuilt. This removes `anymap2` and the
  RUSTSEC-2026-0319 waiver. The `bincode` and `proc-macro-error` waivers stay,
  because yew 0.23 still uses them.
