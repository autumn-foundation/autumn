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
**or** under a `### Breaking Changes` heading — that exact heading, not any
heading starting with the word. Nothing else counts: the gate reads the
changelog, and prose it cannot see strands users.

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

A bullet parked inside `<!-- ... -->` renders nowhere, so it is not an entry and
declares nothing. The one HTML comment the gate *does* read is the suppression
token below — deliberately, so it stays invisible to readers and visible in the
diff.

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
  link, the reader has to be able to click through. The destination is parsed,
  not searched for, so `[guide](next.md "why")` and `[guide](<next.md>)` both
  count and a path named only in a link *title* does not;
- a guide is missing a required section — *At a glance*, *Summary*, *Before you
  start*, *Breaking changes*, *How to verify*, and *Guide-only upgrade
  walkthrough* — or has a heading with nothing under it, still carries
  `TEMPLATE.md`'s banner or any placeholder token `TEMPLATE.md` itself emits
  (an HTML comment is not content — `<!-- TODO -->` under a heading is a stub)
  (the vocabulary is read from the template, so `{:?}` and `Route { .. }` in a
  guide are fine and `{X.Y.Z}` in a code sample is not), or is not linked from
  the Index above (a mention elsewhere in this file is not an index entry);
- a released guide's walk-through status does not **begin** with
  `performed YYYY-MM-DD` or `backfilled`; `pending` is allowed only on
  `next.md`. The whole value is checked, not searched — `not performed
  2026-08-11` says the opposite of what a substring match would conclude;

  `next.md` is a **draft**, so it is also exempt from the placeholder and
  empty-section checks — the release checklist recreates it from
  `TEMPLATE.md` after every release, and the template ships placeholders by
  design. Every exemption lapses the moment it is renamed to `<version>.md`.
- `next.md` is missing. The rolling draft is permanent, not conditional on
  there being an unreleased break: this file and `STABILITY.md` both link it by
  name, and nothing here checks markdown links;
- it cannot read the changelog: a `## ` heading it fails to parse and an
  unclosed code fence are hard errors, because either one silently removes
  whole sections from every check above.

Details that save a round trip: `**Breaking:**` is matched case-insensitively,
and `- ` and `* ` are both bullets. A marker inside an inline code span (any
backtick-run length) is a mention — but if a marker survives *only* inside a
span, the gate stops and asks, because a stray backtick swallowing a real
marker looks exactly the same. Add the suppression to say "mention", or fix the
backticks to say "declaration".
Fenced code blocks (``` or `~~~`) are skipped wholesale in both the changelog
and the guides — a `breaking` key in a config sample is not a breaking change,
and a guide cannot satisfy its own required headings from inside an example.
Raw HTML blocks are skipped for the same reason. They open on one of
CommonMark's block tag names — with or without trailing content, so `<div>` and
`<div>example` both start one — or on any other complete tag standing alone on
its line, which leaves `Vec<Route>` and `<MyWidget>` in prose as prose. They end
where CommonMark ends them: a `<script>`, `<style>`, `<pre>` or `<textarea>`
block at its end tag, anything else at the next blank line. So a link on the
line after `</script>` counts, and one sharing that line with it does not.

The lint is textual, so it removes the *silent* failure mode rather than
replacing review: a break described without the word "breaking" and without the
marker still needs a reviewer to catch it.
