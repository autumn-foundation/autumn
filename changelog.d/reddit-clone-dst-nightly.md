### Testing

- **sim:** a DST nightly for the reddit-clone example. Each seed runs the whole
  app: racing votes, virtual time, jobs, the scheduled rank sweep, replicas
  with skewed clocks, a crash, `SimNet`, the LLM stub and SMTP faults. The
  `sim-nightly.yml` workflow sweeps 4,000,000 seeds each night. A gate test
  makes the nightly call every public `autumn_web::sim` function. See
  `docs/guide/simulation-testing.md`, "The reddit-clone nightly".

### Fixed

- **reddit-clone:** a post whose `created_at` is more than 2 hours ahead of
  the clock (clock skew) no longer ranks as NaN, and no longer makes the
  scheduled hot-rank `UPDATE` fail for all posts. The age is now 0 or more.
  The DST nightly found it.
