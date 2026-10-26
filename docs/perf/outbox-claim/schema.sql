CREATE TABLE IF NOT EXISTS autumn_outbox (
    seq           BIGSERIAL PRIMARY KEY,
    id            TEXT      NOT NULL UNIQUE,
    aggregate     TEXT      NOT NULL,
    topic         TEXT      NOT NULL,
    payload       TEXT      NOT NULL,
    created_at    BIGINT    NOT NULL,
    available_at  BIGINT    NOT NULL,
    attempts      INTEGER   NOT NULL DEFAULT 0,
    last_error    TEXT,
    claim_token   TEXT,
    locked_until  BIGINT,
    dispatched_at BIGINT,
    dead_at       BIGINT
);
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_pending
    ON autumn_outbox (aggregate, seq)
    WHERE dispatched_at IS NULL AND dead_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_claim
    ON autumn_outbox (seq)
    WHERE dispatched_at IS NULL AND dead_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_autumn_outbox_dispatched
    ON autumn_outbox (dispatched_at)
    WHERE dispatched_at IS NOT NULL;
CREATE TABLE IF NOT EXISTS autumn_inbox (
    consumer   TEXT   NOT NULL,
    message_id TEXT   NOT NULL,
    seen_at    BIGINT NOT NULL,
    PRIMARY KEY (consumer, message_id)
);
CREATE INDEX IF NOT EXISTS idx_autumn_inbox_seen_at ON autumn_inbox (seen_at);