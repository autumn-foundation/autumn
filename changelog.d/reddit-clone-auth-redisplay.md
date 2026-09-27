### Fixed

- **🧭 Wayfinder: redisplay signup/login forms on failure in
  `examples/reddit-clone` (error-path 0/8 → 8/8, username/email
  preserved):** an error-path inventory of `reddit-clone`'s auth flow — the
  README's own "Canonical feature showcase" example, and the entry point
  every visitor hits before they can post, vote, or comment — found all 8 of
  its recoverable failure modes (registrations closed, invalid/out-of-range
  username, invalid password, invalid email, duplicate username at both the
  pre-check and the insert, plus unknown username and wrong password at
  login) sent through a bare `Err(AutumnError::unprocessable_msg(...))` /
  `Err(AutumnError::bad_request_msg(...))`: 0 of 8 was adjacent to cause,
  persisted in place, said how to recover, or preserved what the user
  typed — every rejected submission bounced the visitor to the framework's
  generic error page, losing the username and email they had just typed.
  This is the same anti-pattern already fixed in `saas`/`teams`'s auth forms
  (#2530, #2554) and `reddit-clone`'s own create-community form (#2665) —
  everywhere except its own signup and login. Fix: `register`/`login`'s
  markup moves into `register_page`/`login_page` render functions, called by
  both the `GET` handlers and every failure branch of the `POST` handlers,
  which now re-render the same form at HTTP 200 with the submitted
  username/email preserved (length-bounded before being echoed back) and the
  message(s) shown adjacent to the form via the existing `role="alert"`
  convention, instead of navigating away. The duplicate-username race
  between the pre-check and the insert is classified precisely on Postgres's
  `users_username_key` unique-violation (via the framework's own
  `unique_violation_field`, the same helper `examples/teams` uses) rather
  than any insert error, so a real mid-transaction failure still propagates
  as the 500/503 it is instead of rendering a fake "username already taken"
  page. No redesign, no new dependency, the existing Tailwind classes
  untouched. Verified with new unit tests on `register_page`/`login_page`
  covering preserved values, multi-message rendering, and the clean
  (no-error) render (`cargo test -p reddit-clone --lib routes::auth`).
  `cargo clippy -p reddit-clone --all-targets -- -D warnings` and
  `cargo fmt --all -- --check`: clean.
