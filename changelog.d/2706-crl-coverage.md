### Fixed

- **tls:** the mTLS server now fails fast at startup when the configured CRL
  does not cover every CA in the client-CA bundle (issue #2706). Once any CRL
  is configured, rustls denies handshakes whose revocation status is *unknown*,
  so a CRL published for only some of the bundle's CAs silently refused the
  clients of the rest — the availability trap in the documented CA rotation
  (old + new CA in one bundle, new CA's CRL published late). The startup error
  names the uncovered CAs; a trust-store reload that breaks coverage keeps the
  previous verifier instead of taking the listener down. `autumn doctor` grades
  the gap as a warning on the `tls_client_auth` check (an error under
  `--strict`), and the [TLS guide](../docs/guide/tls.md) documents the
  invariant: every CA in the bundle needs a CRL — an empty one counts.
