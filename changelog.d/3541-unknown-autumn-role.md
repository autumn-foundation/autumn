### Breaking Changes

- **Breaking:** an unknown `AUTUMN_ROLE` value stops the boot (issue #3541).
  Before, the app printed a warning and booted as `combined`, with user
  routes, all `#[job]` queues and the cron scheduler. The error names the
  value and lists the accepted values. An empty `AUTUMN_ROLE=` is still unset
  ([migration guide](docs/migrations/next.md)).

### Changed

- **config:** `role` in `autumn.toml` and `AUTUMN_ROLE` use one parser, so
  they give the same result. `role` is now case-insensitive, as
  `AUTUMN_ROLE` was. New API: `FromStr` for `ProcessRole`,
  `ProcessRole::ACCEPTED_VALUES`, `ProcessRole::parse_env_override` and
  `config::UnknownProcessRole`.
- **doctor:** `autumn doctor` fails the `process_role_backend` row on an
  unknown `AUTUMN_ROLE` or `role`, with the boot error text. It reads
  `AUTUMN_ROLE` and `AUTUMN_JOBS__BACKEND` from `.env` too, as the app does.
