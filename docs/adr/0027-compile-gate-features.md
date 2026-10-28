# ADR 0027: Gate Rarely Used Modules Behind Default-On Features

- Status: Accepted
- Date: 2026-10-28
- Deciders: Autumn maintainers
- Tags: build-time, features, cold-start

## Terms

- **Cold start**: `autumn new` to first HTTP 200, with a clean build.
- **IR**: LLVM intermediate representation that rustc makes for a crate.

## Context

Issue #2795: the cold-start build is too slow. `autumn-web` is the largest
compile unit. Earlier work gated `test` and `sim`. That gave no change.
Line count did not predict cost.

New data (no-DB daemon feature set, self-profile and IR count):

- Backend work (LLVM emit, passes, codegen) is larger than front-end work.
- `psl`, the public suffix list, makes 12% of the IR of `autumn-web`.
  Only `custom_domain` uses it.
- Serde `Deserialize` code for config structs is the next largest group.

## Planning

Brainstorming. Options: gate modules; split `autumn-web` into crates; lower
`opt-level` for dependencies; use a shared build cache; trim derive output.

Reverse brainstorming (how to make it worse):
gate with a flag that is off by default, which breaks every app; gate
without a test, so the gate rots; gate a module that is used everywhere, so
the flag does nothing; hide the change from the changelog.

Six hats:

- White (facts): profile and IR numbers above.
- Red (feeling): maintainers want a visible win. A pre-1.0 break is allowed.
- Black (risk): a module used by `app.rs` or `router.rs` can break a
  build with some flags off. The feature matrix must compile.
- Yellow (gain): apps that do not use a module do not build it. `psl` leaves
  the dependency graph.
- Green (ideas): later, split the crate. Not now.
- Blue (process): one feature per module, all in `default`, the daemon
  starter turns them off. Measure after each step. Keep a gate only if it
  helps or if it removes a dependency.

## Decision

Add 16 features: `ledger`, `experiments`, `feature-flags`, `idempotency`,
`custom-domains`, `job-tracking`, `upgrade`, `nested-form`, `alerts`,
`data-retention`, `version-history`, `wizard`, `money`, `gdpr`, `slo`,
`graph`. Each is in `default`. A spec test (`compile_gate_features`) checks
this.

Rules:

- Some features need others: `experiments` needs `feature-flags`, `ledger`
  needs `version-history`, `data-retention` needs `gdpr`, and `acme` needs
  `custom-domains`.
- `db` turns on `ledger`, `money`, `data-retention`, `idempotency` and
  `graph`. The `#[repository]` macros emit code that names them.
- `psl` is optional and `custom-domains` owns it.
- `graph` also switches on a feature of `autumn-macros`. With it off, `#[get]`
  and `#[job]` emit no route descriptor.
- With `idempotency` off, hidden stand-ins keep the macro output and the
  session layer compiling. They do nothing, because no layer can replay.
- `job-tracking` gates the status route only. The job runtime wraps tracked
  payloads in an envelope. A build without the envelope code would pass the
  wrapper to a handler, so that code stays compiled.
- `fault_injection` and `slo` share `ppm`, a small module that always builds.

## Consequences

- A default build does not change.
- An app with `default-features = false` must name the features it uses.
  The migration guide lists them.
- Without `idempotency`, the one-time submit token does nothing. The
  extractors still compile.
- The feature-combination gate must build each feature alone.
- Measured effect on `autumn-web` alone (4 vCPU, daemon feature set): see the
  PR. The IR fell by 14%, but line count did not predict time.
