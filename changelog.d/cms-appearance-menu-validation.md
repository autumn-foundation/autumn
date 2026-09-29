### Fixed

- **🧭 Wayfinder: validate and redisplay the "New menu" card in
  `examples/cms`'s Appearance screen (error-path 0/1 → 1/1, name/location
  preserved):** `Menu::name` declares `#[validate(length(min = 1, max =
  200))]`, but `create_menu` (`POST /admin/appearance/menus`) passed the
  submitted name straight to `content::replace_menu_at_location`, which
  writes the row through a raw `diesel::insert_into` that never runs the
  model's generated `validator::Validate` — so nothing enforced the bound
  anywhere. A blank or whitespace-only name (which the form's `required`
  attribute does not reject) fell back to `slugify`'s hashed-token slug and
  was silently persisted with an empty display name; an overlong one was
  persisted uncapped. Fix: `create_menu` now checks the trimmed name's
  length before writing, and on either that check or a
  `replace_menu_at_location` failure (a location race, or the slug
  allocator's own "too many menus share that name") redisplays the
  Appearance screen at 422 with the "New menu" card's name and location
  preserved and the message shown via the existing `role="alert"`
  convention, instead of silently persisting bad data or falling through to
  the generic error page. Covered by a new integration test asserting both
  the blank and overlong cases are refused, redisplayed with the location
  choice intact, and never reach the `menus` table.
