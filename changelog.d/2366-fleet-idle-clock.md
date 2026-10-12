### Fixed

- **sqlite fleet:** a database's idle clock now starts when it finishes opening,
  not when it is claimed. A slow open could make a new database look idle at
  once.
