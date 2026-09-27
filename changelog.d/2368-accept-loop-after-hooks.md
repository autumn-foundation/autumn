### Fixed

- **hot-upgrade:** an in-place-upgrade successor no longer starts its accept
  loop before its startup hooks have run (#2368). The inherited listening
  socket is still adopted up front (so a failure to adopt aborts early, as
  before), but the accept-loop future is only spawned once
  `run_startup_hooks` returns `Ok` — previously both processes accepted on the
  same socket while the successor's hooks were still running, and every
  connection the successor won in that window was answered by the startup
  barrier with a 503 while the healthy predecessor sat right there. The
  predecessor now keeps serving for the whole window, and a successor whose
  hooks fail exits having never competed for a connection. The deferral
  applies only to upgrade successors (`handoff_requested()`): a cold start
  still spawns its accept loop up front, as before, so `/live` and `/startup`
  stay reachable behind the startup barrier while hooks run.
