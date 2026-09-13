# 🪝 Snag: exploratory QA session — `examples/cms` import/export, 2026-09-12

## 🎯 Charter

*Persona × workflow*: a site owner backs up and restores their content —
export, re-import the same file into the same site, and (the scenario this
session was chosen for) restore an export into a **fresh** site, including
a file in a legacy export format (`READABLE_EXPORT_VERSIONS`'s older
entries) — turned out over the course of the session to be neither
narrower nor limited to "an old backup" in the way earlier drafts of this
report claimed (see the two corrections under 🐛 Bug filed below). This
was the first of the
three follow-up charters the prior session proposed
(`docs/reports/2026-09-11-snag-cms-session.md`): "Media library and
import/export — both have strong built-in oracles (MIME allowlist is a
platform contract; import/export idempotency is a clean round-trip
property)." Driven directly over HTTP (`curl` with per-instance cookie
jars) against several live `cms` instances, each on its own fresh
PostgreSQL database, so a "restore into a fresh site" claim could be tested
literally rather than simulated.

Time-boxed to one sitting (spread across ~2 hours of active work, with a
gap in between). Same environment constraint as the prior session: no
Docker daemon in this sandbox, so PostgreSQL 16 ran as a native
`pg_ctlcluster` service; ten separate databases (`cms` through `cms10`,
`cms7` unused — a naming slip, not a missing trial) were created over the
course of the session to model independent site instances without
touching each other's state, as later reproductions (round six onward,
prompted by Codex review comments) needed fresh databases beyond the six
the session started with.

## 📌 Environment

- Commit: `77eccefb5196cae848e3fd8c4c05bf7cfb165fb5` (branch
  `claude/brave-goldberg-gyr60j`, trunk-dev tip at session start)
- Platform: Ubuntu 24.04.4 LTS container, PostgreSQL 16.13 (native
  `pg_ctlcluster` service — Docker daemon unavailable), rustc/cargo 1.94.1
- Each `cms` instance run with `AUTUMN_PROFILE=dev`, `local` blob storage
  (a separate `AUTUMN_STORAGE__LOCAL__ROOT` per instance), auto-migrated on
  first boot, default `autumn.toml` otherwise
- Driven via `curl` with a cookie jar per instance; no browser needed — every
  claim under test is server-side (JSON shape, HTTP status, database rows)

## 🔬 Coverage record

**Toured, and held up against a named oracle:**

- **Same-site idempotent re-import** (oracle: Tools screen's "an existing
  item is left alone rather than duplicated, so re-running an import is
  safe"). Built a real site with nested pages (`/about`, `/about/team`,
  `/company`, `/company/team` — two pages sharing the slug `team` under
  different parents, deliberately, since the code's own comments name this
  as the historically tricky case), a password-protected post, a category
  term, a guest comment, and a media attachment with a featured-image
  association. Exported, then re-imported the same file into the same site:
  "0 imported, 5 already present," post/term/attachment/comment/revision row
  counts unchanged (verified directly in Postgres), nothing duplicated.
- **Fresh-site restore, current format (version 5)**. Imported the same
  export into a brand-new site (fresh database, first account registered
  fresh — the actual "restore after disaster" shape, not same-site
  re-import): "5 imported, 0 already present, 1 comment restored," every
  URL resolved correctly including both same-slug `team` pages under their
  correct distinct parents (no cross-linking, no `team-2` mis-suffix),
  password protection intact (body withheld, `password_protected: true` on
  the REST API), the pending comment's moderation state preserved, and — once
  the blob store's contents were also copied over (a full backup restores
  both halves) — the featured image served byte-for-byte identical with the
  correct content-type. Author fell back to the importing account for
  content whose original username (`admin`) does not exist on the restore
  target, as documented.
- **Renamed-then-reimported page is still recognized as "ours."** Renamed a
  previously-imported page's slug (`team` → `the-team`, keeping its parent)
  on the restore-target site, then re-ran the *original* import file again.
  The importer matched it via its permanent `post_meta` source-slug marker
  (not the current, now-different, slug) and correctly left it alone rather
  than creating a duplicate `team` page beside the renamed one — a real
  "site owner tweaked something, then accidentally re-ran an old backup"
  sequence, and it held.
- **Attachment metadata is not overwritten on re-import** (oracle: the code
  comment "the site's own metadata is more current than the file's").
  Changed an attachment's `alt_text` on the restore-target site, re-imported
  the original export: the local edit survived untouched.
