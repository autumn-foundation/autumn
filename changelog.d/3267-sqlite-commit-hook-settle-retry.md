### Fixed

- **db:** the SQLite commit-hook worker now retries a failed ack or nack write.
  A write lost to a shared-cache table lock left the hook `running` for 60 s.
