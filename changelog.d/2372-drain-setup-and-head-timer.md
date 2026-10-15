### Fixed

- **upgrade:** the drain of a fresh connection now ends at its head timer, not
  only at the 2 s grace (issue #2372). A stuck make-service no longer hangs the
  drain: after a shutdown, connection setup gets the same grace, then the
  connection closes.
