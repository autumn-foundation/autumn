### Added

- **cli:** `autumn simulate` (PR #1572) [no-plugin] — a built-in traffic generator for
  exercising a running Autumn app locally without external tools like `hey` or
  `wrk`. Takes `--url` (default `http://127.0.0.1:3000`), `--duration` seconds,
  and `--concurrency` simulated users, drives GET requests from that many
  threads, and reports total requests and requests/sec — handy for feeding
  live data into `autumn monitor`.
