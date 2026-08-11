# Migration Guides

**Every release with a breaking change ships a migration guide.** Pre-1.0 that
means most releases: Autumn ships every 2–4 weeks and, until `1.0.0`, a minor
bump may break existing apps (see the [Stability Policy](../../STABILITY.md)).
A release without an upgrade path is treated as a broken build — the guide is a
gate, not a courtesy (issue #1588).

The gate is `scripts/check-migration-guides.sh`, which runs on every pull
request and in the publish gate. See
[Enforcement](#enforcement-scriptscheck-migration-guidessh) below.

## Index

- [`0.4.0.md`](0.4.0.md) — `autumn-web 0.3.x → 0.4.0`
- [`0.5.0.md`](0.5.0.md) — `autumn-web 0.4.x → 0.5.0`
- [`0.6.0.md`](0.6.0.md) — `autumn-web 0.5.x → 0.6.0`
- [`next.md`](next.md) — rolling draft for `## [Unreleased]`, renamed to
  `<version>.md` at release time

- [`TEMPLATE.md`](TEMPLATE.md) — template for new migration guides. Copy this
  when starting the guide for the next release.

Guides are not backfilled for releases earlier than `0.4.0`.

## Declaring a breaking change in the CHANGELOG

A breaking change is declared **either** with the inline `**Breaking:**` token
**or** under a `### Breaking Changes` heading. Nothing else counts — the gate
reads the changelog, and prose it cannot see strands users:

```markdown
- **repository:** **Breaking:** `with_pool` is renamed to `with_pool_untracked`.
  Uses on generated repositories must be updated (only the name changes). See
  the [migration guide](docs/migrations/0.6.0.md).
```

Every breaking entry must link its own guide, so a reader lands on the fix path
straight from the changelog line. Entries under `## [Unreleased]` link
[`next.md`](next.md).

If a change is *not* breaking, say so in the words the gate recognises —
"non-breaking", "no breaking change", "without breaking …" — and the line
passes untouched. A `**Breaking:**` token inside a code span is a mention, not
a declaration, so documenting the convention costs nothing.

For an entry that talks *about* breaking changes without being one — release
tooling, policy docs — append an explicit suppression naming its reason:

```markdown
- **release:** the gate fails when a section declares a breaking change with no
  guide. <!-- migration-guide-gate: describes the gate itself -->
```

It is greppable and shows up in the diff, so using it on a real break is a
reviewable act rather than a silent one.

## Process for a breaking release

1. **Open the draft with the first breaking change.** The first PR that lands a
   breaking change after a release copies [`TEMPLATE.md`](TEMPLATE.md) to
   [`next.md`](next.md) and links it from its changelog entry.
2. **Grow the guide with each breaking change.** Every subsequent
   breaking-change PR appends a section with *before* / *after* snippets and the
   compiler error the user will see. For a contributor opening a
   breaking-change PR, that section is part of "done".
3. **Perform the guide-only walk-through before publishing.** Upgrade an app
   scaffolded with `autumn new` on the *previous* release using only the guide —
   no changelog, no source reading — and record the result in the guide's
   `### Guide-only upgrade walkthrough` section. See
   [`docs/release-checklist.md`](../release-checklist.md).
4. **Rename at release.** `next.md` becomes `<version>.md`, its version
   placeholders are filled in, the index above is updated, and the changelog
   links are repointed from `next.md` to `<version>.md`.

## Enforcement (`scripts/check-migration-guides.sh`)

```bash
./scripts/check-migration-guides.sh          # the gate
./scripts/check-migration-guides.sh --list   # inventory per changelog section
```

It fails when:

- a changelog entry describes breaking something without the marker (so an
  unmarked break cannot hide from the coverage check);
- a section with a breaking entry has no guide at `docs/migrations/<version>.md`
  (or `next.md` for `## [Unreleased]`). A release candidate section
  (`## [0.7.0-rc.1]`) is gated against its release's guide, `0.7.0.md`;
- a breaking entry does not *link* its guide — a bare path mention is not a
  link, the reader has to be able to click through;
- a guide is missing a required section — *At a glance*, *Summary*, *Before you
  start*, *Breaking changes*, *How to verify*, and *Guide-only upgrade
  walkthrough* — or has a heading with nothing under it, still carries
  `TEMPLATE.md`'s banner or placeholders, or is not indexed above;
- a released guide's walk-through is not recorded as `performed YYYY-MM-DD`
  (or explicitly `backfilled`); `pending` is allowed only on `next.md`;

  `next.md` is a **draft**, so it is also exempt from the placeholder and
  empty-section checks — the release checklist recreates it from
  `TEMPLATE.md` after every release, and the template ships placeholders by
  design. Every exemption lapses the moment it is renamed to `<version>.md`.
- it cannot read the changelog: a `## ` heading it fails to parse and an
  unclosed code fence are hard errors, because either one silently removes
  whole sections from every check above.

Details that save a round trip: `**Breaking:**` is matched case-insensitively,
`- ` and `* ` are both bullets, and a marker inside a code span is a mention.
Fenced code blocks (``` or `~~~`) are skipped wholesale in both the changelog
and the guides — a `breaking` key in a config sample is not a breaking change,
and a guide cannot satisfy its own required headings from inside an example.

The lint is textual, so it removes the *silent* failure mode rather than
replacing review: a break described without the word "breaking" and without the
marker still needs a reviewer to catch it.
