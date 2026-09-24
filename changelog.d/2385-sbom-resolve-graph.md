### Fixed

- **`autumn sbom`:** the document now walks `cargo metadata`'s resolve graph
  instead of inventorying every resolved package (issue #2385).
  Dev-dependencies — resolved but never linked into a shipped artifact — no
  longer appear, and `--manifest-path <member>` on a virtual workspace now
  describes the named member rather than the whole workspace.
