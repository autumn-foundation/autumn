### Fixed

- **db:** a fleet host now renews its slot ranges at the same time, and each
  store call has one renew interval (issue #3361). A slow range no longer
  makes the other ranges lapse.
