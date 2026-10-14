### Changed

- **cli [no-plugin]:** `autumn sbom --binary` now gives a clear error for a byte-swapped
  fat Mach-O header (`FAT_CIGAM`). Apple tools always write fat headers
  big-endian (issue #2394).
