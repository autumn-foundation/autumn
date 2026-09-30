### Fixed

- **Graceful shutdown / in-place upgrade:** a connection accepted just before
  shutdown began is now served instead of being closed with no response. hyper's
  graceful drain closes any connection that is still idle, and one accepted a
  moment earlier, whose request had not been read yet, counted as idle. The
  server now stops accepting first and waits a short settle window (100 ms)
  before starting the drain. That window is long enough for such a connection
  to read its request and be served. During an in-place upgrade, connections
  that arrive in the window go to the successor on the shared socket. Found by
  the hot-upgrade live test (`examples/hot-upgrade/tests/live_upgrade.rs`),
  which saw a read return zero bytes across the cutover.
