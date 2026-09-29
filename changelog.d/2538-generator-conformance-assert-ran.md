### Fixed

- **generator-conformance.yml (#2538):** two silent-pass gaps closed. First,
  `autumn-cli/tests/integration/mod.rs` is now in both trigger path lists —
  the registry decides which test files compile into the `cli_tests` binary at
  all, so removing or renaming a `mod` declaration previously never fired the
  workflow. Second, every routed per-test `cargo test ... -- --ignored --exact`
  invocation now pipes through `tee` and asserts its log shows at least one
  passing test (`cargo test` exits 0 when an `--exact` filter matches zero
  tests, so a renamed or deleted test used to go silently green); `set -o
  pipefail` keeps a failing test binary failing the step through the pipe.
  `repo_hygiene.rs`'s conformance gate now pins both: the `mod.rs` trigger
  entry and a per-step assert-ran guard.
