### Breaking Changes

- **Breaking:** `MetricsSnapshot` has a new field `db_connection_holds`
  (issue #3579). A `MetricsSnapshot` literal needs an update
  ([migration guide](docs/migrations/next.md)).

### Added

- **metrics:** `/actuator/metrics` has a `db_connection_holds` map (issue
  #3579). The key is the route, for example `"POST /census"`. Each sample is
  the time a request held a `Db` connection, from checkout to release.
  `MetricsCollector::record_db_connection_hold` records one.

### Deprecated

- **metrics:** the hold time that the `Db` extractor writes under
  `db_queries["<route> SELECT"]` (issue #3579). It is not a query time. A
  route that only writes also shows it. Read `db_connection_holds["<route>"]`
  instead. The next release stops this write. After that, the key holds only
  `SELECT` statements from `run_instrumented`.
