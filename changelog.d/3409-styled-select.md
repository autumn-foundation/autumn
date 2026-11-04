### Added

- **forms:** select with `<optgroup>`, `multiple` and an opt-in style
  (issue #3409). See `docs/guide/forms.md#select-groups-multiple-and-style`.
  - `grouped_select_input`, `grouped_select_input_with` and
    `required_grouped_select_input`: top-level options, then one
    `<optgroup>` for each group. `FieldControl::GroupedSelect` renders the
    same select in `form_for`.
  - `FieldOptions::multiple()`: `multiple` on a `<select>` or a file input.
    A multiple select marks each option whose value is in
    `Changeset::field_values`. Bind it to a `#[serde(default)] Vec<T>`. In
    `form_for`, the model `required` flag does not make it required. A
    widget-pack control gets `multiple` from `FieldContext::control`.
  - `FieldOptions::styled_select()`: the `autumn-select` class. A browser
    that supports `appearance: base-select` draws the select from the design
    tokens. Other browsers keep the native control. There is no JavaScript.

### Fixed

- **forms:** a single select marks only the first option that matches. Two
  options with the same value no longer get two `selected` attributes.
