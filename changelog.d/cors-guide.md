### Documentation

- **cors:** the guide now documents the `[cors]` section, on a page of its own
  ([CORS and Cross-Origin Requests](docs/guide/cors.md)). `allowed_methods`,
  `allowed_headers` and `allow_credentials` appeared in `docs/guide/` zero
  times, `allowed_origins` only in prose on two pages answering a different
  question, and the 162-entry guide index never said "CORS" — so the only
  heading in the corpus carrying the word was
  `what-happens-when.md`'s "What Happens When CORS Is Misconfigured?", a
  failure-mode page that says prod leaves the origin list empty without showing
  how to fill it. The new page covers all five keys and their defaults, the five
  `AUTUMN_CORS__*` overrides, and which default a given build actually gets —
  `dev` seeds `["*"]` while `prod` leaves the list empty, and with no
  `AUTUMN_ENV`/`AUTUMN_PROFILE`/`--profile` set that follows the build mode, so a
  debug binary is permissive and a release binary is closed. It also documents
  the `allow_credentials = true` + `allowed_origins = ["*"]` pair rejected at
  config load, the preflight rules behind an `OPTIONS` failure, three interactions
  a reader otherwise meets as an unexplained failure — under `prod` a mutating
  cross-origin request that sends its CSRF token in a header needs that header in
  `allowed_headers` (a form-field or query token, and a CSRF-exempt path, do not),
  a cached `#[static_get]` hit never reaches the CORS layer, and a caller served by
  another Autumn app is blocked by that app's own default
  `connect-src 'self'` before any CORS request is sent — and one limit
  with no configuration behind it: the built-in CSRF cookie is `SameSite=Lax` in
  code, so
  cross-site cookie-authenticated writes are not a shape the built-in stack
  supports.
