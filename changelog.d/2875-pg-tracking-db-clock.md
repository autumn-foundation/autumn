### Fixed

- **jobs:** the Postgres tracked-job store uses the database clock only to
  expire records (issue #2875). Clock skew between replicas no longer changes
  `jobs.tracking.ttl_secs`. The store clamps a TTL above 100 years to 100
  years.
