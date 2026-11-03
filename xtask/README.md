# xtask

The Autumn workspace's maintainer task runner. Run it with `cargo xtask`
(an alias in `.cargo/config.toml`) from anywhere in the tree. It is never
published. Why it exists: [ADR 0033](../docs/adr/0033-xtask-for-repo-tooling.md).

```bash
cargo xtask list                      # every lane and every script
cargo xtask pre-push                  # compile what CI compiles, before you push
cargo xtask ci lint                   # CI's `lint` job, the same commands
cargo xtask check docs-links          # scripts/check-docs-links.sh
cargo xtask script update-changelog   # any script under scripts/
```

## Lanes

A lane is an ordered list of steps, defined in [`src/lanes.rs`](src/lanes.rs).

| Lane | Run with | Used by |
| --- | --- | --- |
| `lint` | `cargo xtask ci lint` | `ci.yml` `lint` job, `toolchain-drift.yml` |
| `pre-push` | `cargo xtask pre-push` | you, before pushing; `scripts/pre-push-check.sh` |

Options for both:

- `--list`: the step names.
- `--only <step>` / `--skip <step>` (repeatable), `--from <step>`. An unknown
  step name is an error, never a silent no-op.
- `--dry-run`: print the commands.
- `--keep-going` / `--fail-fast`: `lint` keeps going by default so one CI run
  reports every break; `pre-push` stops at the first.

Under GitHub Actions each step's log is a collapsible group, a failed step is
an error annotation, and a summary table goes to the job summary.

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

`anyhow` and `clap`, default features only, both already in `Cargo.lock`.
xtask is a workspace member, so whatever features it enables are unified into
every `cargo build --workspace`. Never add autumn-web, diesel or sqlx.

## Windows

Lanes and `cargo` steps run natively. Steps that call a `scripts/*.sh` need
`bash` on `PATH` (Git for Windows provides it); `*.py` scripts run under
`python`.
