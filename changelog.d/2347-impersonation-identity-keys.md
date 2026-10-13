### Added

- **auth:** `ImpersonationPolicy::target_session_values` swaps extra session
  identity keys (for example `user_email`) during impersonation
  (issue #2347). `end_impersonation` restores the operator's values. A
  reserved key, or the auth key, makes `begin_impersonation` fail with `500`.