- **MIME allowlist enforced on the import path** (oracle: the
  platform-contract security comment in `media.rs`/`tools.rs` — an import is
  a file, and a tampered one can claim disallowed bytes are something else).
  A crafted export naming an attachment `mime_type: "text/html"` was refused
  outright (422, "this export cannot be restored as it stands") rather than
  silently stored. The *upload* form's own rejection of a disallowed
  `Content-Type` was not independently driven over HTTP this session either
  (only read in `media.rs`) — same gap the prior session left open, carried
  into the next-charter list below rather than claimed as covered here.

**Investigated and ruled out (would have been a false report):** a `500`
(not `404`) the first time `/media/{slug}` was hit against a restore target
whose *database* was restored but whose blob store bytes were not. Looked
like a bug at first glance — an HTTP-semantics oracle says a missing
resource should be a client-facing `404`, not a server error — but the
code's own comment at that exact line names this distinction deliberately:
`attachment.file` being `None` is a 404 by explicit check; `attachment.file`
being `Some` but the underlying blob store lookup failing (bytes never
restored) is treated as "a caller with no better answer" and left as a 500.
That is a real, already-considered design line, not an oversight —
restoring the blob store's actual contents alongside the database (what a
full backup restore does) made the same request return `200` with
byte-identical content, which is what closed this out as a self-inflicted
setup gap rather than a finding. (The source comment attributes a
`file: None` row to "a version-2 import," but that doesn't hold up on a
second look — `Export::attachments` defaults to empty for a version-2 file,
since attachments were only introduced at version 3, so a genuine v2 import
creates no attachment row at all, `file: None` or otherwise; a
`/media/{slug}` request against one 404s from the slug lookup finding
nothing, never reaching the null-handle check. A hand-written row, or an
attachment entry whose `file` field is simply absent from the JSON, is the
scenario that actually reaches it — not specifically "a version-2 import."
Another Codex catch on this PR, not something either cms session verified
independently.)

## 🐛 Bug filed

