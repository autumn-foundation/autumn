### Added

- **custom domains:** `/actuator/health` now shows the issuance budget under
  `custom_domains.issuance_budget`: orders used this hour, the caps, when room
  returns, and the domains at their daily cap. A spent budget does not make
  health `Down` (issue #2644).
