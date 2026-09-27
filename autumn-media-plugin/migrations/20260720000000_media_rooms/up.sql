-- Shared, multi-process-safe mesh-room state for the media plugin's DB-backed
-- room store (`autumn_media_plugin::rooms_db::DbRoomStore`, epic #1974).
--
-- Apply this migration in any app that sets `[media] room_store_backend = "db"`.
-- Timestamps are TIMESTAMP (NOT TIMESTAMPTZ) so the schema is portable across
-- the Postgres and SQLite runtime lanes; the store writes UTC `NaiveDateTime`
-- values. Room lookups key on the `(namespace, room_id)` pair so tenants never
-- collide, and the reaper sweeps purely by `last_seen_at` / `created_at`.

CREATE TABLE IF NOT EXISTS media_rooms (
    namespace         TEXT      NOT NULL,
    room_id           TEXT      NOT NULL,
    max_participants  INTEGER   NOT NULL,
    created_at        TIMESTAMP NOT NULL,
    PRIMARY KEY (namespace, room_id)
);

CREATE TABLE IF NOT EXISTS media_room_participants (
    namespace         TEXT      NOT NULL,
    room_id           TEXT      NOT NULL,
    participant_id    TEXT      NOT NULL,
    display_name      TEXT,
    token             TEXT      NOT NULL,
    joined_at         TIMESTAMP NOT NULL,
    token_expires_at  TIMESTAMP NOT NULL,
    last_seen_at      TIMESTAMP NOT NULL,
    PRIMARY KEY (namespace, room_id, participant_id),
    FOREIGN KEY (namespace, room_id)
        REFERENCES media_rooms (namespace, room_id) ON DELETE CASCADE
);

-- The last-write-wins reaper deletes stale participants by `last_seen_at` and
-- now-empty rooms by `created_at`; index both sweep keys.
CREATE INDEX IF NOT EXISTS media_room_participants_last_seen_idx
    ON media_room_participants (last_seen_at);
CREATE INDEX IF NOT EXISTS media_rooms_created_at_idx
    ON media_rooms (created_at);
