### Fixed

- **db:** require the bundled libpq source (`pq-src`) at 0.3.13 or later.
  0.3.12 called `timingsafe_bcmp`, which glibc does not have, so a new
  `autumn new` app failed to link with an undefined symbol; 0.3.13 builds
  libpq's own implementation of it.
