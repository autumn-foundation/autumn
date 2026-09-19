-- Local mirror of provider billing state for autumn-billing.
-- Portable SQL: TEXT ids, BIGINT money in minor units, TIMESTAMP in UTC.
-- The provider stays the source of truth.

CREATE TABLE IF NOT EXISTS billing_customers (
    id                    TEXT      NOT NULL PRIMARY KEY,
    user_id               TEXT,
    provider              TEXT      NOT NULL,
    provider_customer_id  TEXT      NOT NULL UNIQUE,
    email                 TEXT,
    created_at            TIMESTAMP NOT NULL,
    updated_at            TIMESTAMP NOT NULL
);
-- One mirrored customer per application user. Partial, so unlinked rows
-- (provider-only customers) never collide. Valid on Postgres and SQLite.
CREATE UNIQUE INDEX IF NOT EXISTS billing_customers_user_uidx
    ON billing_customers (user_id) WHERE user_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS billing_subscriptions (
    id                        TEXT      NOT NULL PRIMARY KEY,
    customer_id               TEXT      NOT NULL REFERENCES billing_customers (id) ON DELETE CASCADE,
    provider_subscription_id  TEXT      NOT NULL UNIQUE,
    provider_price_id         TEXT,
    plan_id                   TEXT,
    status                    TEXT      NOT NULL,
    quantity                  BIGINT    NOT NULL,
    current_period_end        TIMESTAMP,
    cancel_at_period_end      BIGINT    NOT NULL,
    last_event_at             TIMESTAMP NOT NULL,
    created_at                TIMESTAMP NOT NULL,
    updated_at                TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS billing_subscriptions_customer_idx ON billing_subscriptions (customer_id);

CREATE TABLE IF NOT EXISTS billing_invoices (
    id                    TEXT      NOT NULL PRIMARY KEY,
    customer_id           TEXT      NOT NULL REFERENCES billing_customers (id) ON DELETE CASCADE,
    subscription_id       TEXT,
    provider_invoice_id   TEXT      NOT NULL UNIQUE,
    status                TEXT      NOT NULL,
    amount_due_minor      BIGINT    NOT NULL,
    amount_paid_minor     BIGINT    NOT NULL,
    currency              TEXT      NOT NULL,
    attempt_count         BIGINT    NOT NULL,
    next_payment_attempt  TIMESTAMP,
    last_event_at         TIMESTAMP NOT NULL,
    created_at            TIMESTAMP NOT NULL,
    updated_at            TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS billing_invoices_customer_idx ON billing_invoices (customer_id);

-- Idempotency ledger keyed by the provider event id.
CREATE TABLE IF NOT EXISTS billing_events (
    provider_event_id  TEXT      NOT NULL PRIMARY KEY,
    kind               TEXT      NOT NULL,
    claimed_at         TIMESTAMP NOT NULL,
    applied_at         TIMESTAMP
);

-- Dunning schedule. One row per invoice; the retry job reads it.
CREATE TABLE IF NOT EXISTS billing_dunning (
    invoice_id       TEXT      NOT NULL PRIMARY KEY,
    customer_id      TEXT      NOT NULL,
    subscription_id  TEXT,
    attempt          BIGINT    NOT NULL,
    next_attempt_at  TIMESTAMP NOT NULL,
    state            TEXT      NOT NULL,
    updated_at       TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS billing_dunning_state_idx ON billing_dunning (state, next_attempt_at);
