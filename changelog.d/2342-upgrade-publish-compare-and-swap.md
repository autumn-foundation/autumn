### Fixed

- **upgrade:** `autumn upgrade` no longer loses a concurrent write (issue
  #2342). A scaffold file that changes while `--apply` writes is refused, not
  replaced. Two `--accept` commands at the same time keep both pins.
