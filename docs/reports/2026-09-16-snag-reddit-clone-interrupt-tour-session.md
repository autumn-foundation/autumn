# 🪝 Snag: exploratory QA session — `examples/reddit-clone`, interrupt tour (double-submit races)

## 🎯 Charter

*Persona × workflow*: a user double-clicks **Submit** (or a flaky mobile
connection retries the POST) while creating a post or leaving a comment on
`reddit-clone` — driven as truly concurrent HTTP requests (thread-barrier
synchronized, not backgrounded shell loops) against a live instance, to
**confirm or rule out** duplicate-row creation. This is the follow-up the
[2026-09-03 reddit-clone session](2026-09-03-snag-reddit-clone-session.md)
proposed as its next charter #1, after concluding from code-reading alone
that `/submit` and the comment form "almost certainly" create duplicates
since neither embeds the framework's `_submit_token` field. This session
replaces "almost certainly" with a measured rate.

Time spent: ~1.5 hours (environment setup dominated again: no Docker in this
sandbox, so Postgres 16 and Redis were run as native services rather than
containers).

## 📌 Environment

- Commit: `9823eca` (`trunk-dev` tip at session start), workspace version
  `0.7.0`, Linux container, rustc/cargo `1.94.1`.
- Postgres 16 native service (`postgres://autumn:autumn@127.0.0.1:5432/reddit`,
  overriding the app's default `AUTUMN_DATABASE__URL` which points at a
  Dockerized `127.0.0.1:5433`), Redis native service on `6379` (unused by the
  scenarios below — the app's default idempotency/submit-token backend is
  `memory`).
- `reddit-clone` built and run as the real compiled binary
  (`cargo build -p reddit-clone`, `AUTUMN_PROFILE=dev`, `127.0.0.1:3000`),
  migrations applied automatically on boot (`embed_migrations!()` +
  `autumn_web::migrate::FRAMEWORK_MIGRATIONS`).
- Driven with a small Python/`requests` harness
  (`race_submit.py`, reproduce section below) using `threading.Barrier` to
  synchronize N threads so every request is issued in the same instant, not a
  backgrounded-and-hopefully-overlapping loop. One correction needed versus a
  naive `requests.Session()`: this app's `[session] secure = true` (by
  design — session cookie carries `Secure` even on loopback HTTP, matching
  the note in the 2026-09-03 report) makes `requests`' cookie jar silently
  refuse to send `autumn.sid` back over plain `http://127.0.0.1` (unlike a
  real browser, which special-cases the loopback address). Worked around by
  tracking `Set-Cookie` values by hand and sending them as a raw `Cookie`
  header on every request instead of relying on the jar.

## Method and result

1. Registered a user, created a subreddit (`POST /r/create`), then fired
   **10 truly concurrent, byte-identical `POST /submit`** requests (same
   session, same `_csrf`, same title/body/subreddit_id) at a barrier release.
2. Registered a second user, opened the resulting post's page, and fired
   **10 truly concurrent, byte-identical `POST /comments/Post/{id}`**
   requests (same session, same `_csrf`, same comment body) at a barrier
   release.
3. Read `autumn/src/security/submit_token.rs` and
   `docs/guide/submit-tokens.md` first to confirm the oracle question before
   driving anything: is submit-token protection framework-default-on (as the
   startup log line `One-time submit-token protection enabled` suggested) or
   per-form opt-in? The guide is explicit both ways at once, and both halves
   matter here: *"Submit-token protection is on by default — the framework
   installs `SubmitTokenLayer`..."* — but the layer only ever guards a
   request that already carries the `_submit_token` field; a form that never
   emits the hidden input for the extractor's `SubmitToken` is never
   protected regardless of the layer being mounted (confirmed in code by the
   guard's own `missing_token_passes_through` unit test, and confirmed live
   below). `grep -rn "_submit_token\|SubmitToken" examples/reddit-clone/src`
   returns nothing — no form in this app opts in.
4. Also read `autumn/src/commentable.rs`'s `add_comment` — a plain
   `insert_comment` inside a transaction, no uniqueness column, no
   submit-token/idempotency involvement at all — and
   `docs/guide/submit-tokens.md`'s own "The problem" section, which uses
   *"post a comment"* verbatim as its example of a mutation with "no natural
   uniqueness key" that submit tokens are meant to close.
