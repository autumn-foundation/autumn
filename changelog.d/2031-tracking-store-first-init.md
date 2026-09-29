### Fixed

- **Job tracking store first-init race:** resetting the process-global job
  tracking store while another task installed one for the first time could
  silently fail to clear it (the reset's `get()`-then-`set()` lost the race
  and its write was discarded). The reset now goes through
  `OnceLock::get_or_init` like the install, and a Loom model in `loom_models`
  covers the interleaving.
