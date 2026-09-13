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
`pg_ctlcluster` service; fifteen separate databases (`cms` through
`cms15`, `cms7` unused — a naming slip, not a missing trial) were created
over the course of the session to model independent site instances
without touching each other's state, as later reproductions (round six
onward, prompted by Codex review comments) needed fresh databases beyond
the six the session started with.

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
silently drops a pathless nested page whenever that same bare slug is
already spoken for — either by a persisted top-level (or otherwise
depth-tied) row `find_local` matches, or, more broadly still, by *any*
earlier pathless import's `_import_source_slug` marker, even one attached
to a page that is correctly, deeply nested and has nothing to do with the
top level at all (data loss, repro 10/10).**

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
- **The *second* (incoming) page must be pathless — but "both pages
  pathless" overstates the requirement on the first (persisted) page's
  side.** `find_local` compares the *second* page's own file `identity()`
  against the first row's already-persisted `local_identity`; it is not a
  blanket "any row with this slug blocks any other." Verified by giving
  the second `Team` page (under `b`) its own correct `path: "b/team"`
  while leaving the first (under `a`, still unresolved when processed)
  pathless: the file identity of the second page is now `"b/team"`, which
  does not equal the first page's persisted `local_identity` of `"team"`,
  so `find_local` finds no match and the second page is created normally.
  The import reported `"4 imported, 0 already present"`, `/b/team`
  resolved `200`, and — because the deferred ancestry pass (see the
  "shallowest first" comment in `tools.rs`) re-parents the first page once
  its own parent `a` is later created in the same run — `/a/team` also
  resolved `200` and both pages ended up correctly nested with no loss at
  all. A Codex catch on this PR, verified live on a fresh database
  (`cms11`) rather than accepted from reasoning alone.

  But the *first* (persisted) page's own `path` field, if it has one, is
  irrelevant either way — `local_identity()` recomputes purely from actual
  database ancestry (`page_ancestry`), never from whatever `path` the row's
  originating file entry happened to carry. Verified with a ninth
  reproduction (database `cms14`): a top-level `Team` page imported with an
  *explicit* `"path": "team"` (not omitted at all) still collides with a
  later, separately-imported pathless `Team` page nested under `b` exactly
  as when the first page had no `path` field — `"1 already present"`,
  `/b/team` → 404. So the real asymmetry is: the *incoming* page being
  checked must be pathless (or otherwise resolve to the bare slug) for the
  collision to fire, but the *persisted* row it collides against can have
  gotten its top-level `local_identity` from any combination of `path`
  presence, absence, or content in whatever file originally created it —
  that detail never survives into the database. A Codex catch on this PR,
  verified live rather than accepted from reasoning alone.
- **Nor does the first page's parent need to have been unresolved at all
  — a genuinely, permanently top-level page triggers it the same way.**
  Every reproduction so far involved a first page whose parent was merely
  *not yet created* at the moment it was processed. That framing is too
  narrow: `find_local` does not care *why* a persisted row's
  `local_identity` is the bare slug, only that it is. Verified with a file
  containing a **real** top-level page (`"parent": null`, no unresolved
  anything) named `Team`, a top-level page `B`, and a *third*, pathless
  `Team` page nested under `b` — no page named `a` anywhere in the file:
  the nested `Team` was still dropped (`"2 imported, 1 already present"`,
  `/b/team` → 404), even though the first `Team` was never "waiting" on
  anything; it was simply, correctly, a top-level page, and that alone was
  enough to collide. A Codex catch on this PR, verified live (`cms12`).
- **And it isn't limited to one import run — and a Codex catch on this PR
  showed the cross-run case is actually caught by *both* internal checks
  at once, not just one.** The colliding row does not need to come from
  the same file at all: an *existing* top-level page on the site, created
  by an entirely separate, earlier import (or, by the same logic, by
  ordinary page authoring through the admin UI), blocks a later import's
  pathless nested page with the same slug just as permanently. Verified
  across two sequential imports on a fresh database (`cms13`): first, a
  file creating top-level `Team` and `B` pages only (`"2 imported, 0
  already present"`); then, a *second*, separate import of a single
  pathless `Team` page nested under `b` — dropped (`"1 already present"`,
  `/b/team` → 404) even though the first import had already fully
  committed and the site was in a completely settled state by the time
  the second import began. Reading `imported_source_slugs`/
  `record_import_source` in `tools.rs` and `content.rs`, then querying
  `cms13`'s `post_meta` table directly, showed the first run's `Team` row
  carrying `_import_source_slug = "team"`. An earlier revision of this
  report claimed this meant `find_local` was "never reached" here — that
  was wrong, and a Codex catch caught it: `slug_taken` (which calls
  `find_local`) is computed *unconditionally* on every iteration, before
  the `marker_owned` branch is even inspected (`tools.rs:955-957`,
  ahead of the `if let Some(ours) = marker_owned` at line 959) — so
  `find_local` *is* called for this input, and it *also* returns a match,
  since the persisted top-level page's `local_identity` ("team") equals
  the incoming pathless page's file identity ("team") too. Both checks
  agree the post should be skipped. What's true is narrower than "only
  one check applies": the code *acts* on whichever check's branch runs
  first, and `marker_owned`'s branch is checked before `slug_taken`'s
  (line 959 precedes line 1032), so the marker match is what actually
  produces the observed skip — but `slug_taken` would independently
  produce the same skip immediately afterward if the marker check alone
  were fixed and no longer matched. Within a *single* run, only
  `find_local`/`slug_taken` can ever apply — `imported_source_slugs` is a
  fixed snapshot loaded once before the loop starts, so a page created
  earlier in the *same* run has no marker yet for a later post in that run
  to match against — but across two separate import requests, both checks
  independently return true. This means the bug is not really about
  import *ordering* at all in the general case, and a fix confined to
  `find_local` alone, or to the marker key alone, is insufficient for the
  cross-run reproductions specifically: closing either one leaves this
  exact input still dropped via the other.
