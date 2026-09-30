### Fixed

- **`cache::jittered_ttl` no longer panics on huge TTLs:** a base duration
  near `Duration::MAX` jittered upward overflowed `Duration::mul_f64` and
  panicked; the result now saturates at `Duration::MAX`.
