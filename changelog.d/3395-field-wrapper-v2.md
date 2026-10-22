### Added

- **forms:** `FieldOptions` and a `*_with` variant of each field helper,
  `RowScope` included (issue #3395). `FieldOptions` adds a hint
  (`{field}-hint`, named in `aria-describedby` before the error), the required
  state, typed attributes (`placeholder`, `autocomplete`, `inputmode`,
  `pattern`, `min`, `max`, `disabled`, `readonly`) and `data-*`/`hx-*`
  attributes. It has no setter for `id`, `name` or `aria-*`.
  `FormFor::field_options` sets options for one field. It works with every
  `FieldControl`.
- **forms:** `required_password_input`, `required_textarea_input`,
  `required_checkbox_input`, and `RowScope::required_number_input` /
  `required_textarea_input`.

### Changed

- **forms:** a required field, rich text included, shows a `*` marker in its
  label, with `aria-hidden="true"`. `required_text_input` now has the
  `autumn-field*` classes. A field with no hint and no error no longer renders
  an empty `aria-describedby=""`. `checkbox_input` shows the box before its
  label, the same as `form_for`. In `form_for`, a required textarea is now
  `required`.
- **widgets:** each `error_summary` entry links to `#field`. The summary has
  `id="error-summary"`, `tabindex="-1"` and `autofocus`, so focus moves to it
  when a submit fails.
- **ui:** the stylesheet shows the browser's invalid state with
  `:user-invalid`, and styles `.autumn-field__hint`, `.autumn-field__required`,
  `.autumn-field--checkbox` and a disabled control.

### Performance

- **forms:** the field helpers write their markup with one buffer. A 12-field
  form render makes 13% fewer allocations and allocates 16% fewer bytes.
