-- Derivation backfill state (issue #1769), SQLite variant.
--
-- Backend-forked from the Postgres migration in `derivation_migrations/`. The
-- version dir name is kept identical so `__diesel_schema_migrations`
-- bookkeeping does not diverge across backends. Differences from the Postgres
-- DDL: `updated_at TEXT DEFAULT CURRENT_TIMESTAMP`, because SQLite has neither
-- `TIMESTAMPTZ` nor `NOW()`.

CREATE TABLE IF NOT EXISTS _autumn_derivations (
    name            TEXT    PRIMARY KEY,
    definition_hash TEXT    NOT NULL,
    backfill_state  TEXT    NOT NULL
        CHECK (backfill_state IN ('pending', 'running', 'complete')),
    checkpoint      BIGINT,
    backfilled_rows BIGINT  NOT NULL DEFAULT 0,
    updated_at      TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);
