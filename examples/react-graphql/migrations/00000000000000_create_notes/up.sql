CREATE TABLE notes (
    id         BIGSERIAL PRIMARY KEY,
    title      TEXT      NOT NULL,
    body       TEXT      NOT NULL DEFAULT '',
    pinned     BOOLEAN   NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_notes_pinned ON notes (pinned);
