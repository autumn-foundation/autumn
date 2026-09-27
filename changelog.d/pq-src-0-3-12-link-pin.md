### Fixed

- **db:** hold the bundled libpq source (`pq-src`) below 0.3.12. That
  release calls `timingsafe_bcmp`, which glibc does not have, so a new
  `autumn new` app failed to link with an undefined symbol. Remove the pin
  when a fixed `pq-src` release is out.
