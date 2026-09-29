### Fixed

- **Job tracking store first-init race:** clearing the process-global job
  tracking store while another task installed one for the first time could
  drop that install (a lost update between `get()` and `set()`). Both paths
  now go through `OnceLock::get_or_init`, and a Loom model in `loom_models`
  covers the interleaving.
