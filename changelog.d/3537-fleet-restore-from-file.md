### Added

- **fleet:** `DatabaseFleet::restore_from_file(key, src, LocalRestore)`
  puts a local backup back over the live database of a key, with no
  replication. It checks the integrity and the identity row of the file,
  applies migrations, stamps the epoch, and swaps the file while openers
  wait. With replication, it ships a new base snapshot. A backup of another
  key and a deleted key are refused unless `allow_rekey` or
  `clear_tombstone` is set. `autumn fleet restore <key> --from <file>`
  runs it from the command line (issue #3537).
