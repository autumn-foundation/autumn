# 🪝 Snag: exploratory QA session — `examples/cms` import/export, 2026-09-12

## 🎯 Charter

*Persona × workflow*: a site owner backs up and restores their content —
export, re-import the same file into the same site, and (the scenario this
session was chosen for) restore an export into a **fresh** site, including an
**old backup taken in a legacy export format**. This was the first of the
three follow-up charters the prior session proposed
(`docs/reports/2026-09-11-snag-cms-session.md`): "Media library and
import/export — both have strong built-in oracles (MIME allowlist is a
platform contract; import/export idempotency is a clean round-trip
property)." Driven directly over HTTP (`curl` with per-instance cookie
jars) against several live `cms` instances, each on its own fresh
PostgreSQL database, so a "restore into a fresh site" claim could be tested
literally rather than simulated.

Time-boxed to one sitting (~2 hours). Same environment constraint as the
prior session: no Docker daemon in this sandbox, so PostgreSQL 16 ran as a
native `pg_ctlcluster` service; six separate databases (`cms`, `cms2` … `cms6`)
were created to model six independent site instances without touching each
other's state.

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
- **MIME allowlist enforced on import, not just upload** (oracle: the
  platform-contract security comment in `media.rs`/`tools.rs` — an import is
  a file, and a tampered one can claim disallowed bytes are something else).
  A crafted export naming an attachment `mime_type: "text/html"` was refused
  outright (422, "this export cannot be restored as it stands") rather than
  silently stored.
- **A row whose blob store bytes were never restored answers 404 for a
  `file: null` handle** — read but not separately re-verified this session,
  since the prior session and the code's own comment already cover it
  directly; not re-litigated here.

**Investigated and ruled out (would have been a false report):** a `500`
(not `404`) the first time `/media/{slug}` was hit against a restore target
whose *database* was restored but whose blob store bytes were not. Looked
like a bug at first glance — an HTTP-semantics oracle says a missing
resource should be a client-facing `404`, not a server error — but the
code's own comment at that exact line names this distinction deliberately:
`attachment.file` being `None` (a version-2 import, or a hand-written row)
is a 404 by explicit check; `attachment.file` being `Some` but the
underlying blob store lookup failing (bytes never restored) is treated as
"a caller with no better answer" and left as a 500. That is a real,
already-considered design line, not an oversight — restoring the blob
store's actual contents alongside the database (what a full backup restore
does) made the same request return `200` with byte-identical content, which
is what closed this out as a self-inflicted setup gap rather than a finding.

## 🐛 Bug filed

**[#2737](https://github.com/autumn-foundation/autumn/issues/2737) — legacy-format
(version 2/3) import silently drops a same-slug sibling page when its parent
is created later in the same run (data loss, repro 4/4).**

The "shallowest first" ordering that already fixes this exact class of bug
for the *current* export format (sorting posts by how many `/` their `path`
contains, so a parent is always created before its children) is a no-op for
version-2 and version-3 files: those predate the `path` field, so
`identity()` falls back to the bare slug, which never contains `/` — every
page in a legacy file sorts as depth `0`. When two same-slug pages under
different parents are listed in the file before their respective parents (a
plain function of the exporting site's row-id order, e.g. a page created
flat and re-parented later), the import loop misidentifies the *second* one
as "somebody else's row that merely shares the slug" against the *first*
one it just created — and drops it permanently, with the summary screen
reporting an unremarkable "N imported, M already present" and no orphan
count. Reproduced 4/4 (two independent fresh databases on version 2, one on
version 3, all deterministic given the file's post ordering — not a race).
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

- **Bugs filed:** 1 — #2737 (data loss on legacy-format import; see above).
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
   the MIME allowlist is now confirmed on both the upload *and* import
   paths, but the upload form's own multipart handling — oversized files,
   zero-byte files, a filename that is only an extension, concurrent
   uploads racing the `OrphanedBlobGuard` cleanup — has not been driven at
   all yet.
2. **The scheduled-publishing sweep, end-to-end** — still open from the
   prior session's list; a good pairing with this one, since
   `import_status`'s "an elapsed `future` schedule becomes a publish"
   logic is the import-time analogue of the same sweep and was exercised
   only through the import path this session, never through the real timer.
3. **A fix for #2737** would benefit from a design decision on how to order
   posts by ancestry depth when the file carries no `path` (recursively
   resolving each post's `parent` chain rather than counting `/`) — flagged
   in the issue as work for whoever picks it up, not attempted here.
