-- Bitemporal, tamper-evident record ledger (issue #1699).
--
-- Every write to a `#[repository(..., ledgered = true)]` model appends one
-- immutable row here, inside the same transaction as the write itself. The
-- table is append-only: there is no public API that UPDATEs or DELETEs a row,
-- and doing so out of band is exactly what `ledger_verify()` detects.
--
-- Schema notes:
--   table_name  TEXT        -- the Diesel table name (e.g. "invoices")
--   tenant_id   TEXT        -- tenant scope for tenant_scoped repositories (nullable)
--   record_id   BIGINT      -- the row PK; assumes BIGSERIAL / i64 PKs
--   seq         BIGINT      -- 1-based position in this record's chain
--   op          TEXT        -- 'insert' | 'update' | 'delete'
--   actor       TEXT        -- authenticated user_id, or 'system'
--   request_id  TEXT        -- trace / correlation ID (nullable)
--   snapshot    JSONB       -- FULL column values after the mutation
--   valid_from  TIMESTAMPTZ -- valid time: when the fact became true
--   recorded_at TIMESTAMPTZ -- transaction time: when the DB learned it
--   prev_hash   TEXT        -- hash of revision seq-1 (NULL at seq = 1)
--   hash        TEXT        -- SHA-256 over this revision's fields + prev_hash

CREATE TABLE IF NOT EXISTS _autumn_ledger_revisions (
    id          BIGSERIAL   PRIMARY KEY,
    table_name  TEXT        NOT NULL,
    tenant_id   TEXT,
    record_id   BIGINT      NOT NULL,
    seq         BIGINT      NOT NULL,
    op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    actor       TEXT        NOT NULL DEFAULT 'system',
    request_id  TEXT,
    snapshot    JSONB       NOT NULL DEFAULT '{}',
    valid_from  TIMESTAMPTZ NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    prev_hash   TEXT,
    hash        TEXT        NOT NULL
);

-- One revision per (record, seq). Makes a lost update between the chain read
-- and the append a hard error rather than a silently forked chain, and makes an
-- out-of-band INSERT of a duplicate revision impossible rather than merely
-- detectable. `tenant_id` is nullable, and Postgres treats NULLs as distinct in
-- a unique index, so the tenant leg is coalesced.
CREATE UNIQUE INDEX IF NOT EXISTS idx_autumn_ledger_revisions_chain
    ON _autumn_ledger_revisions (table_name, COALESCE(tenant_id, ''), record_id, seq);

-- The read path for as-of / diff / verify: one record's chain in seq order.
CREATE INDEX IF NOT EXISTS idx_autumn_ledger_revisions_record
    ON _autumn_ledger_revisions (table_name, record_id, seq ASC);

-- Transaction-time scans across one table (as-of over many records).
CREATE INDEX IF NOT EXISTS idx_autumn_ledger_revisions_recorded_at
    ON _autumn_ledger_revisions (table_name, recorded_at ASC);
