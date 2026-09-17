-- SQLite fork of the double-entry money ledger (issue #1837). See the Postgres
-- copy under `migrations/` for what each table is for. Only the
-- dialect differs: TEXT timestamps, INTEGER PRIMARY KEY for the row id, and
-- RAISE(ABORT) triggers in place of a plpgsql function. SQLite has no TRUNCATE
-- statement, so it needs no counterpart to the Postgres TRUNCATE triggers.

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
