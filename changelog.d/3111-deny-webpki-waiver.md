### Fixed

- **supply chain:** an app made with `autumn new --bundled-pg` ships a
  `deny.toml` waiver for RUSTSEC-2026-0381. The `webpki-roots` 0.26 copy that
  the embedded-Postgres build stack pins has no fix. Cargo.lock now uses
  `webpki-roots` 1.0.10, `webpki-root-certs` 1.0.10 and `rustls-webpki` 0.103.16.
