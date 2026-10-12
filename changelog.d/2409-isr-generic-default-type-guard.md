### Fixed

- **static:** ISR now refuses a regeneration that changes the `Content-Type`
  on extensioned routes whose handler returns a bare `String` or `Vec<u8>`
  (issue #2409). `autumn build` now stores the extension type in
  `manifest.json` (for example `text/css; charset=utf-8`) where it stored
  nothing. The served header does not change. Rebuild `dist/` to get the
  guard on existing routes.
