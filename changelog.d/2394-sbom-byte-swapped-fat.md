### Changed

- **cli:** `autumn sbom --binary` now explains why it refuses a byte-swapped
  fat Mach-O header (`FAT_CIGAM`): Apple tools always write fat headers
  big-endian (issue #2394).
