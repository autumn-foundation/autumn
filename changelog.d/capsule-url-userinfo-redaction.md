### Fixed

- **capsule:** a failure capsule no longer records the `user:password@` part of
  an outbound HTTP URL (or its redirect target). It is masked as
  `[FILTERED]`, as an `Authorization` header already was, and the values are fed
  to the echo pass so they are masked elsewhere in the capsule too.
