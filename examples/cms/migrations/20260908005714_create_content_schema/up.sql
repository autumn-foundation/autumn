-- ============================================================================
-- Autumn CMS — the WordPress-core-parity schema.
--
-- The table names deliberately echo WordPress's own (`posts`, `terms`,
-- `options`, `comments`) because the concepts are the same and the mapping
-- should be obvious to anyone migrating. What differs is the typing: every id
-- is a BIGINT, every timestamp is a real timestamp rather than a
-- `0000-00-00 00:00:00` sentinel, and the taxonomy/term relationship is two
-- tables instead of WordPress's three (`term`, `term_taxonomy`,
-- `term_relationships`) because a term here is always scoped to exactly one
-- taxonomy.
-- ============================================================================

-- ── Users and roles ─────────────────────────────────────────────────────────
--
-- `role` is one of the five WordPress core roles. It is a plain column rather
-- than the `wp_capabilities` serialized-PHP blob WordPress stores in usermeta:
-- capabilities are derived from the role at request time by
-- `capabilities::Role::can`, so there is exactly one source of truth and no
-- possibility of a user's stored capability set drifting from their role.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    username      TEXT      NOT NULL UNIQUE,
    email         TEXT      NOT NULL UNIQUE,
    password_hash TEXT      NOT NULL,
    display_name  TEXT      NOT NULL DEFAULT '',
    role          TEXT      NOT NULL DEFAULT 'subscriber',
    bio           TEXT      NOT NULL DEFAULT '',
    website       TEXT      NOT NULL DEFAULT '',
    created_at    TIMESTAMP NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_users_role ON users (role);

