### Breaking Changes

- **Breaking:** `[failure_capture] enabled = true` fails boot in a build
  without the `reporting` feature (issue #2795, ADR 0046). Before, that
  build did not know the section and did nothing. The section now parses in
  every build. A default build does not change
  ([migration guide](docs/migrations/next.md#features-failure-capture-needs-reporting)).

### Added

- **features:** `error-reporting` is the panic-catch layer and the
  `ErrorReporter` pipeline on their own (issue #2795, ADR 0046). `reporting`
  turns it on and adds failure capture and `autumn replay`, as before. Both
  are in `default`. `autumn new --daemon` names `error-reporting`, so the
  starter does not compile failure capture.
- **dev-loop:** the cold-start report has `prepare_ms`: the time of the
  toolchain install, the index update and the crate downloads. The driver
  does this work once before the samples, so the samples measure the build.
  The report also names the compiler that the scaffold selects, not the one
  of the repository root.

### Performance

- **build:** the no-DB daemon starter builds faster from cold (issue #2795,
  ADR 0046). The two largest macro analysers moved from `autumn-macros` to
  the `autumn-macros-support` library, which Cargo builds in parallel with
  other crates. The unused `tokio-cron-scheduler` dependency is gone. The
  daemon starter does not compile the `capsule` module.
