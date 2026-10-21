### Fixed

- **cli:** `autumn generate auth` forms now bind each `<label>` to its input
  with `for`/`id` (issue #3170). Screen readers can name the fields. The 2FA
  confirm input gains a label.
