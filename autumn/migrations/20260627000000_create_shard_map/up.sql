-- Boot-time shard slot-map guard (issue #1277).
--
-- Records the slot→shard assignment that was active on first boot when
-- auto-split is in use. On subsequent boots the framework compares the
-- freshly-computed auto-split against this stored map and refuses to start if
-- they differ — preventing silent data misrouting caused by topology changes
-- (adding/removing/reordering shards) without explicit slot pinning.
--
-- One row per shard:
--   shard_name  TEXT  -- the configured [[database.shards]] name
--   slots       TEXT  -- compact range notation, e.g. "0-8191" or "0-5460"
--                     -- empty string for a drained shard (auto-split: never)
--   updated_at        -- last write, for audit / debugging

CREATE TABLE IF NOT EXISTS _autumn_shard_map (
    shard_name TEXT        NOT NULL PRIMARY KEY,
    slots      TEXT        NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
