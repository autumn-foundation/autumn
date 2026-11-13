DROP TRIGGER IF EXISTS _autumn_money_accounts_replace_balance;
DROP TRIGGER IF EXISTS _autumn_money_postings_balance;
ALTER TABLE _autumn_money_accounts DROP COLUMN balance_minor;
