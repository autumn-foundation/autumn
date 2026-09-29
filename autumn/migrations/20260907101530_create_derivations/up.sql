-- Derivation backfill state (issue #1769).
--
-- One row per registered `#[derivation]`. `definition_hash` content-addresses
-- the derivation's lowered shape (tables, columns, transform, filter), so a
-- changed filter enqueues a backfill and a rename or a reformat does not.
--
-- `checkpoint` is the highest parent primary key the backfill has repaired. It
-- is written in the same transaction as the batch it describes, so a killed
-- backfill resumes from the last committed batch instead of restarting.

CREATE TABLE IF NOT EXISTS _autumn_derivations (
    name            TEXT        PRIMARY KEY,
    definition_hash TEXT        NOT NULL,
    backfill_state  TEXT        NOT NULL
        CHECK (backfill_state IN ('pending', 'running', 'complete')),
    checkpoint      BIGINT,
    backfilled_rows BIGINT      NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
