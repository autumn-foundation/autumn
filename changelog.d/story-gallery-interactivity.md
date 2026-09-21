### Fixed

- **stories:** the `/_stories` gallery loaded its own `data-*`-hook runtime
  but never htmx itself, so every `hx-get`/`hx-post`-driven widget preview
  (active search, autocomplete, reactions, comment threads, the infinite
  feed sentinel) rendered correct `hx-*` attributes that nothing on the page
  processed. Now loads htmx, and the Active search / Autocomplete stories
  get small live demo backends (namespaced under `/_stories/demo/*` so they
  can never collide with an app's own routes) so typing actually works;
  every other story's action URL stays the existing synthetic
  404-on-submit. [no-plugin]: fixes an existing internal dev gallery's own
  behavior; no new API, widget, or agent-facing surface.
