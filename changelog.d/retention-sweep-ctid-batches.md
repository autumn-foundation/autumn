### Performance

- **data-retention:** the Postgres retention sweep names each batch's rows by
  `ctid`, so the delete no longer looks every row up again through the primary
  key. A pass reads 51–62% fewer shared buffers on 3M-row framework tables
  (`job_history` 3,203,525 → 1,203,525), with the same rows deleted
  (`docs/perf/retention-sweep/`).
