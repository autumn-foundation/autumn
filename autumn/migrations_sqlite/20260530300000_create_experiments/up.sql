-- A/B experiments, SQLite variant. Backend-forked from the Postgres
-- migration in `autumn/migrations/`. The version dir name matches Postgres.
-- This keeps `__diesel_schema_migrations` bookkeeping the same across
-- backends.
--
-- Differences from the Postgres DDL:
--   * `CREATE TYPE ... AS ENUM` is dropped — SQLite has no enum type;
--     `state` is TEXT with the same values enforced by a CHECK constraint;
--   * `id INTEGER PRIMARY KEY` is a rowid alias. SQLite autoincrements it.
--     `BIGSERIAL` only gets NUMERIC affinity on SQLite, not autoincrement;
--   * `variants` is TEXT — SQLite has no `JSONB` type;
--   * every `TIMESTAMPTZ` becomes TEXT, and `NOW()` becomes
--     `CURRENT_TIMESTAMP` — neither exists on SQLite;
--   * `COMMENT ON COLUMN` is dropped — SQLite has no column comments;
--   * the `pg_notify` trigger is dropped — SQLite has no LISTEN/NOTIFY.
--     There is no `SQLite` experiments store yet to read a notify channel,
--     so this drops cache-invalidation reach, not behavior.
--
-- See the Postgres file for what each table is for.

CREATE TABLE IF NOT EXISTS autumn_experiments (
    id               INTEGER NOT NULL PRIMARY KEY,
    name             TEXT    NOT NULL UNIQUE,
    description      TEXT,
    state            TEXT    NOT NULL DEFAULT 'draft'
        CHECK (state IN ('draft', 'running', 'concluded', 'archived')),
    variants         TEXT    NOT NULL DEFAULT '[]',
    winner           TEXT,
    exclusion_group  TEXT,
    created_at       TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at       TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_autumn_experiments_name
    ON autumn_experiments (name);
CREATE INDEX IF NOT EXISTS idx_autumn_experiments_state
    ON autumn_experiments (state);

-- Sticky assignments: once an actor is assigned to a variant the row is
-- recorded here and returned on all subsequent assign() calls without
-- re-computing the bucket. is_override=true when the assignment came from
-- an operator override rather than weight-based bucketing.
CREATE TABLE IF NOT EXISTS autumn_experiment_assignments (
    id               INTEGER NOT NULL PRIMARY KEY,
    experiment       TEXT    NOT NULL,
    actor            TEXT    NOT NULL,
    variant          TEXT    NOT NULL,
    is_override      BOOLEAN NOT NULL DEFAULT FALSE,
    assigned_at      TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (experiment, actor)
);

CREATE INDEX IF NOT EXISTS idx_autumn_exp_assignments_experiment
    ON autumn_experiment_assignments (experiment);
CREATE INDEX IF NOT EXISTS idx_autumn_exp_assignments_actor
    ON autumn_experiment_assignments (actor);

-- QA/staff overrides: pins an actor to a specific variant bypassing weights.
-- Takes precedence over sticky assignments on each assign() call.
CREATE TABLE IF NOT EXISTS autumn_experiment_overrides (
    id          INTEGER NOT NULL PRIMARY KEY,
    experiment  TEXT    NOT NULL,
    actor       TEXT    NOT NULL,
    variant     TEXT    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (experiment, actor)
);

CREATE INDEX IF NOT EXISTS idx_autumn_exp_overrides_experiment
    ON autumn_experiment_overrides (experiment, actor);

-- Audit log: every mutation (create, set_weights, state change, override) is
-- appended here with the operator identity and a timestamp.
CREATE TABLE IF NOT EXISTS autumn_experiment_changes (
    id          INTEGER NOT NULL PRIMARY KEY,
    experiment  TEXT    NOT NULL,
    mutation    TEXT    NOT NULL,
    actor       TEXT,
    changed_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_autumn_exp_changes_experiment_time
    ON autumn_experiment_changes (experiment, changed_at DESC);
