### Fixed

- **upgrade:** a connection accepted just before shutdown now gets its first
  request served (issue #2372). Before, a task that read the request late could
  see the socket closed with the request unread, and the client got
  `ECONNRESET`. The drain now waits up to 500 ms for the first request of such a
  connection. A connection that is still silent after that is closed.
