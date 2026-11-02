### Added

- **form:** native input helpers (issue #3405). `switch_input`,
  `range_input`, `radio_group`, `checkbox_group`, `segmented_control`,
  `email_input`, `tel_input`, `url_input`, `time_input`, `month_input`,
  `color_input` and `hidden_input`, each with a `*_with` variant, and
  `required_*` variants where HTML allows `required`. `FieldControl` has a
  variant for each one, and `form_for` renders them. A group is a
  `<fieldset>` with a `<legend>`. A `#[model]` `NaiveTime` column renders
  `<input type="time">`.
- **generate:** the field DSL has `time` (`chrono::NaiveTime`, `TIME`),
  `phone` (alias `tel`), `color` and `money` (alias of `decimal{12,2}`). The
  scaffold form renders the matching native control.
