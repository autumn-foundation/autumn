# 🪝 Snag: exploratory QA session — `examples/reddit-clone`, 2026-09-03

## 🎯 Charter

*Persona × workflow*: a returning visitor registers, creates a community,
submits a post, votes, and threads comments on it — driven directly over
HTTP (curl / raw sockets, cookie jar carried by hand) against a live
`reddit-clone` instance, bypassing the browser entirely so CSRF, session,
and validation behavior could be observed at the wire level. Concerns:
auth/session/CSRF, votable race-safety, commentable depth/authorization
bounds, record-level authorization, and input-boundary handling (NUL bytes,
oversized bodies, injection-shaped strings, malformed path params).

Time-boxed to one sitting (~2 hours wall clock, most of it environment
setup: no Docker in this sandbox, so Postgres 16 was run as a native
`postgresql` service instead of the `driver.sh` container path, and Tailwind
was skipped as irrelevant to HTTP-level behavior).

## 📌 Environment

- Commit: `c0f6545` (branch `claude/brave-goldberg-vp0nct`, at the tip of
  `trunk-dev` when the session started)
- Platform: Ubuntu 24.04 container, Postgres 16 (native service, not the
  `autumn-run-pg` container the skill normally uses — Docker was unavailable)
- `reddit-clone` run with `AUTUMN_PROFILE=dev`, `AUTUMN_DATABASE__URL`
  pointed at a fresh local database, default `autumn.toml` otherwise (CSRF
  enabled, submit-token protection enabled, idempotency enabled)
- Driven via `curl` with a cookie jar, and a small `http.client`-based
  Python harness (`naughty.py`) for payloads awkward to pass through a shell
  (NUL bytes, RTL/astral-plane unicode, oversized bodies)

## 🔬 Coverage record

**Toured, and held up against a named oracle:**

- **Votable race-safety** (oracle: `docs/guide/votable.md` race-safety
  contract). `submit()` (`routes/posts.rs`) hand-writes a self-upvote —
  a raw `INSERT INTO votes` plus `score = 1` in the same transaction as the
  post insert, bypassing `react()` entirely — so every post starts already
  upvoted by its author (worth calling out on its own: it's exactly the
  "hand-written insert/update/delete on the edge table" pattern
  `docs/guide/votable.md`'s "Known limits and warnings" flags as leaving
  `score` stale, safe here only because both writes happen atomically
  against a `score` the code computes by hand, not because the pattern is
  generally safe). Corrected re-run, thread-barrier-synchronized for true
  concurrency (not shelled-out `curl … &`) rather than my first pass's
  sequential-looking backgrounded loop: a *fresh* post starts at
  `score = 1` / one `votes` row (the author's auto-upvote) before any test
  vote is cast. 10 truly concurrent same-value `POST /upvote` requests from
  one other session then landed back at `score = 1` / one row — consistent
  with `react()`'s documented toggle (an even number of toggles from a
  voted starting state returns to voted), not a parity violation. The
  underlying `votes.id` did change between runs (row deleted and
  re-inserted an odd number of times across the ten toggles), confirming
  the deletes/inserts actually happened serially rather than being
  silently dropped. My first pass reported the `score = 1` / one-row
  outcome without stating the pre-existing auto-vote, which made the
  parity look unexplained — flagged by an automated PR reviewer
  (`chatgpt-codex-connector`) on this same report and confirmed by
  rerunning against a known-fresh post; corrected here. A same-user
  re-upvote afterward correctly toggled the vote off (`score` back to 0,
  row deleted) — matches the toggle semantics documented in
  `routes/votes.rs`.
