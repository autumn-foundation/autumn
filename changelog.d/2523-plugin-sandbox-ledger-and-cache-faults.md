### Fixed

- **plugin-sandbox:** a corrupt cache value now reaches a guest as a KV
  failure, not as a missing key (issue #2523).
- **plugin-sandbox:** `ActivitySummary::truncated` tells an operator that the
  log shed drop counts. `PluginJob::is_for_artifact` lets a runner refuse a job
  from an older artifact.
