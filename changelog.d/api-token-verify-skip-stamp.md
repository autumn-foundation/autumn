### Performance

- **auth:** `DbApiTokenStore::verify_scoped` no longer sends the
  `last_used_at` `UPDATE` when the 5-minute stamp is not due. A bearer
  request on a busy token now costs 2 statements instead of 3, and 43% fewer
  shared buffers (`docs/perf/api-token-verify/`).