- **Commentable depth bound** (oracle: `docs/guide/commentable.md` §"Depth
  is bounded on the write path", `max_depth = 5` default). Chained replies
  0→5 all inserted; the depth-6 attempt was refused with `"Replies are
  nested at most 5 deep here"` and no row was written. Confirmed the HTTP
  status for that refusal is `200` (not `422`) for both htmx and plain-form
  submissions — this looked like a doc mismatch at first (the repository
  function's rustdoc says `422`), but `docs/guide/commentable.md`'s widget
  section explicitly documents the router swallowing the `422` and
  re-rendering inline instead, specifically so htmx doesn't refuse to swap
  a non-2xx response. Working as designed, not a finding.
- **Cross-record comment grafting** (same oracle). `reply_to` naming a
  comment that belongs to a *different* post's thread was refused
  (`"Cannot reply to that comment: it is not on this record"`); no comment
  was inserted under the second post.
- **Record-level authorization on post edit/delete** (oracle:
  `examples/reddit-clone/src/policies.rs`, `PostPolicy`: update/delete
  restricted to the author or an admin). A second registered user could not
  load `GET .../edit` (`404`) or forge `POST` to the update route with a
  *correctly-scoped* CSRF token (still `404`, title unchanged in the DB).
  No CSRF, no auth — `403`.
- **Stored-XSS / rich-text sanitization** (oracle: reddit-clone's own edit
  form copy, *"Raw HTML and images are removed when the post is
  displayed"*). Submitted a title and body carrying `<script>`,
  `<img onerror>`, and a `javascript:` link. Raw text is stored verbatim
  (documented as intentional — the author sees what they typed on re-edit),
  but the rendered show page HTML-escapes the title, escapes/strips the
  `<script>`/`<img>` in the Markdown-rendered body, and drops the
  `javascript:` link's `href` entirely (renders as plain text). SEO
  `<meta>` tags built from the same input are correctly attribute-escaped.
- **CSRF enforcement**. Missing `_csrf` field and a well-formed-but-wrong
  token both got `403` on `POST /submit`; no post was created either way.
- **Boundary tour on path params**. `/posts/-1`, `/posts/0`,
  `/posts/999999999999999999999999` (i64 overflow), `/posts/1.5`,
  `/posts/abc`, `%00`-embedded segments, `../` traversal attempts, and a
  CRLF-in-path header-injection attempt all resolved to `400`/`404` — no
  `500`s, no traversal.
- **Data tour on comment body**: NUL byte (`"has\x00nul"`) → `200` with
  *"The submitted text contains a NUL character (0x00), which cannot be
  stored."* — this is the behavior added by commit 6435128 (*"Reject a NUL
  byte in a form field instead of 500ing at Postgres"*), confirmed working
  end-to-end through the commentable router. Whitespace-only body →
  rejected ("Comment cannot be empty"). A 1MB body → rejected ("Comment is
  too long (limit 10000 bytes)"). Emoji / Arabic / Hebrew / mathematical
  double-struck unicode → stored and rendered intact. SQL-injection-shaped
  string, `%s`-format-string-shaped string, and a CRLF-header-injection-
  shaped string were all stored and rendered back as inert text — no
  injection, no corruption.

**Touched but not exhaustively toured** (candidates for a follow-up
charter):

- Interrupt tour proper — cancel-mid-submit, browser back-and-resubmit,
  double-click race on `POST /submit` itself. `reddit-clone`'s own forms
  (submit, register, comment) do not opt into the framework's
  [one-time submit-token protection](../guide/submit-tokens.md) (`grep` for
  `_submit_token` / `SubmitToken` in `examples/reddit-clone/src` returns
  nothing), so a double-click almost certainly *does* create duplicate
  posts/subreddits/comments — but since the framework never claims this is
  automatic (the guide is explicit that it's per-form opt-in) and the app
  makes no claim of double-submit protection either, this isn't a
  documented-claim violation. It reads as a legitimate gap in the example's
  own showcase, not a framework or app bug — flagged here as a rough edge
  rather than filed.
- Account lockout (`docs/guide/authentication.md` §"Account lockout") does
  not apply to this app at all: `reddit-clone`'s `users` table is
  hand-rolled (no `failed_attempts`/`locked_at` columns, no
  `[auth.lockout]` config) rather than generated via `autumn generate
  auth`, so the framework's lockout feature was never exercised here.
  Twelve consecutive bad-password attempts against a real account all
  returned an identical `400 Bad Request` / "Invalid username or password"
  — consistent (no enumeration signal), but there is no lockout behind it
  to test. **Correction**: an earlier draft of this report proposed
  re-running this charter against `examples/saas` or `examples/teams`,
  assuming one of them used `autumn generate auth`. A reviewer
  (`chatgpt-codex-connector`) checked and both turned out to hand-roll
  their own login handlers and user schemas the same way reddit-clone
  does — no `failed_attempts`/`locked_at` columns, no `[auth.lockout]`
  config, no `/auth/admin/unlock` route in either. Re-checked directly:
  **no example shipped in this repo actually uses the generated lockout
  feature** (`grep -rl failed_attempts examples/*/src examples/*/migrations`
  matches nothing; the columns exist only in
  `autumn-cli/src/generate/auth.rs`'s template output). The follow-up
  charter below is corrected accordingly.
- Session cookie carries `Secure` even though the app was driven over
  plain `http://127.0.0.1` — this is `[session] secure = true` by design
  (documented default, works in Chromium/Firefox's loopback exception).
  Not investigated further: the existing
  `docs/reports/auth-pipeline-security-audit-2026-08.md` already covers the
  session/CSRF cookie surface at the framework level in more depth than
  this session attempted.
- Never reached: avatar upload, the live SSE feed (`/posts/stream`,
  `/r/{slug}/posts/stream`), tags on posts, the webhook intake route, and
  any multi-session/two-tab concurrency scenario (two browsers voting or
  commenting as the same or different users at once, beyond the 10-way
  concurrent-upvote race already covered).

## Findings summary

- **Bugs filed: 0.** Nothing found here clears the hard gate (minimal
  repro + rate + pinned environment + named oracle + dedup search) — every
  oracle checked came back agreeing with the implementation.
- **Digest (oracle-less, not filed as bugs):**
  - Double-click / back-button resubmission on `reddit-clone`'s own forms
    likely creates duplicate posts/comments/subreddits, since none of them
    use `SubmitToken`. Expectation, not a documented contract — the example
    simply doesn't showcase this framework feature.
- **Solid areas** (toured, held up, no further attention needed absent new
  changes to the surface): votable race-safety and toggle semantics,
  commentable depth/cross-record bounds, post edit/delete authorization,
  XSS/rich-text sanitization on post title+body, CSRF enforcement on
  `/submit`, path-param boundary handling, and comment-body input handling
  (NUL bytes, size limits, blank input, unicode, injection-shaped strings).

## Proposed next charters

1. **Interrupt tour, for real** — script actual double-click/back-button
   races against `reddit-clone`'s submit/register/comment forms and confirm
   (or rule out) duplicate-row creation, now that the mechanism (`no
   SubmitToken`) is already identified. If duplicates are confirmed, this
   becomes a legitimate digest entry (or a bug, if any of these forms turn
   out to have an implicit uniqueness claim to violate) rather than a guess.
2. **Account lockout** — no shipped example uses `autumn generate auth`'s
   lockout feature (see correction above), so this charter needs a fresh
   fixture: run `autumn generate auth` against a scratch app (or extend
   `examples/saas`/`examples/teams` with it) to actually exercise the
   threshold/cooloff/same-response-while-locked/admin-unlock-refusal
   behavior documented in `docs/guide/authentication.md`.
3. **Two-tab / multi-session state tour**: same account voting from two
   sessions concurrently, comment thread refresh mid-reply-chain, session
   revocation while a second tab holds a live SSE connection.
4. **Avatar upload and the live SSE feed** — untouched this session; both
   are plausible homes for state/interrupt-class bugs (partial uploads,
   stream reconnect behavior) that a curl-only session can't easily reach
   without more scaffolding.
