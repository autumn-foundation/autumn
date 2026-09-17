-- SQLite fork of the double-entry money ledger (issue #1837). See the Postgres
-- copy under `migrations/` for what each table is for. Only the
-- dialect differs: TEXT timestamps, INTEGER PRIMARY KEY for the row id, and
-- RAISE(ABORT) triggers in place of a plpgsql function. SQLite has no TRUNCATE
-- statement, so it needs no counterpart to the Postgres TRUNCATE triggers. It
-- does have REPLACE, which Postgres has not, so it needs two triggers more.

CREATE TABLE IF NOT EXISTS _autumn_money_accounts (
    id             TEXT    PRIMARY KEY,
    currency       TEXT    NOT NULL,
    allow_negative BOOLEAN NOT NULL DEFAULT 1,
    created_at     TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS _autumn_money_transactions (
    id              TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    request_hash    TEXT NOT NULL,
    currency        TEXT NOT NULL,
    memo            TEXT NOT NULL DEFAULT '',
    posted_at       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS _autumn_money_postings (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Deferred on purpose; see the Postgres copy. SQLite checks a deferred
    -- foreign key at COMMIT too, with PRAGMA foreign_keys = ON (the pool sets
    -- it on every connection).
    transaction_id TEXT    NOT NULL
        REFERENCES _autumn_money_transactions(id) DEFERRABLE INITIALLY DEFERRED,
    seq            BIGINT  NOT NULL,
    account_id     TEXT    NOT NULL
        REFERENCES _autumn_money_accounts(id),
    amount_minor   BIGINT  NOT NULL,
    currency       TEXT    NOT NULL,
    posted_at      TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (transaction_id, seq)
);

CREATE INDEX IF NOT EXISTS _autumn_money_postings_account_idx
    ON _autumn_money_postings (account_id);

CREATE TRIGGER IF NOT EXISTS _autumn_money_transactions_no_update
    BEFORE UPDATE ON _autumn_money_transactions
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: UPDATE is refused');
END;

CREATE TRIGGER IF NOT EXISTS _autumn_money_transactions_no_delete
    BEFORE DELETE ON _autumn_money_transactions
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: DELETE is refused');
END;

CREATE TRIGGER IF NOT EXISTS _autumn_money_postings_no_update
    BEFORE UPDATE ON _autumn_money_postings
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: UPDATE is refused');
END;

CREATE TRIGGER IF NOT EXISTS _autumn_money_postings_no_delete
    BEFORE DELETE ON _autumn_money_postings
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: DELETE is refused');
END;


-- `INSERT OR REPLACE` is a rewrite in disguise. SQLite satisfies the conflict
-- by deleting the row that is in the way, but it does not fire DELETE triggers
-- for that deletion unless `PRAGMA recursive_triggers` is on, which is off by
-- default. So these two guards refuse an insert that collides with a row that
-- is already there.
--
-- They test the row keys only, never `idempotency_key`. A post writes its
-- transaction row with `ON CONFLICT (idempotency_key) DO NOTHING` to collapse a
-- concurrent duplicate, and a BEFORE INSERT trigger fires before SQLite applies
-- that clause. A guard on the key would therefore abort the replay the ledger
-- must allow. A post always supplies a new row id, so these guards never fire
-- for it.
--
-- The deferred foreign key covers the key. A REPLACE that collides on
-- `idempotency_key` with a new row id deletes the transaction row, which leaves
-- its postings without a parent, and the COMMIT fails on the foreign key. Every
-- transaction the ledger writes has postings, because a post writes them first
-- and refuses an empty transaction.

CREATE TRIGGER IF NOT EXISTS _autumn_money_transactions_no_replace
    BEFORE INSERT ON _autumn_money_transactions
    WHEN EXISTS (
        SELECT 1 FROM _autumn_money_transactions WHERE id = NEW.id
    )
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: REPLACE is refused');
END;

CREATE TRIGGER IF NOT EXISTS _autumn_money_postings_no_replace
    BEFORE INSERT ON _autumn_money_postings
    WHEN EXISTS (
        SELECT 1 FROM _autumn_money_postings WHERE id = NEW.id
    ) OR EXISTS (
        -- Two EXISTS, not one with an OR: each is then a plain index lookup,
        -- so the guard costs the same on a ledger of any size.
        SELECT 1 FROM _autumn_money_postings
        WHERE transaction_id = NEW.transaction_id AND seq = NEW.seq
    )
BEGIN
    SELECT RAISE(ABORT, 'autumn money ledger is append-only: REPLACE is refused');
END;
