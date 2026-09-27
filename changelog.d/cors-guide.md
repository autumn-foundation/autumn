### Documentation

- **cors:** the guide now documents the `[cors]` section, on a page of its own
  ([CORS and Cross-Origin Requests](../docs/guide/cors.md)). `allowed_methods`,
  `allowed_headers` and `allow_credentials` appeared in `docs/guide/` zero
  times, `allowed_origins` only in prose on two pages answering a different
  question, and the 162-entry guide index never said "CORS" — so the only
  heading in the corpus carrying the word was
  `what-happens-when.md`'s "What Happens When CORS Is Misconfigured?", a
  failure-mode page that says prod leaves the origin list empty without showing
  how to fill it. The new page covers all five keys and their defaults, the
  five `AUTUMN_CORS__*` overrides, the `dev`-seeds-`["*"]` /
  `prod`-leaves-it-empty split that makes a front-end stop working at deploy
  time, the preflight rules behind an `OPTIONS` failure, and the
  `allow_credentials = true` + `allowed_origins = ["*"]` pair Autumn rejects at
  config load.
