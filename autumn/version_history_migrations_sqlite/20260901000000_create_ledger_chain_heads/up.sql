-- Per-record ledger high-water marks (issue #2323), SQLite variant.
--
-- Backend-forked from the Postgres migration in `version_history_migrations/`,
-- following the same convention as `20260826000000_create_ledger_revisions`:
-- the version dir name is kept identical so `__diesel_schema_migrations`
-- bookkeeping and `framework_migration_versions()` do not diverge across
-- backends. See the Postgres file for what the table is for.
--
-- Differences from the Postgres DDL:
--   * `recorded_at` is TEXT — the writer always binds it explicitly (there is no
--     server-side default), matching how `_autumn_ledger_revisions` stores its
--     own instants, so stored and filter values share one encoding;
--   * no `LOCK TABLE` — SQLite serializes writers on the database write lock, so
--     no append can commit between the backfill's SELECT and this migration's
--     commit in the first place.

CREATE TABLE IF NOT EXISTS _autumn_ledger_chain_heads (
    table_name  TEXT   NOT NULL,
    tenant_key  TEXT   NOT NULL,
    record_id   BIGINT NOT NULL,
    high_seq    BIGINT NOT NULL,
    head_hash   TEXT   NOT NULL,
    recorded_at TEXT   NOT NULL,
    PRIMARY KEY (table_name, tenant_key, record_id)
);

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
