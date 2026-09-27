-- Schema mirrors exactly what `autumn generate mailer --list-unsubscribe`
-- emits for an app (see autumn/tests/integration/mail_unsubscribe.rs's
-- `newsletter_unsubscribe_end_to_end_db_backed`, which creates the identical
-- DDL against a testcontainers Postgres) and what
-- `autumn::mail::db_suppression::DbSuppressionStore` queries
-- (autumn/src/mail.rs).

CREATE TABLE IF NOT EXISTS mail_unsubscribes (
    id              BIGSERIAL   PRIMARY KEY,
    subscriber      TEXT        NOT NULL,
    list_id         TEXT        NOT NULL,
    unsubscribed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (subscriber, list_id)
);