-- ── Options (WordPress `wp_options` / Settings) ─────────────────────────────
--
-- The site-wide key/value settings store: site title, tagline, permalink
-- structure, posts-per-page, discussion settings, active theme. `autoload`
-- marks the rows the settings cache warms on boot, exactly as WordPress does.
CREATE TABLE options (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT      NOT NULL UNIQUE,
    value      TEXT      NOT NULL DEFAULT '',
    autoload   BOOLEAN   NOT NULL DEFAULT TRUE,
    updated_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_options_autoload ON options (autoload);

-- ── Media library (WordPress attachments) ───────────────────────────────────
--
-- WordPress models an attachment as a row in `wp_posts` with
-- `post_type = 'attachment'`, which means every post query has to remember to
-- exclude it. Here the media library is its own table: an attachment has a
-- genuinely different shape (mime type, byte size, pixel dimensions, alt text)
-- and is never addressable as a piece of front-end content.
--
-- `file` holds an `autumn_web::storage::Blob` handle — the bytes live in the
-- configured `BlobStore` (local disk in dev, S3 in production), never in
-- Postgres.
CREATE TABLE attachments (
    id          BIGSERIAL PRIMARY KEY,
    title       TEXT      NOT NULL,
    slug        TEXT      NOT NULL UNIQUE,
    file        JSONB     NULL,
    mime_type   TEXT      NOT NULL,
    byte_size   BIGINT    NOT NULL DEFAULT 0,
    width       INTEGER,
    height      INTEGER,
    alt_text    TEXT      NOT NULL DEFAULT '',
    caption     TEXT      NOT NULL DEFAULT '',
    uploader_id BIGINT    REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_attachments_uploader ON attachments (uploader_id);
CREATE INDEX idx_attachments_created ON attachments (created_at DESC);

-- ── Content (posts, pages, and every custom post type) ──────────────────────
--
-- One table for all content types, keyed by `post_type` — the WordPress model,
-- and the right one: a custom post type must be able to reuse revisions,
-- taxonomies, meta, comments and the editor without a schema change.
--
--   status:         draft | pending | publish | private | future | trash
--   comment_status: open | closed
--   parent_id:      hierarchical pages (and post-type ancestry generally)
--   menu_order:     manual ordering for hierarchical types
--   password:       a non-empty value makes the post password-protected
--   sticky:         pinned to the top of the blog index
--   published_at:   when the post went (or goes) live — drives `future`
--                   scheduling, which the publish sweep polls
CREATE TABLE posts (
    id                BIGSERIAL PRIMARY KEY,
    post_type         TEXT      NOT NULL DEFAULT 'post',
    title             TEXT      NOT NULL,
    slug              TEXT      NOT NULL,
    excerpt           TEXT      NOT NULL DEFAULT '',
    body              TEXT      NOT NULL DEFAULT '',
    status            TEXT      NOT NULL DEFAULT 'draft',
    author_id         BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    parent_id         BIGINT    REFERENCES posts (id) ON DELETE SET NULL,
    featured_media_id BIGINT    REFERENCES attachments (id) ON DELETE SET NULL,
    menu_order        INTEGER   NOT NULL DEFAULT 0,
    comment_status    TEXT      NOT NULL DEFAULT 'open',
    password          TEXT      NOT NULL DEFAULT '',
    sticky            BOOLEAN   NOT NULL DEFAULT FALSE,
    comment_count     BIGINT    NOT NULL DEFAULT 0,
    published_at      TIMESTAMP,
    lock_version      INTEGER   NOT NULL DEFAULT 0,
    created_at        TIMESTAMP NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMP NOT NULL DEFAULT NOW()
);
-- Uniqueness follows the URL a row actually mints, which for a page depends on
-- where it sits in the hierarchy.
--
-- A slug is unique within its post type, so a page `/about` and a post
-- `/2026/about` can coexist — the same guarantee WordPress gives. Nested pages
-- are excluded here and covered by `idx_pages_parent_slug` below: they are
-- addressed by their full ancestry, so `/about/team` and `/company/team` are
-- different URLs and both are legitimate. A global `(post_type, slug)` forced
-- the second to become `team-2`, which is a rename WordPress does not make and
-- which `resolve_page_path` — it disambiguates by `parent_id` — never needed.
CREATE UNIQUE INDEX idx_posts_type_slug
    ON posts (post_type, slug)
    WHERE post_type <> 'page' OR parent_id IS NULL;

-- A nested page competes only with its siblings: they are the rows that would
-- mint the same path under the same parent.
CREATE UNIQUE INDEX idx_pages_parent_slug
    ON posts (parent_id, slug)
    WHERE post_type = 'page' AND parent_id IS NOT NULL;

-- ...and `post` and top-level `page` both mint a BARE path (`/about`), where
-- only one row can be served. `(post_type, slug)` does not stop them colliding
-- with each other, so the application de-duplicates on write — and this index
-- is what makes that hold under concurrency, where two inserts can each check
-- first and each find the slug free.
--
-- A custom type is addressed under its own prefix (`/product/widget`) and a
-- nested page under its ancestry, so neither mints a bare path and neither
-- belongs here.
CREATE UNIQUE INDEX idx_posts_bare_path_slug
    ON posts (slug)
    WHERE post_type = 'post' OR (post_type = 'page' AND parent_id IS NULL);
CREATE INDEX idx_posts_status_published ON posts (status, published_at DESC);
CREATE INDEX idx_posts_author ON posts (author_id);
CREATE INDEX idx_posts_parent ON posts (parent_id);
CREATE INDEX idx_posts_type_status ON posts (post_type, status);

-- Full-text search over the editable content (WordPress's `s=` query, but a
-- real tsvector index rather than a chain of `LIKE '%…%'` clauses that no index
-- can serve). A STORED generated column keeps the vector in lockstep with the
-- row with no trigger and no application code; `#[searchable]` on the `Post`
-- model reads it back through the repository's `search()` method.
ALTER TABLE posts ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('english'::regconfig, coalesce(title, '')), 'A') ||
    setweight(to_tsvector('english'::regconfig, coalesce(excerpt, '')), 'B') ||
    setweight(to_tsvector('english'::regconfig, coalesce(body, '')), 'C')
) STORED;
CREATE INDEX idx_posts_search_vector ON posts USING gin (search_vector);

