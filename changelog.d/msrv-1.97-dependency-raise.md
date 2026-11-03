### Breaking Changes

- **msrv:** **Breaking:** the MSRV is now Rust 1.97.0 (was 1.88.0). The
  maintainers set it. Dependencies need 1.95. 1.97.0 is less than 6 months
  old, so this is outside the STABILITY.md window
  ([migration guide](docs/migrations/next.md)).
- **http:** **Breaking:** `autumn-web` and `autumn-media-plugin` use `reqwest`
  0.13. Its types are in `http::ClientError`, `HttpShadowTransport::new`, the
  `oauth2` API and `recording_available`. `autumn-web` turns on
  `rustls-no-provider`, so an app's own `reqwest` client needs a TLS feature
  ([migration guide](docs/migrations/next.md)).

### Changed

- **deps:** every dependency that can move is raised: `webauthn-rs` 0.6,
  `p256` 0.14, `sha2` 0.11, `hmac` 0.13, `aes-gcm` 0.11, `lru` 0.19,
  `rustyline` 19, `sysinfo` 0.40, `serde-saphyr` 2,
  `postgresql_embedded` 0.22 and `aws-sdk-s3` 1.157. `autumn generate auth`
  stamps `sha2` 0.11, `rand` 0.10, `base64` 0.23, `aes-gcm` 0.11 and
  `webauthn-rs` 0.6, and warns when the app pins an older `rand` or
  `webauthn-rs`.
- **http:** `autumn-web` `reqwest` clients use `ring` and the Mozilla roots
  (`webpki-roots`), as before. They do not need a process-wide crypto
  provider. `autumn-media-plugin` clients use `aws-lc-rs` and the
  operating-system root store.
- **openapi:** `serde-saphyr` 2 no longer quotes a string with an inline `#`,
  so `openapi.yaml` can change by a few bytes.

### Removed

- **passkeys:** a new passkey app does not need OpenSSL. `webauthn-rs` 0.6
  uses pure-Rust crypto, so `autumn doctor` no longer asks Windows users for
  vcpkg.

### Security

- **push:** Web Push uses `p256` 0.14. Its `crypto-bigint` 0.7 fixes
  RUSTSEC-2026-0387 on the Web Push path. The `jsonwebtoken` ES256/ES384 path
  still has the advisory, so its waiver stays.
- **deps:** the waivers for `instant` (RUSTSEC-2024-0384) and `lru` 0.16
  (RUSTSEC-2026-0253) are gone, because neither is in the dependency graph
  now.
