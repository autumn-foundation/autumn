### Fixed

- **db:** a fleet host now renews its slot ranges at the same time (issue
  #3361). Each store call has one renew interval. A slow range fails with a
  time-out and no longer makes the other ranges lapse. The order of epochs
  between ranges is not fixed.
