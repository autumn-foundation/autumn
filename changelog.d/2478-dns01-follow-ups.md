### Fixed

- **acme:** DNS-01 follow-ups (issue #2478).
  - Cleanup now tries every record. One failed zone no longer strands
    challenge records in the others.
  - Route 53 zone lookup reads all pages for the name. A public zone on a
    later page is found.
  - `autumn doctor --online` checks a host that a wildcard covers for address
    and port reachability. A wildcard-only config no longer warns.
