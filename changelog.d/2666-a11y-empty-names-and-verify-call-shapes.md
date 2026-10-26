### Changed

- **a11y:** an empty or blank accessible name trips a `debug_assert!` at the
  call site (issue #2666). This applies to `Img::new`, `Button`, `Link`,
  `MenuItem`, `SelectOption::new`, `Select::option`, `RadioOption::new`, and
  every `.label(..)`, `.aria_label(..)` and `.labelled_by(..)`. A release
  build does not change. Use `Img::decorative(src)` for an image that carries
  no information, and give a name from data a fallback. The new
  `autumn_web::a11y::is_blank` tells you when a text is blank
  ([migration guide](docs/migrations/next.md)).
- **scaffold:** the generated select-options loader of a `references` field
  uses the row id when the display column is blank, so no option has an
  empty name.

### Fixed

- **a11y:** a `RadioGroup` choice id is a valid CSS selector, so
  `hx-target="#…"` and `querySelector` work on it (issue #2666). If the first
  part of the id, after any leading `_`, does not start with a letter, the id
  gets one more `_` at the start: group `1plan` gives `_1plan-…`, and `_1plan`
  gives `__1plan-…`. Other ids do not change
  ([migration guide](docs/migrations/next.md)).

### Added

- **cli:** `autumn a11y verify` follows more calls when it finds the routes of
  a finding (issue #2666): a turbofish call (`sidebar::<Html>()`), a function
  passed by name (`.map(render_row)`), and a helper `fn` declared in the
  handler body. An item or a call under a `#[cfg(feature = "…")]` that is off
  no longer gives a route. The scan reads the default features of the nearest
  `Cargo.toml`. The new `--features`, `--all-features` and
  `--no-default-features` flags change them. Features change only the route
  attribution, not the findings or the exit code.
