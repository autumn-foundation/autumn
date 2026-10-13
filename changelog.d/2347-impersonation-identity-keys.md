### Added

- **auth:** `ImpersonationPolicy::target_session_values` swaps extra session
  keys (for example `user_email`) during impersonation (issue #2347).
  `end_impersonation` restores the operator's values. If the policy returns a
  reserved key or the auth key, `begin_impersonation` fails with `500`. The
  key `impersonator_identity_keys` is now reserved.

### Changed

- **admin:** the admin plugin passes layout state in one `AdminChrome` value
  (issue #2347). Page output does not change.