**[#2737](https://github.com/autumn-foundation/autumn/issues/2737) — import
silently drops a same-slug sibling page whenever an earlier same-slug
sibling in the file was itself inserted before *its own* parent existed
(data loss, repro 6/6).**

The "shallowest first" ordering that already fixes this exact class of bug
(sorting posts by how many `/` their `path` contains, so a parent is
always created before its children) is a no-op whenever a post's `path` is
absent: `identity()` falls back to the bare slug, which never contains
`/`. The condition is purely "this post's `path` is missing" — **not**
"this file's version is old or new." `ExportPost::path` is
`#[serde(default)]` and the importer's version check
(`READABLE_EXPORT_VERSIONS`) only validates the declared `version` number
against an allowlist; it never inspects, requires, or strips `path` based
on that number. So the two are independent: a *period-authentic*
version-2 or -3 export (one an old exporter of this software actually
produced) never carries `path` at all, since that format predates the
field — but an arbitrary file merely *labeled* version 2 or 3 is not
guaranteed to lack it (nothing stops a hand-crafted file from including
`path` values despite an old version label, in which case it sorts
correctly and does not trigger the bug), and conversely a version-4 or
-5 file that simply omits `path` on some posts, while declaring a current
version, hits the identical fallback and the identical bug — confirmed by
a fourth reproduction below, a Codex catch on this PR.

The precise mechanism, refined twice more by Codex catches on this PR,
each verified live rather than accepted from reasoning alone:

- **The parents' own `path` is not part of the condition.** A top-level
  parent's path (`"a"`, `"b"`) never contains `/` either way, so it sorts
  at the same depth-0 tier as a pathless child whether the parent carries
  `path` or not. Verified with the 4-post `version: 5` file, giving the
  two parent posts real `path` values and leaving only the two colliding
  children pathless: still `"3 imported, 1 already present"`, still a 404
  on `/b/team`.
- **Nor is "each colliding page must precede its own parent."** What
  actually matters is only the *first* colliding page in file order: if
  it is inserted while its own parent is still unresolved, it lands as a
  top-level row whose `local_identity` is the bare slug — and *that*
  persisted row is what the *second* colliding page collides against,
  via `find_local`, regardless of whether the second page's own parent
  already exists by then. Verified with the file ordered
  `a/team, b, b/team, a`: `b/team` comes *after* its own parent `b` in the
  file (so `b/team`'s own parent is already resolved when it's processed),
  yet `b/team` is still dropped — because `a/team`,
  processed first while `a` didn't exist yet, was already sitting in the
  database as a top-level page with bare identity `"team"`, and that's
  what `b/team` collided against.

Either way, the import loop misidentifies the *second* colliding page as
"somebody else's row that merely shares the slug" against the first one's
already-persisted state — and drops it permanently, with the summary
screen reporting an unremarkable "N imported, M already present" and no
orphan count. Reproduced 6/6 across independent fresh databases: two on
version 2 (one a hand-reordered full export, one a minimized 4-post
file), one on version 3, one a version-5-labeled file with `path` omitted
on all four posts, one with `path` present on the parents only, and one
reordered so the dropped page follows its own parent in the file. All six
are deterministic given the file's post ordering, not a race.

**Two corrections from earlier drafts of this report, both from Codex
review comments on this PR.** First: the original framing called this a
risk to "an old real backup," reasoning that a page created flat and
later re-parented could plausibly leave a lower row id under a newer
sibling. That doesn't hold up for a *version-2/3* file specifically:
`ExportPost::path`'s own doc comment (`tools.rs`) says the pre-version-4
schema made a page's bare slug its whole identity, so two pages could not
have shared a slug at all under that era's schema — a period-authentic
version-2 or -3 export could never have contained the colliding rows this
repro needs. Second, and this is what the version-5 reproduction above
settles: the bug is not actually confined to old-format files at all, so
"is this reachable from a genuine backup" has a different answer than
either earlier draft gave — it depends only on whether the *specific
posts in question* carry `path`, which a hand-edited file of **any**
accepted version (2 through 5) can omit, and which the importer never
cross-checks against the file's declared version. Filed as data loss on
an input shape this importer accepts across every `READABLE_EXPORT_VERSIONS`
entry and applies no path-presence consistency check to.

Full repro script, root cause, and both oracles are in the issue.

Not added as a regression test in this PR: `.github/workflows/ci.yml`'s
comment on the `cargo test -p cms --test integration_test -- --ignored`
step is explicit that this suite's ignored tests are deliberately **not**
narrowed with a `--skip` list (unlike `saas`/`teams` just above it in the
same job) — "this suite has been run green as a whole from the start... a
new test added to the file runs here automatically." Landing a new,
currently-failing test into that file would turn this job red for every
future PR until the ordering bug is fixed, and choosing to add a `--skip`
line would reverse a deliberate policy stated in that file's own comment —
a call for whoever picks up the fix, not for this QA pass. The issue's
`🔬 Reproduce` section is a self-contained script so the eventual fix PR can
turn it directly into the `#[ignore = "requires Docker (testcontainers)"]`
regression test the sweep will then pick up automatically.

## Findings summary

- **Bugs filed:** 1 — #2737 (data loss on import, when an earlier
  same-slug sibling page lands at the top level — pathless, or top-level
  with `path` — before a later same-slug sibling is checked against it;
  see above — not a risk to ordinary pathless *posts*, which never
  collide on slug in the first place, only to same-slug *pages* under
  different parents).
- **Digest:** none new. The one candidate rough edge investigated this
  session (the `500` on a blob-missing media request) turned out to be
  already-considered, documented behavior, not an oracle-less friction
  point, so it isn't added to the digest either.
- **Solid areas** (toured, held up, no further attention needed absent new
  changes to the surface): same-site idempotent re-import including
  attachment-metadata preservation and rename-survives-reimport via the
  source marker; fresh-site restore of the *current* (version 5) export
  format end-to-end (pages, nested hierarchy including the historically
  tricky same-slug-under-different-parents case, password protection,
  comments and their moderation state, featured images once the blob store
  itself is restored); the import path's MIME allowlist enforcement.

## Proposed next charters

1. **Media library upload itself** (still untouched by either cms session):
   the MIME allowlist is confirmed on the import path, but the upload
   form's own rejection of a disallowed `Content-Type` — plus its other
   multipart handling: oversized files, zero-byte files, a filename that
   is only an extension, concurrent uploads racing the
   `OrphanedBlobGuard` cleanup — has not been driven at all yet.
2. **The scheduled-publishing sweep, end-to-end** — still open from the
   prior session's list. `import_status`'s "an elapsed `future` schedule
   becomes a publish" logic is the import-time analogue of the same sweep
   and reads correctly, but — a Codex catch on this PR — this session
   never actually constructed a `status: "future"` post with an elapsed
   date to drive that branch over HTTP; none of this session's fixtures
   used `future` at all. Both the import-time analogue and the real timer
   sweep remain unverified end-to-end and belong together in a follow-up.
3. **A fix for #2737** would benefit from a design decision on how to order
   posts by ancestry depth when the file carries no `path` (recursively
   resolving each post's `parent` chain rather than counting `/`) — flagged
   in the issue as work for whoever picks it up, not attempted here.
