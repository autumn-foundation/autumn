### Fixed

- **supply chain:** dropped the `RUSTSEC-2026-0381` waiver from `deny.toml`,
  `deny-sqlite.toml`, and the `autumn new --bundled-pg` scaffold template. The
  advisory's own metadata now marks `webpki-roots` 0.26.11 unaffected whenever
  its resolved `1.x` parent carries the fix, which this workspace's `1.0.10`
  copy already does — `cargo deny` confirmed no crate matches the advisory
  under either policy. A freshly scaffolded `--bundled-pg` app's `deny.toml`
  now ships one fewer (stale) waiver.
