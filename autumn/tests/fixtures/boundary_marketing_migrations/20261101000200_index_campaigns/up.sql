-- Outside a transaction, so the schema switch is saved and restored.
CREATE INDEX CONCURRENTLY campaigns_name_idx ON campaigns (name);
