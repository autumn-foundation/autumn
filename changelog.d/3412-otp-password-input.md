### Added

- **form:** `otp_input`, `required_otp_input` and `otp_input_with` (issue
  #3412). One `<input autocomplete="one-time-code" inputmode="numeric">` over
  a row of boxes, so SMS autofill and paste work. `OtpFormat::digits(n)`
  sets the length (4 to 10). `.group(g)` shows the digits in groups.
  `FieldControl::Otp` renders it in `form_for`.
- **form:** password options on `FieldOptions`: `current_password()`,
  `new_password()`, `reveal()` (a "Show password" button with
  `aria-pressed`), `strength_hint()` (a live region in `aria-describedby`),
  and `reveal_label` and `strength_labels` for translations.
- **form:** `FieldOptions::minlength` and `maxlength`.
- **system tests:** `Page::paste(text)` pastes into the focused element.
- **generate:** `autumn generate auth` renders each password field with a
  reveal button and an `autocomplete` token. Signup, reset and the new
  password get a strength hint. The two 6-digit fields on `/account/2fa` use
  `otp_input_with`. In a new scaffold, a field id is its field name (for
  example `current_password`, not `current-password`).
