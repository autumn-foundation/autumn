### Fixed

- **static:** ISR now keeps its `Content-Type` guard on extensioned routes
  whose handler returns a bare `String` or `Vec<u8>` (issue #2409).
  `autumn build` records the extension type (for example
  `text/css; charset=utf-8`) instead of nothing. ISR applies the same
  normalization to fresh responses. Rebuild `dist/` to get the guard on
  existing routes.
