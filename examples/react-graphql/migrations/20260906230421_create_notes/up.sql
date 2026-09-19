CREATE TABLE notes (
    id         BIGSERIAL PRIMARY KEY,
    title      TEXT      NOT NULL,
    body       TEXT      NOT NULL DEFAULT '',
    pinned     BOOLEAN   NOT NULL DEFAULT FALSE,
    -- `TIMESTAMP` has no zone, and `Note::created_at` labels it UTC on the way
    -- out (`and_utc()`), so store UTC regardless of the session time zone.
    created_at TIMESTAMP NOT NULL DEFAULT (NOW() AT TIME ZONE 'UTC')
);

CREATE INDEX idx_notes_pinned ON notes (pinned);
