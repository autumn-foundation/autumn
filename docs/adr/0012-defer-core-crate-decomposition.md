# ADR 0012 [deferred]: Splitting the Core `autumn` Crate's Largest Modules

- Status: Deferred
- Date: 2026-09-01
- Deciders: Keystone (architecture review agent)
- Tags: crate-boundaries, modularity, deferral

## Decision under review

Whether to split `autumn/src`'s largest modules — `job.rs` (20,487 lines),
`config.rs` (16,819), `app.rs` (16,563), `router.rs` (13,072) — out of the
core crate, either into submodule files or across a crate boundary.

## Door class and reversal cost

**Two-way door.** A same-crate module split (breaking `job.rs` into a
`job/` directory of files with no public API change) is reversible in
hours and needs no review — it is ordinary refactoring. Moving a module
across a crate boundary (new `Cargo.toml`, semver surface, possible
crates.io publish) is reversible in roughly 1–2 engineer-weeks — still
well under the ~2-engineer-week bar past which this framework requires an
RFC rather than a PR-level call.

## Evidence (Tier 2 — repository record)

Reproduce with `git log --name-only` over the full history
(2026-03-20 → 2026-09-01, 1250 commits after `git fetch --unshallow`).

- `autumn/src/*.rs` files appear in 672 of 1220 file-touching commits
  (55%) — expected, since `autumn/src` is the framework's entire product
  surface.
- Within that: `app.rs` in 192 commits (28.6% of `autumn/src` commits),
  `config.rs` 155 (23.1%), `router.rs` 138 (20.5%), `job.rs` 44 (6.5%).
- Co-change is mechanical, not coordination cost: `lib.rs`↔`prelude.rs`
  move together in 84 commits (0.71 of the smaller file's commits) because
  every new public item needs an export in both — one author, one commit,
  same PR. `app.rs`↔`test.rs` (60 shared, 0.67), `app.rs`↔`session.rs`
  (30, 0.64) and `app.rs`↔`state.rs` (28, 0.60) reflect `app.rs`'s role as
  the framework's central builder that wires every subsystem in — the
  domain, not an accident of file layout.
- **Sole-author signal:** 1118 of 1250 commits (89.4%) are from one human
  (Mark Masterson); the rest are AI assistants and dependabot. There is no
  second team, so "cross-team change count" and "ownership map" (the
  metrics this framework treats as primary justification for a split) are
  undefined here — there is nothing to reduce.
- **CI resource cost:** exactly one incident in the full history, #2361
  ("Cap workspace clippy parallelism so the Lint job stops being
  OOM-killed"), fixed with a one-line parallelism cap rather than a crate
  split. One data point does not meet this framework's ≥3-data-point bar
  for an asymptotic-complexity argument.
- **Prior precedent:** `autumn-harvest` *was* extracted to its own repo in
  `ba4e3421` (2026-04-18) — but that was a dated, Tier-4 forcing fact
  (crates.io publish ordering: `autumn-web` had to publish first, then
  `autumn-harvest` against it), not a response to file-size or
  coordination pain. It shows the team already knows how and when to do
  this when a real forcing fact shows up — it doesn't show one exists now
  for `job.rs`/`config.rs`/`app.rs`/`router.rs`.

## Do nothing / decide later — 12-month baseline

Nothing in the record shows a cost being paid today. Lint/compile
resource pressure has needed one cheap, targeted fix in 5.5 months of
history. File size correlates with churn, but the churn is expected for
the crate that *is* the product; no postmortem, incident, or blocked PR
in the record names these files as the cause. There is no second
maintainer waiting on a lock or a review queue. Leaving `job.rs`,
`config.rs`, `app.rs`, and `router.rs` exactly as they are costs nothing
measurable before a second maintainer joins or a compile-time budget is
set and missed.

## Impact floor check

None of the six clearing conditions are met: no Tier‑1 incident data
exists at all (this is a framework, not an operated service); no
cross-team change count to reduce (one author); no dated Tier‑4 fact;
no Tier‑3 spike showing a committed requirement fails; no removed cost
exceeding a migration cost (the one CI cost was removed for near-zero
effort already); no ≥3-data-point asymptotic-complexity trend (one
incident). This does not clear the floor — it is not RFC-worthy.

## Default path

Leave the current single-crate structure in place. The maintainer splits
individual files into submodules opportunistically, in ordinary PRs,
whenever a specific file becomes locally painful to navigate — that is
routine refactoring and needs no architecture review.

## Seam kept open

No new seam is needed. The crate already gates optional subsystems behind
Cargo features (`db`, `ws`, `mail`, `redis`, `i18n`, `maud`, …) and already
extracts genuinely separable subsystems into their own crates when a real
reason appears — `autumn-harvest`, `autumn-search`, `autumn-cache-redis`,
`autumn-storage-s3`, `autumn-admin-plugin`, and `autumn-media-plugin` are
all already separate crates. That existing feature-gate-plus-plugin-crate
mechanism is what would make a future extraction cheap; nothing new needs
to be built now to preserve the option.

## Trigger to revisit

Revisit this decision if any of the following occurs:

- A second maintainer or team joins and produces ≥3 merge conflicts on the
  same file (`job.rs`, `config.rs`, `app.rs`, or `router.rs`) within one
  quarter.
- CI compile/link resource incidents (OOM kills, linker timeouts) recur
  ≥3 times within a rolling 90 days despite the existing parallelism cap.
- A dated product commitment requires publishing one of these subsystems
  independently, as happened with `autumn-harvest`.
