### Fixed

- **deploy:** a fleet rollout no longer ends green when `drain-old` fails and the
  old slot continues to run (#2279). The old slot runs job workers and the
  scheduler, so scheduled tasks and jobs could run two times. The rollout now
  retries `drain-old` one time and reads the old unit's `ActiveState`. A proven
  stop keeps the host degraded and the rollout continues. Any other result
  halts the rollout and compensates, and the output names the risk.
