### Added

- **widgets:** tags mode for `autocomplete_input` (issue #3410).
  `AutocompleteConfig::multiple()` shows each pick as a removable chip.
  Each chip submits one hidden `name=value` input, so the repeated keys
  decode into a `Vec<T>`. Backspace in an empty input removes the last
  chip. The arrow keys move between chips, and Delete removes one. With
  `free_text()`, Enter adds the typed text. The no-JS fallback is a
  `<select multiple>`.
- **widgets:** `autocomplete_input_for(changeset, …)` pre-fills the chips
  (or the single value) and the field errors from a `Changeset`.
  `AutocompleteConfig::selected` sets the first chips. `remove_label`,
  `added_message` and `removed_message` translate the remove buttons and
  the status text.
- **widgets:** `autocomplete_create_option(value, label)` renders a
  "create new" entry for tags. A "Tags input" story shows the mode.

### Changed

- **widgets:** the `autumn-widgets.js` size budget is now 52 KiB raw and
  15 KiB gzip (from 46 KiB and 14 KiB), for the tags runtime.

### Breaking Changes

- **Breaking:** **widgets:** `AutocompleteConfig` has the public fields
  `multiple`, `selected`, `remove_label`, `added_message` and
  `removed_message` (issue #3410). A struct literal must set
  them. `AutocompleteConfig::new` sets them
  ([migration guide](docs/migrations/next.md)).
