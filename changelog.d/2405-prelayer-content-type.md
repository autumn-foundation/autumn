### Fixed

- **SSG/ISR: `autumn build` no longer renders through the app's custom Tower
  layers, so a `Content-Type`-rewriting layer stops freezing ISR (#2405).**
  The build used to record the *post-layer* `Content-Type` while ISR
  regeneration saw the *pre-layer* response, so the #2400 type guard refused
  every refresh for such an app and the route froze until the next build. The
  build now renders through the pre-layer router — user layers drained via the
  new `partition_custom_layers_for_static_render`, the same partition the SSG
  serve path applies — so the manifest records the handler's type and the body
  on disk is the handler's body. The serve path still applies those layers to
  the cached response at request time, which also ends the old
  double-application (once at generation, once per request) for any
  body-rewriting layer. Apps without a `Content-Type`-rewriting layer are
  unaffected: build and ISR already agreed on the type. The i18n
  `AmbientLocaleLayer` and its bundle `Extension` stay on the pre-layer router
  (the extension is what `Locale::from_request_parts` reads the bundle from),
  so translated `#[static_get]` handlers still render localized text rather
  than raw translation keys.
