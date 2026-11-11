-- IF NOT EXISTS: see 00000000000000_create_posts.
CREATE TABLE IF NOT EXISTS api_tokens (
    id         BIGSERIAL    PRIMARY KEY,
    token      TEXT         NOT NULL UNIQUE,
    principal  TEXT         NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
