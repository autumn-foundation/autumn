### Fixed

- **🧭 Wayfinder: redisplay the "Send Invitation" form on failure in
  `examples/teams` (error-path 0/3 → 3/3, email preserved):**
  `POST /invitations` — the admin-facing "Send Invitation" form on
  `/members` — discarded the submission on all three of its recoverable
  failure modes (a malformed email, an unrecognized role, and a duplicate
  pending invitation for the same email) onto a generic
  `application/problem+json`/error-page dead end, the same anti-pattern
  already fixed on this app's `/signup` and `/invite/{token}/accept` forms
  (Wayfinder, prior PRs) but missed here since `create_invitation` lives in
  a different module. `create_invitation` now redisplays the same members
  page at 422 on each of the three, with the submitted email (and role)
  preserved, via a `members_content` helper shared with `list_members`'s
  own GET render — so the admin only has to fix the one bad field instead
  of losing the whole roster view and re-entering the invite from scratch.
