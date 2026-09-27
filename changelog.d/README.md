# Changelog fragments

A release note goes here, in its own file. It does **not** go into
[`CHANGELOG.md`](../CHANGELOG.md).

Every open PR used to write to the top of the `## [Unreleased]` section, so
every PR conflicted with every other PR, and the conflict was never about the
code. A fragment is a file of its own, which two PRs never both edit.

## Write one

Create `changelog.d/<slug>.md`. Name it after the change — the issue number
and a few words read well:

```markdown
### Added

- **money:** typed `Money<C>` and an enforced double-entry ledger
  (issue #1837). `Money<Usd>` plus `Money<Eur>` does not compile.
```

Rules the gate enforces:

- The file name is lowercase, and ends in `.md`.
- The file opens with a `### <Kind>` heading.
- At least one `- ` bullet sits under the heading.
- No `## ` heading. That starts a release section, and would cut the
  Unreleased section in half.

A kind is one of: `Breaking Changes`, `Added`, `Changed`, `Deprecated`,
`Removed`, `Fixed`, `Security`, `Performance`, `Documentation`, `Testing`,
`Miscellaneous`. One fragment may carry more than one kind heading.

Write the bullet as you would write it in the changelog: this is the text a
reader gets. Not every change needs one — a note is for what a user of the
framework can see.

## Breaking changes

A breaking entry carries the `**Breaking:**` marker and a link to its
migration guide, exactly as it does in the changelog:

```markdown
### Breaking Changes

- **Breaking:** `with_pool` is now `with_pool_untracked`
  ([migration guide](../docs/migrations/next.md)).
```

`scripts/check-migration-guides.sh` reads the fragments together with the
changelog, so the guide is gated while the change is still in review.

## At release

`scripts/update-changelog.sh` folds every fragment into `## [Unreleased]`,
merges the kinds, and deletes the files. See
[`docs/release-checklist.md`](../docs/release-checklist.md).
