# ADR 0033: `cargo xtask` for repository tooling

- Status: Accepted
- Date: 2026-11-03
- Deciders: Autumn maintainers
- Tags: tooling, ci, contributor-experience

## Context

The repository's maintainer tooling had no single entry point:

- `scripts/` holds 66 scripts, about 67,000 lines, almost all bash. Several
  are program-sized (`check-docs-config.sh` is over 9,000 lines), 22 call
  Python inline, and 27 carry a hand-written `--self-test` harness.
- CI's `lint` job listed its commands in `ci.yml`. `scripts/pre-push-check.sh`
  repeated them with a "keep in sync" comment, and `toolchain-drift.yml` read
  the gated feature list out of the pre-push script with `sed`.
- Rust tests read `ci.yml` to check that a command is still wired in, so a
  rule could live in three places: the YAML, a bash script, and a Rust test.
- The pre-push gate was bash only, so on Windows it ran only through Git Bash
  or WSL.

## Options

1. **Keep bash and YAML**, and add more "keep in sync" tests.
2. **`just` or `make`**: one entry point, but a new tool to install, and the
   gates stay bash.
3. **An `xtask` crate**: a workspace binary run with `cargo xtask`, the
   common Rust pattern. It needs nothing but cargo, runs on every platform,
   and its checks are tested with `cargo test`.
4. **Fold the tasks into `autumn-cli`.** `autumn-cli` ships to users;
   maintainer gates do not belong in it. Checks apps also need
   (`autumn schema width`, `autumn a11y verify`) stay there.

## Decision

Option 3, in phases.

**Phase 0 (#3590):** `xtask/` with `cargo xtask list`,
`cargo xtask script <name>` and `cargo xtask check <name>`. Scripts are
found by listing `scripts/`; the description is the script's header comment.
There is no registry to keep in step, and a test fails on a script without a
header.

**Phase 1 (#3590):** lanes are defined in Rust, in
`xtask/src/lanes.rs`.

- CI's `lint` job is `cargo xtask ci lint`. The lane runs every step even
  after a failure, folds each step's log into a GitHub group, annotates
  failures, and writes a summary table to the job summary.
- `cargo xtask pre-push` replaces the body of `scripts/pre-push-check.sh`,
  which now only calls it. It builds its clippy and compile legs from the
  `lint` lane's constants, so the two cannot drift.
- `toolchain-drift.yml` runs single steps of the `lint` lane
  (`--only clippy-gated`) instead of reading a shell variable with `sed`.
- `check-panic-gate.sh` and `check-determinism-gate.sh` prove each gated
  module is linted with its feature on. They now also read the clippy
  command literals in `lanes.rs`, and trust them only while `ci.yml` runs
  `cargo xtask ci lint`. Their self-tests cover both halves of that rule.

**Phase 2 (in progress):** port the docs gates onto one shared model of the
markdown corpus in `xtask/src/docs/`, one gate per change. A gate's script is
deleted in the same change that ports it, and only once:

- its self-test cases are `#[test]`s;
- the port gives the same report on the tree;
- the port gives the same report as the script on mutated copies of the
  corpus (broken headings and links, injected edge cases, deleted and
  duplicated lines), compared defect by defect.

Ported so far:

| Gate | Was | Now |
| --- | --- | --- |
| Markdown links and anchors | `scripts/check-docs-links.sh` | `xtask/src/docs/links.rs` (`cargo xtask check docs-links`) |
| Guide index completeness | `scripts/check-docs-guide-index.sh` | `xtask/src/docs/guide_index.rs` (`cargo xtask check docs-guide-index`) |

The per-line markdown shapes (block quote, fence, list item, heading) are
matched by hand rather than by regex. A regex there made the first port ten
times slower than the Python in a debug build; the hand matchers are tested
against the original patterns on every line of the corpus.

The guide-index gate reads links the way a renderer does, inside block
quotes, list items and raw HTML, which is finer than the line model the link
gate shares. It carries that model in its own modules
(`xtask/src/docs/guide_index/`), indexed by character as the Python was, and
uses the shared corpus listing. Its patterns run on a one-byte-per-character
view of the text, so a regex offset is a character offset and Python's `\s`
and `\d` classes keep their meaning. The script's 297 self-test cases were
captured mechanically, one `#[test]` each. The port matched the script on
2,500 mutated copies of the indexes and README, and the patterns rewritten
without lookahead matched Python's on 260,000 fuzzed inputs.

The `migration-guides` job installs a toolchain for the ported gates, after
its bash gates, so those still report in seconds.

**Later phases:** the rest of the docs gates, then the release tooling and
the other CI lanes.

## Consequences

- One command lists every maintainer task and script, and the pre-push gate
  runs natively on Windows.
- A lint check is added in one place, `lanes.rs`, next to the reason for it.
  The `lint` job shows one GitHub step; the per-check view is the log groups
  and the job summary table.
- The `lint` job compiles the xtask binary before its first check, about
  20 seconds cold and nothing on a warm cache.
- xtask is a workspace member, so its features are unified into every
  `cargo build --workspace`. It uses `anyhow`, `clap` and `regex` on default
  features, all already in the build through other crates, plus `smop` with
  only its `sh` and `fs` modules, which adds no crate but smop itself. It must
  never depend on autumn-web, diesel or sqlx.
- The two clippy gates read `lanes.rs` as text. A CI clippy command stays one
  string literal on one line; an xtask test fails when a line the gates would
  read is not a command the `lint` lane runs.
