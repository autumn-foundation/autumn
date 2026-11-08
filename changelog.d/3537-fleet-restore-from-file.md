### Added

- **fleet:** `DatabaseFleet::restore_from_file(key, source, LocalRestore)`
  puts a local backup in place of the database of a key. Replication is not
  needed. The restore checks the integrity and the identity row of the
  file, applies migrations by the boot policy, stamps the epoch, and swaps
  the file while openers wait. With replication, it ships a new base
  snapshot. A backup of another key is refused unless `allow_rekey` is set.
  A deleted key with a tombstone is refused unless `clear_tombstone` is set.
  `autumn fleet restore <key> --from <file>` runs it from the command line
  (issue #3537).
