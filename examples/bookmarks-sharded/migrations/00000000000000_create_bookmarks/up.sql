-- Bookmarks are tenant data: this migration runs on EVERY shard (and the
-- control database, harmlessly) via `autumn migrate` / startup
-- auto-migrate. Each row carries its tenant id; the owning shard is
-- decided by hashing that tenant id onto a logical slot.
CREATE TABLE bookmarks (
    id BIGSERIAL PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    url TEXT NOT NULL,
    title TEXT NOT NULL,
    tag TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_bookmarks_tenant ON bookmarks (tenant_id);
CREATE INDEX idx_bookmarks_tag ON bookmarks (tag);
