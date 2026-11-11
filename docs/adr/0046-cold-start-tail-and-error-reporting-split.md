# ADR 0046: The Cold-Start Tail, and an `error-reporting` Feature

- Status: Accepted
- Date: 2026-11-11
- Deciders: Autumn maintainers
- Tags: build-time, features, cold-start, measurement

## Terms

- **Daemon starter**: the app that `autumn new --daemon` makes. It has no DB.
- **Unit-second**: one second of one Cargo compile unit (one crate build).
- **Tail**: build time in which one core works and the others wait.
- **Sample**: one timed run of the cold-start gate.

## Context

Issue #2795: the cold build of the daemon starter is slow. The target is p95
60 s and max 90 s on the CI gate. ADR 0027 and ADR 0045 gated modules and cut
the config IR. The 2026-11-09 scheduled run measured 101.2 s, 89.3 s and
90.0 s.

New data, daemon feature set, one 4 vCPU box, `CARGO_INCREMENTAL=0` as in CI:

- A cold build takes 80-86 s (5 samples). CI is about 1.1 times that.
- The build has three phases. Dependencies: 0 to 42 s, on four cores, about
  151 unit-seconds. `autumn-web`: 36 s, mostly on one core. The app: 2.5 s.
- `autumn-macros` is a proc-macro crate, and `autumn-web` waits for it. Cargo
  starts it late: it is ready at 27-32 s, starts 6-7 s later, and then runs
  alone for 3.4-5.2 s. That is a tail before the `autumn-web` tail.
- A model that fits: `T = (D - M - L) / 4 + M + W + A`. `D` is all dependency
  unit-seconds, `M` is `autumn-macros`, `L` is the units that start after it,
  `W` is `autumn-web`, `A` is the app. It gives 41.6 s for the first phase.
  The measured value is 41.8 s.
- So one second less in `autumn-web` or `autumn-macros` saves one second. One
  unit-second less in another dependency saves a quarter of a second.
- `-Z self-profile`: `autumn-web` has no hot function. 15,706 function bodies
  share the type-check and borrow-check time. `AppBuilder::run` (2,885 lines)
  costs 0.7 s. `cargo llvm-lines`: the largest function is 0.1% of the IR.
- The `capsule` module (failure capture) is 6.5% of the IR and 6.5% of the
  front-end time. The daemon starter compiled it only because the `reporting`
  feature gated two things: the panic-catch layer and failure capture.
- Two analysers, `query_budget.rs` and `agent_authority.rs`, are 41% of the
  non-test source of `autumn-macros`.
- No source file names `tokio-cron-scheduler`. It cost 4.5 unit-seconds.
- In each of the last three scheduled CI runs, sample 1 is the slowest, by
  6-12 s. The scaffold pins toolchain 1.97.0 and has no lockfile. The job
  installs 1.99.0. So the first `cargo` call in a scaffold can install a
  toolchain and download crates. The job log cannot show this, because the
  driver captures the output of `cargo`. With 3 samples, p95 is the slowest
  sample, so the gate reported sample 1.
- The report named `rustc 1.99.0`, read in the repository root. The scaffold
  builds with the toolchain that it pins.
- The patch that points the scaffold at the local `autumn-web` makes it a
  path dependency. Cargo builds a path dependency in incremental mode, which
  is 8-10 s slower from cold. CI was correct only because an action sets
  `CARGO_INCREMENTAL=0`.

## Planning

Brainstorming: split `autumn-web` into crates; move code out of the
proc-macro crate; gate `capsule`, `shadow`, `fault_injection`, `webhook`,
`plugin_conformance` and `stories`; remove dead dependencies; make `multer`,
the axum `ws` feature, `brotli` and HTTP/2 optional; build dependencies with
no debug info; pin the unicode-rs IDNA back end; split giant functions; a lib
plus a thin bin in the scaffold; a warm-up step in the gate; drop `--target`
from the timed build.

Reverse brainstorming (how to make it worse, or make the gate lie): a warm-up
that compiles, so the build is not cold; a workflow that names a toolchain
version, which drifts; a warm-up that hides a cost a new user pays; a gate
that takes its incremental mode from an action; move the capsule hooks to a
new feature, so an app that names `reporting` loses failure capture with no
error; a section that is set, a feature that is off, and an app that boots
and does nothing; a second multipart boundary parser in the CSRF guard; move
`axum/ws` under a feature that is not in `default`; a tighter budget with no
CI data on the changed tree; a lockfile change that is not removal-only.

