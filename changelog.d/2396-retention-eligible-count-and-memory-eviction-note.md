### Fixed

- **retention:** a sweep that fails part way now reports `eligible_rows`, so the
  audit record shows how many rows were due (issue #2396).
- **retention:** the report note for the built-in idempotency (memory and
  database), webhook replay, and job tracking stores now says the store removes
  expired records only on later writes (issue #2396).
