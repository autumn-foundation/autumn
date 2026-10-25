# ADR 0025: Generator Tests Force a Stamp Straddle; No Clock Pin

- Status: Accepted
- Date: 2026-10-25
- Deciders: Autumn maintainers
- Tags: generators, testing, migrations

## Terms

- **Stamp**: the 14-digit `YYYYMMDDHHMMSS` prefix of a migration dir. It
  comes from `generate::timestamp_now()`.
- **Straddle**: two generator runs that are in different seconds, so their
  stamps differ.

## Context

A test that compares two generator runs fails when the runs straddle a second
and a stamp is not normalized. Issue #2600 found this twice in one gate
(`scaffold_csv_import.rs`): first in the paths, then in the
`.autumn/generated.toml` body. The failure occurred only on slow runners, so
each gap stayed hidden for some time.

The tests run the `autumn` binary as a subprocess. Only an environment variable
can pin its clock.

An audit of all `autumn-cli` tests found no other test that compares two
clock-stamped runs.

## Options

1. Add a clock pin: an environment variable that `timestamp_now()` reads.
2. Read `SOURCE_DATE_EPOCH`.
3. Add a pin in debug builds only.
4. Keep the normalization. Make each compare wait for the next second between
   the runs, and assert that the stamps differ.

## Decision

Option 4.

- Option 1: if a user leaves the variable set, each new migration gets the
  same version. Diesel tracks a migration by version only, so it skips or
  rejects the second one.
- Option 2: Nix shells set `SOURCE_DATE_EPOCH` by default. This gives the
  same collision, and the user did not ask for it.
- Option 3: `cargo test --release` runs the flaky path again, with no signal.
- Option 4: no production change. A forced straddle makes the compare
  deterministic: it passes on every run or fails on every run. The stamp
  assert makes sure that the test cannot pass without a straddle.

## Consequences

- Each forced straddle adds up to 1 s to the test.
- A new test that compares two generator runs must use `tree_snapshot` and
  `wait_for_next_second` from `scaffold_csv_import.rs`. Move them to a shared
  module when a second test needs them.
- If a future need for reproducible generator output occurs, think about
  option 1 again. It then needs a guard against duplicate migration versions.
