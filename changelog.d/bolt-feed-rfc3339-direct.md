### Changed

- **feed:** Atom feed timestamps are written straight into the output buffer
  instead of through chrono's formatter. Output is unchanged; a render takes
  about 18% fewer instructions and 31% fewer allocations. [no-plugin]