Six hats:

- White (facts): the data above. No CI sample exists after ADR 0045.
- Red (feeling): a faster gate with no faster build looks like a moved goal.
  A 280-site rename in busy files is a rebase trap.
- Black (risk): a panic-catch site under the wrong feature compiles, and the
  starter loses panic handling. A budget set from a model fails on noise.
- Yellow (gain): the macro move helps every build. The report tells the truth
  about the compiler and the network time.
- Green (ideas): split `reporting` the narrow way, so no manifest changes.
  Move analysers to the support library before a new crate.
- Blue (process): one commit for each step. Test first. Measure base against
  head, interleaved. Keep the gate change apart from the real gain.

## Decision

1. **`error-reporting` feature.** It gates the `reporting` module: the
   panic-catch layer and the `ErrorReporter` pipeline. `reporting` turns it on
   and keeps the `capsule` module and its hooks. Both are in `default`. A
   manifest that names `reporting` builds the same code as before. The daemon
   starter names `error-reporting`.
2. `ErrorEvent::capsule` and `CapsuleRef` are in every build that has
   `error-reporting`. The field is `None` without `reporting`. A reporter
   compiles the same in both builds.
3. `[failure_capture]` parses in every build. Boot fails on `enabled = true`
   without `reporting`, as for `cluster` and `push`.
4. `query_budget.rs` and `agent_authority.rs` move to `autumn-macros-support`.
   That crate is a library, so Cargo builds it in the parallel phase.
   `autumn-macros` keeps the two `#[proc_macro_attribute]` entries. The two
   files are behind the `analysers` feature, which only `autumn-macros`
   turns on. `autumn-cli` links the support crate for its wire types, and
   that copy does not compile the analysers.
5. `tokio-cron-scheduler` leaves the manifests. A test fails when a
   dependency that always compiles is named by no source file.
6. The cold-start driver does one untimed preparation step for each shape:
   `cargo fetch` in a throwaway scaffold. It installs the pinned toolchain,
   updates the index and downloads the crates. It compiles nothing. The report
   shows the `hello` shape's time as `prepare_ms`, and the compiler that the
   scaffold selects. The database shape's time goes to stderr only.
   The driver sets `CARGO_INCREMENTAL=0` itself. No sample is dropped, and the
   sample count does not change.
7. New apps build their dependencies with no debug info. See "Scaffold
   profile".
8. The `ColdStartHello` budget is p95 100 s and max 120 s, from p95 125 s and
   max 150 s. The measure job does not run on a pull request, so two manual
   runs of the gate on this branch gave the samples. p95 is 30% above the
   slowest of the ten samples. Two runs on two runners differ by 25 s, so the
   margin is wider than the 20% of ADR 0045.

Tests:

- `compile_gate_features`: `error-reporting` is a default-on gate of the
  `reporting` module; `reporting` implies it and gates `capsule`;
  `[failure_capture]` has no `cfg`; each always-on dependency is named.
- The `error-reporting` lane of `test-features` compiles the daemon feature
  set and runs the reporter specs with `reporting` off.
- `autumn-macros-support` fails if an analyser file goes back.
- `dev_loop_bench` and `cold_start_driver`: the preparation command is
  `cargo fetch` in the project directory; the fetch and build commands have
  the cold environment with `CARGO_INCREMENTAL=0`; the preparation time is
  not a sample.

## Options not selected

- A `capsule` feature split from `reporting`. An app that names `reporting`
  with `default-features = false` would lose failure capture with no error.
  It is also a 280-site change.
- Gates for `shadow`, `fault_injection`, `webhook`, `plugin_conformance` and
  `stories`. Each is under 1 s, and each needs the full gate checklist.
- An own multipart boundary parser, to drop `multer`. It saves 0.6 s and puts
  a second parser on the CSRF path.
- `axum/ws` as a default-on feature (0.9 s), `validator` `card`, `brotli`,
  HTTP/2, a croner replacement. Each is small and changes a manifest contract.
