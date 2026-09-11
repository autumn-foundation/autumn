# 🪝 Snag: exploratory QA session — `examples/cms`, 2026-09-11

## 🎯 Charter

*Persona × workflow*: a site owner registers as the first account, then
drives the admin content workflow — create/edit/publish/password-protect a
post, moderate a guest comment, change the permalink structure live, and
push a post through 29 edits to probe the revision cap — plus a returning
visitor unlocking a password-protected post and posting a comment. Driven
directly over HTTP (`curl` with a hand-carried cookie jar) against a live
`cms` instance, with one real-Chromium check (Playwright) where a
curl-only signal would have been misleading. Concerns: the state-machine
lifecycle, the documented WordPress-parity claims in
`examples/cms/README.md`, permalink round-tripping, and the interrupt tour
(double-submit).

Time-boxed to one sitting (~2.5 hours wall clock, a large share of it
environment setup: no Docker in this sandbox, so Postgres 16 ran as a
native service rather than the `docker-compose.yml` path, and `diesel_cli`
would not build here — `libpq-dev` failed to fetch — so migrations were
applied via the embedded-migration `AUTUMN_MIGRATE=1` one-shot instead of
`autumn migrate`).

## 📌 Environment

- Commit: `c078091` (branch `claude/brave-goldberg-igwpja`, `trunk-dev` tip
  at session start)
- Platform: Ubuntu 24.04 container, Postgres 16.13 (native `pg_ctlcluster`
  service, not the container path — Docker daemon unavailable), rustc/cargo
  1.94.1
- `cms` run with `AUTUMN_PROFILE=dev`, `AUTUMN_MIGRATE=1` then a normal
  boot, database matching `docker-compose.yml` (`autumn`/`autumn`/`cms`),
  default `autumn.toml` otherwise
- Driven via `curl` with a cookie jar; one check used Playwright against
  the pre-installed Chromium (`/opt/pw-browsers/chromium-1194`)

## 🔬 Coverage record

**Toured, and held up against a named oracle:**

