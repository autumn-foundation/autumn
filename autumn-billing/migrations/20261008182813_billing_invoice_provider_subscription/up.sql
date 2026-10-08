-- Keep the provider subscription id on an invoice. A failure webhook can
-- arrive before the subscription event; mirroring the subscription then
-- links the invoice (and its open dunning row) by this id.
ALTER TABLE billing_invoices ADD COLUMN provider_subscription_id TEXT;

-- Linking looks up invoices by this id, linked or not.
CREATE INDEX IF NOT EXISTS billing_invoices_provider_subscription_idx
    ON billing_invoices (provider_subscription_id)
    WHERE provider_subscription_id IS NOT NULL;
