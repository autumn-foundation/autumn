### Fixed

- **`plugin add` follow-ups (issue #2381):** `autumn plugin add` now queues the
  `Cargo.toml` edit before the `src/main.rs` mount edit, so a mid-install I/O
  failure leaves an inert dependency instead of an uncompilable mount; refuses
  with a clear error at a virtual workspace root (no `[package]` table) instead
  of appending a `[dependencies]` section the workspace cannot own; no longer
  anchors the mount to a helper method named `main` inside an `impl` block
  (only a top-level `async fn main` counts as the entry point); and the source
  scanner now recognises raw byte strings (`br"…"`) and raw C strings (`cr"…"`)
  so a probe inside one can never read as code.
