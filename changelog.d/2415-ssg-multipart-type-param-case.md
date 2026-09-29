### Fixed

- **SSG/ISR:** `normalize_content_type` no longer hard-codes `charset` as the
  only case-insensitive MIME parameter. A parameter whose value is itself a
  media type — `type` on `multipart/related` (RFC 2387 §3.1), `protocol` on
  `multipart/signed` and `multipart/encrypted` (RFC 1847 §§2.1, 2.2) — is now
  compared case-insensitively, so a harmless case-changing reserialization
  between `autumn build` and regeneration no longer freezes the route with a
  refused refresh (issue #2415).
