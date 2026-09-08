# 🪝 Snag: exploratory QA session — `examples/reddit-clone` interrupt tour, 2026-09-08

## 🎯 Charter

*Persona × workflow*: a user double-clicks Submit / has a flaky connection that
retries a POST, on `reddit-clone`'s "create a post" and "post a comment" forms
— driven directly over HTTP (curl, cookie jar carried by hand, true concurrent
requests fired from a synchronization point) against a live instance. This is
the direct follow-up to charter #1 queued by the prior session
(`docs/reports/2026-09-03-snag-reddit-clone-session.md`), which identified —
by reading the code, not by driving it — that neither form opts into the
framework's submit-token protection, and proposed confirming with an actual
race instead of a guess.

Also touched, opportunistically while set up: whether the framework's own
`CommentThread` widget (the thing `reddit-clone`'s comment form is built from)
exposes any way to opt into that protection at all.

Time-boxed to one sitting (~90 minutes wall clock: ~30 min re-reading the
relevant code and the prior report to avoid duplicating it, ~15 min
environment setup — same native-Postgres-16 + native-Redis path as the prior
session, since Docker is unavailable in this sandbox too — ~15 min build,
~20 min driving, ~10 min write-up).

## 📌 Environment

- Commit: `ac5a7ff` (branch `claude/brave-goldberg-37tzx3`, tip of `trunk-dev`
  at session start)
- Platform: Ubuntu 24.04.4 LTS container, `cargo 1.94.1`
- Postgres 16 native service (`autumn`/`autumn` role, fresh `reddit_snag` db,
  dropped at session end), Redis 6 (`redis-server`, default config,
  backgrounded for the session, killed at session end) — `reddit-clone`
  compiles the `redis` feature but nothing in this session's paths needed it
  at runtime (idempotency backend stayed the dev default, `memory`)
- `reddit-clone` run with `AUTUMN_PROFILE=dev`,
  `AUTUMN_DATABASE__URL=postgres://autumn:autumn@127.0.0.1:5432/reddit_snag`,
  default `autumn.toml` otherwise (CSRF enabled, submit-token layer installed
  at the framework level per its `enabled = true` default, idempotency
  enabled, `memory` backend). Tailwind CLI not installed — irrelevant to
  HTTP-level behavior, so skipped (page renders unstyled, all routes/forms
  still present and functional).
- Driven via `curl` with a cookie jar; concurrency via backgrounded (`&`)
  `curl` invocations issued from one shell statement plus `wait`, all against
  one already-warm connection pool (not the first request to the server, so
  no cold-start skew between racers)

## 🔬 Coverage record

**Toured, and held up against a named oracle:**

- **`docs/guide/submit-tokens.md`'s own framing of the double-submit gap.**
  The doc states plainly that submit-token protection is opt-in per form (the
  layer "scans the form body for the configured field... A request with no
  such field passes straight through unchanged, so only forms that embed the
  field are guarded") and names "post a comment" by name as one of the
  motivating "no natural uniqueness key" examples the feature exists to
  solve. Confirmed directly: `GET /submit` and the post-detail page's comment
  form (`GET /r/{slug}/posts/{slug}`) both render a `_csrf` hidden field but
  **no** `_submit_token` field (`grep -o 'name="_submit_token"'` against both
  rendered pages: zero matches). This matches the prior session's read of the
  source and the doc's own stated default — not a violation of any claim.

**Newly driven this session (the actual interrupt tour, not a read):**

- **`POST /submit` under true concurrency.** Five identical POST requests
  (same session cookie, same CSRF token, same title/body/subreddit_id) fired
  from one synchronization point (`for i in 1..5; do curl ... & done; wait`)
  all returned `303` — all five succeeded, none replayed, none conflicted.
  Queried directly: **5 distinct `posts` rows**, same title, slugs
  `double-click-race-test` through `-5` (the `unique_slug`/retry-on-conflict
  path from #2544 doing exactly its documented job of avoiding a *slug*
  collision, not a *content* duplicate). Rate: **5/5**, reproduced once, no
  retries needed — this is deterministic given the precondition (a form with
  no `_submit_token` field, hit concurrently), not a flake.
- **`POST /comments/Post/{id}` under the same concurrency.** Five identical
  comment submissions (same body text) against the post created above, same
  synchronization pattern, all returned `200`. Queried directly: **5
  identical `comments` rows** — same `body`, same `author_id`, same
  `commentable_id` — with no uniqueness backstop at all (unlike posts, the
  `comments` table has no unique constraint that could even coincidentally
  catch this; its only indexes are `id`, `author_id`, `parent_id`, and the
  thread-listing index). Rate: **5/5**, same determinism as above. No errors,
  panics, or warnings beyond expected `slow database query` log lines from
  the serialized retry contention on the `/submit` case.
- **Oracle check on both.** Neither clears the bug bar:
  `docs/guide/submit-tokens.md` explicitly documents this exact outcome as
  what happens to a form that doesn't render the token field, and neither
  form (nor the framework's `CommentThread` widget that builds the comment
  form — see below) claims otherwise anywhere in its own docs or UI copy.
  This *confirms* (rather than merely repeats) the prior session's digest
  entry: what was a plausible-but-untested read of the source is now a
  driven, reproducible 5/5 result. Still routed to the digest, not the
  tracker, for exactly the reason the prior session gave.

**One level deeper than the prior session went — a framework-side gap, not an
app-side one:**

- `reddit-clone`'s comment form is not hand-rolled; it comes from
  `autumn_web::widgets::CommentThread` (`autumn/src/widgets.rs`), the
  framework's reusable commentable-model UI. Checked its public builder
  surface: `CommentThread` has `csrf_token()` / `csrf_field()` methods that
  thread CSRF protection into every rendered form, mirroring
  `NestedChangesetForm`'s builder — but has **no `submit_token()` /
  `submit_field()` equivalent**, while `NestedChangesetForm` (the
  framework's other major generated-form widget, for master-detail forms)
  *does* have exactly that (`with_submit_token`, per
  `autumn/src/nested_form.rs:1045`, and documented in
  `submit-tokens.md`'s "Related" section: "Nested `has_many` forms... already
  integrate submit tokens via `NestedChangesetForm::with_submit_token`").
  So an app author using the built-in comment widget — for the literal
  example `submit-tokens.md` names when motivating the whole feature —
  cannot opt into the protection the framework itself flags as the fix,
  through the widget's own API. They would have to abandon
  `CommentThread` and hand-write the comment form to get it, defeating the
  point of the widget.
  - **Not filed as a bug**: no doc promises `CommentThread` includes this,
    so there is no claim to violate — it is an asymmetry in the framework's
    own DX surface (one generated-form widget integrates the feature its own
    docs recommend, a sibling widget for the doc's own headline example does
    not), not a broken contract. Flagged here as a concrete, scoped
    **test-gap / feature-gap candidate** for a framework maintainer:
    `submit_token()`/`submit_field()` builder methods on `CommentThread`,
    mirroring `NestedChangesetForm::with_submit_token`'s existing shape. Not
    opened as a fix PR here — it needs a design call this charter is not
    positioned to make alone (default behavior for existing callers who
    don't pass a token, whether `from_spec` should thread it automatically
    given a `SubmitToken` extractor, back-compat for apps rendering the
    widget from a cached/pre-existing markup snapshot) and is not the "≤10
    lines, unambiguous under a named oracle" bar this charter can self-fix.

## Findings summary

- **Bugs filed: 0.** Both driven races landed exactly where the framework's
  own documentation says an unprotected form will land — no oracle is
  violated by either outcome.
- **Digest (oracle-less, not filed as bugs):**
  - `POST /submit` double-click / retry creates a second, fully-visible
    (different slug, same content) post — **now confirmed 5/5 under true
    concurrency**, upgraded from the prior session's code-read hypothesis.
  - `POST /comments/{type}/{id}` double-click / retry creates a fully
    duplicate comment row with **no** uniqueness backstop of any kind —
    **confirmed 5/5**, and a worse user-visible symptom than the post case
    (identical comments stacked in the same thread, versus differently-slugged
    posts that at least read as distinct URLs).
  - **New this session**: `CommentThread` (framework widget) has no
    `submit_token`/`submit_field` builder, unlike its sibling
    `NestedChangesetForm`. Proposed as a scoped framework feature-gap for a
    maintainer to pick up — see above for why it's not self-fixed here.
- **Solid areas**: no crashes, panics, 500s, or data corruption from either
  race — five concurrent writers landed as five clean, independently valid
  rows in both cases; the `unique_slug` retry loop (#2544) held up correctly
  under real (not simulated) concurrent contention, serializing cleanly
  through Postgres's unique-constraint conflict-and-retry path with only
  `slow database query` warnings as a visible side effect.

## Proposed next charters

1. **`submit_token()` on `CommentThread`** — a framework-team-owned design
   task (not a Snag fix): add the builder method, decide the default when
   omitted (today's behavior, unchanged), and land a regression test that a
   token-bearing render round-trips through the widget's hidden field the
   same way `NestedChangesetForm::with_submit_token` already does. `reddit-clone`
   would then be the natural place to actually *wire it up* (`comment_thread`
   already takes a `SubmitToken`-shaped value nowhere today), closing the
   digest entry above for real.
2. **Account lockout**, still open from the prior session: no shipped example
   uses `autumn generate auth`'s lockout feature, so exercising
   `docs/guide/authentication.md`'s threshold/cooloff/admin-unlock contract
   needs a fresh scaffold (`autumn generate auth` against a scratch app, or
   added to `examples/saas`/`examples/teams`).
3. **Two-tab / multi-session state tour** and **avatar upload / live SSE
   feed** — both still untouched; this session stayed scoped to the queued
   interrupt-tour follow-up rather than opening new surface.
