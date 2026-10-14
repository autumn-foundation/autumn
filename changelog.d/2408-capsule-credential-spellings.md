### Fixed

- **capsule:** failure capsules now mask a credential split from its scheme by
  HTAB or Unicode space, and the quoted spelling of a quoted cookie value
  (issue #2408). A `Cookie:` header records at most 256 pairs.

### Security

- **deps:** bump `rustls` to 0.23.46, which fixes RUSTSEC-2026-0346 to
  RUSTSEC-2026-0349.
