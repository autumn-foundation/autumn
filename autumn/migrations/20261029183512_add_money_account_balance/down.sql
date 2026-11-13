DROP TRIGGER IF EXISTS _autumn_money_postings_balance ON _autumn_money_postings;
DROP FUNCTION IF EXISTS _autumn_money_apply_postings();
ALTER TABLE _autumn_money_accounts DROP COLUMN IF EXISTS balance_minor;
