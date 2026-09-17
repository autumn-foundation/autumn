DROP TRIGGER IF EXISTS _autumn_money_postings_append_only ON _autumn_money_postings;
DROP TRIGGER IF EXISTS _autumn_money_transactions_append_only ON _autumn_money_transactions;
DROP FUNCTION IF EXISTS _autumn_money_append_only();
DROP TABLE IF EXISTS _autumn_money_postings;
DROP TABLE IF EXISTS _autumn_money_transactions;
DROP TABLE IF EXISTS _autumn_money_accounts;
