### Fixed

- **auth:** generated account lockout logs `account_locked` once per lock
  (issue #2531). `login` and `reauth` share one guarded `stamp_lockout`
  helper. It reads the clock when it writes, so a slow request does not store an
  old `locked_at`.
