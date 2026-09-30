### Added

- **`Db::tx_immediate`:** an explicit immediate-mode transaction for
  write-heavy closures (issue #2885). On SQLite it begins with
  `BEGIN IMMEDIATE` — issued through diesel's transaction manager, so nested
  savepoints keep working — taking the write lock up front. A concurrent
  writer on a file database then queues on the pool's `busy_timeout` instead
  of failing its deferred read→write snapshot upgrade with
  `SQLITE_BUSY_SNAPSHOT`. `Db::tx` deliberately stays deferred so read-only
  transactions keep their read concurrency; `ShardedDb` gains the same
  `tx_immediate` delegate. Building a pool over a `cache=shared` target now
  logs a loud boot warning: shared-cache lock contention returns
  `SQLITE_LOCKED` without consulting the busy handler (so `tx_immediate` does
  not help there), and the warning steers operators toward WAL-mode file
  databases.
