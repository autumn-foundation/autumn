### Fixed

- **autumn setup / autumn new:** `autumn setup`'s downloaded Tailwind CSS CLI
  is now reachable by `autumn dev` and the scaffold's generated `build.rs` on
  Windows, and whenever `CARGO_TARGET_DIR` is set (issue #2457). `setup` now
  installs to `cargo metadata`'s `target_directory` (the same resolution
  `dev` already used), instead of an unconditional `./target/autumn` — and the
  generated `build.rs` looks for the platform-correct binary name
  (`tailwindcss.exe` on Windows) in that same directory instead of a
  hardcoded, non-`.exe` `target/autumn/tailwindcss`. Before this, a missing
  Tailwind CLI is treated as optional, so both gaps failed silently: the app
  built and ran, just with unstyled/stale CSS and no warning that anything
  was wrong. `autumn doctor`'s `tailwind_binary` check resolves the same way,
  so it no longer reports the binary missing after a `CARGO_TARGET_DIR`
  install that actually succeeded.
