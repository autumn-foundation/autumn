### Fixed

- **PATCH validation:** merged models now enforce `credit_card` and
  `non_control_character` rules. The workspace enables `validator`'s `card`
  and `unic` features so both attributes are available to `#[model]` structs,
  and a merged update runs them (issue #1751).