- A pin on `idna_adapter` in the scaffold. Not measured, and an exact pin in
  each new app fails when a crate asks for a newer version.
- Split giant functions. The profile shows under 1 s.
- Drop `--target` from the timed build. Two samples showed no gain.
- A split of `autumn-web` into crates now. See the roadmap.

## Consequences

Measured on one 4 vCPU box: 5 interleaved samples for each tree, fully cold
target, `CARGO_INCREMENTAL=0`, the gate's `cargo build --target <host>`.

| Tree | Samples (s) | Median |
|---|---|---:|
| trunk before this change | 81.5, 81.1, 81.8, 83.1, 79.6 | 81.5 |
| this change, dependencies with line tables | 78.5, 77.5, 74.7, 73.8, 74.0 | 74.7 |
| this change, as scaffolded | 71.7, 71.4, 70.6, 71.5, 68.6 | 71.4 |

- The scaffolded build is 10.1 s faster (-12%). The code changes give 6.8 s
  of that. The scaffold profile gives 3.3 s.
- Medians of the Cargo timing reports: `autumn-web` starts at 41.5 s on
  trunk and at 39.1 s after the code changes. Its own unit takes 37.0 s on
  trunk and 33.8 s after them.
- The analyser move makes `autumn-macros-support` slower (1.4 s to 5.4 s) and
  `autumn-macros` faster (5.8 s to 4.9 s). The gain is that the larger crate
  now builds while other cores are busy. A default (database) app does not
  change: two pairs of cold builds took 145.1 and 145.1 s on trunk, and
  144.2 and 146.8 s here.
  Its macro crates wait for `diesel`, not for the support crate.
- CI, two manual runs of the gate on this branch, 5 samples each:

  | Run | Preparation | Samples (s) | p95 = max |
  |---|---:|---|---:|
  | 43101462273 | 10.6 s | 76.9, 76.3, 76.1, 75.8, 76.0 | 76.9 |
  | 43103677335 | 10.8 s | 50.4, 51.9, 51.7, 52.8, 52.7 | 52.8 |

  The last scheduled run on trunk (2026-11-09) measured 101.2 s, 89.3 s and
  90.0 s. The two runs here used two runners, and the runners differ by
  25 s. So a comparison with the scheduled run has that much uncertainty.
- The first sample is no longer the slowest by seconds. The preparation step
  took 10.6 s and 10.8 s, which is the size of the old gap (6-12 s). That
  part of the fall in p95 is not a faster build. The report shows the
  preparation time, so the cost stays visible.
- The 60 s / 90 s target is met on the faster runner (p95 52.8 s) and not on
  the slower one (p95 76.9 s). It is still open as a target for each runner.
- An app with `default-features = false`, no `reporting`, and
  `[failure_capture] enabled = true` fails boot. See `docs/migrations/next.md`.
- A new app builds its dependencies with no debug info. A backtrace frame in
  a dependency shows the function name with no file and line. The template
  comment gives the setting that brings them back.

## Scaffold profile

The maintainer chose `[profile.dev.package."*"] debug = false` for new apps.
The app keeps `line-tables-only`. An earlier report,
`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md`,
left the debug level as a maintainer decision, because a lower level costs
file and line in backtraces. This choice keeps them for the app's own frames.

## Roadmap to 60 s / 90 s

These are estimates from the model, not measurements.

- Today's work packs into 48 s on four cores at best. The target needs about
  52 s on this box. So the target needs less work in total, and a shorter
  single-core tail.
- Thin proc-macro shell: move the other modules of `autumn-macros` into a
  library. About 1 s more.
- `autumn-config` crate: `config.rs` is 17% of the IR and 11% of the
  front-end time of `autumn-web`, about 5 s. As an upstream crate it builds
  in the parallel phase and saves about 3.5 s. First move its 88 references
  to other modules under `config/`.
- A second crate for the view code (`form`, `widgets`, `a11y`), then the pure
  parts of `aeo`. Each is a few seconds.
- The hubs (`app`, `router`, `state`, `middleware`) need trait seams before a
  split.
- A second gate wave (`aeo`, `agent_authority`) needs changes in the scaffold
  template and in the route macro first.
