-- Password-reset tokens (issue #1342 retention-sweeps demo). Short-lived by
-- design: a token is only ever useful for the ~1 day after it's issued, so
-- this table is exactly the kind of "framework user's own transient model"
-- the retention feature exists for — see `#[repository(..., retention(...))]`
-- below and docs/guide/retention-sweeps.md.
CREATE TABLE password_reset_tokens (
    id         BIGSERIAL PRIMARY KEY,
    tenant_id  TEXT      NOT NULL,
    user_id    BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash TEXT      NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_password_reset_tokens_tenant ON password_reset_tokens (tenant_id);
CREATE INDEX idx_password_reset_tokens_user ON password_reset_tokens (user_id);
