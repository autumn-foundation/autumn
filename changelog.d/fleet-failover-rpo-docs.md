### Documentation

- **fleet:** the fleet guide, the SQLite production guide and the
  `restore_missing` config docs now say that a takeover after a crash can lose
  up to `rpo_secs` of committed writes whose requests already returned
  success. Replication bounds that loss; only a planned move loses nothing.
