### Fixed

- **CLI diagnostics:** `autumn setup`/`autumn build`/`autumn routes` no
  longer swallow `cargo metadata`'s own error output when it fails — they
  print `cargo`'s stderr alongside the existing "Failed to read cargo
  metadata" message instead of exiting with no indication of why.
