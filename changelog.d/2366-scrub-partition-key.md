### Fixed

- **`autumn db scrub`:** a partition-key column is now refused as PII (issue
  #2366, item 3). A rewrite through the parent moved rows between partitions.
  It aborted when no partition fit. Declare the key `safe`.
