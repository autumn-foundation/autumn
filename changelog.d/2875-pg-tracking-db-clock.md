### Fixed

- **jobs:** the Postgres tracked-job store expires records by the database
  clock only (issue #2875). Clock skew between web and worker replicas no
  longer shortens or extends `jobs.tracking.ttl_secs`.
