### Documentation

- **db:** the SQLite fleet guide has a new section, "What a failover costs".
  Replication is asynchronous, so a failover to another volume can lose
  acknowledged writes: at most `rpo_secs` in steady state. After a crash,
  another host serves the range at most `lease_secs + lease_secs / 3` after
  the last renew. A fail-back restores a stale file without
  `restore_missing`. The rustdoc of `rpo_secs`, `restore_missing` and
  `lease_secs` says the same.
