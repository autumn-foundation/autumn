-- Tenant data for the fleet sweeper test (issue 3234).
CREATE TABLE sw_posts (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL
);
CREATE TABLE sw_comments (
    id INTEGER PRIMARY KEY,
    post_id BIGINT NOT NULL REFERENCES sw_posts(id),
    body TEXT NOT NULL
);
