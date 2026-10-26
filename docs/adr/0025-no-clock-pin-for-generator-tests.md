# ADR 0025: Generator Tests Force a Stamp Straddle; No Clock Pin

- Status: Accepted
- Date: 2026-10-25
- Deciders: Autumn maintainers
- Tags: generators, testing, migrations

## Terms

- **Stamp**: the 14-digit `YYYYMMDDHHMMSS` prefix of a migration directory.
  It comes from `generate::timestamp_now()`.
- **Straddle**: two generator runs that are in different seconds, so their
  stamps differ.

## Context

A test that compares two generator runs fails when the runs straddle a second
and a stamp is not normalized. One gate (`scaffold_csv_import.rs`) had this
failure two times: in the paths, then in the `.autumn/generated.toml` body
(issues #2590 and #2600). The failure occurred only on slow runners, so each
gap stayed hidden for some time.

The tests run the `autumn` binary as a subprocess. A clock pin must then be an
environment variable or a CLI flag.

An agent audit read all `autumn-cli` tests and `src` test modules. No other
test compares stamp-bearing output of two runs. Other tests use a literal
stamp, compare files that have no stamp, or find a migration by its suffix.

## Options

1. Add an environment variable that `timestamp_now()` reads.
2. Read `SOURCE_DATE_EPOCH`.
3. Add a hidden CLI flag that sets the stamp.
4. Add a pin in debug builds only.
5. Keep the normalization. Make each comparison wait until the clock passes
   the first stamp, and assert that the stamps differ.

## Decision

Option 5.

- Option 1: if a user leaves the variable set, each new migration gets the
  same version. Diesel tracks a migration by version only, so it skips or
  rejects the second one. The current guard finds app-to-framework
  collisions only (`app_framework_version_collisions`), not app-to-app.
- Option 2: Nix shells set `SOURCE_DATE_EPOCH=315532800` by default, so the
  collision of option 1 occurs without a user action. The generated `build.rs`
  reads this variable, but there it sets build metadata. A stamp is a
  migration identity and sets the apply order.
- Option 3: a flag does not leak, but a script can still pass it two times
  and get the same collision. It adds a production surface that only tests
  use.
- Option 4: `cargo test --release` can fail at random again, and no check
  shows it.
- Option 5: no production change. A stamp that is not normalized fails on
  every run. The stamp assert makes sure that the test cannot pass without
  a straddle.

## Consequences

- Each forced straddle adds up to 1 s to the test.
- A new test that compares stamp-bearing output of two runs moves
  `tree_snapshot` and `wait_until_after` to a shared module and uses them.
- If generator output must be reproducible, examine option 1 again. It then
  needs a guard against duplicate app migration versions.
