### Fixed

- **auth:** generated account lockout logs `account_locked` once per lock
  (issue #2531). `login` and `reauth` share one guarded `stamp_lockout`
  helper. It reads the clock at write time, so a slow request cannot save an
  old `locked_at`.
