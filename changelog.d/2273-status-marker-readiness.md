### Fixed

- **`deploy status` marker ignored `/ready` (#2273):** a deployed host whose
  `/ready` was not `2xx`, or gave no answer, showed a green `✅`. It now shows
  `⚠️`, and a line under the table names the host. Readiness is not drift, so
  `--strict` does not change.
