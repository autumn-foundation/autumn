-- `close_dunning_for` (reconcile.rs) looks up the open dunning rows of one
-- subscription when it cancels. Before this index the only usable access
-- path was `billing_dunning_state_idx (state, next_attempt_at)`, which has
-- no leading column for `subscription_id` -- every lookup scanned every
-- pending/running row system-wide and filtered in application code. Partial
-- (WHERE subscription_id IS NOT NULL) because a dunning row can be linked to
-- no subscription at all (a payment-failed event whose invoice snapshot
-- carries no provider_subscription_id -- see reconcile.rs's `upsert_invoice`)
-- and those rows are never queried by this predicate. Portable: partial
-- indexes work on both Postgres and SQLite (the two backends this store
-- supports), matching billing_customers_user_uidx in the mirror migration.
CREATE INDEX IF NOT EXISTS billing_dunning_subscription_idx
    ON billing_dunning (subscription_id) WHERE subscription_id IS NOT NULL;
