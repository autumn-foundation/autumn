### Changed

- **posture:** acknowledging a new CSRF exemption now covers only the routes
  that serve the exempt URLs (issue #2501). A route that a more specific route
  shadows no longer changes the digest.
