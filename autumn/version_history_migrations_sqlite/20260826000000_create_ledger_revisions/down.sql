-- WARNING: this permanently destroys the entire tamper-evident ledger.
--
-- Worse than losing an audit trail: once the table is gone and recreated empty,
-- `ledger_verify()` reports every record as intact with zero revisions, so a
-- full-history wipe becomes indistinguishable from "never written" through the
-- API. Take a copy of `_autumn_ledger_revisions` (and of each record's head
-- hash) before reverting this migration.

DROP INDEX IF EXISTS idx_autumn_ledger_revisions_record;
DROP INDEX IF EXISTS idx_autumn_ledger_revisions_chain;
DROP TABLE IF EXISTS _autumn_ledger_revisions;
