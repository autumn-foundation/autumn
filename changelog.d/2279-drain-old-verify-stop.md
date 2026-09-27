### Fixed

- **deploy:** a fleet rollout no longer reports success when `drain-old` fails
  and the old slot can still run (#2279). The old slot runs job workers and the
  scheduler, so scheduled tasks and jobs could run two times. The rollout now
  retries `drain-old` one time, then reads the old unit. If the unit is stopped
  and disabled, the host is degraded and the rollout continues. All other
  results halt the rollout and roll back the hosts on the new release (with
  `--no-rollback`, they stay as they are). Such a run now exits non-zero, where
  it exited 0 before. The output names the risk, and so does the halt alert
  while the host stays on the new release.
