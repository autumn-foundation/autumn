### Added

- **lock:** `LeaseLock`, a lease lock with fencing tokens (issue #3053). Each
  grant gets a strictly larger `FencingToken`. The lease renews in the
  background, and `lease_lost()` signals a lost lease. Check the token at the
  resource with `WHERE fencing_token <= $token`. See
  `docs/guide/distributed-locks.md` and `verification/lease_fencing.rs`.
- **lock:** new `LockError::LeaseLost { name, token }` variant. It maps to
  `503 Service Unavailable`.
- **db:** the pool builder logs one warning per target when the database
  target points to a well-known connection pooler (`PgBouncer`, RDS Proxy,
  Supabase, Neon). Session advisory locks are not safe in transaction mode.

### Documentation

- **lock:** `Lock` is documented as mutual exclusion for efficiency, not
  correctness, with a connection-pooler compatibility matrix.
