# ADR 0037: Run the Lint Lane as Parallel Shards

- Status: Accepted
- Date: 2026-11-04
- Deciders: Autumn maintainers
- Tags: ci, build-time, xtask

## Context

CI's `Lint` job ran the whole `lint` lane of `xtask/src/lanes.rs` in one
job (ADR 0033). Every test job `needs: [lint]`, so the whole test suite
waited for it. On run 14466 it took 45 minutes on a cold cache:

| Step | Time |
|---|---|
| `fmt`, widget parses, panic, determinism, sqlite, bin-names, bolt | about 1m |
| `clippy-workspace` | 13m05s |
| `clippy-gated` | 9m04s |
| `clippy-plugin-sandbox` | 8m05s |
| `clippy-repl` | 7m08s |
| `table-width` | 5m18s |
| `a11y` | 41s |

The steps do not depend on each other. Only one runner was doing the work.

Two shell gates, `check-panic-gate.sh` and `check-determinism-gate.sh`, read
the clippy commands in `lanes.rs` as CI's enforcing lanes. They trust them
only while `ci.yml` runs the whole lane. A filtered run such as
`--only fmt` must not count, and their self-tests check that.

## Decision

- Every step of the `lint` lane names a **shard**. Each clippy lane is a shard
  of its own. The other steps share `gates`, about 7 minutes in total.
- `cargo xtask ci lint --shard <name>` runs one shard's steps. `--list` shows
  each step's shard. An unknown shard is an error that lists the real ones.
- `ci.yml` runs a `lint-shard` matrix, one job per shard (`Lint (<shard>)`).
  Each shard has its own rust-cache `key`.
- A `lint` job named `Lint` aggregates the shards, the way `test-gate`
  aggregates the test jobs. Every `needs: [lint]` and the `Lint` check name
  stay the same.
- xtask tests check that every lint step names a shard. They also check that
  the matrix lists exactly the lane's shards, once each.
- The panic and determinism gates accept the matrix line
  `cargo xtask ci lint --shard ${{ matrix.shard }}` as running the lane.
  The xtask test above is what makes that true. They still refuse `--only` and
  a literal single shard (`--shard gates`); new self-test cases check both.

## Consequences

- `Lint` should finish in about the time of its longest shard plus setup,
  instead of the sum of all steps. That longest shard is `clippy-workspace`,
  13 minutes on a cold cache.
- Five runners each compile their own dependency set. `CARGO_BUILD_JOBS: 2`
  stays on every shard; the timings above were measured with it.
- A new lint step needs a shard. A new shard needs a matrix entry. Tests fail
  on either gap.
