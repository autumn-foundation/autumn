-- Edge cases for equivalence.sh. It runs them in a transaction it rolls back.
-- 40001: a post with no comments. 40002: every top-level comment deleted.
-- 40003: a dangling author (no users row), a TOASTed 200 kB body, five
-- top-level comments with one created_at (ties broken by id), a live reply
-- under a dead parent (SQL outside the framework), a deleted reply under a
-- live parent, a nested reply whose created_at ties top-level rows, and a
-- comment of post 40004 whose parent is in 40003.
INSERT INTO posts (id, title) VALUES (40001, 'empty'), (40002, 'all dead'), (40003, 'edges'), (40004, 'other');
INSERT INTO comments (id, commentable_type, commentable_id, parent_id, author_id, body, created_at, deleted_at) VALUES
 (9000001, 'Post', 40002, NULL, 1, 'dead top', '2026-01-01', '2026-01-02'),
 (9000002, 'Post', 40002, 9000001, 1, 'dead reply', '2026-01-01 01:00', '2026-01-02'),
 (9000010, 'Post', 40003, NULL, 999999, 'dangling author', '2026-01-01', NULL),
 (9000011, 'Post', 40003, NULL, 2, repeat(md5('toast'), 6400), '2026-01-01 00:00:01', NULL),
 (9000016, 'Post', 40003, NULL, 3, 'tie e', '2026-01-02', NULL),
 (9000013, 'Post', 40003, NULL, 3, 'tie b', '2026-01-02', NULL),
 (9000015, 'Post', 40003, NULL, 3, 'tie d', '2026-01-02', NULL),
 (9000012, 'Post', 40003, NULL, 3, 'tie a', '2026-01-02', NULL),
 (9000014, 'Post', 40003, NULL, 3, 'tie c', '2026-01-02', NULL),
 (9000020, 'Post', 40003, NULL, 4, 'dead parent', '2026-01-03', '2026-01-04'),
 (9000021, 'Post', 40003, 9000020, 4, 'live reply under dead parent', '2026-01-03 01:00', NULL),
 (9000022, 'Post', 40003, 9000010, 5, 'deleted reply', '2026-01-03 02:00', '2026-01-04'),
 (9000023, 'Post', 40003, 9000011, 5, 'reply to toast', '2026-01-03 03:00', NULL),
 (9000024, 'Post', 40003, 9000023, 5, 'nested reply, created_at ties top-level rows', '2026-01-02', NULL),
 (9000030, 'Post', 40004, 9000010, 6, 'cross-thread reply', '2026-01-03', NULL);
