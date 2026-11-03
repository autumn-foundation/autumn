### Added

- **roles:** `RoleCapabilities` and `Capability` (issue #3542). A process
  resolves one capability set at boot from its role. Each background
  subsystem reads its own field: `job_workers`, `scheduler`, `outbox_relay`,
  `commit_hooks`, `fleet_relay`, `restore_check` and `retention`.
  `serves_http` is the HTTP field. Read the set with `state.capabilities()`,
  and ask about one subsystem with
  `state.capabilities().runs(Capability::Scheduler)`. `combined`, `web` and
  `worker` resolve to the same subsystems as before. No app changes are
  necessary.
- **actuator:** `/actuator/info` shows the process role and its resolved
  capabilities under a new `role` key.
