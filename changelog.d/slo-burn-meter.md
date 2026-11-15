### Added

- **slo:** `slo_meter::BurnMeter` counts an SLO's good and bad events in
  process and reports its burn rate over any window. `firing(&BURN_WINDOWS[0])`
  asks the same question as the generated Prometheus alert, so an app can shed
  optional work or turn a feature flag off while its error budget burns.
  Memory is fixed by `retention / bucket`.