- **And the marker path (Path B) doesn't need a top-level row at all — a
  Codex catch on this PR found it fires even against a page that is
  correctly, deeply nested and has nothing to do with the top level.**
  `record_import_source` always records the *file's own* `identity()`
  string as the marker key — never the row's actual resolved position —
  so a pathless page whose `parent` already exists at import time, and
  which therefore gets created at its *correct* nested location, still
  leaves behind a marker keyed on its bare slug alone. Verified with a
  tenth reproduction (`cms15`): imported `A` (top-level) and a pathless
  `Team` under `a` in one file — `Team` lands correctly at `/a/team`
  (confirmed `200`), and `post_meta` shows its `_import_source_slug` is
  the bare `"team"`, not `"a/team"`. A wholly separate, later import of
  `B` (top-level) plus a pathless `Team` under `b` then drops the second
  `Team` (`"1 already present"`, `/b/team` → 404) — even though
  `find_local("page", "team")` would find *no* top-level `team` row at
  all (`/a/team`'s `Team` is genuinely, correctly nested, not sitting at
  the bare path), so Path A does not apply here. This is Path B firing
  completely alone, and it means the earlier framing of the general
  condition — "a page already persisted as a top-level row" — was too
  narrow: any earlier pathless import of a page with that slug, correctly
  nested anywhere, poisons the bare-slug marker namespace for every
  subsequent import indefinitely.

Either way — via `find_local`'s bare-identity match against a persisted
top-level row, and/or via the `_import_source_slug` marker matching *any*
earlier pathless import regardless of where that import's page actually
ended up — the import loop misidentifies the pathless nested page as
content that is already accounted for, and drops it permanently, with the
summary screen reporting an unremarkable "N imported, M already present"
and no orphan count. Reproduced 10/10
across independent fresh databases: two on version 2 (one a
hand-reordered full export, one a minimized 4-post file), one on version
3, one a version-5-labeled file with `path` omitted on all four posts,
one with `path` present on the parents only, one reordered so the dropped
page follows its own parent in the file, one where the earlier same-slug
page is a genuine, permanently top-level page rather than a
temporarily-unresolved one, one where the earlier same-slug page comes
from a wholly separate, already-completed prior import rather than the
same run, one where that earlier, separately-imported page carried an
explicit `path` equal to its own bare slug rather than omitting `path`
altogether, and one where the earlier same-slug page is not top-level at
all but correctly, deeply nested — dropping the later page purely through
the marker path, with no top-level row for `find_local` to match. Every
case that *does* involve file ordering within a single run is a pure
function of that ordering, not a race — but, per the last four
reproductions, neither same-run ordering, the persisted row's own `path`
history, nor even that row being top-level is actually a precondition of
the bug at all; only the *incoming* page's own pathlessness is.

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

- **Bugs filed:** 1 — #2737 (data loss on import: a pathless nested page is
  silently dropped whenever that same bare slug is already spoken for —
  either by a page persisted as a top-level row (pre-existing on the
  site, from an earlier separate import, or created moments earlier in
  the same run before its own parent existed), or, more broadly, by *any*
  earlier pathless import's leftover marker, even one attached to a page
  that is correctly, deeply nested and was never top-level at all — and no
  error, orphan count, or other signal is given; see above — not a risk to
  ordinary pathless *posts*, which never collide on slug in the first
  place, only to same-slug *pages* under different parents).
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
3. **A fix for #2737** needs to close two distinct code paths, not one —
   and, for the cross-run reproductions specifically, closing only one of
   them is not enough on its own. Recursively resolving each post's
   `parent` chain (rather than counting `/`) would close the *same-run*
   ordering cases, which go through `find_local` alone. But in the
   `cms13`/`cms14` cross-run reproductions, `find_local` *also* matches
   (confirmed by querying `cms13`'s `post_meta` table directly: the
   persisted top-level page and the incoming pathless page share the
   identity `"team"`) — it is the `imported_source_slugs` marker lookup,
   checked first, that happens to be what actually produces the observed
   skip there, a Codex catch on this PR. So fixing the marker lookup's key
   alone would still leave the cross-run cases dropped via `find_local`
   immediately afterward, and fixing `find_local`'s ancestry-blindness
   alone would still leave them dropped via the marker match. Worse, the
   marker lookup's own key problem is not confined to top-level rows
   either — a Codex catch on this PR (`cms15`) showed `record_import_source`
   always records a page's *file* identity, never its actual resolved
   position, so a pathless page correctly nested anywhere still poisons
   the bare-slug marker namespace for every later import indefinitely,
   with no top-level row for an ancestry-aware `find_local` to ever catch.
   A complete fix needs both: `find_local`'s comparison of an incoming
   page's *file* identity against a persisted row's *ancestry-derived*
   identity should account for where the incoming page's file `parent`
   chain actually says it belongs, and the `imported_source_slugs` lookup
   needs a key derived from each page's *actual* resolved position (e.g.
   its resolved parent chain) rather than the bare file identity alone —
   flagged in the issue as work for whoever picks it up, not
   attempted here.
