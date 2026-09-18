-- Double-entry money ledger (issue #1837).
--
-- `_autumn_money_`, not `_autumn_ledger_`: the bitemporal record ledger (#1699)
-- already owns `_autumn_ledger_revisions` and `_autumn_ledger_high_water`.
--
-- Three tables. `_autumn_money_accounts` names the accounts and holds the one
-- policy flag in scope (`allow_negative`). `_autumn_money_transactions` is one
-- row per posted transaction, keyed by its idempotency key: the UNIQUE index is
-- what makes a retried charge collapse to one entry. `_autumn_money_postings`
-- holds the lines, signed — a debit is positive, a credit is negative — so the
-- balance of an account is the sum of its postings and a balanced transaction
-- sums to zero.
--
-- The transaction and posting tables are append-only. `post` never updates or
-- deletes a row, and the trigger below refuses one that comes from anywhere
-- else.

CREATE TABLE IF NOT EXISTS _autumn_money_accounts (
    id             TEXT        PRIMARY KEY,
    currency       TEXT        NOT NULL,
    allow_negative BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS _autumn_money_transactions (
    id              TEXT        PRIMARY KEY,
    idempotency_key TEXT        NOT NULL UNIQUE,
    request_hash    TEXT        NOT NULL,
    currency        TEXT        NOT NULL,
    memo            TEXT        NOT NULL DEFAULT '',
    posted_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS _autumn_money_postings (
    id             BIGSERIAL   PRIMARY KEY,
    -- DEFERRABLE INITIALLY DEFERRED on purpose: `post` writes the postings
    -- BEFORE the transaction row they belong to, so that a `post` whose future
    -- is dropped part-way leaves the enclosing transaction holding postings
    -- with no parent. The database then refuses the COMMIT, and nothing
    -- half-written reaches the books.
    transaction_id TEXT        NOT NULL
        REFERENCES _autumn_money_transactions(id) DEFERRABLE INITIALLY DEFERRED,
    seq            BIGINT      NOT NULL,
    account_id     TEXT        NOT NULL
        REFERENCES _autumn_money_accounts(id),
    amount_minor   BIGINT      NOT NULL,
    currency       TEXT        NOT NULL,
    posted_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (transaction_id, seq)
);

-- The balance query and the trial balance both read by account.
CREATE INDEX IF NOT EXISTS _autumn_money_postings_account_idx
    ON _autumn_money_postings (account_id);

-- Append-only. A rewritten posting is how a ledger loses money quietly, so the
-- refusal lives in the database rather than in the code that is supposed to
-- avoid it.
CREATE OR REPLACE FUNCTION _autumn_money_append_only() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        'autumn money ledger is append-only: % on % is refused',
        TG_OP, TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS _autumn_money_transactions_append_only
    ON _autumn_money_transactions;
CREATE TRIGGER _autumn_money_transactions_append_only
    BEFORE UPDATE OR DELETE ON _autumn_money_transactions
    FOR EACH ROW EXECUTE FUNCTION _autumn_money_append_only();

DROP TRIGGER IF EXISTS _autumn_money_postings_append_only
    ON _autumn_money_postings;
CREATE TRIGGER _autumn_money_postings_append_only
    BEFORE UPDATE OR DELETE ON _autumn_money_postings
    FOR EACH ROW EXECUTE FUNCTION _autumn_money_append_only();

-- A row trigger does not fire on TRUNCATE, so the pair above would let one
-- operational `TRUNCATE` erase the books without raising anything. These are
-- statement-level, which is the only level TRUNCATE has. (`DROP TABLE` is not
-- covered by any trigger; `down.sql` relies on that.)
DROP TRIGGER IF EXISTS _autumn_money_transactions_no_truncate
    ON _autumn_money_transactions;
CREATE TRIGGER _autumn_money_transactions_no_truncate
    BEFORE TRUNCATE ON _autumn_money_transactions
    FOR EACH STATEMENT EXECUTE FUNCTION _autumn_money_append_only();

DROP TRIGGER IF EXISTS _autumn_money_postings_no_truncate
    ON _autumn_money_postings;
CREATE TRIGGER _autumn_money_postings_no_truncate
    BEFORE TRUNCATE ON _autumn_money_postings
    FOR EACH STATEMENT EXECUTE FUNCTION _autumn_money_append_only();

-- An account's currency is what `post` checks a posting against, so it must not
-- move under the postings that already refer to it. Changing it would relabel
-- every stored minor unit and let the next posting in the new currency pass
-- that check, which mixes two currencies in one account. The policy flag stays
-- editable; only the currency is fixed.
--
-- `INSERT ... ON CONFLICT DO UPDATE` is an UPDATE, so this trigger sees it too.
-- A DELETE and re-INSERT is refused by the postings foreign key.
CREATE OR REPLACE FUNCTION _autumn_money_currency_is_fixed() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        'autumn money ledger: account % cannot change currency from % to %',
        OLD.id, OLD.currency, NEW.currency;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS _autumn_money_accounts_currency_fixed
    ON _autumn_money_accounts;
CREATE TRIGGER _autumn_money_accounts_currency_fixed
    BEFORE UPDATE OF currency ON _autumn_money_accounts
    FOR EACH ROW WHEN (NEW.currency IS DISTINCT FROM OLD.currency)
    EXECUTE FUNCTION _autumn_money_currency_is_fixed();