- **Password-protected posts** (oracle: README's "Password-protected
  posts... body withheld from the page, the feed **and** the REST API").
  Set a password on a published post: front-end page hides the body behind
  an unlock form, `/api/v1/posts/{id}` drops both `body` and reports
  `password_protected: true`, and the Atom feed excludes the body. The
  correct password (submitted to `/unlock/{id}`, not the post's own URL —
  the front-end page's own `<form action="/unlock/{id}">`) sets a cookie
  that unlocks the front-end page only; a wrong password leaves it locked;
  the REST API stays withheld regardless of the unlock cookie, matching the
  "and the REST API" clause literally (no session-based bypass).
- **Permalink structure change, live** (oracle: `permalinks.rs`'s own
  doc comment, quoted in README: "changing the setting does not 404 the
  URLs already in the wild, which is the single most common WordPress
  permalink complaint"). Published a post under `post_name`, then changed
  `[settings] permalink_structure` to `month_and_name` via
  `/admin/settings`. The old bare-slug URL kept returning `200`, the new
  dated URL also resolved (`200`), and both the REST API's `url` field and
  the homepage's post link switched to the new shape immediately — this is
  an end-to-end HTTP-level confirmation of a property the codebase already
  covers with a unit-level round-trip test, and it held.
- **Revision cap** (oracle: README's "capped at 25 per post"). Edited one
  post's body 29 times through the real admin form (fetching and resending
  the `lock_version` hidden field each time, so this is the actual
  optimistic-lock path a browser would exercise, not a bypass of it). The
  revisions screen showed exactly 25 entries, the 5 oldest correctly
  evicted.
- **Guest comment moderation + sanitization**. A guest comment containing
  `<script>alert(1)</script>`, an `onerror` payload, and a
  `javascript:` link landed in the pending queue exactly as entered
  (correctly HTML-escaped, not executed, in the admin moderation list);
  after approval the same payload rendered on the public post page as
  inert escaped text — comments are not Markdown-rendered at all (unlike
  post bodies, which go through `render_content`'s sanitize→shortcode→
  filter pipeline), so there was never an HTML-injection surface to begin
  with. Aside: `examples/cms/Cargo.toml`'s dependency comment says
  Markdown rendering is something "the editor and comment bodies both
  need" — comment bodies visibly do not use it (plain escaped text via the
  framework's `widgets::CommentView`/`comment_thread`). Not filed: it is
  an internal doc comment, not a user-facing claim, and the actual
  behavior is safe either way — noted here only so a future session
  doesn't re-discover it as a lead.
- **State-machine edges**: `draft → publish → password-protect → unlock`,
  `draft → trash` all behaved as declared in `Post`'s `#[state_machine]`.
  Did not attempt to find an undeclared-edge violation — the README
  already cites this as directly covered by an existing test
  (`compile_fail`-style refusal), and a fresh manual search over ~19
  declared edges without a specific lead looked lower-value than the other
  charters this session.
- **Emoji/unicode in title** — a `🎉🎊` emoji-prefixed title round-tripped
  correctly end-to-end (admin edit form, REST API, and the public page's
  `<title>`/`<h1>` all preserved it); the slug generator correctly
  stripped the emoji and fell through to the ASCII words, and
  `ensure_unique_slug` correctly suffixed the second `party-post`
  collision. (First pass of this check mis-fetched a sibling post's URL
  and looked like silent emoji-stripping on the front end — re-verified
  against the REST API's canonical `url` before treating it as a lead, and
  it was a self-inflicted false alarm, not a bug.)

**Investigated and ruled out (would have been false reports):**

- **Session cookie carries `Secure` over plain `http://localhost:3000`.**
  Registering and immediately loading `/admin` with a spec-strict cookie
  jar (Python's `http.cookiejar`, which enforces `Secure` literally) drops
  the session and bounces to `/login` — looked like a broken quick-start
  at first. Re-checked with real Chromium (Playwright, headless, against
  the same running server): the cookie **is** sent back over
  `http://localhost:3000`, because Chromium (and Firefox) special-case
  `localhost`/`127.0.0.1` as a potentially-trustworthy origin for the
  `Secure` attribute. `[session] secure = true` is documented as the
  literal default in `docs/guide/authentication.md`'s config example, dev
  profile does not override it (only prod's smart-defaults block does),
  and the framework's dev server binds `127.0.0.1` by default — both of
  which are in the browser loopback exception. This exact question was
  independently investigated (with the same conclusion) by the prior Snag
  session against `reddit-clone`
  (`docs/reports/2026-09-03-snag-reddit-clone-session.md`), which pointed
  at `docs/reports/auth-pipeline-security-audit-2026-08.md` as the deeper
  treatment. Confirming it again here against a second app used a
  strict-cookiejar client rather than trusting memory of browser behavior,
  and it holds for `cms` too.
- **Double-submit on the admin post-create form and the guest-comment
  form creates duplicate rows.** Two concurrent identical
  `POST /admin/content/post` (backgrounded `curl … & … & wait`, no
  scripting harness needed) reliably created two published posts with the
  same title and suffixed slugs (`published-race-test`,
  `published-race-test-2`), both publicly live at `200`; the same pattern
  reproduced 2/2 for a guest comment on `/comments/{id}`. Neither
  `/admin/content/{post_type}/new` nor the comment form embeds
  `_submit_token` (`docs/guide/submit-tokens.md`'s opt-in field), so
  neither is guarded by the framework's one-time-submit protection.
  **This looked, before checking precedent, like a strong bug** — the
  submit-tokens guide names "post a comment" by name as its motivating
  double-submit scenario. Checked against
  `docs/reports/2026-09-03-snag-reddit-clone-session.md` and its filed
  follow-up (#2544) first, since they cover the identical mechanism on a
  different app, and the precedent is direct: a plain missing-`SubmitToken`
  duplicate row is *not* a documented-claim violation
  ("the framework never claims `SubmitToken` protection is automatic...
  and the app makes no claim of double-submit protection either") —
  it only becomes a bug when the duplicate also breaks an invariant the
  app itself claims, which is what #2544 found in `reddit-clone`
  (colliding, not just duplicate, slugs, because that app's `unique_slug()`
  was a bare count-then-insert with no database-level `UNIQUE` constraint
  backing it, so the app's own single-row lookup non-deterministically
  served the wrong post at a shared URL). Checked whether `cms` has the
  same TOCTOU hole: it does not — `idx_posts_type_slug` and
  `idx_posts_bare_path_slug` are real `CREATE UNIQUE INDEX`es
  (`migrations/20260908005714_create_content_schema/up.sql`), and
  `insert_with_unique_slug` retries per-attempt on a savepoint specifically
  to survive a racing collision (confirmed by the observed behavior: every
  trial produced two *distinct*, validly suffixed slugs, never a
  collision). So the `cms` instance of this pattern is the same
  already-triaged rough edge as `reddit-clone`'s, not a new bug — see the
  digest entry below rather than a filed issue.

**Not reached this session** (candidates for a follow-up charter):

- Media library upload (MIME allowlist enforcement, image variant
  generation) — never touched; needs `multipart` exercised over HTTP,
  which is more scaffolding than a curl-only session easily reaches.
- Import/export JSON round-trip, claimed idempotent on
  `(post_type, slug)` — a good target for a differential-result oracle
  (export, mutate nothing, re-import, diff) next time.
- Scheduled publishing's actual sweep (`#[scheduled(every = "1m")]`) —
  `require_future_publish_date`'s validation was read and looks correct
  (rejects a missing or past date, and a DST-nonexistent local time), but
  the sweep itself moving a `future` post to `publish` at its scheduled
  time was never observed end-to-end; waiting out a real minute-boundary
  sweep didn't fit this session's time box.
- Users/roles admin (the "guard against removing the last administrator"
  claim), widgets/sidebars, themes, shortcodes, the REST API beyond single
  posts, the sitemap/robots.txt, and any multi-session/two-tab concurrency
  scenario beyond the one interrupt-tour race above.

## Findings summary

- **Bugs filed:** none. Every oracle checked this session (password
  protection, permalink round-trip, revision cap, comment sanitization,
  state-machine edges as covered by the existing test) came back agreeing
  with the implementation.
- **Digest (oracle-less, not filed as bugs):**
  - Double-click / resubmit on `cms`'s admin post-create form and the
    guest-comment form creates duplicate rows (distinctly slugged, no
    collision, no misrouting — the DB-backed uniqueness index holds).
    Same triaged status as the equivalent `reddit-clone` gap: an
    expectation, not a documented contract, since neither form opts into
    `SubmitToken` and the app makes no claim it will collapse a double
    submission into one row. Worth a single follow-up across every
    Autumn-shipped example (`grep -rL _submit_token examples/*/src`) if
    the project ever decides this is worth standardizing rather than
    leaving per-example.
  - `examples/cms/Cargo.toml`'s dependency comment overclaims: it says
    the Markdown-sanitize pipeline is something "the editor and comment
    bodies both need," but comment bodies render as escaped plain text,
    never through `render_content`. Cosmetic (an internal comment, not a
    shipped claim), fix-on-touch rather than worth its own PR.
- **Solid areas** (toured, held up, no further attention needed absent new
  changes to the surface): password-protected content across page/feed/API,
  live permalink-structure switching, the revision cap and eviction order,
  guest-comment moderation and its escaping, emoji/unicode round-tripping
  through slug generation, and the loopback `Secure`-cookie exception
  (now confirmed against a second example app).

## Proposed next charters

1. **Media library and import/export** — both untouched this session and
   both have strong built-in oracles (MIME allowlist is a platform
   contract; import/export idempotency is a clean round-trip property).
2. **Scheduled publishing, end-to-end** — schedule a post a few minutes
   out and actually observe the `#[scheduled]` sweep publish it, rather
   than only reading the validation that gates entry into `future`.
3. **Cross-example submit-token audit** — now that the missing-`SubmitToken`
   pattern is confirmed independently on two example apps
   (`reddit-clone`, `cms`) with the same "not a bug without a broken
   invariant" conclusion both times, a single session auditing every
   shipped example for the pattern (and specifically for any app where the
   uniqueness backstop is *not* DB-enforced, which is what would flip it
   from digest to bug) would settle it project-wide instead of
   rediscovering it example by example.
