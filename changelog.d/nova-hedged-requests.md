### Added

- **hedge:** `autumn_web::hedge::hedged` races a second copy of a slow call
  after a delay and returns the first answer, with an `allow` hook so a retry
  budget or load check can veto extra copies.
