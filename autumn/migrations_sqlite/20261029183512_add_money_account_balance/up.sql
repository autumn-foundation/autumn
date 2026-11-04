-- SQLite fork of the stored account balance (issue #3339). See the Postgres
-- copy under `migrations/` for why the balance is stored.
--
-- SQLite has no statement trigger, so this trigger adds each posting in turn.
-- A step can leave `i64` while the net change of the transaction does not.
-- SQLite would then change the value to REAL and lose precision. The trigger
-- writes NULL in that case, and the next `post` sums the postings again.

ALTER TABLE _autumn_money_accounts ADD COLUMN balance_minor BIGINT DEFAULT 0;

-- The rows that exist now are not known. New rows start at 0.
UPDATE _autumn_money_accounts SET balance_minor = NULL;

CREATE TRIGGER IF NOT EXISTS _autumn_money_postings_balance
    AFTER INSERT ON _autumn_money_postings
BEGIN
    UPDATE _autumn_money_accounts
       SET balance_minor = CASE
               WHEN NEW.amount_minor > 0
                    AND balance_minor > 9223372036854775807 - NEW.amount_minor
               THEN NULL
               WHEN NEW.amount_minor < 0
                    AND balance_minor < (-9223372036854775807 - 1) - NEW.amount_minor
               THEN NULL
               ELSE balance_minor + NEW.amount_minor
           END
     WHERE id = NEW.account_id
       AND balance_minor IS NOT NULL;
END;

-- `INSERT OR REPLACE` of an account row puts the column default (0) in place
-- of the stored balance. If the account has postings, make the balance not
-- known, so that the next `post` sums the postings again.
CREATE TRIGGER IF NOT EXISTS _autumn_money_accounts_replace_balance
    AFTER INSERT ON _autumn_money_accounts
    WHEN EXISTS (
        SELECT 1 FROM _autumn_money_postings WHERE account_id = NEW.id
    )
BEGIN
    UPDATE _autumn_money_accounts SET balance_minor = NULL WHERE id = NEW.id;
END;
