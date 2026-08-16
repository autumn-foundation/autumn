-- Schema mirrors exactly what `autumn generate model Post title:String` +
-- `autumn generate scaffold Comment body:Text post:references` emit for an
-- app (autumn-cli/src/generate/scaffold.rs's
-- `scaffold_references_field_emits_fk_column_constraint_and_index` test
-- pins the migration DDL this reproduces): a parent table with a string
-- display column (issue #1146 resolves `title` as the display column via
-- the name/title/first-string heuristic) and a child table with a
-- `belongs_to` foreign key (NOT NULL, indexed) plus the framework's
-- standard `id`/`created_at` columns.

CREATE TABLE IF NOT EXISTS posts (
    id         BIGSERIAL   PRIMARY KEY,
    title      TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS comments (
    id         BIGSERIAL   PRIMARY KEY,
    post_id    BIGINT      NOT NULL REFERENCES posts(id),
    body       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_comments_post_id ON comments (post_id);
