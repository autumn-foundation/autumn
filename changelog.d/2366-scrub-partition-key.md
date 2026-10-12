### Fixed

- **`autumn db scrub`:** a partition-key column is now refused as PII (issue
  #2366, item 3). A rewrite through the parent moved every row to the partition
  of the new value: it aborted when none existed, and collapsed the table into
  one partition when one did. Declare the key `safe`.
