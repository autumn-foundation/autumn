-- A "collection" is a curated set of related external links — the parent
-- record. Each collection has_many `collection_links`, and the two are created,
-- edited, and removed together through one nested (`has_many`) form
-- (`NestedChangesetForm<CollectionForm, LinkForm>`), saved atomically in a
-- single transaction. See docs/guide/nested-forms.md.
CREATE TABLE collections (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE TABLE collection_links (
    id BIGSERIAL PRIMARY KEY,
    collection_id BIGINT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    url TEXT NOT NULL,
    -- 0-based render/display order, stamped from the submitted row order.
    position INT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_collection_links_collection_id ON collection_links (collection_id);
