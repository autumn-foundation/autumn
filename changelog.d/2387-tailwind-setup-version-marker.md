### Fixed

- `autumn setup` now records the installed Tailwind CSS pin in a
  `<target>/autumn/.tailwindcss.version` marker and re-downloads the binary
  whenever the marker is missing or names a different pin. Previously a pin
  bump printed "already installed" while silently keeping the old binary —
  now a pin bump replaces it without `--force`, a marker-less binary left by
  an older CLI is re-downloaded once (self-healing), and the fast path still
  skips the download when the marker names the current pin (issue #2387).
