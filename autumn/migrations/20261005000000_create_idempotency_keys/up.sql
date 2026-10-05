-- Idempotency keys for `DbIdempotencyStore` (issue #3061).
--
-- One row per scoped key. `locked_by` and `locked_until_ms` hold the in-flight
-- lock. `record` holds the stored response. A handler can write `record` in
-- its own transaction, so the response commits with the mutation.
-- `recovery_point` marks the last committed step of a multi-step handler.
-- Times are Unix milliseconds from the app clock.

CREATE TABLE IF NOT EXISTS autumn_idempotency_keys (
    storage_key     TEXT   PRIMARY KEY,
    record          BYTEA,
    recovery_point  TEXT,
    locked_by       TEXT,
    locked_until_ms BIGINT NOT NULL,
    expires_at_ms   BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS autumn_idempotency_keys_expires_at_idx
    ON autumn_idempotency_keys (expires_at_ms);
