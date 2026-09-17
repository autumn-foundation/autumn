DROP TRIGGER IF EXISTS _autumn_ledger_postings_append_only ON _autumn_ledger_postings;
DROP TRIGGER IF EXISTS _autumn_ledger_transactions_append_only ON _autumn_ledger_transactions;
DROP FUNCTION IF EXISTS _autumn_ledger_append_only();
DROP TABLE IF EXISTS _autumn_ledger_postings;
DROP TABLE IF EXISTS _autumn_ledger_transactions;
DROP TABLE IF EXISTS _autumn_ledger_accounts;