5. Separately, noticed `examples/reddit-clone/src/routes/posts.rs`'s
   `submit` handler already has *some* race-hardening: issue #2544 added a
   `unique_slug`/`is_post_slug_conflict` retry loop (backed by the
   `posts_subreddit_id_slug_key` composite `UNIQUE (subreddit_id, slug)`
   constraint added in migration `20260906163932`) specifically because two
   concurrent identical submits used to race a slug SELECT-then-INSERT and
   let the loser silently overwrite/collide with the winner's permalink. This
   is a **different** race than the one this charter targets: its own
   regression test (`tests/post_slug_race_e2e.rs`, `#[ignore]`, needs
   Docker) asserts only that no two posts ever *share* a slug — the retry
   loop's whole job is to hand every racing submit its own suffixed slug
   (`-2`, `-3`, ...) so they *all succeed as separate posts*, not to collapse
   them into one. Worth being precise about this distinction up front, since
   a shallower read could mistake "there's already a race fix here" for
   "duplicate posts from a double-click are already handled" — they are not
   the same claim, and the retry loop's own design guarantees the opposite
   of the second one.

**Result — both scenarios, 10/10, reproduced against the real running
app and confirmed in the database, not inferred from response codes alone:**

- **Posts**: all 10 concurrent `/submit` requests returned `303` (success).
  The `posts` table gained **10 distinct rows**, same title/body/author/
  subreddit, slugs `race-test-post-<ts>` through `-10` (the #2544 retry loop
  working exactly as designed — see point 5). All 10 are independently
  visible on the subreddit's front page and each has its own permalink.
- **Comments**: all 10 concurrent `POST /comments/Post/{id}` requests
  returned `200`. The `comments` table gained **10 identical rows** (same
  body, same author, same parent). The post's `comment_count` counter cache
  correctly reads `10` — the counter-cache mechanism itself is not at fault;
  it accurately counts 10 real, distinct, duplicate rows.

```
 id |           title           |             slug             | subreddit_id
----+---------------------------+------------------------------+--------------
  1 | Race test post 1789542866 | race-test-post-1789542866    |            1
  9 | Race test post 1789542866 | race-test-post-1789542866-2  |            1
 17 | Race test post 1789542866 | race-test-post-1789542866-3  |            1
... (10 rows total)

 id |          body            | commentable_id | author_id
----+--------------------------+----------------+-----------
  1 | Race comment 1789542957  |              1 |         6
  2 | Race comment 1789542957  |              1 |         6
... (10 rows total)
```

## Findings

**Bugs filed: 0.** No oracle is violated:

- `docs/guide/submit-tokens.md` is explicit that the protection is per-form
  opt-in (*"Two steps opt an individual form in"*) — it never claims any
  particular app's forms use it, and `reddit-clone`'s don't.
- `docs/guide/commentable.md`'s own "What this deliberately does not do"
  list doesn't promise duplicate-submission protection either, and its
  worked idempotency example is specifically about *delete* (`delete_comment`
  is documented and tested as idempotent — a double-submit there is safe by
  design), not create.
- No README or in-app copy in `reddit-clone` claims duplicate-post or
  duplicate-comment protection.
- The #2544 slug-uniqueness fix is real and does its documented job (no two
  posts ever share a slug/permalink) — it does not purport to be a
  duplicate-content guard, and its own regression test's invariant
  (`assert_no_duplicate_slugs`) is satisfied even by this session's 10
  intentionally-duplicated posts.

**Digest entry (confirmed, promoted from the prior session's "likely" to a
measured 10/10 live reproduction):**

