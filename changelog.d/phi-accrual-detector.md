### Added

- **phi_accrual:** `phi_accrual::PhiDetector` is a phi accrual failure
  detector. Feed it heartbeat arrival times with `heartbeat_at(now)` and it
  answers `phi_at(now)`: how surprised it is by the silence, as
  `-log10(P(the peer is still alive))`. Pick one threshold (8.0 by default)
  and every peer gets a timeout that follows its own heartbeat rhythm and
  jitter, with `with_window`, `with_min_std_dev` and `with_acceptable_pause`
  to tune it.
