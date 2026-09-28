### Fixed

- **plugin-sandbox:** a manifest route path carrying a literal non-ASCII
  character (e.g. `/hello/café`) is now refused at validation time. The
  router compares against the raw, percent-encoded path clients send, so the
  literal spelling mounted a route no client could reach while `plugin
  inspect` printed it as served (issue #2481). Write the route
  percent-encoded (`/hello/caf%C3%A9`) instead; the refusal names that
  spelling. Plain-ASCII routes are unaffected.
