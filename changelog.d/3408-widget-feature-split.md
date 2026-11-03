### Breaking Changes

- **Breaking:** the widgets are in feature-gated families (issue #3408,
  ADR 0033). `widgets-search` (active search, autocomplete),
  `widgets-social` (reaction controls, comment thread, infinite feed),
  `widgets-workflow` (transition controls, bulk actions) and
  `widgets-charts` (sparkline, bar chart, line chart) are default-on. A
  default build does not change, and every `autumn_web::widgets::*` path
  stays the same. An app with `default-features = false` must name the
  families it uses. The `#[commentable]` router needs `widgets-social`
  ([migration guide](docs/migrations/next.md)).

### Performance

- **widgets:** an app without a family does not compile its code, and
  `/static/css/autumn-widgets.css` and `/static/js/autumn-widgets.js` do not
  include its CSS or script. The core stylesheet is 15% smaller and the core
  script is 13% smaller.

### Fixed

- **stories:** the `/_stories/demo/save` route is in the route listing and in
  the OpenAPI/MCP path check. One list now drives the gallery's demo routes.