-- ── Post meta (WordPress custom fields) ─────────────────────────────────────
CREATE TABLE post_meta (
    id         BIGSERIAL PRIMARY KEY,
    post_id    BIGINT    NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    meta_key   TEXT      NOT NULL,
    meta_value TEXT      NOT NULL DEFAULT '',
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX idx_post_meta_key ON post_meta (post_id, meta_key);

-- ── Taxonomies and terms (categories, tags, custom taxonomies) ──────────────
--
-- WordPress splits this across `wp_terms` + `wp_term_taxonomy` so one term row
-- can be shared by two taxonomies. In practice nothing uses that, and it is the
-- source of the notorious `term_taxonomy_id` vs `term_id` confusion — so a term
-- here belongs to exactly one taxonomy and carries its own hierarchy pointer.
CREATE TABLE terms (
    id          BIGSERIAL PRIMARY KEY,
    taxonomy    TEXT      NOT NULL DEFAULT 'category',
    name        TEXT      NOT NULL,
    slug        TEXT      NOT NULL,
    description TEXT      NOT NULL DEFAULT '',
    parent_id   BIGINT    REFERENCES terms (id) ON DELETE SET NULL,
    post_count  BIGINT    NOT NULL DEFAULT 0,
    created_at  TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX idx_terms_taxonomy_slug ON terms (taxonomy, slug);
CREATE INDEX idx_terms_parent ON terms (parent_id);

-- The post ↔ term join (WordPress `wp_term_relationships`).
CREATE TABLE post_terms (
    id      BIGSERIAL PRIMARY KEY,
    post_id BIGINT NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    term_id BIGINT NOT NULL REFERENCES terms (id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX idx_post_terms_pair ON post_terms (post_id, term_id);
CREATE INDEX idx_post_terms_term ON post_terms (term_id);

-- ── Revisions (WordPress post revisions / autosaves) ────────────────────────
--
-- An append-only snapshot of the editable fields, written on every content
-- change. Restoring a revision writes the snapshot back onto the post *and*
-- appends a new revision for the restore itself, so the trail is never rewritten.
CREATE TABLE revisions (
    id         BIGSERIAL PRIMARY KEY,
    post_id    BIGINT    NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    title      TEXT      NOT NULL,
    excerpt    TEXT      NOT NULL DEFAULT '',
    body       TEXT      NOT NULL DEFAULT '',
    status     TEXT      NOT NULL,
    author_id  BIGINT    REFERENCES users (id) ON DELETE SET NULL,
    summary    TEXT      NOT NULL DEFAULT '',
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_revisions_post ON revisions (post_id, created_at DESC);

-- ── Comments (with the WordPress moderation queue) ──────────────────────────
--
-- Deliberately NOT the framework's polymorphic `#[commentable]` table: that one
-- requires every author to be a registered `User` and carries no moderation
-- state, and WordPress-core parity needs both guest commenters (name/email/url,
-- no account) and the approved/pending/spam/trash queue. `author_id` is
-- therefore nullable and the guest identity columns sit alongside it.
CREATE TABLE comments (
    id           BIGSERIAL PRIMARY KEY,
    post_id      BIGINT    NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    parent_id    BIGINT    REFERENCES comments (id) ON DELETE CASCADE,
    author_id    BIGINT    REFERENCES users (id) ON DELETE SET NULL,
    author_name  TEXT      NOT NULL DEFAULT '',
    author_email TEXT      NOT NULL DEFAULT '',
    author_url   TEXT      NOT NULL DEFAULT '',
    author_ip    TEXT      NOT NULL DEFAULT '',
    body         TEXT      NOT NULL,
    status       TEXT      NOT NULL DEFAULT 'pending',
    created_at   TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_comments_post ON comments (post_id, created_at);
CREATE INDEX idx_comments_status ON comments (status, created_at DESC);
CREATE INDEX idx_comments_parent ON comments (parent_id);

-- ── Navigation menus (WordPress Appearance → Menus) ─────────────────────────
CREATE TABLE menus (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT      NOT NULL,
    slug       TEXT      NOT NULL UNIQUE,
    location   TEXT      NOT NULL DEFAULT '',
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
-- One menu per theme location. The application clears the incumbent before
-- assigning a new one, and this is what makes that hold under concurrency:
-- without it two administrators assigning `primary` at once each clear what
-- they saw and both insert, after which the renderer picks one of two with no
-- defined ordering. `''` is the unassigned marker and is deliberately excluded,
-- since any number of menus may sit unassigned.
CREATE UNIQUE INDEX idx_menus_location ON menus (location) WHERE location <> '';

-- A menu item points at a post, a term, or a raw URL — whichever is set wins,
-- checked in that order. `parent_id` gives WordPress's nested sub-menus.
CREATE TABLE menu_items (
    id        BIGSERIAL PRIMARY KEY,
    menu_id   BIGINT  NOT NULL REFERENCES menus (id) ON DELETE CASCADE,
    parent_id BIGINT  REFERENCES menu_items (id) ON DELETE CASCADE,
    label     TEXT    NOT NULL,
    url       TEXT    NOT NULL DEFAULT '',
    post_id   BIGINT  REFERENCES posts (id) ON DELETE CASCADE,
    term_id   BIGINT  REFERENCES terms (id) ON DELETE CASCADE,
    position  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_menu_items_menu ON menu_items (menu_id, position);

-- ── Widgets (WordPress Appearance → Widgets) ────────────────────────────────
--
-- A widget is an instance of a registered widget kind placed in a named
-- sidebar. `settings` is the kind-specific JSON payload (how many recent posts
-- to list, which text to render, …).
CREATE TABLE widgets (
    id       BIGSERIAL PRIMARY KEY,
    sidebar  TEXT    NOT NULL DEFAULT 'primary',
    kind     TEXT    NOT NULL,
    title    TEXT    NOT NULL DEFAULT '',
    settings JSONB   NOT NULL DEFAULT '{}'::jsonb,
    position INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_widgets_sidebar ON widgets (sidebar, position);
