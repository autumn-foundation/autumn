# 🪝 Snag: exploratory QA session — `examples/reddit-clone` post-submit race, 2026-09-06

## 🎯 Charter

Follow-up charter #1 proposed by the prior session
(`docs/reports/2026-09-03-snag-reddit-clone-session.md`): "script actual
double-click/back-button races against `reddit-clone`'s submit/register/comment
forms and confirm (or rule out) duplicate-row creation, now that the mechanism
(no `SubmitToken`) is already identified." Interrupt tour, persona: an
authenticated user double-clicks Submit (or a flaky client silently retries) on
the post-create form.

Time-boxed to one sitting. No Docker in this sandbox (as in the prior
session), so Postgres 16 ran as a native service rather than the
`docker-compose.yml` container path.

## 📌 Environment

- Commit: `f920bc8` (branch `claude/brave-goldberg-0s1r1j`, `trunk-dev` tip at
  session start)
- Platform: Ubuntu 24.04 container, PostgreSQL 16.13 (native service), rustc
  / cargo 1.94.1
- `reddit-clone` run with `AUTUMN_PROFILE=dev`, `AUTUMN_DATABASE__URL`
  pointed at a fresh local database, default `autumn.toml` otherwise (CSRF
  enabled)
- Driven via `curl` with a cookie jar (both a `threading.Barrier`-synchronized
  Python harness for guaranteed simultaneity, and plain backgrounded
  `curl … & wait` to confirm the simpler repro also holds)

## 🔬 Coverage record

**Investigated and filed:**

- **Concurrent `/submit` races two posts onto the same slug** — filed as
  [#2544](https://github.com/autumn-foundation/autumn/issues/2544), data
  integrity, repro 15/15 (10/10 barrier-synchronized at N=2, 5/5 plain
  backgrounded `curl`). `unique_slug()`'s check-then-act (`SELECT count`, then
  later `INSERT`) has no database backstop — `posts.slug` carries only a
  plain index, not the `UNIQUE` constraint that `subreddits.slug` and
  `users.username` both have — so two concurrent submits with the same title
  both see `count == 0` and both insert. The visible effect is stronger than
  a plain duplicate row: `show()`'s `.filter(slug.eq(...)).first()` has no
  `ORDER BY`, so one of the two post IDs becomes permanently unreachable at
  its own canonical URL — visiting it renders the *other* post's title, body,
  votes, and comments, with a `200` and no error anywhere in the path.
- This directly resolves the prior session's open question. That session
  observed the mechanism (no `SubmitToken` on this form) but filed it only
  as an oracle-less digest entry, reasoning that "the app makes no claim of
  double-submit protection." Driving it confirmed that reasoning was right
  for a *generic* duplicate row, but incomplete for this specific field: the
  app does make an implicit uniqueness claim on `posts.slug` (the existence
  of `unique_slug()`, the sibling DB-backed uniqueness on subreddits/users,
  and `show()`'s single-row lookup all assume it), and that claim is what
  breaks. The digest entry in the prior report should be read as superseded
  by #2544 for the slug-collision angle specifically; the broader
  "posts/comments have no double-submit backstop at all" observation still
  stands as a design note, not a bug, for any duplicate-row case that
  *doesn't* have a claimed uniqueness key behind it.

**Touched but not exhaustively toured** (candidates for a follow-up charter):

- Register and community-create were re-confirmed non-racy (both already
  backed by real `UNIQUE` constraints — `users.username`,
  `subreddits.slug` — so a racing double-submit lands as a rejection on the
  loser, not a duplicate; consistent with the prior session's finding).
- Comment creation (`reply_to` under a post) was not raced this session —
  it has the same "plain insert, no uniqueness key" shape as post creation
  but no `unique_slug`-style invariant sits behind it, so a race there would
  most likely be a plain duplicate comment (oracle-less, digest-only per the
  prior session's reasoning) rather than a wrong-content bug. Worth a
  dedicated pass to confirm rather than assume.
- The two-tab / multi-session state tour, avatar upload, and the live SSE
  feed remain untouched, as in the prior session.

## Findings summary

- **Bugs filed: 1.** [#2544](https://github.com/autumn-foundation/autumn/issues/2544)
  — concurrent `/submit` races duplicate a post's slug; the permalink
  silently serves the wrong post. Data integrity, medium severity (no crash,
  no data loss, no security exposure — but user-invisible wrong content with
  no error signal anywhere in the path). No fix proposed: the correct
  remedy is a design decision, out of charter for a QA-only pass — a
  composite `UNIQUE` constraint/index on `(subreddit_id, slug)` with
  `ON CONFLICT` retry, an advisory lock keyed on `(subreddit_id, slug)`, and
  serializing inserts by locking the *parent subreddit row* with
  `SELECT ... FOR UPDATE` (not the post row, which doesn't exist yet at the
  point uniqueness needs to be checked — a Codex review on this PR
  correctly flagged an earlier draft of this list for eliding that
  distinction, and for calling the index option "partial," which implies a
  `WHERE` predicate this table has no soft-delete column to justify) are
  each reasonable but not equivalent in behavior/cost.
- No regression test committed this session: reproducing this reliably needs
  true concurrent requests against a live Postgres-backed server
  (`threading.Barrier`-style synchronization), which doesn't fit the
  workspace's existing `#[tokio::test]` / testcontainers integration-test
  shape without a design decision of its own (how to synchronize two
  in-process requests against the same handler to guarantee they race). This
  sandbox also has no Docker, so a testcontainers-based version could not be
  validated against the real CI harness before landing. Flagged as a
  follow-up rather than guessed at.
- **Solid areas** (re-confirmed, no new attention needed): register/login
  and community-creation uniqueness under concurrency.

## Proposed next charters

1. **Land a fix for #2544** and, in the same PR, a `#[ignore =
   "requires Docker (testcontainers)"]` regression test exercising two
   concurrent `/submit` requests against a real Postgres instance — this
   session intentionally left that to whoever picks up the fix, per Snag's
   "reproduction over patch" charter, since designing the concurrency
   backstop and the test that proves it belong together.
2. **Race `reddit-clone`'s comment-creation path** the same way, to confirm
   or rule out the weaker "plain duplicate comment" hypothesis the prior
   session left as a digest entry.
3. Continue down the prior session's remaining list: two-tab/multi-session
   state tour, avatar upload, live SSE feed.
