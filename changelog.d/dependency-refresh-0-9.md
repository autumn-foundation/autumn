### Changed

- **deps:** dependency refresh ahead of 0.9.0. `opentelemetry`,
  `opentelemetry_sdk` and `opentelemetry-otlp` 0.33, `tracing-opentelemetry`
  0.34, `brotli` 9, `croner` 4, `css-inline` 0.23 and `getrandom` 0.4. Every
  other dependency moved to its newest release that still builds on the 1.88
  MSRV. `wasmi` stays on 0.40: 2.0.0 crashes on Windows when it and its caller
  are built at different optimization levels.
- **openapi:** `openapi.yaml` is now written with `serde-saphyr`, a safe-Rust
  YAML serializer, in place of the deprecated `serde_yaml`. The document is
  unchanged; the YAML layout of a few scalars (quoting) can differ.

### Security

- `unsafe-libyaml` (RUSTSEC-2024-0449, unmaintained) is no longer in the
  dependency graph, so its waiver is gone from the workspace `deny.toml` and
  from the `deny.toml` that `autumn new` writes.
