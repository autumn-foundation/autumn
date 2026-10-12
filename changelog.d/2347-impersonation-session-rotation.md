### Added

- **session:** rotation hooks (issue #2347). `AppBuilder::on_session_rotation`
  and `AppState::session_rotation_hooks` run code after a session-id rotation
  is stored. `autumn generate auth` registers a hook that re-points the
  tracked-session row, so impersonation no longer ends in a `401`.
- **auth:** `impersonation::rebind` keeps an impersonation record across a
  trusted same-user rotation.

### Fixed

- **auth:** an operator who impersonates a user now spends their own throttle
  budget, not the customer's. Per-user throttle buckets for impersonated
  traffic move from the customer to the operator.
