# xtask

The Autumn workspace's maintainer task runner. Run it with `cargo xtask`
(an alias in `.cargo/config.toml`) from anywhere in the tree. It is never
published. Why it exists: [ADR 0033](../docs/adr/0033-xtask-for-repo-tooling.md).

```bash
cargo xtask list                      # every lane, docs gate and script
cargo xtask pre-push                  # compile what CI compiles, before you push
cargo xtask ci lint                   # CI's `lint` job, the same commands
cargo xtask ci lint --shard gates     # one of the CI jobs it is split into
cargo xtask check docs-links          # a docs gate ported to Rust
cargo xtask check msrv                # scripts/check-msrv.sh
cargo xtask script update-changelog   # any script under scripts/
```

## Lanes

A lane is an ordered list of steps, defined in [`src/lanes.rs`](src/lanes.rs).

| Lane | Run with | Used by |
| --- | --- | --- |
| `lint` | `cargo xtask ci lint` | `ci.yml` `lint-shard` jobs (one per shard), `toolchain-drift.yml` |
| `pre-push` | `cargo xtask pre-push` | you, before pushing; `scripts/pre-push-check.sh` |

Options for both:

- `--list`: the step names, and each step's shard.
- `--only <step>` / `--skip <step>` (repeatable), `--from <step>`. An unknown
  step name is an error, never a silent no-op.
- `--shard <shard>`: the steps one CI job runs. CI runs the `lint` lane as a
  matrix of its shards, side by side. Every `lint` step names a shard, and a
  test checks that ci.yml's matrix lists every shard: the panic and determinism
  gates trust the lane's clippy commands only while it does. An unknown shard
  is an error.
- `--dry-run`: print the commands.
- `--keep-going` / `--fail-fast`: `lint` keeps going by default so one CI run
  reports every break; `pre-push` stops at the first.

Under GitHub Actions each step's log is a collapsible group, a failed step is
an error annotation, and a summary table goes to the job summary.

## Docs gates

The `scripts/check-docs-*.sh` gates are being ported to
[`src/docs/`](src/docs/), onto one shared model of the markdown corpus:

- [`corpus.rs`](src/docs/corpus.rs): the tracked `*.md` files and how they
  are decoded.
- [`markdown.rs`](src/docs/markdown.rs): live prose (fences, comments and
  code spans removed), heading anchors and link destinations.
- One module per gate: [`links.rs`](src/docs/links.rs),
  [`guide_index.rs`](src/docs/guide_index.rs) and
  [`aliases.rs`](src/docs/aliases.rs). The guide-index gate reads
  links inside block quotes, list items and raw HTML, so it carries a finer,
  character-indexed model of its own in `src/docs/guide_index/`.

`cargo xtask check <name>` runs a ported gate first and falls back to
`scripts/check-<name>.sh`, so CI and docs call a gate the same way before and
after it is ported. `cargo xtask docs <name> --root <dir>` runs one over
another checkout.

To port a gate:

1. Add `src/docs/<name>.rs` and an entry in `GATES` (`src/docs/mod.rs`),
   building on `corpus` and `markdown` rather than re-parsing.
2. Carry every case of the script's `--self-test` over as a `#[test]`.
3. Show the port and the script agree: the same report on the tree, and on
   mutated copies of the corpus, defect by defect.
4. In the same change, delete the script and point its CI step at
   `cargo xtask check <name>`. A test fails while both exist.

## Adding a check

- **A lint check:** add a step to `lint()` in `src/lanes.rs`, with a comment
  saying why. ci.yml does not change.
- **A CI clippy lane:** write the command as one string literal on one line,
  with `-p autumn-web`, a quoted `--features "…"` list and `-D warnings`.
  `scripts/check-panic-gate.sh` and `scripts/check-determinism-gate.sh` read
  those lines to prove each gated module is linted with its feature on. Do not
  write such a command anywhere else in that file; a test fails if you do.
- **A script:** put it in `scripts/` with a header comment. `cargo xtask
  list` picks it up and shows the header's first sentence.

## Dependencies

`anyhow`, `clap` and `regex` on default features, all already in the build
through other crates, and [`smop`](https://crates.io/crates/smop) with only its
`sh` and `fs` modules (`default-features = false`), which adds smop itself and
nothing else: `sh` has no dependencies and `fs` needs only `tempfile`.

- `smop::sh` runs every lane step and script: `sh::cargo()` is the cargo that
  launched xtask, and a failure names its exit code and command line.
- `smop::fs` writes the job summary and gives the tests their temp dirs.

xtask is a workspace member, so whatever features it enables are unified into
every `cargo build --workspace`. Never add autumn-web, diesel or sqlx.

## Windows

Lanes, `cargo` steps and the docs gates run natively (the docs gates need
`git` on `PATH`). Steps that call a `scripts/*.sh` need
`bash` on `PATH` (Git for Windows provides it); `*.py` scripts run under
`python`.
