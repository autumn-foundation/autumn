### Changed

- **posture:** a `csrf_exemption_added` acknowledgment binds only to routes
  that serve an exempted URL (issue #2501). A route that a more specific route
  fully shadows no longer changes the digest.
