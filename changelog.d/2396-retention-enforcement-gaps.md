### Fixed

- **retention:** the audit archive purge now rewrites the target of a symlinked
  `audit.path` and keeps the file owner. It fails if it cannot keep the owner
  (issue #2396).
- **retention:** the audit archive sweep is its own `per_replica` task,
  `autumn-retention-archive-sweep`. The fleet task no longer purges only one
  replica's file (issue #2396).
- **retention:** the report no longer implies enforcement it cannot do.
  `job_history` on a `redis` or `local` jobs backend says `NOT enforced`. A
  legal hold on `autumn_job_tracking` says `NOT preserved` off Postgres
  (issue #2396).
- **retention:** `autumn db retention --purge` is checked again inside the app,
  with the profile the app resolved (issue #2396).
- **migrations:** the standalone commit-hook migration set adds the retention
  sweep index (issue #2396).
