### Fixed

- **🧭 Wayfinder: redisplay the Users screen row on `update`/`delete`
  failure in `examples/cms` (error-path 0/2 → 2/2, role selection
  preserved):** `POST /admin/users/{id}` (the inline role-change "Save"
  button) and `POST /admin/users/{id}/delete` both `?`d every failure from
  `content::with_administrator_guard` — including the realistic "This is
  the only administrator account; promote another user first" and a
  stale-email rejection — straight to the framework's generic
  `application/problem+json` error page, discarding the role change an
  administrator had just made. This is the exact anti-pattern this file's
  own `create` handler (the "Add user" card) was already fixed for; `update`
  and `delete` were simply missed when that fix landed. Both handlers now
  redisplay the Users table at 422 with a persistent, adjacent
  `role="alert"` message under the affected row instead — `update` also
  re-selects the role the administrator attempted rather than silently
  reverting it. A corrected resubmission (promote another administrator
  first, or fix the email) still succeeds, and the administrator never loses
  the account table they were looking at.
