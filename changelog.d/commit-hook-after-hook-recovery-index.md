### Changed

- **repository:** [no-plugin] the durable commit-hook worker no longer reads
  the whole `autumn_repository_commit_hooks` table, finished history
  included, on every 250 ms tick of every replica, in an app that registers
  `FRAMEWORK_MIGRATIONS`. The new migration
  `20261115143207_add_commit_hooks_after_hook_recovery_index` adds a partial
  index on `claimed_at` for rows in `pending_after_hook` or
  `after_hook_succeeded`, the predicate the standalone
  `repository_commit_hook_migrations` set already ships. The stale after-hook
  recovery reads about 1 buffer per tick instead of the whole heap (23,800 at
  1M rows). The build takes a SHARE lock on the table (about 80 ms at 1M
  rows), so hook writes wait for it.
