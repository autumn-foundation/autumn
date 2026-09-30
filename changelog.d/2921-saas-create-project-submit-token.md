### Fixed

- **examples/saas:** the dashboard's "Create Project" form now embeds a
  one-time `_submit_token` like the signup form already did, so a
  double-clicked or browser-retried submission creates exactly one project
  instead of duplicate rows (issue #2921).
