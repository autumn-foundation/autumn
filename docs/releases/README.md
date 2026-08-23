# Release Walkthroughs

A narrative tour of each release: what changed, why, and what it looks like in
an application. These sit alongside — not instead of — the two documents that
are gates rather than prose:

- [`CHANGELOG.md`](../../CHANGELOG.md) is the complete, entry-by-entry record.
- [`docs/migrations/`](../migrations/README.md) is the upgrade path, and is
  enforced by `scripts/check-migration-guides.sh`.

A walkthrough is neither. It is the document you send someone who asks "what's
new in this one?" and does not want 144 changelog entries.

## Index

- [`0.7.0.md`](0.7.0.md) — *Ship it, then prove it*: host-preparing deploys and
  fleets, deterministic simulation testing, `#[translatable]` /
  `#[commentable]` / `#[votable]` / `position`, failure-capsule replay,
  call-site metrics, and a request path that allocates ~59% less.

## Writing one

Cut it with the release, after the CHANGELOG section is finalised, and link it
from the release's migration guide *At a glance* header. Keep it thematic —
group by what a reader is trying to do, not by changelog category — and quote
real numbers and real API spellings from the changelog rather than
paraphrasing them.