> Double-clicking **Submit** on `reddit-clone`'s post-creation or
> comment-creation forms reliably creates one full duplicate row per extra
> click — confirmed 10/10 at true concurrency, not just theorized from a
> missing `_submit_token` grep. Every duplicate post gets its own
> independently-suffixed permalink (courtesy of the unrelated #2544 slug fix)
> and shows up as N identical entries on the subreddit's own front page;
> every duplicate comment shows up as N identical entries in the thread with
> a correctly-inflated `comment_count`. This is expectation, not a
> documented contract — `SubmitTokenLayer` exists in this exact codebase,
> ships default-on at the middleware level, and `docs/guide/submit-tokens.md`
> literally names "post a comment" as its own motivating example — but no
> form in this showcase app takes the two-line step (a `SubmitToken`
> extractor param plus a hidden `_submit_token` field) to actually turn it
   on. Worth a maintainer's attention as a showcase gap even though it
   clears no bug bar: this is the framework's flagship example, the exact
   feature that would close the hole already exists and is mounted, and the
   guide's own docs use this app's own workflow as the textbook case for why
   the feature exists.

**Solid areas** (toured, held up): the `posts_subreddit_id_slug_key`
uniqueness invariant survives real concurrent duplicate submissions exactly
as `tests/post_slug_race_e2e.rs` expects — 10 racing identical submits never
collided on a slug, each got its own suffix; the `comment_count` counter
cache stayed numerically accurate under the same real race (10 rows in, `10`
reported, no drift); session cookie `Secure`-on-loopback behavior noted
again (matches the 2026-08 security audit and the 2026-09-03 report — not a
finding, a client-tooling gotcha for anyone else scripting against this app
over plain HTTP).

## Proposed next charters

1. **The showcase fix itself** — wiring `SubmitToken` into `reddit-clone`'s
   submit/comment/register/community-create forms would be a legitimate,
   maintainer-facing improvement (arguably more valuable as a "fix the
   example" PR than as a QA finding, since Snag's charter is repro-and-report,
   not design decisions — and choosing *which* forms, and whether registration
   and community-create need it given their existing uniqueness backstops, is
   exactly that kind of call).
2. **Two-tab / multi-session state tour** — still untouched: same account
   voting from two sessions concurrently, comment thread refresh mid-reply-
   chain, session revocation while a second tab holds a live SSE connection.
3. **Avatar upload and the live SSE feed** (`/posts/stream`,
   `/r/{slug}/posts/stream`) — still untouched, both plausible homes for
   state/interrupt-class bugs a curl/Python-only session reaches less easily.
4. **Cross-example submit-token audit** (carried over from the 2026-09-11
   cms session's proposed charters) — now that the missing-token pattern is
   confirmed live (not just via code-reading) on two independent mutation
   types in one app, a project-wide sweep across every shipped example for
   the same gap — and specifically for which of those mutations lack any
   uniqueness backstop at all (the "flips it from digest to bug" condition
   named in that report) — would settle the question everywhere at once.

## Reproduce

```bash
# Environment (no Docker in this sandbox — native services instead):
service postgresql start
redis-server --daemonize yes --port 6379
sudo -u postgres psql -c "CREATE ROLE autumn WITH LOGIN PASSWORD 'autumn' SUPERUSER;"
sudo -u postgres psql -c "CREATE DATABASE reddit OWNER autumn;"

cd autumn  # repo root
AUTUMN_DATABASE__URL="postgres://autumn:autumn@127.0.0.1:5432/reddit" cargo build -p reddit-clone
AUTUMN_DATABASE__URL="postgres://autumn:autumn@127.0.0.1:5432/reddit" AUTUMN_PROFILE=dev \
  ./target/debug/reddit-clone &

# Harness: register a user, create a subreddit, fire 10 truly concurrent
# identical POST /submit at a thread barrier, then re-run the same pattern
# against POST /comments/Post/{id}. Full script used this session is in the
# report's accompanying scratch harness (register -> /r/create -> /submit
# x10 concurrent; register a second user -> GET the post -> POST
# /comments/Post/{id} x10 concurrent). Verify via:
psql "postgres://autumn:autumn@127.0.0.1:5432/reddit" \
  -c "SELECT count(*) FROM posts WHERE title = '<the race title>';"
psql "postgres://autumn:autumn@127.0.0.1:5432/reddit" \
  -c "SELECT count(*) FROM comments WHERE body = '<the race comment body>';"
# expect (before any SubmitToken fix): 10 and 10.
```
