### Fixed

- **cli:** `autumn generate auth` forms now give every field an accessible
  name. Signup, login, password/email change, reauth, reset and resend forms
  used a sibling `<label>` with no `for=`, so screen readers announced
  unlabeled inputs and clicking the label did not focus the field
  (`autumn a11y verify` on a fresh scaffold: 15 serious `label` findings → 0).
