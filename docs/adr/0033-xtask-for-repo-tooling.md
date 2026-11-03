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

**Phase 0 (this change):** `xtask/` with `cargo xtask list`,
`cargo xtask script <name>` and `cargo xtask check <name>`. Scripts are
found by listing `scripts/`; the description is the script's header comment.
There is no registry to keep in step, and a test fails on a script without a
header.

**Phase 1 (this change):** lanes are defined in Rust, in
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

**Later phases:** port the docs gates onto one shared Rust parser, one gate
per change, deleting each bash version only once the Rust one gives the same
result on the tree. Then the release tooling and the other CI lanes.

## Consequences

- One command lists every maintainer task and script, and the pre-push gate
  runs natively on Windows.
- A lint check is added in one place, `lanes.rs`, next to the reason for it.
  The `lint` job shows one GitHub step; the per-check view is the log groups
  and the job summary table.
- The `lint` job compiles the xtask binary before its first check, about
  20 seconds cold and nothing on a warm cache.
- xtask is a workspace member, so its features are unified into every
  `cargo build --workspace`. It uses `anyhow` and `clap` on default features,
  both already in `Cargo.lock`, and must never depend on autumn-web, diesel or
  sqlx.
- The two clippy gates read `lanes.rs` as text. A CI clippy command stays one
  string literal on one line; an xtask test fails when a line the gates would
  read is not a command the `lint` lane runs.
