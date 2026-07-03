-- Tags, and the Post <-> Tag many-to-many join table (#1324). `#[model]`
-- generates the join-table Diesel type from `#[has_many(Tag, through =
-- post_tags)]` / `#[has_many(Post, through = post_tags)]`, so `post_tags`
-- deliberately has no hand-written `diesel::table!` counterpart in
-- `schema.rs` -- the macro supplies it.
CREATE TABLE tags (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    slug TEXT NOT NULL UNIQUE
);

CREATE INDEX idx_tags_slug ON tags (slug);

-- Composite primary key doubles as the unique constraint that makes
-- `add_tag`'s `ON CONFLICT DO NOTHING` idempotent.
CREATE TABLE post_tags (
    post_id BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    tag_id BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (post_id, tag_id)
);

CREATE INDEX idx_post_tags_tag_id ON post_tags (tag_id);
