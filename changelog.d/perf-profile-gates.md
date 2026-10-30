### Added

- **ledger-capture:** a new off-by-default cargo feature. With
  `AUTUMN_LEDGER_CAPTURE_DIR` set, it records every SQL statement the
  framework issues, with its production call site and two generic plans
  (`EXPLAIN (GENERIC_PLAN)` as planned, and with `enable_seqscan = off`).
  Postgres 16 or later is needed for the plans. See `docs/perf/README.md`.

### Testing

- Every bench target now has a committed Bolt profile (callgrind + dhat) in
  `docs/perf/bolt/`, and every statement the Docker sweep issues has a Ledger
  profile in `docs/perf/ledger/queries/`. CI fails a new workload or query
  that has none: `scripts/check-perf-profiles.py`.
