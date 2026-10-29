-- Stored balance for each money account (issue #3339).
--
-- `post` read all the postings of each account it touched, so the cost of a
-- post increased with the history. Now the account row holds the balance, and
-- the trigger below keeps it. The trigger fires for each INSERT into the
-- postings, also for an INSERT that does not come from `post`.
--
-- NULL means "not known". Then `post` and `balance` sum the postings, and
-- `post` writes the sum back. The rows that exist before this migration start
-- at NULL, so this migration does not read the postings. New rows start at 0.
ALTER TABLE _autumn_money_accounts ADD COLUMN IF NOT EXISTS balance_minor BIGINT;
ALTER TABLE _autumn_money_accounts ALTER COLUMN balance_minor SET DEFAULT 0;

-- One UPDATE for each statement, with the net change for each account.
-- NUMERIC prevents an overflow in a part of the sum. A result outside BIGINT
-- becomes NULL. `post` refuses such a result before it writes, so only an
-- INSERT from outside `post` can cause it.
CREATE OR REPLACE FUNCTION _autumn_money_apply_postings() RETURNS TRIGGER AS $$
BEGIN
    UPDATE _autumn_money_accounts AS a
       SET balance_minor = CASE
               WHEN a.balance_minor::NUMERIC + d.delta
                    BETWEEN -9223372036854775808 AND 9223372036854775807
               THEN (a.balance_minor::NUMERIC + d.delta)::BIGINT
           END
      FROM (
          SELECT account_id, SUM(amount_minor) AS delta
            FROM _autumn_money_new_postings
           GROUP BY account_id
      ) AS d
     WHERE a.id = d.account_id
       AND a.balance_minor IS NOT NULL;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS _autumn_money_postings_balance ON _autumn_money_postings;
CREATE TRIGGER _autumn_money_postings_balance
    AFTER INSERT ON _autumn_money_postings
    REFERENCING NEW TABLE AS _autumn_money_new_postings
    FOR EACH STATEMENT EXECUTE FUNCTION _autumn_money_apply_postings();
