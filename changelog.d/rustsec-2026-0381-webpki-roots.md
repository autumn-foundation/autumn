### Security

- **deps:** `webpki-roots` 1.0.10, `webpki-root-certs` 1.0.10 and
  `rustls-webpki` 0.103.16 remove the distrusted emSign roots
  (RUSTSEC-2026-0381, RUSTSEC-2026-0382, RUSTSEC-2026-0383). An app made with
  `autumn new --bundled-pg` gets a `deny.toml` waiver for the
  `webpki-roots` 0.26.11 shim, which re-exports the fixed 1.x roots. Run
  `cargo update -p webpki-roots@1 -p webpki-root-certs -p rustls-webpki` in an
  existing app.
