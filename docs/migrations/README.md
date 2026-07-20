# Migration Guides

Every **major** release of Autumn ships with a migration guide describing
what changed and how to update application code. Minor and patch releases
are backwards-compatible per the [Stability Policy](../../STABILITY.md) and
do not need a dedicated guide — their changes are called out in the
[CHANGELOG](../../CHANGELOG.md).

## Index

- [`0.4.0.md`](0.4.0.md) - `autumn-web 0.3.x -> 0.4.0`
- [`0.6.0.md`](0.6.0.md) - `autumn-web 0.5.x -> 0.6.0`

- [`TEMPLATE.md`](TEMPLATE.md) — template for new migration guides. Copy
  this when drafting the guide for the next major release.

As Autumn ships `1.0.0` and beyond, each release will be indexed here, for
example:

- `1.x-to-2.0.md` — `autumn-web 1.x → 2.0.0`
- `2.x-to-3.0.md` — `autumn-web 2.x → 3.0.0`

## Process for a new major release

1. **Open a draft guide early.** The first PR that lands a breaking change
   targeting the next major copies `TEMPLATE.md` to
   `docs/migrations/<from>-to-<to>.md` and links it from this index.
2. **Grow the guide with each breaking change.** Every subsequent
   breaking-change PR appends a section with *before* / *after* snippets
   and the compiler error users will see.
3. **Polish during the prerelease window.** The `x.0.0-rc.*` tags exist so
   we can dogfood the guide against real user upgrades and fill gaps.
4. **Ship on day one.** `x.0.0` does not go out unless its migration guide
   is complete and the index above points to it.

If you are a contributor opening a breaking-change PR, adding the matching
entry to the in-flight migration guide is part of "done."
