-- Per-record ledger high-water marks (issue #2323, follow-up to #1699 / #2318).
--
-- `_autumn_ledger_revisions` allocates a revision's sequence number from the
-- rows that survive in that table. Delete the newest revision and let an
-- ordinary application write land: the append reads `N-1`, re-allocates `N`,
-- chains onto `N-1`'s hash and matches the live row, so both the chain walk and
-- the #2318 live-row cross-check report intact. The deleted state leaves no
-- trace, and the attacker closes the detection window with ordinary traffic.
--
-- This table is the high-water mark that lives *outside* the deletable revision
-- rows. Every append allocates `max(chain head, high-water) + 1`, so the same
-- attack now allocates `N+1` and leaves a permanent gap at `N` that
-- `ledger_verify` reports as `MissingRevision`.
--
-- The mark is never blindly authoritative: `ledger_verify` cross-checks it
-- against the chain in both directions, so rolling it back, rewriting its hash
-- or deleting its row is itself reported (`high_water_behind`,
-- `high_water_mismatch`, `high_water_missing`) rather than silently believed.
--
-- Schema notes:
--   table_name  TEXT        -- the Diesel table name (e.g. "invoices")
--   tenant_key  TEXT        -- COALESCE(tenant_id, ''); NOT NULL so the primary
--                           -- key is a plain column list rather than the
--                           -- expression index the revisions table needs
--   record_id   BIGINT      -- the row PK
--   high_seq    BIGINT      -- highest sequence number ever allocated, never
--                           -- decreased (the writer's upsert guards it)
--   head_hash   TEXT        -- hash of the revision at `high_seq`
--   recorded_at TIMESTAMPTZ -- transaction time of that revision; also the floor
--                           -- the writer clamps the next revision's
--                           -- `recorded_at` against, so transaction time is
--                           -- non-decreasing along a chain by construction

CREATE TABLE IF NOT EXISTS _autumn_ledger_chain_heads (
    table_name  TEXT        NOT NULL,
    tenant_key  TEXT        NOT NULL,
    record_id   BIGINT      NOT NULL,
    high_seq    BIGINT      NOT NULL,
    head_hash   TEXT        NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (table_name, tenant_key, record_id)
);

-- Backfill from the revisions that already exist, so a chain written before
-- this migration is marked exactly like one written after it. Without the
-- backfill, "revisions but no mark" would have to be tolerated forever — and
-- tolerating it is precisely the hole this table closes, since deleting the
-- mark row would then restore the original attack.
--
-- SHARE conflicts with the ROW EXCLUSIVE an INSERT takes, so a ledger append
-- cannot commit between the SELECT below and this migration's own commit and
-- leave its record's mark one behind.
LOCK TABLE _autumn_ledger_revisions IN SHARE MODE;

INSERT INTO _autumn_ledger_chain_heads
    (table_name, tenant_key, record_id, high_seq, head_hash, recorded_at)
SELECT r.table_name, COALESCE(r.tenant_id, ''), r.record_id, r.seq, r.hash, r.recorded_at
FROM _autumn_ledger_revisions r
WHERE r.seq = (
    SELECT MAX(inner_r.seq)
    FROM _autumn_ledger_revisions inner_r
    WHERE inner_r.table_name = r.table_name
      AND COALESCE(inner_r.tenant_id, '') = COALESCE(r.tenant_id, '')
      AND inner_r.record_id = r.record_id
)
ON CONFLICT DO NOTHING;
