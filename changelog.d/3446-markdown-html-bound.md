### Added

- **markdown:** `render_user_content_html_bounded` fails when the HTML
  before sanitizing exceeds a byte limit (issue #3446). The Constela
  `markdown` node uses it with the remaining output budget.

### Fixed

- **markdown:** `render_user_content_html` reads at most 1,048,576 parser
  events (issue #3446). Later events are dropped.
