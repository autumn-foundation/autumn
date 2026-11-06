# ADR 0041: Comment Threads Read in Pages

- Status: Accepted
- Date: 2026-11-06
- Deciders: Autumn maintainers
- Tags: commentable, widgets, performance

## Context

Issue #3325 profiled the reddit-clone read routes on a large fixture. Two
statements used 77% of all buffers:

1. The post page read the whole comment thread (`comment_thread`). The cost
   grew with the thread: 36k heap blocks and a 7.6 MB sort for one post.
   The comment router `GET` and the re-render after a `POST` read the same
   way.
2. The front page "Top posts by votes" summed every row of `votes` on each
   view.

## Decision

### Comment threads

The framework reads a thread in pages of top-level comments.

- `commentable::comment_thread_page(…, after, page_size)` and the generated
  `{Model}Comments::comment_thread_page` read up to `page_size` top-level
  comments after a keyset cursor, and every live reply under them.
- The cursor is a comment id. The SQL reads the `(created_at, id)` key of
  that row, scoped to the same thread. Thus the cursor does not depend on how
  a backend stores time.
- `commentable::thread_page_containing` returns the page that shows one
  comment.
- The router serves one page (`CommentsConfig::page_size`, default 50). After
  a post it shows the page that holds the new comment.
- The widget renders "More comments" and "Earlier comments" links. htmx
  appends the next page with `hx-select`. No new endpoint is necessary.
- `comment_thread` stays. It is the explicit read of the whole thread.

### Leaderboard

reddit-clone reads `posts.score`. `#[votable(aggregate = sum)]` keeps it
equal to the vote sum, in the vote transaction. A partial index
`posts (score DESC, id) WHERE score > 0` gives the order.

## Options not taken

- Covering index on `votes (post_id) INCLUDE (value)` (PR #3331): the read
  stays O(votes), the index is 90 MB, and every vote writes 6.6% more WAL.
- A cached leaderboard: stale, and each miss is still O(votes).
- `OFFSET` pages: the cost grows with the page number.
- A flat `LIMIT` on all comments: it cuts reply trees in half.

## Consequences

- A mounted router shows the first 50 top-level comments, not all of them.
  This is a change in behaviour. The page links show the rest.
- A page holds the full reply tree of each of its top-level comments. A
  top-level comment with very many replies still makes a large page.
- The page read walks replies by the parent column. The comments table needs
  an index on it.
- Rows in a parent cycle (only possible by hand-written SQL) are not under a
  top-level comment, so a page does not show them. `comment_thread` does.
- The leaderboard lists only posts with a positive score.
